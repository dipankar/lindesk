//! The `cgroup-stat` sensor: per-target resource usage from cgroup v2 counters.
//!
//! For every [`WatchedTarget`] with a cgroup path it reads `cpu.stat` (`usage_usec`),
//! `io.stat` (summed over devices) and `memory.current` through
//! [`CgroupFs::usage`], and [`UsageTracker`] turns the cumulative counters into
//! per-interval deltas (`mem_current` is a gauge and passes through). The first poll of a
//! cgroup only sets the baseline. Missing cgroups are skipped silently and forgotten, so a
//! recreated cgroup starts from a fresh baseline; missing controller files count as zero.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use lindesk_plugin_api::{
    Plugin, PluginInfo, PluginKind, PluginResult, Sensor, SensorInput, SensorOutput, Target, TargetUsage,
};
use lindesk_sys::Roots;
use lindesk_sys::cgroupfs::{CgroupFs, UsageTracker};

/// Plugin name.
pub const NAME: &str = "cgroup-stat";

/// The built-in `cgroup-stat` sensor. See the [module docs](self).
#[derive(Debug)]
pub struct CgroupStatSensor {
    fs: CgroupFs,
    tracker: UsageTracker,
}

impl Default for CgroupStatSensor {
    fn default() -> Self {
        Self::new()
    }
}

impl CgroupStatSensor {
    /// A sensor reading the host's cgroup mount.
    pub fn new() -> Self {
        Self::with_cgroup_root(Roots::host().cgroup)
    }

    /// A sensor reading the cgroup hierarchy mounted at `root` (fixtures in tests).
    pub fn with_cgroup_root(root: impl Into<PathBuf>) -> Self {
        CgroupStatSensor { fs: CgroupFs::new(root), tracker: UsageTracker::new() }
    }

    /// Number of cgroups with a baseline.
    pub fn tracked(&self) -> usize {
        self.tracker.len()
    }
}

impl Plugin for CgroupStatSensor {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Sensor],
            description: "Per-target CPU, I/O and memory usage from cgroup v2 cpu.stat, io.stat and memory.current"
                .into(),
        }
    }
}

impl Sensor for CgroupStatSensor {
    fn poll(&mut self, input: &SensorInput) -> PluginResult<SensorOutput> {
        // Several targets may share a cgroup: read each cgroup once.
        let mut by_cgroup: BTreeMap<&str, Vec<&Target>> = BTreeMap::new();
        for w in &input.targets {
            if let Some(cg) = w.cgroup.as_deref() {
                by_cgroup.entry(cg).or_default().push(&w.target);
            }
        }
        let mut out = SensorOutput::default();
        let mut seen = BTreeSet::new();
        for (cg, targets) in by_cgroup {
            let Ok(cur) = self.fs.usage(cg) else {
                continue;
            };
            seen.insert(cg);
            if let Some(usage) = self.tracker.update(cg, input.now, cur) {
                out.usage.extend(targets.into_iter().map(|t| TargetUsage { target: t.clone(), usage: usage.clone() }));
            }
        }
        self.tracker.retain(|k| seen.contains(k));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_plugin_api::{Usage, WatchedTarget};
    use lindesk_sys::fixture::Fixture;

    const UNIT: &str = "/user.slice/user-1000.slice/user@1000.service/app.slice/app-foo-1.scope";

    fn counters(f: &Fixture, cg: &str, cpu: u64, rbytes: u64, wios: u64, mem: u64) {
        f.cgroup(
            cg,
            &[
                ("cpu.stat", &format!("usage_usec {cpu}\nuser_usec 0\nsystem_usec 0")),
                (
                    "io.stat",
                    &format!(
                        "259:0 rbytes={rbytes} wbytes=10 rios=1 wios={wios} dbytes=0 dios=0\n\
                         7:0 rbytes=1 wbytes=0 rios=0 wios=0 dbytes=0 dios=0"
                    ),
                ),
                ("memory.current", &mem.to_string()),
            ],
        );
    }

    fn watched(target: Target, cg: Option<&str>) -> WatchedTarget {
        WatchedTarget { target, cgroup: cg.map(str::to_string) }
    }

    #[test]
    fn deltas_per_target() {
        let dir = crate::actuators::test_dir();
        let f = Fixture::new(dir.path());
        counters(&f, UNIT, 1_000, 4096, 2, 100);
        f.cgroup("/system.slice/cpu-only.service", &[("cpu.stat", "usage_usec 5")]);
        let mut s = CgroupStatSensor::with_cgroup_root(f.roots().cgroup);
        let unit = Target::user_unit(1000, "app-foo-1.scope");
        let raw = Target::Cgroup { path: UNIT.into() };
        let cpu_only = Target::system_unit("cpu-only.service");
        let input = |now| SensorInput {
            now,
            targets: vec![
                watched(unit.clone(), Some(UNIT)),
                watched(raw.clone(), Some(UNIT)),
                watched(cpu_only.clone(), Some("/system.slice/cpu-only.service")),
                watched(Target::system_unit("gone.service"), Some("/system.slice/gone.service")),
                watched(Target::Process { pid: 1 }, None),
            ],
            foreground: None,
            foreground_family: Vec::new(),
        };
        assert!(s.poll(&input(1_000)).unwrap().usage.is_empty(), "baseline only");
        assert_eq!(s.tracked(), 2);

        counters(&f, UNIT, 251_000, 8192, 7, 300);
        f.cgroup("/system.slice/cpu-only.service", &[("cpu.stat", "usage_usec 15")]);
        let out = s.poll(&input(1_500)).unwrap();
        assert_eq!(out.usage.len(), 3);
        let want = Usage {
            cpu_usage_us: 250_000,
            io_rbytes: 4096,
            io_wbytes: 0,
            io_rios: 0,
            io_wios: 5,
            mem_current: 300,
            interval_ms: 500,
        };
        assert_eq!(out.usage.iter().find(|u| u.target == unit).unwrap().usage, want);
        assert_eq!(out.usage.iter().find(|u| u.target == raw).unwrap().usage, want);
        let c = &out.usage.iter().find(|u| u.target == cpu_only).unwrap().usage;
        assert_eq!((c.cpu_usage_us, c.io_rbytes, c.mem_current), (10, 0, 0));
    }

    #[test]
    fn missing_cgroups_are_skipped_and_forgotten() {
        let dir = crate::actuators::test_dir();
        let f = Fixture::new(dir.path());
        counters(&f, UNIT, 1_000_000, 0, 0, 0);
        let mut s = CgroupStatSensor::with_cgroup_root(f.roots().cgroup);
        let t = Target::user_unit(1000, "app-foo-1.scope");
        let input = |now| SensorInput {
            now,
            targets: vec![watched(t.clone(), Some(UNIT))],
            foreground: None,
            foreground_family: Vec::new(),
        };
        s.poll(&input(0)).unwrap();
        f.remove(&format!("sys/fs/cgroup{UNIT}"));
        assert!(s.poll(&input(1_000)).unwrap().usage.is_empty());
        assert_eq!(s.tracked(), 0);
        // Recreated with small counters: baseline again, not a bogus delta.
        counters(&f, UNIT, 10, 0, 0, 0);
        assert!(s.poll(&input(2_000)).unwrap().usage.is_empty());
        counters(&f, UNIT, 30, 0, 0, 0);
        assert_eq!(s.poll(&input(3_000)).unwrap().usage[0].usage.cpu_usage_us, 20);
        // Unwatched targets are dropped.
        s.poll(&SensorInput { now: 4_000, targets: vec![], foreground: None, foreground_family: Vec::new() }).unwrap();
        assert_eq!(s.tracked(), 0);
    }
}
