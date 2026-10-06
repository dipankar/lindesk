//! cgroup v1 (legacy and hybrid hierarchies): the closest equivalents of the v2 settings.
//!
//! v1 mounts one hierarchy per controller (or controller group) under the cgroup root,
//! e.g. `cpu,cpuacct/`, `blkio/`, `memory/`. Mapping from [`Setting`]:
//!
//! | Setting | v1 file | Notes |
//! |---|---|---|
//! | `CpuWeight` | `cpu.shares` | `shares = weight * 1024 / 100`, clamped to 2..=262144 |
//! | `CpuIdle` | `cpu.idle` | Linux 5.15+ |
//! | `UclampMin/Max` | `cpu.uclamp.{min,max}` | same format as v2 |
//! | `IoWeight` | `blkio.bfq.weight`, else `blkio.weight` | clamped to 1..=1000 (10..=1000 for `blkio.weight`); only BFQ honours it |
//! | `IoMax` | `blkio.throttle.{read,write}_{bps,iops}_device` | direct I/O only: v1 can't attribute buffered writeback |
//! | `MemoryHigh` | `memory.soft_limit_in_bytes` | reclaim target under global pressure, not a throttle |
//!
//! `MemoryLow` has no v1 equivalent (soft limits express the opposite: who to reclaim
//! first) and is `Unsupported`, as are the per-process knobs.

use std::io;
use std::path::{Path, PathBuf};

use lindesk_plugin_api::{IoLimit, Knob, Setting};

use super::{IoWeight, format_uclamp, parse_pid_list, parse_u64, parse_uclamp};
use crate::util::{self, check_device, invalid_data, invalid_input, read_opt, under};

/// Values at or above this in `memory.*_in_bytes` mean "unlimited" (the kernel prints
/// `PAGE_COUNTER_MAX * PAGE_SIZE`, e.g. 9223372036854771712 with 4 KiB pages).
pub const UNLIMITED_THRESHOLD: u64 = 1 << 62;

const THROTTLE_FILES: [&str; 4] = [
    "blkio.throttle.read_bps_device",
    "blkio.throttle.write_bps_device",
    "blkio.throttle.read_iops_device",
    "blkio.throttle.write_iops_device",
];

/// Convert a v2 `cpu.weight` (default 100) to v1 `cpu.shares` (default 1024).
pub fn weight_to_shares(weight: u64) -> u64 {
    (weight.saturating_mul(1024) / 100).clamp(2, 262_144)
}

/// Convert v1 `cpu.shares` to a v2 `cpu.weight`, rounding to nearest.
pub fn shares_to_weight(shares: u64) -> u64 {
    (shares.saturating_mul(100).saturating_add(512) / 1024).clamp(1, 10_000)
}

/// Access to the v1 hierarchies mounted under `root` (usually `/sys/fs/cgroup`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V1Fs {
    root: PathBuf,
}

impl V1Fs {
    /// Knobs this backend can map.
    pub const KNOBS: &'static [Knob] =
        &[Knob::CpuWeight, Knob::CpuIdle, Knob::UclampMin, Knob::UclampMax, Knob::IoWeight, Knob::MemoryHigh];

    /// Hierarchies mounted under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        V1Fs { root: root.into() }
    }

    /// The mount root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// True if this backend maps the knob (`IoMax` with any device).
    pub fn supports(knob: &Knob) -> bool {
        matches!(knob, Knob::IoMax(_)) || Self::KNOBS.contains(knob)
    }

    /// The directory of the hierarchy holding `controller` (`cpu` → `cpu,cpuacct/`).
    pub fn hierarchy(&self, controller: &str) -> Option<PathBuf> {
        let exact = self.root.join(controller);
        if exact.is_dir() {
            return Some(exact);
        }
        util::subdirs(&self.root)
            .ok()?
            .into_iter()
            .find(|d| d.split(',').any(|c| c == controller))
            .map(|d| self.root.join(d))
    }

    /// Controllers with a mounted v1 hierarchy (named hierarchies such as `systemd` and
    /// the hybrid `unified` mount excluded), sorted.
    pub fn controllers(&self) -> Vec<String> {
        let mut out: Vec<String> = util::subdirs(&self.root)
            .unwrap_or_default()
            .iter()
            .filter(|d| !matches!(d.as_str(), "unified" | "systemd") && !d.starts_with("name="))
            .flat_map(|d| d.split(',').map(str::to_string).collect::<Vec<_>>())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Path of an attribute in a controller's hierarchy.
    pub fn attr_path(&self, controller: &str, cg: &str, name: &str) -> io::Result<PathBuf> {
        util::check_attr_name(name)?;
        let h = self.hierarchy(controller).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("no cgroup v1 hierarchy for '{controller}'"))
        })?;
        Ok(under(&h, cg)?.join(name))
    }

    fn read_opt(&self, controller: &str, cg: &str, name: &str) -> io::Result<Option<String>> {
        match self.attr_path(controller, cg, name) {
            Ok(p) => read_opt(&p),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn write(&self, controller: &str, cg: &str, name: &str, value: &str) -> io::Result<()> {
        util::write_attr(&self.attr_path(controller, cg, name)?, value)
    }

    fn blkio_weight_file(&self, cg: &str) -> io::Result<Option<&'static str>> {
        for f in ["blkio.bfq.weight", "blkio.weight"] {
            if self.read_opt("blkio", cg, f)?.is_some() {
                return Ok(Some(f));
            }
        }
        Ok(None)
    }

    /// Processes in a cgroup of one controller's hierarchy.
    pub fn procs(&self, controller: &str, cg: &str) -> io::Result<Vec<i32>> {
        parse_pid_list(&util::read_trimmed(&self.attr_path(controller, cg, "cgroup.procs")?)?)
    }

    /// Move a process into a cgroup of one controller's hierarchy.
    pub fn move_pid(&self, controller: &str, cg: &str, pid: i32) -> io::Result<()> {
        if pid <= 0 {
            return Err(invalid_input(format!("invalid pid {pid}")));
        }
        self.write(controller, cg, "cgroup.procs", &pid.to_string())
    }

    /// Current value of a knob, mapped back to a v2 [`Setting`]. `Ok(None)` when the file
    /// (or hierarchy) is missing.
    pub fn read_setting(&self, cg: &str, knob: &Knob) -> io::Result<Option<Setting>> {
        Ok(match knob {
            Knob::CpuWeight => self
                .read_opt("cpu", cg, "cpu.shares")?
                .map(|s| parse_u64(&s).map(|v| Setting::CpuWeight { value: shares_to_weight(v) }))
                .transpose()?,
            Knob::CpuIdle => self
                .read_opt("cpu", cg, "cpu.idle")?
                .map(|s| parse_u64(&s).map(|v| Setting::CpuIdle { value: v != 0 }))
                .transpose()?,
            Knob::UclampMin => self
                .read_opt("cpu", cg, "cpu.uclamp.min")?
                .map(|s| parse_uclamp(&s).map(|percent| Setting::UclampMin { percent }))
                .transpose()?,
            Knob::UclampMax => self
                .read_opt("cpu", cg, "cpu.uclamp.max")?
                .map(|s| parse_uclamp(&s).map(|percent| Setting::UclampMax { percent }))
                .transpose()?,
            Knob::IoWeight => match self.blkio_weight_file(cg)? {
                Some(f) => {
                    let raw = self.read_opt("blkio", cg, f)?.unwrap_or_default();
                    Some(Setting::IoWeight { value: IoWeight::parse(&raw)?.default })
                }
                None => None,
            },
            Knob::IoMax(dev) => {
                check_device(dev)?;
                let mut vals = [None; 4];
                let mut any = false;
                for (slot, f) in vals.iter_mut().zip(THROTTLE_FILES) {
                    if let Some(raw) = self.read_opt("blkio", cg, f)? {
                        any = true;
                        *slot = parse_throttle(&raw, dev)?;
                    }
                }
                any.then(|| Setting::IoMax {
                    device: dev.clone(),
                    limit: IoLimit { rbps: vals[0], wbps: vals[1], riops: vals[2], wiops: vals[3] },
                })
            }
            Knob::MemoryHigh => self
                .read_opt("memory", cg, "memory.soft_limit_in_bytes")?
                .map(|s| parse_u64(&s).map(|v| Setting::MemoryHigh { bytes: (v < UNLIMITED_THRESHOLD).then_some(v) }))
                .transpose()?,
            Knob::MemoryLow | Knob::Nice | Knob::IoPrio | Knob::TimerSlack => return Err(unsupported(knob)),
        })
    }

    /// Apply a setting through its closest v1 equivalent (see the module docs).
    pub fn write_setting(&self, cg: &str, setting: &Setting) -> io::Result<()> {
        match setting {
            Setting::CpuWeight { value } => {
                if !(1..=10_000).contains(value) {
                    return Err(invalid_input(format!("weight {value} out of 1..=10000")));
                }
                self.write("cpu", cg, "cpu.shares", &weight_to_shares(*value).to_string())
            }
            Setting::CpuIdle { value } => self.write("cpu", cg, "cpu.idle", if *value { "1" } else { "0" }),
            Setting::UclampMin { percent } => self.write("cpu", cg, "cpu.uclamp.min", &format_uclamp(*percent, false)?),
            Setting::UclampMax { percent } => self.write("cpu", cg, "cpu.uclamp.max", &format_uclamp(*percent, true)?),
            Setting::IoWeight { value } => {
                let f = self.blkio_weight_file(cg)?.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "no blkio.bfq.weight or blkio.weight (BFQ not in use?)")
                })?;
                let lo = if f == "blkio.weight" { 10 } else { 1 };
                self.write("blkio", cg, f, &(*value).clamp(lo, 1000).to_string())
            }
            Setting::IoMax { device, limit } => {
                check_device(device)?;
                let vals = [limit.rbps, limit.wbps, limit.riops, limit.wiops];
                for (v, f) in vals.into_iter().zip(THROTTLE_FILES) {
                    // 0 removes the limit.
                    self.write("blkio", cg, f, &format!("{device} {}", v.unwrap_or(0)))?;
                }
                Ok(())
            }
            Setting::MemoryHigh { bytes } => {
                let v = bytes.map_or_else(|| "-1".to_string(), |b| b.to_string());
                self.write("memory", cg, "memory.soft_limit_in_bytes", &v)
            }
            Setting::MemoryLow { .. } | Setting::Nice { .. } | Setting::IoPrio { .. } | Setting::TimerSlack { .. } => {
                Err(unsupported(&setting.knob()))
            }
        }
    }
}

fn unsupported(knob: &Knob) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, format!("{knob} has no cgroup v1 equivalent"))
}

/// Parse a `blkio.throttle.*_device` file and return the limit for `dev` (None = no limit).
fn parse_throttle(raw: &str, dev: &str) -> io::Result<Option<u64>> {
    for line in raw.lines() {
        let mut it = line.split_whitespace();
        if it.next() == Some(dev) {
            let v = it.next().ok_or_else(|| invalid_data(format!("bad throttle line '{line}'")))?;
            let v = parse_u64(v)?;
            return Ok((v != 0).then_some(v));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, V1Fs) {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let files: [(&str, &str); 10] = [
            ("cpu,cpuacct/user.slice/cpu.shares", "1024\n"),
            ("cpu,cpuacct/user.slice/cpu.idle", "0\n"),
            ("cpu,cpuacct/user.slice/cgroup.procs", "1\n2\n"),
            ("blkio/user.slice/blkio.bfq.weight", "default 100\n"),
            ("blkio/user.slice/blkio.throttle.read_bps_device", ""),
            ("blkio/user.slice/blkio.throttle.write_bps_device", "8:0 1048576\n"),
            ("blkio/user.slice/blkio.throttle.read_iops_device", ""),
            ("blkio/user.slice/blkio.throttle.write_iops_device", ""),
            ("memory/user.slice/memory.soft_limit_in_bytes", "9223372036854771712\n"),
            ("systemd/user.slice/cgroup.procs", ""),
        ];
        for (p, c) in files {
            let p = r.join(p);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, c).unwrap();
        }
        let v = V1Fs::new(r);
        (d, v)
    }

    #[test]
    fn share_conversion() {
        assert_eq!(weight_to_shares(100), 1024);
        assert_eq!(weight_to_shares(1), 10);
        assert_eq!(weight_to_shares(10_000), 102_400);
        assert_eq!(weight_to_shares(0), 2);
        assert_eq!(shares_to_weight(1024), 100);
        assert_eq!(shares_to_weight(2), 1);
        assert_eq!(shares_to_weight(262_144), 10_000);
        for w in [1, 50, 100, 250, 1000, 10_000] {
            assert!(shares_to_weight(weight_to_shares(w)).abs_diff(w) <= 1, "{w}");
        }
    }

    #[test]
    fn hierarchies() {
        let (_d, v) = fixture();
        assert_eq!(v.controllers(), ["blkio", "cpu", "cpuacct", "memory"]);
        assert!(v.hierarchy("cpu").unwrap().ends_with("cpu,cpuacct"));
        assert!(v.hierarchy("cpuacct").is_some());
        assert!(v.hierarchy("pids").is_none());
        assert_eq!(v.procs("cpu", "/user.slice").unwrap(), [1, 2]);
        v.move_pid("cpu", "/user.slice", 77).unwrap();
        assert_eq!(v.procs("cpu", "/user.slice").unwrap(), [77]);
    }

    #[test]
    fn settings() {
        let (_d, v) = fixture();
        let cg = "/user.slice";
        assert_eq!(v.read_setting(cg, &Knob::CpuWeight).unwrap(), Some(Setting::CpuWeight { value: 100 }));
        assert_eq!(v.read_setting(cg, &Knob::IoWeight).unwrap(), Some(Setting::IoWeight { value: 100 }));
        assert_eq!(v.read_setting(cg, &Knob::MemoryHigh).unwrap(), Some(Setting::MemoryHigh { bytes: None }));
        assert_eq!(
            v.read_setting(cg, &Knob::IoMax("8:0".into())).unwrap(),
            Some(Setting::IoMax { device: "8:0".into(), limit: IoLimit { wbps: Some(1048576), ..Default::default() } })
        );
        assert_eq!(v.read_setting(cg, &Knob::UclampMin).unwrap(), None);
        assert_eq!(v.read_setting("/other", &Knob::CpuWeight).unwrap(), None);

        for s in [
            Setting::CpuWeight { value: 50 },
            Setting::CpuIdle { value: true },
            Setting::IoWeight { value: 300 },
            Setting::MemoryHigh { bytes: Some(1 << 30) },
        ] {
            v.write_setting(cg, &s).unwrap();
            assert_eq!(v.read_setting(cg, &s.knob()).unwrap(), Some(s));
        }
        let root = v.root().to_path_buf();
        let rd = |p: &str| fs::read_to_string(root.join(p)).unwrap();
        assert_eq!(rd("cpu,cpuacct/user.slice/cpu.shares"), "512");
        v.write_setting(cg, &Setting::MemoryHigh { bytes: None }).unwrap();
        assert_eq!(rd("memory/user.slice/memory.soft_limit_in_bytes"), "-1");
        v.write_setting(cg, &Setting::IoWeight { value: 5000 }).unwrap();
        assert_eq!(rd("blkio/user.slice/blkio.bfq.weight"), "1000");
        v.write_setting(
            cg,
            &Setting::IoMax { device: "8:0".into(), limit: IoLimit { riops: Some(200), ..Default::default() } },
        )
        .unwrap();
        assert_eq!(rd("blkio/user.slice/blkio.throttle.read_iops_device"), "8:0 200");
        assert_eq!(rd("blkio/user.slice/blkio.throttle.write_bps_device"), "8:0 0");

        assert_eq!(v.read_setting(cg, &Knob::MemoryLow).unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert_eq!(
            v.write_setting(cg, &Setting::MemoryLow { bytes: 1 }).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert!(v.write_setting(cg, &Setting::CpuWeight { value: 0 }).is_err());
        assert!(V1Fs::supports(&Knob::IoMax("1:2".into())) && !V1Fs::supports(&Knob::MemoryLow));
    }
}
