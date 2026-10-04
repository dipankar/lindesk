//! The `cgroup-v1` actuator: the closest cgroup v1 equivalents of lindesk's settings.
//!
//! Targets are `Target::Cgroup { path }`, the same relative path in every controller
//! hierarchy (`cpu,cpuacct/<path>`, `blkio/<path>`, `memory/<path>`). The mapping lives in
//! [`lindesk_sys::cgroupfs::v1`]: `cpu.shares`, `cpu.idle`, uclamp, BFQ/blkio weights,
//! blkio throttling and `memory.soft_limit_in_bytes`. `MemoryLow` has no v1 equivalent and
//! is `Unsupported`, like the per-process knobs.

use std::path::PathBuf;

use lindesk_plugin_api::{Actuator, ActuatorCaps, Knob, Plugin, PluginInfo, PluginKind, PluginResult, Setting, Target};
use lindesk_sys::Roots;
use lindesk_sys::cgroupfs::v1::V1Fs;
use lindesk_sys::cgroupfs::{CgroupFs, default_setting};

use super::{gone, io_error, unsupported_knob, unsupported_target};

/// Plugin name.
pub const NAME: &str = "cgroup-v1";

/// Controllers whose hierarchies may hold a target cgroup.
const CONTROLLERS: [&str; 3] = ["cpu", "blkio", "memory"];

/// The built-in `cgroup-v1` actuator. See the [module docs](self).
#[derive(Clone, Debug)]
pub struct CgroupV1Actuator {
    v1: V1Fs,
}

impl Default for CgroupV1Actuator {
    fn default() -> Self {
        Self::new()
    }
}

impl CgroupV1Actuator {
    /// An actuator for the host's v1 hierarchies under `/sys/fs/cgroup`.
    pub fn new() -> Self {
        Self::with_cgroup_root(Roots::host().cgroup)
    }

    /// An actuator for the v1 hierarchies mounted under `root`.
    pub fn with_cgroup_root(root: impl Into<PathBuf>) -> Self {
        CgroupV1Actuator { v1: V1Fs::new(root) }
    }

    /// True if the cgroup exists in at least one relevant hierarchy.
    fn exists(&self, cg: &str) -> bool {
        CONTROLLERS.iter().any(|c| self.v1.hierarchy(c).is_some_and(|h| CgroupFs::new(h).exists(cg)))
    }

    fn locate<'t>(&self, target: &'t Target, knob: &Knob) -> PluginResult<&'t str> {
        let Target::Cgroup { path } = target else {
            return Err(unsupported_target(NAME, target));
        };
        if !V1Fs::supports(knob) {
            return Err(unsupported_knob(NAME, knob, target));
        }
        Ok(path)
    }
}

impl Plugin for CgroupV1Actuator {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Actuator],
            description: "Maps settings onto legacy cgroup v1 hierarchies (cpu.shares, blkio, memory soft limits)"
                .into(),
        }
    }
}

impl Actuator for CgroupV1Actuator {
    fn caps(&self) -> ActuatorCaps {
        let mut knobs = V1Fs::KNOBS.to_vec();
        knobs.push(Knob::IoMax(String::new()));
        ActuatorCaps { knobs, targets: vec!["cgroup".into()] }
    }

    fn read(&mut self, target: &Target, knob: &Knob) -> PluginResult<Option<Setting>> {
        let cg = self.locate(target, knob)?;
        if !self.exists(cg) {
            return Err(gone(target));
        }
        self.v1.read_setting(cg, knob).map_err(|e| io_error(NAME, knob, target, e))
    }

    fn apply(&mut self, target: &Target, setting: &Setting) -> PluginResult<()> {
        let knob = setting.knob();
        let cg = self.locate(target, &knob)?;
        if !self.exists(cg) {
            return Err(gone(target));
        }
        self.v1.write_setting(cg, setting).map_err(|e| io_error(NAME, &knob, target, e))
    }

    fn reset(&mut self, target: &Target, knob: &Knob, original: Option<&Setting>) -> PluginResult<()> {
        let cg = self.locate(target, knob)?;
        if !self.exists(cg) {
            return Ok(());
        }
        let Some(setting) = original.cloned().or_else(|| default_setting(knob)) else {
            return Err(unsupported_knob(NAME, knob, target));
        };
        match self.v1.write_setting(cg, &setting) {
            // The attribute is gone (e.g. BFQ switched off): nothing left to restore.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r.map_err(|e| io_error(NAME, knob, target, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_plugin_api::{IoLimit, PluginError};
    use lindesk_sys::fixture::{self, Fixture};

    fn setup() -> (tempfile::TempDir, CgroupV1Actuator) {
        let dir = crate::actuators::test_dir();
        let (roots, _) = fixture::cgroup_v1(dir.path());
        let f = Fixture::new(dir.path());
        f.file("sys/fs/cgroup/cpu,cpuacct/user.slice/cpu.idle", "0\n");
        for name in ["read_bps_device", "write_bps_device", "read_iops_device", "write_iops_device"] {
            f.file(&format!("sys/fs/cgroup/blkio/user.slice/blkio.throttle.{name}"), "");
        }
        (dir, CgroupV1Actuator::with_cgroup_root(roots.cgroup))
    }

    fn read(dir: &tempfile::TempDir, rel: &str) -> String {
        std::fs::read_to_string(dir.path().join("sys/fs/cgroup").join(rel)).unwrap().trim().to_string()
    }

    #[test]
    fn round_trips() {
        let (dir, mut a) = setup();
        let t = Target::Cgroup { path: "/user.slice".into() };
        let original = a.read(&t, &Knob::CpuWeight).unwrap();
        assert_eq!(original, Some(Setting::CpuWeight { value: 100 }));
        a.apply(&t, &Setting::CpuWeight { value: 20 }).unwrap();
        assert_eq!(read(&dir, "cpu,cpuacct/user.slice/cpu.shares"), "204");
        a.reset(&t, &Knob::CpuWeight, original.as_ref()).unwrap();
        assert_eq!(read(&dir, "cpu,cpuacct/user.slice/cpu.shares"), "1024");

        a.apply(&t, &Setting::CpuIdle { value: true }).unwrap();
        assert_eq!(read(&dir, "cpu,cpuacct/user.slice/cpu.idle"), "1");
        a.reset(&t, &Knob::CpuIdle, None).unwrap();
        assert_eq!(read(&dir, "cpu,cpuacct/user.slice/cpu.idle"), "0");

        a.apply(&t, &Setting::IoWeight { value: 10 }).unwrap();
        assert_eq!(read(&dir, "blkio/user.slice/blkio.bfq.weight"), "10");

        let limit = IoLimit { wbps: Some(8 << 20), ..Default::default() };
        a.apply(&t, &Setting::IoMax { device: "8:0".into(), limit }).unwrap();
        assert_eq!(read(&dir, "blkio/user.slice/blkio.throttle.write_bps_device"), "8:0 8388608");
        a.reset(&t, &Knob::IoMax("8:0".into()), None).unwrap();
        assert_eq!(read(&dir, "blkio/user.slice/blkio.throttle.write_bps_device"), "8:0 0");

        a.apply(&t, &Setting::MemoryHigh { bytes: Some(1 << 30) }).unwrap();
        assert_eq!(read(&dir, "memory/user.slice/memory.soft_limit_in_bytes"), "1073741824");
        a.reset(&t, &Knob::MemoryHigh, None).unwrap();
        assert_eq!(read(&dir, "memory/user.slice/memory.soft_limit_in_bytes"), "-1");
    }

    #[test]
    fn unsupported_and_gone() {
        let (_dir, mut a) = setup();
        let t = Target::Cgroup { path: "/user.slice".into() };
        assert!(matches!(a.apply(&t, &Setting::MemoryLow { bytes: 1 }), Err(PluginError::Unsupported(_))));
        assert!(matches!(a.read(&t, &Knob::Nice), Err(PluginError::Unsupported(_))));
        assert!(matches!(
            a.apply(&Target::Process { pid: 1 }, &Setting::CpuWeight { value: 1 }),
            Err(PluginError::Unsupported(_))
        ));
        let missing = Target::Cgroup { path: "/system.slice/gone.service".into() };
        assert!(super::super::is_gone(&a.apply(&missing, &Setting::CpuWeight { value: 50 }).unwrap_err()));
        a.reset(&missing, &Knob::CpuWeight, None).unwrap();
        assert!(!a.caps().supports_knob(&Knob::MemoryLow));
        assert!(a.caps().supports_knob(&Knob::IoMax("8:0".into())));
    }
}
