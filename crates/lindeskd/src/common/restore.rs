//! `--restore`: undo everything recorded in an applied-state file, without a running daemon.
//!
//! Used by `ExecStopPost=` (after a clean stop or a crash) and by hand. It loads the state
//! file into a bare engine that has only actuators, reverts every entry and rewrites the
//! file with whatever could not be restored (normally nothing). A missing or empty file means
//! there is nothing to do. Running it twice is harmless.

use std::path::Path;
use std::time::Duration;

use lindesk_core::config::Config;
use lindesk_host::{ActionOutcome, Engine, EngineOptions, MemoryEnvironment};
use lindesk_plugin_api::{Actuator, Privilege};

/// Hard limit for a restore run; systemd's stop timeout must not be reached.
pub const RESTORE_TIMEOUT: Duration = Duration::from_secs(25);

/// What a restore did.
#[derive(Debug, Default, PartialEq)]
pub struct RestoreOutcome {
    /// Entries found in the state file.
    pub entries: usize,
    pub restored: usize,
    /// Human-readable failures; their entries stay in the state file.
    pub failed: Vec<String>,
}

impl RestoreOutcome {
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Revert every entry of `state_path` with `actuators` (highest priority first).
pub fn restore_state(
    state_path: &Path,
    host: &str,
    privilege: Privilege,
    uid: Option<u32>,
    actuators: Vec<Box<dyn Actuator>>,
) -> anyhow::Result<RestoreOutcome> {
    if !state_path.exists() {
        return Ok(RestoreOutcome::default());
    }
    let mut opts = EngineOptions::new(host, privilege);
    opts.uid = uid;
    opts.state_path = Some(state_path.to_path_buf());
    let mut config = Config::default();
    config.telemetry.enabled = false;
    let mut engine = Engine::new(config, Box::new(MemoryEnvironment::new()), opts)?;
    let n = actuators.len() as i32;
    for (i, a) in actuators.into_iter().enumerate() {
        let name = a.info().name;
        if let Err(e) = engine.registry_mut().register_actuator(a, 100 + n - i as i32) {
            tracing::warn!("actuator {name}: {e}");
        }
    }
    let entries = engine.load_state(state_path)?;
    let mut out = RestoreOutcome { entries, ..Default::default() };
    if entries == 0 {
        return Ok(out);
    }
    for a in engine.restore_all() {
        match &a.outcome {
            ActionOutcome::Reverted => {
                tracing::info!("{a}");
                out.restored += 1;
            }
            _ => {
                tracing::warn!("{a}");
                out.failed.push(a.to_string());
            }
        }
    }
    engine.registry_mut().shutdown();
    Ok(out)
}

/// Exit the process if a restore takes longer than [`RESTORE_TIMEOUT`].
pub fn arm_timeout(what: &'static str) {
    std::thread::spawn(move || {
        std::thread::sleep(RESTORE_TIMEOUT);
        tracing::error!("{what} did not finish within {RESTORE_TIMEOUT:?}; giving up");
        std::process::exit(1);
    });
}

#[cfg(test)]
pub mod tests {
    use std::sync::{Arc, Mutex};

    use lindesk_core::reconcile::AppliedState;
    use lindesk_plugin_api::{
        ActuatorCaps, Knob, Plugin, PluginError, PluginInfo, PluginKind, PluginResult, Provenance, Setting, Target,
    };

    use super::*;

    /// A fake actuator for units; `fail` makes resets of that unit fail.
    #[derive(Clone, Default)]
    pub struct FakeActuator {
        pub log: Arc<Mutex<Vec<String>>>,
        pub fail: Option<String>,
    }

    impl Plugin for FakeActuator {
        fn info(&self) -> PluginInfo {
            PluginInfo {
                name: "fake".into(),
                version: "0".into(),
                kinds: vec![PluginKind::Actuator],
                description: String::new(),
            }
        }
    }

    impl Actuator for FakeActuator {
        fn caps(&self) -> ActuatorCaps {
            ActuatorCaps { knobs: vec![Knob::CpuWeight, Knob::IoWeight], targets: vec!["unit".into()] }
        }
        fn read(&mut self, _t: &Target, _k: &Knob) -> PluginResult<Option<Setting>> {
            Ok(None)
        }
        fn apply(&mut self, t: &Target, s: &Setting) -> PluginResult<()> {
            self.log.lock().unwrap().push(format!("apply {t} {s}"));
            Ok(())
        }
        fn reset(&mut self, t: &Target, k: &Knob, o: Option<&Setting>) -> PluginResult<()> {
            if self.fail.as_deref().is_some_and(|f| t.to_string().contains(f)) {
                return Err(PluginError::Failed("nope".into()));
            }
            let to = o.map_or("default".to_string(), ToString::to_string);
            self.log.lock().unwrap().push(format!("reset {t} {k} -> {to}"));
            Ok(())
        }
    }

    pub fn write_state(path: &Path) {
        let mut st = AppliedState::default();
        let p = Provenance { plugin: "threshold".into(), ..Default::default() };
        st.record_apply(
            &Target::system_unit("packagekit.service"),
            &Setting::CpuWeight { value: 10 },
            Some(Setting::CpuWeight { value: 100 }),
            &p,
            1,
        );
        st.record_apply(&Target::system_unit("updatedb.service"), &Setting::IoWeight { value: 10 }, None, &p, 2);
        std::fs::write(path, st.to_json()).unwrap();
    }

    #[test]
    fn restores_everything_and_is_idempotent() {
        let dir = crate::common::test_dir();
        let path = dir.path().join("state.json");
        write_state(&path);
        let fake = FakeActuator::default();
        let out = restore_state(&path, "lindeskd", Privilege::System, None, vec![Box::new(fake.clone())]).unwrap();
        assert_eq!(out, RestoreOutcome { entries: 2, restored: 2, failed: vec![] });
        let mut log = fake.log.lock().unwrap().clone();
        log.sort();
        assert_eq!(
            log,
            vec![
                "reset system:packagekit.service cpu.weight -> cpu.weight=100",
                "reset system:updatedb.service io.weight -> default"
            ]
        );
        let left = AppliedState::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(left.is_empty(), "state file emptied");

        // Second run: nothing to do.
        let again = restore_state(&path, "lindeskd", Privilege::System, None, vec![Box::new(fake.clone())]).unwrap();
        assert_eq!(again, RestoreOutcome::default());
        assert_eq!(fake.log.lock().unwrap().len(), 2);
    }

    #[test]
    fn missing_file_is_nothing_to_do() {
        let dir = crate::common::test_dir();
        let path = dir.path().join("nope.json");
        let out = restore_state(&path, "lindeskd", Privilege::System, None, vec![]).unwrap();
        assert_eq!(out, RestoreOutcome::default());
        assert!(!path.exists(), "no file is created");
    }

    #[test]
    fn failures_stay_in_the_file() {
        let dir = crate::common::test_dir();
        let path = dir.path().join("state.json");
        write_state(&path);
        let fake = FakeActuator { fail: Some("updatedb".into()), ..Default::default() };
        let out = restore_state(&path, "lindeskd", Privilege::System, None, vec![Box::new(fake)]).unwrap();
        assert_eq!((out.entries, out.restored, out.failed.len()), (2, 1, 1));
        let left = AppliedState::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left.entries().next().unwrap().target, Target::system_unit("updatedb.service"));
        // Without any actuator nothing can be restored, and nothing is lost.
        let out = restore_state(&path, "lindeskd", Privilege::System, None, vec![]).unwrap();
        assert_eq!((out.entries, out.restored, out.failed.len()), (1, 0, 1));
        assert_eq!(AppliedState::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap().len(), 1);
    }

    #[test]
    fn corrupt_file_is_an_error() {
        let dir = crate::common::test_dir();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(restore_state(&path, "lindeskd", Privilege::System, None, vec![]).is_err());
    }
}
