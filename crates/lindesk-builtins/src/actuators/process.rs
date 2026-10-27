//! The `process` actuator: per-process niceness, I/O priority and uclamp.
//!
//! The fallback for hosts without usable cgroup controllers, and the home of `Nice` and
//! `IoPrio` intents everywhere. Targets are `Target::Process { pid }`. Every setting is
//! applied to all threads in `/proc/<pid>/task`, because `setpriority`, `ioprio_set` and
//! `sched_setattr` act on single threads. Threads that exit meanwhile are skipped.
//!
//! | Knob | Syscall | `read` returns `None` for |
//! |---|---|---|
//! | `Nice` | `setpriority(PRIO_PROCESS, tid)` | never |
//! | `IoPrio` | `ioprio_set(IOPRIO_WHO_PROCESS, tid)` | no class set (derived from nice), or realtime |
//! | `UclampMin` / `UclampMax` | `sched_setattr` with `SCHED_FLAG_KEEP_ALL` | the default (0 / 1024) |
//!
//! **Never realtime**: [`IoPrioClass`] can't express RT and the uclamp call keeps the
//! scheduling policy. A realtime I/O priority set by someone else reads as `None`, so a reset
//! clears it to the nice-derived default instead of restoring RT.
//!
//! `reset` without an original falls back to nice 0, `ioprio_reset` and `UCLAMP_RESET`.
//! Lowering niceness needs `CAP_SYS_NICE` (or `RLIMIT_NICE`), so unprivileged hosts can
//! raise niceness but may fail to restore it.

use std::io;
use std::path::PathBuf;

use lindesk_plugin_api::{
    Actuator, ActuatorCaps, IoPrioClass, Knob, Plugin, PluginError, PluginInfo, PluginKind, PluginResult, Setting,
    Target,
};
use lindesk_sys::syscalls::{self, IoPrio, SCHED_CAPACITY_SCALE, UCLAMP_RESET};
use lindesk_sys::{Roots, procfs};

use super::{gone, unsupported_knob, unsupported_target};

/// Plugin name.
pub const NAME: &str = "proc";

/// The built-in `process` actuator. See the [module docs](self).
#[derive(Clone, Debug)]
pub struct ProcessActuator {
    proc_root: PathBuf,
}

impl Default for ProcessActuator {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessActuator {
    /// An actuator for the host's `/proc`.
    pub fn new() -> Self {
        Self::with_proc_root(Roots::host().proc)
    }

    /// An actuator whose thread lists come from `proc_root`. The syscalls themselves always
    /// act on the running kernel.
    pub fn with_proc_root(root: impl Into<PathBuf>) -> Self {
        ProcessActuator { proc_root: root.into() }
    }

    fn pid(target: &Target, knob: &Knob) -> PluginResult<i32> {
        let Target::Process { pid } = target else {
            return Err(unsupported_target(NAME, target));
        };
        if !matches!(knob, Knob::Nice | Knob::IoPrio | Knob::UclampMin | Knob::UclampMax) {
            return Err(unsupported_knob(NAME, knob, target));
        }
        if *pid <= 0 {
            return Err(PluginError::Failed(format!("{NAME}: invalid pid {pid}")));
        }
        Ok(*pid)
    }

    fn alive(&self, pid: i32) -> bool {
        self.proc_root.join(pid.to_string()).is_dir()
    }

    /// Run `f` on every thread of `pid`. Threads that vanish are skipped; a process with no
    /// thread left is gone.
    fn each_thread(&self, target: &Target, pid: i32, f: impl Fn(i32) -> io::Result<()>) -> PluginResult<()> {
        let tids = match procfs::threads(&self.proc_root, pid) {
            Ok(t) if !t.is_empty() => t,
            Ok(_) => vec![pid],
            Err(_) if !self.alive(pid) => return Err(gone(target)),
            Err(e) => return Err(PluginError::Failed(format!("{NAME}: listing threads of {pid}: {e}"))),
        };
        let mut done = 0usize;
        let mut first_err = None;
        for tid in &tids {
            match f(*tid) {
                Ok(()) => done += 1,
                Err(e) if e.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) => {}
                Err(e) => {
                    first_err.get_or_insert((*tid, e));
                }
            }
        }
        match first_err {
            Some((tid, e)) => Err(PluginError::Failed(format!(
                "{NAME}: thread {tid} of {pid}: {e} ({done}/{} threads updated)",
                tids.len()
            ))),
            None if done == 0 => Err(gone(target)),
            None => Ok(()),
        }
    }

    fn write(&self, target: &Target, pid: i32, setting: &Setting) -> PluginResult<()> {
        match setting {
            Setting::Nice { value } => self.each_thread(target, pid, |t| syscalls::set_nice(t, *value)),
            Setting::IoPrio { class, level } => {
                self.each_thread(target, pid, |t| syscalls::ioprio_set(t, *class, *level))
            }
            Setting::UclampMin { percent } => {
                let util = util(*percent)?;
                self.each_thread(target, pid, |t| syscalls::uclamp_set(t, Some(util), None))
            }
            Setting::UclampMax { percent } => {
                let util = util(*percent)?;
                self.each_thread(target, pid, |t| syscalls::uclamp_set(t, None, Some(util)))
            }
            other => Err(unsupported_knob(NAME, &other.knob(), target)),
        }
    }
}

fn util(percent: f32) -> PluginResult<u32> {
    syscalls::percent_to_util(percent).map_err(|e| PluginError::Failed(format!("{NAME}: {e}")))
}

fn syscall_error(target: &Target, what: &str, e: io::Error) -> PluginError {
    if e.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) {
        gone(target)
    } else {
        PluginError::Failed(format!("{NAME}: {what} of {target}: {e}"))
    }
}

impl Plugin for ProcessActuator {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Actuator],
            description: "Per-process nice, I/O priority and uclamp on every thread (never realtime)".into(),
        }
    }
}

impl Actuator for ProcessActuator {
    fn caps(&self) -> ActuatorCaps {
        ActuatorCaps {
            knobs: vec![Knob::Nice, Knob::IoPrio, Knob::UclampMin, Knob::UclampMax],
            targets: vec!["process".into()],
        }
    }

    fn read(&mut self, target: &Target, knob: &Knob) -> PluginResult<Option<Setting>> {
        let pid = Self::pid(target, knob)?;
        if !self.alive(pid) {
            return Err(gone(target));
        }
        Ok(match knob {
            Knob::Nice => {
                Some(Setting::Nice { value: syscalls::get_nice(pid).map_err(|e| syscall_error(target, "nice", e))? })
            }
            Knob::IoPrio => match syscalls::ioprio_get(pid).map_err(|e| syscall_error(target, "ioprio", e))? {
                IoPrio::BestEffort(level) => Some(Setting::IoPrio { class: IoPrioClass::BestEffort, level }),
                IoPrio::Idle => Some(Setting::IoPrio { class: IoPrioClass::Idle, level: 0 }),
                IoPrio::None | IoPrio::Realtime(_) => None,
            },
            Knob::UclampMin | Knob::UclampMax => {
                let u = syscalls::uclamp_get(pid).map_err(|e| syscall_error(target, "uclamp", e))?;
                if *knob == Knob::UclampMin {
                    (u.min != 0).then(|| Setting::UclampMin { percent: syscalls::util_to_percent(u.min) })
                } else {
                    (u.max < SCHED_CAPACITY_SCALE)
                        .then(|| Setting::UclampMax { percent: syscalls::util_to_percent(u.max) })
                }
            }
            _ => unreachable!("filtered by ProcessActuator::pid"),
        })
    }

    fn apply(&mut self, target: &Target, setting: &Setting) -> PluginResult<()> {
        let pid = Self::pid(target, &setting.knob())?;
        self.write(target, pid, setting)
    }

    fn reset(&mut self, target: &Target, knob: &Knob, original: Option<&Setting>) -> PluginResult<()> {
        let pid = Self::pid(target, knob)?;
        if !self.alive(pid) {
            return Ok(());
        }
        let r = match (knob, original) {
            (_, Some(s)) => self.write(target, pid, s),
            (Knob::Nice, None) => self.write(target, pid, &Setting::Nice { value: 0 }),
            (Knob::IoPrio, None) => self.each_thread(target, pid, syscalls::ioprio_reset),
            (Knob::UclampMin, None) => {
                self.each_thread(target, pid, |t| syscalls::uclamp_set(t, Some(UCLAMP_RESET), None))
            }
            (Knob::UclampMax, None) => {
                self.each_thread(target, pid, |t| syscalls::uclamp_set(t, None, Some(UCLAMP_RESET)))
            }
            _ => unreachable!("filtered by ProcessActuator::pid"),
        };
        match r {
            // Exited meanwhile: nothing left to restore.
            Err(e) if super::is_gone(&e) => Ok(()),
            r => r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};

    struct Sleeper(Child);

    impl Sleeper {
        fn spawn() -> Self {
            Sleeper(Command::new("sleep").arg("30").spawn().unwrap())
        }
        fn target(&self) -> Target {
            Target::Process { pid: self.0.id() as i32 }
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn stat_nice(pid: i32) -> i64 {
        let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let rest = &s[s.rfind(')').unwrap() + 2..];
        rest.split_whitespace().nth(16).unwrap().parse().unwrap()
    }

    #[test]
    fn nice_on_a_child() {
        let child = Sleeper::spawn();
        let t = child.target();
        let mut a = ProcessActuator::new();
        let original = a.read(&t, &Knob::Nice).unwrap().unwrap();
        let Setting::Nice { value: base } = original else { panic!() };
        // Raising niceness is always allowed.
        let up = (base + 5).min(19);
        a.apply(&t, &Setting::Nice { value: up }).unwrap();
        assert_eq!(stat_nice(child.0.id() as i32), i64::from(up));
        assert_eq!(a.read(&t, &Knob::Nice).unwrap(), Some(Setting::Nice { value: up }));
        // Restoring needs CAP_SYS_NICE or RLIMIT_NICE; either way the error is clean.
        match a.reset(&t, &Knob::Nice, Some(&original)) {
            Ok(()) => assert_eq!(stat_nice(child.0.id() as i32), i64::from(base)),
            Err(e) => assert!(matches!(e, PluginError::Failed(ref m) if m.contains("ermission")), "{e}"),
        }
    }

    #[test]
    fn ioprio_and_uclamp_on_every_thread() {
        // Our own process has several threads (the test harness).
        // Other tests in this process may change their own threads' ioprio concurrently (the
        // telemetry writer lowers itself to idle), so only check the threads we control.
        let (tx, rx) = std::sync::mpsc::channel();
        let _workers: Vec<_> = (0..2)
            .map(|_| {
                let tx = tx.clone();
                let (release, wait) = std::sync::mpsc::channel::<()>();
                let worker = std::thread::spawn(move || {
                    tx.send(rustix::thread::gettid().as_raw_nonzero().get()).unwrap();
                    // Keep our TIDs alive through every assertion, even on a loaded host.
                    // Dropping the sender also releases the worker during panic unwinding.
                    let _ = wait.recv();
                });
                (release, worker)
            })
            .collect();
        let mut ours: Vec<i32> = rx.iter().take(2).collect();
        ours.push(rustix::thread::gettid().as_raw_nonzero().get());
        let pid = std::process::id() as i32;
        let t = Target::Process { pid };
        let mut a = ProcessActuator::new();
        let original = a.read(&t, &Knob::IoPrio).unwrap();
        a.apply(&t, &Setting::IoPrio { class: IoPrioClass::BestEffort, level: 7 }).unwrap();
        for tid in procfs::threads(std::path::Path::new("/proc"), pid).unwrap() {
            if !ours.contains(&tid) {
                continue;
            }
            // Threads may exit meanwhile.
            if let Ok(p) = syscalls::ioprio_get(tid) {
                assert_eq!(p, IoPrio::BestEffort(7), "tid {tid}");
            }
        }
        a.reset(&t, &Knob::IoPrio, original.as_ref()).unwrap();
        assert_eq!(a.read(&t, &Knob::IoPrio).unwrap(), original);

        // uclamp needs CONFIG_UCLAMP_TASK; skip quietly where the kernel lacks it.
        let before = a.read(&t, &Knob::UclampMax).unwrap();
        match a.apply(&t, &Setting::UclampMax { percent: 50.0 }) {
            Ok(()) => {
                assert_eq!(syscalls::uclamp_get(pid).unwrap().max, 512);
                a.reset(&t, &Knob::UclampMax, before.as_ref()).unwrap();
                assert_eq!(a.read(&t, &Knob::UclampMax).unwrap(), before);
            }
            Err(e) => eprintln!("uclamp unavailable: {e}"),
        }
    }

    #[test]
    fn unsupported_gone_and_invalid() {
        let mut a = ProcessActuator::new();
        let unit = Target::system_unit("x.service");
        assert!(matches!(a.apply(&unit, &Setting::Nice { value: 1 }), Err(PluginError::Unsupported(_))));
        let me = Target::Process { pid: std::process::id() as i32 };
        assert!(matches!(a.apply(&me, &Setting::CpuWeight { value: 1 }), Err(PluginError::Unsupported(_))));
        assert!(matches!(a.read(&me, &Knob::MemoryHigh), Err(PluginError::Unsupported(_))));
        assert!(matches!(
            a.apply(&Target::Process { pid: 0 }, &Setting::Nice { value: 1 }),
            Err(PluginError::Failed(_))
        ));
        assert!(a.apply(&me, &Setting::UclampMin { percent: 150.0 }).is_err());

        // A pid that does not exist (in a fixture /proc): gone for read/apply, fine for reset.
        let dir = crate::actuators::test_dir();
        let mut a = ProcessActuator::with_proc_root(dir.path());
        let ghost = Target::Process { pid: 4_000_000 };
        assert!(super::super::is_gone(&a.read(&ghost, &Knob::Nice).unwrap_err()));
        assert!(super::super::is_gone(&a.apply(&ghost, &Setting::Nice { value: 3 }).unwrap_err()));
        a.reset(&ghost, &Knob::Nice, None).unwrap();
    }
}
