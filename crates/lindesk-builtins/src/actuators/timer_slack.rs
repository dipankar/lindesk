//! The `timer-slack` actuator: [`Setting::TimerSlack`] on a unit or cgroup, by writing
//! `/proc/<tid>/timerslack_ns` for every thread in it ([`lindesk_sys::timerslack`]).
//!
//! Timer slack is a property of threads, not of a cgroup, so this is the one knob that
//! neither systemd nor cgroupfs can set. Threads created afterwards inherit the slack of
//! the thread that creates them, so setting it once covers what the unit starts later.
//! A reset writes `0`, which puts every thread back to the default it started with; there
//! is no per-thread original to remember.
//!
//! Changing another process's slack needs `CAP_SYS_NICE`, so only the broker registers
//! this actuator (the session agent's units are done from there too, not relayed).

use lindesk_plugin_api::{
    Actuator, ActuatorCaps, Knob, Plugin, PluginError, PluginInfo, PluginKind, PluginResult, Setting, Target,
};
use lindesk_sys::{Roots, procfs, timerslack};

use super::{gone, unsupported_knob};
use crate::env::{Mode, SysEnvironment, UserWatch};

/// Plugin name.
pub const NAME: &str = "timer-slack";

/// The built-in `timer-slack` actuator. See the [module docs](self).
pub struct TimerSlackActuator {
    roots: Roots,
    /// Only used to find a unit's cgroup.
    env: SysEnvironment,
}

impl Default for TimerSlackActuator {
    fn default() -> Self {
        Self::new(Roots::host())
    }
}

impl TimerSlackActuator {
    pub fn new(roots: Roots) -> Self {
        let env = SysEnvironment::new(roots.clone(), Mode::System).with_users(UserWatch::all_users());
        TimerSlackActuator { roots, env }
    }

    fn cgroup(&self, target: &Target) -> PluginResult<String> {
        match target {
            Target::Cgroup { path } => Ok(path.clone()),
            Target::Unit { .. } => self.env.unit_cgroup(target).ok_or_else(|| gone(target)),
            Target::Process { .. } => Err(super::unsupported_target(NAME, target)),
        }
    }

    fn write(&self, target: &Target, ns: u64) -> PluginResult<()> {
        let cg = self.cgroup(target)?;
        let pids = procfs::pids_in_cgroup_tree(&self.roots.cgroup, &cg).map_err(|_| gone(target))?;
        let (done, err) = timerslack::set_all(&self.roots.proc, &pids, ns);
        match err {
            // Nothing at all could be changed: say why (without CAP_SYS_NICE, every write).
            Some(e) if done.threads == 0 => {
                Err(PluginError::Failed(format!("{NAME}: setting the timer slack of {target}: {e}")))
            }
            _ => Ok(()),
        }
    }
}

impl Plugin for TimerSlackActuator {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Actuator],
            description: "Sets the timer slack of every thread of a unit (timer coalescing)".into(),
        }
    }
}

impl Actuator for TimerSlackActuator {
    fn caps(&self) -> ActuatorCaps {
        ActuatorCaps { knobs: vec![Knob::TimerSlack], targets: vec!["unit".into(), "cgroup".into()] }
    }

    /// The slack differs per thread and a reset doesn't need it: there is no original.
    fn read(&mut self, target: &Target, knob: &Knob) -> PluginResult<Option<Setting>> {
        match knob {
            Knob::TimerSlack => self.cgroup(target).map(|_| None),
            _ => Err(unsupported_knob(NAME, knob, target)),
        }
    }

    fn apply(&mut self, target: &Target, setting: &Setting) -> PluginResult<()> {
        match setting {
            Setting::TimerSlack { ns } => self.write(target, *ns),
            _ => Err(unsupported_knob(NAME, &setting.knob(), target)),
        }
    }

    fn reset(&mut self, target: &Target, knob: &Knob, _original: Option<&Setting>) -> PluginResult<()> {
        match knob {
            // A unit that is gone has no threads left to put back.
            Knob::TimerSlack => match self.write(target, 0) {
                Err(PluginError::Gone(_)) => Ok(()),
                r => r,
            },
            _ => Err(unsupported_knob(NAME, knob, target)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_sys::fixture::Fixture;

    const CG: &str = "/user.slice/user-1000.slice/user@1000.service/app.slice/app-notes-7.scope";

    fn host() -> (tempfile::TempDir, TimerSlackActuator) {
        let dir = crate::actuators::test_dir();
        let f = Fixture::new(dir.path());
        f.cgroup(CG, &[("cgroup.procs", "700\n701")]);
        for (pid, tids) in [(700, vec![700, 710]), (701, vec![701])] {
            for tid in tids {
                f.dir(&format!("proc/{pid}/task/{tid}"));
                f.file(&format!("proc/{tid}/timerslack_ns"), "50000\n");
            }
        }
        let a = TimerSlackActuator::new(f.roots());
        (dir, a)
    }

    fn slack(dir: &tempfile::TempDir, tid: i32) -> String {
        std::fs::read_to_string(dir.path().join(format!("proc/{tid}/timerslack_ns"))).unwrap()
    }

    #[test]
    fn every_thread_of_the_unit_and_back() {
        let (dir, mut a) = host();
        let unit = Target::user_unit(1000, "app-notes-7.scope");
        assert_eq!(a.read(&unit, &Knob::TimerSlack).unwrap(), None);
        a.apply(&unit, &Setting::TimerSlack { ns: 50_000_000 }).unwrap();
        for tid in [700, 710, 701] {
            assert_eq!(slack(&dir, tid), "50000000", "thread {tid}");
        }
        // The same through the cgroup path.
        a.apply(&Target::Cgroup { path: CG.into() }, &Setting::TimerSlack { ns: 20_000_000 }).unwrap();
        assert_eq!(slack(&dir, 710), "20000000");
        a.reset(&unit, &Knob::TimerSlack, None).unwrap();
        for tid in [700, 710, 701] {
            assert_eq!(slack(&dir, tid), "0", "0 = the thread's own default");
        }
    }

    #[test]
    fn only_this_knob_and_only_groups() {
        let (_dir, mut a) = host();
        let unit = Target::user_unit(1000, "app-notes-7.scope");
        assert!(matches!(a.apply(&unit, &Setting::CpuWeight { value: 50 }), Err(PluginError::Unsupported(_))));
        assert!(matches!(a.read(&unit, &Knob::CpuWeight), Err(PluginError::Unsupported(_))));
        let pid = Target::Process { pid: 700 };
        assert!(matches!(a.apply(&pid, &Setting::TimerSlack { ns: 1 }), Err(PluginError::Unsupported(_))));
        // A unit that is gone: applying says so, resetting has nothing to do.
        let gone = Target::user_unit(1000, "app-gone-1.scope");
        assert!(matches!(a.apply(&gone, &Setting::TimerSlack { ns: 1 }), Err(PluginError::Gone(_))));
        a.reset(&gone, &Knob::TimerSlack, None).unwrap();
        assert!(a.caps().supports_knob(&Knob::TimerSlack) && !a.caps().supports_knob(&Knob::CpuWeight));
    }
}
