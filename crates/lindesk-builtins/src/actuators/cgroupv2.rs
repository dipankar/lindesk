//! The `cgroup-v2` actuator: direct cgroup v2 attribute writes for hosts without systemd.
//!
//! Targets are `Target::Cgroup { path }` relative to the cgroup mount. Values are read,
//! written and defaulted with [`CgroupFs::read_setting`], [`CgroupFs::write_setting`] and
//! [`default_setting`], so the file formats live in one place (`lindesk-sys`). A knob whose
//! attribute file is missing (controller not enabled for that cgroup) is `Unsupported`.

use std::path::PathBuf;

use lindesk_plugin_api::{Actuator, ActuatorCaps, Knob, Plugin, PluginInfo, PluginKind, PluginResult, Setting, Target};
use lindesk_sys::Roots;
use lindesk_sys::cgroupfs::{CgroupFs, default_setting, knob_file};

use super::{cgroup_knobs, gone, io_error, unsupported_knob, unsupported_target};

/// Plugin name.
pub const NAME: &str = "cgroup-v2-raw";

/// The built-in `cgroup-v2` actuator. See the [module docs](self).
#[derive(Clone, Debug)]
pub struct CgroupV2Actuator {
    fs: CgroupFs,
}

impl Default for CgroupV2Actuator {
    fn default() -> Self {
        Self::new()
    }
}

impl CgroupV2Actuator {
    /// An actuator for the host's cgroup mount.
    pub fn new() -> Self {
        Self::with_cgroup_root(Roots::host().cgroup)
    }

    /// An actuator for the hierarchy mounted at `root`.
    pub fn with_cgroup_root(root: impl Into<PathBuf>) -> Self {
        CgroupV2Actuator { fs: CgroupFs::new(root) }
    }

    /// The cgroup path and attribute file for `knob` on `target`.
    fn locate<'t>(&self, target: &'t Target, knob: &Knob) -> PluginResult<(&'t str, &'static str)> {
        let Target::Cgroup { path } = target else {
            return Err(unsupported_target(NAME, target));
        };
        let file = knob_file(knob).ok_or_else(|| unsupported_knob(NAME, knob, target))?;
        Ok((path, file))
    }
}

impl Plugin for CgroupV2Actuator {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Actuator],
            description: "Writes cgroup v2 attribute files directly (hosts without systemd)".into(),
        }
    }
}

impl Actuator for CgroupV2Actuator {
    fn caps(&self) -> ActuatorCaps {
        ActuatorCaps { knobs: cgroup_knobs(), targets: vec!["cgroup".into()] }
    }

    fn read(&mut self, target: &Target, knob: &Knob) -> PluginResult<Option<Setting>> {
        let (cg, _) = self.locate(target, knob)?;
        if !self.fs.exists(cg) {
            return Err(gone(target));
        }
        self.fs.read_setting(cg, knob).map_err(|e| io_error(NAME, knob, target, e))
    }

    fn apply(&mut self, target: &Target, setting: &Setting) -> PluginResult<()> {
        let knob = setting.knob();
        let (cg, file) = self.locate(target, &knob)?;
        if !self.fs.exists(cg) {
            return Err(gone(target));
        }
        if !self.fs.has_attr(cg, file) {
            return Err(unsupported_knob(NAME, &knob, target));
        }
        self.fs.write_setting(cg, setting).map_err(|e| io_error(NAME, &knob, target, e))
    }

    fn reset(&mut self, target: &Target, knob: &Knob, original: Option<&Setting>) -> PluginResult<()> {
        let (cg, file) = self.locate(target, knob)?;
        // A removed cgroup, or a controller that was disabled since, has nothing to restore.
        if !self.fs.has_attr(cg, file) {
            return Ok(());
        }
        let Some(setting) = original.cloned().or_else(|| default_setting(knob)) else {
            return Err(unsupported_knob(NAME, knob, target));
        };
        self.fs.write_setting(cg, &setting).map_err(|e| io_error(NAME, knob, target, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_plugin_api::{IoLimit, PluginError};
    use lindesk_sys::fixture::Fixture;

    const CG: &str = "/system.slice/build.service";

    fn setup() -> (tempfile::TempDir, Fixture, CgroupV2Actuator) {
        let dir = crate::actuators::test_dir();
        let f = Fixture::new(dir.path());
        f.cgroup(
            CG,
            &[
                ("cpu.weight", "100"),
                ("cpu.idle", "0"),
                ("cpu.uclamp.min", "0.00"),
                ("cpu.uclamp.max", "max"),
                ("io.weight", "default 100"),
                ("io.max", "259:0 rbps=max wbps=1048576 riops=max wiops=max"),
                ("memory.low", "0"),
                ("memory.high", "max"),
            ],
        );
        f.cgroup("/system.slice/no-cpu.service", &[("memory.high", "max")]);
        let a = CgroupV2Actuator::with_cgroup_root(f.roots().cgroup);
        (dir, f, a)
    }

    fn file(dir: &tempfile::TempDir, cg: &str, name: &str) -> String {
        std::fs::read_to_string(dir.path().join(format!("sys/fs/cgroup{cg}/{name}"))).unwrap()
    }

    #[test]
    fn read_apply_reset_round_trip() {
        let (dir, _f, mut a) = setup();
        let t = Target::Cgroup { path: CG.into() };
        let cases = [
            (Setting::CpuWeight { value: 20 }, "cpu.weight", "20", "100"),
            (Setting::CpuIdle { value: true }, "cpu.idle", "1", "0"),
            (Setting::UclampMax { percent: 60.0 }, "cpu.uclamp.max", "60.00", "max"),
            (Setting::IoWeight { value: 10 }, "io.weight", "default 10", "default 100"),
            (Setting::MemoryHigh { bytes: Some(1 << 30) }, "memory.high", "1073741824", "max"),
        ];
        for (setting, name, applied, restored) in cases {
            let knob = setting.knob();
            let original = a.read(&t, &knob).unwrap();
            assert!(original.is_some());
            a.apply(&t, &setting).unwrap();
            assert_eq!(file(&dir, CG, name), applied);
            assert_eq!(a.read(&t, &knob).unwrap(), Some(setting.clone()));
            a.reset(&t, &knob, original.as_ref()).unwrap();
            assert_eq!(file(&dir, CG, name), restored);
            // Reset without an original writes the kernel default.
            a.apply(&t, &setting).unwrap();
            a.reset(&t, &knob, None).unwrap();
            assert_eq!(file(&dir, CG, name), restored);
        }
        // io.max: original carries the existing wbps limit.
        let knob = Knob::IoMax("259:0".into());
        let original = a.read(&t, &knob).unwrap().unwrap();
        assert_eq!(
            original,
            Setting::IoMax { device: "259:0".into(), limit: IoLimit { wbps: Some(1 << 20), ..Default::default() } }
        );
        a.apply(
            &t,
            &Setting::IoMax { device: "259:0".into(), limit: IoLimit { rbps: Some(8 << 20), ..Default::default() } },
        )
        .unwrap();
        assert_eq!(file(&dir, CG, "io.max"), "259:0 rbps=8388608 wbps=max riops=max wiops=max");
        a.reset(&t, &knob, Some(&original)).unwrap();
        assert_eq!(file(&dir, CG, "io.max"), "259:0 rbps=max wbps=1048576 riops=max wiops=max");
    }

    #[test]
    fn unsupported_and_gone() {
        let (_dir, _f, mut a) = setup();
        let t = Target::Cgroup { path: CG.into() };
        assert!(matches!(a.apply(&t, &Setting::Nice { value: 5 }), Err(PluginError::Unsupported(_))));
        assert!(matches!(a.read(&t, &Knob::IoPrio), Err(PluginError::Unsupported(_))));
        let unit = Target::system_unit("build.service");
        assert!(matches!(a.apply(&unit, &Setting::CpuWeight { value: 5 }), Err(PluginError::Unsupported(_))));
        // Controller not enabled for this cgroup.
        let no_cpu = Target::Cgroup { path: "/system.slice/no-cpu.service".into() };
        assert!(matches!(a.apply(&no_cpu, &Setting::CpuWeight { value: 5 }), Err(PluginError::Unsupported(_))));
        assert_eq!(a.read(&no_cpu, &Knob::CpuWeight).unwrap(), None);
        a.reset(&no_cpu, &Knob::CpuWeight, None).unwrap();
        // Removed cgroup.
        let gone_t = Target::Cgroup { path: "/system.slice/gone.service".into() };
        assert!(super::super::is_gone(&a.apply(&gone_t, &Setting::CpuWeight { value: 5 }).unwrap_err()));
        assert!(super::super::is_gone(&a.read(&gone_t, &Knob::CpuWeight).unwrap_err()));
        a.reset(&gone_t, &Knob::CpuWeight, None).unwrap();
        // Out-of-range values are refused, not clamped silently.
        assert!(matches!(a.apply(&t, &Setting::CpuWeight { value: 0 }), Err(PluginError::Failed(_))));
        // Path traversal is refused.
        let evil = Target::Cgroup { path: "/../../etc".into() };
        assert!(a.apply(&evil, &Setting::CpuWeight { value: 5 }).is_err());
        assert_eq!(a.caps().knobs.len(), 8);
        assert!(a.caps().supports_target(&t) && !a.caps().supports_target(&unit));
    }
}
