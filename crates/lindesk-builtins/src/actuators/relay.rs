//! The `relay` actuator: the broker's (`lindeskd`) stand-in for user units.
//!
//! The root broker never writes user units itself: each user's session agent applies them
//! through its own user manager. Instead, `apply` and `reset` on a
//! `Target::Unit { manager: Manager::User(uid), .. }` record the change in a shared
//! [`RelayTable`]; the broker turns the table (with provenance from the engine's applied
//! state) into the per-uid `SessionState` it sends to that user's agent. `read` returns
//! `Ok(None)`: only the session agent knows (and keeps) the real originals.
//!
//! # Units of the system manager
//!
//! The engine picks the first actuator (by priority) whose caps cover a target *kind*, and
//! both this actuator and `systemd` act on `"unit"` targets. So the broker hands the
//! system-unit actuator to [`RelayActuator::with_fallback`]: user units are relayed, every
//! other target goes to the fallback. Without a fallback, other targets are `Unsupported`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use lindesk_plugin_api::{
    Actuator, ActuatorCaps, InitContext, Knob, Manager, Plugin, PluginInfo, PluginKind, PluginResult, Provenance,
    Setting, Target,
};

use super::{cgroup_knobs, unsupported_target};

/// Plugin name.
pub const NAME: &str = "relay";

/// One relayed value; the provenance is filled in by whoever has it (the actuator API does
/// not carry provenance).
pub type Relayed = (Setting, Option<Provenance>);

/// Relayed settings by uid, then (target, knob).
pub type RelayMap = BTreeMap<u32, BTreeMap<(Target, Knob), Relayed>>;

/// Shared table of relayed settings. Cheap to clone; clones share the table.
#[derive(Clone, Debug, Default)]
pub struct RelayTable(Arc<Mutex<RelayMap>>);

impl RelayTable {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, RelayMap> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A copy of the whole table.
    pub fn snapshot(&self) -> RelayMap {
        self.lock().clone()
    }

    /// A copy of one user's settings (empty when none).
    pub fn user(&self, uid: u32) -> BTreeMap<(Target, Knob), Relayed> {
        self.lock().get(&uid).cloned().unwrap_or_default()
    }

    /// Users with at least one relayed setting.
    pub fn uids(&self) -> Vec<u32> {
        self.lock().keys().copied().collect()
    }

    /// Forget everything relayed for `uid` (its session agent went away).
    pub fn clear_user(&self, uid: u32) {
        self.lock().remove(&uid);
    }

    /// Record a value. `provenance` may be `None`.
    pub fn record(&self, uid: u32, target: &Target, setting: &Setting, provenance: Option<Provenance>) {
        self.lock().entry(uid).or_default().insert((target.clone(), setting.knob()), (setting.clone(), provenance));
    }

    /// Drop a value.
    pub fn remove(&self, uid: u32, target: &Target, knob: &Knob) {
        let mut map = self.lock();
        if let Some(m) = map.get_mut(&uid) {
            m.remove(&(target.clone(), knob.clone()));
            if m.is_empty() {
                map.remove(&uid);
            }
        }
    }
}

/// The built-in `relay` actuator. See the [module docs](self).
pub struct RelayActuator {
    table: RelayTable,
    fallback: Option<Box<dyn Actuator>>,
}

impl std::fmt::Debug for RelayActuator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayActuator")
            .field("table", &self.table)
            .field("fallback", &self.fallback.as_ref().map(|a| a.info().name))
            .finish()
    }
}

impl Default for RelayActuator {
    fn default() -> Self {
        Self::new()
    }
}

impl RelayActuator {
    /// A relay with a fresh table and no fallback.
    pub fn new() -> Self {
        RelayActuator { table: RelayTable::new(), fallback: None }
    }

    /// A relay recording into `table`.
    pub fn with_table(table: RelayTable) -> Self {
        RelayActuator { table, fallback: None }
    }

    /// Hand every target that is not a user unit to `fallback` (normally the system
    /// `systemd` actuator). The fallback must already be initialised: [`Plugin::init`] of the
    /// relay does not forward (the fallback has its own `[plugins.<name>]` config);
    /// [`Plugin::shutdown`] does.
    pub fn with_fallback(mut self, fallback: Box<dyn Actuator>) -> Self {
        self.fallback = Some(fallback);
        self
    }

    /// The shared table.
    pub fn table(&self) -> RelayTable {
        self.table.clone()
    }

    /// Name of the fallback actuator, if any.
    pub fn fallback_name(&self) -> Option<String> {
        self.fallback.as_ref().map(|a| a.info().name)
    }

    fn user_of(target: &Target) -> Option<u32> {
        match target {
            Target::Unit { manager: Manager::User(uid), .. } => Some(*uid),
            _ => None,
        }
    }

    fn other(&mut self, target: &Target) -> PluginResult<&mut Box<dyn Actuator>> {
        self.fallback.as_mut().ok_or_else(|| unsupported_target(NAME, target))
    }
}

/// Every knob (`IoMax` with an empty device, as [`ActuatorCaps`] expects).
fn all_knobs() -> Vec<Knob> {
    let mut k = cgroup_knobs();
    k.extend([Knob::Nice, Knob::IoPrio]);
    k
}

impl Plugin for RelayActuator {
    fn info(&self) -> PluginInfo {
        let description = match self.fallback_name() {
            Some(f) => format!("Relays user-unit settings to session agents; other targets via '{f}'"),
            None => "Relays user-unit settings to session agents".into(),
        };
        PluginInfo {
            name: NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Actuator],
            description,
        }
    }

    fn init(&mut self, _ctx: &InitContext) -> PluginResult<()> {
        Ok(())
    }

    fn shutdown(&mut self) {
        if let Some(f) = self.fallback.as_mut() {
            f.shutdown();
        }
    }
}

impl Actuator for RelayActuator {
    fn caps(&self) -> ActuatorCaps {
        let mut caps = ActuatorCaps { knobs: all_knobs(), targets: vec!["unit".into()] };
        if let Some(f) = &self.fallback {
            for t in f.caps().targets {
                if !caps.targets.contains(&t) {
                    caps.targets.push(t);
                }
            }
        }
        caps
    }

    fn read(&mut self, target: &Target, knob: &Knob) -> PluginResult<Option<Setting>> {
        match Self::user_of(target) {
            // The session agent reads and keeps the real original.
            Some(_) => Ok(None),
            None => self.other(target)?.read(target, knob),
        }
    }

    fn apply(&mut self, target: &Target, setting: &Setting) -> PluginResult<()> {
        match Self::user_of(target) {
            Some(uid) => {
                self.table.record(uid, target, setting, None);
                Ok(())
            }
            None => self.other(target)?.apply(target, setting),
        }
    }

    fn reset(&mut self, target: &Target, knob: &Knob, original: Option<&Setting>) -> PluginResult<()> {
        match Self::user_of(target) {
            Some(uid) => {
                self.table.remove(uid, target, knob);
                Ok(())
            }
            None => self.other(target)?.reset(target, knob, original),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_plugin_api::PluginError;

    /// Records calls; accepts system units only.
    #[derive(Default)]
    struct Fake(Arc<Mutex<Vec<String>>>);

    impl Plugin for Fake {
        fn info(&self) -> PluginInfo {
            PluginInfo {
                name: "fake".into(),
                version: "0".into(),
                kinds: vec![PluginKind::Actuator],
                description: String::new(),
            }
        }
    }

    impl Actuator for Fake {
        fn caps(&self) -> ActuatorCaps {
            ActuatorCaps { knobs: vec![Knob::CpuWeight], targets: vec!["unit".into(), "cgroup".into()] }
        }
        fn read(&mut self, t: &Target, k: &Knob) -> PluginResult<Option<Setting>> {
            self.0.lock().unwrap().push(format!("read {t} {k}"));
            Ok(Some(Setting::CpuWeight { value: 100 }))
        }
        fn apply(&mut self, t: &Target, s: &Setting) -> PluginResult<()> {
            self.0.lock().unwrap().push(format!("apply {t} {s}"));
            Ok(())
        }
        fn reset(&mut self, t: &Target, k: &Knob, _o: Option<&Setting>) -> PluginResult<()> {
            self.0.lock().unwrap().push(format!("reset {t} {k}"));
            Ok(())
        }
    }

    #[test]
    fn records_user_units_and_reads_nothing() {
        let mut r = RelayActuator::new();
        let table = r.table();
        let app = Target::user_unit(1000, "app-x.scope");
        let bg = Target::user_unit(1001, "background.slice");
        assert_eq!(r.read(&app, &Knob::CpuWeight).unwrap(), None);
        r.apply(&app, &Setting::CpuWeight { value: 300 }).unwrap();
        r.apply(&app, &Setting::IoWeight { value: 300 }).unwrap();
        r.apply(&app, &Setting::CpuWeight { value: 400 }).unwrap();
        r.apply(&bg, &Setting::CpuIdle { value: true }).unwrap();
        assert_eq!(table.uids(), vec![1000, 1001]);
        let u = table.user(1000);
        assert_eq!(u.len(), 2);
        assert_eq!(u[&(app.clone(), Knob::CpuWeight)].0, Setting::CpuWeight { value: 400 });
        r.reset(&app, &Knob::CpuWeight, None).unwrap();
        r.reset(&app, &Knob::IoWeight, None).unwrap();
        assert!(table.user(1000).is_empty());
        assert_eq!(table.uids(), vec![1001], "empty users are dropped");
        table.clear_user(1001);
        assert!(table.snapshot().is_empty());
        // Resetting something unknown is fine.
        r.reset(&app, &Knob::MemoryLow, None).unwrap();
    }

    #[test]
    fn caps_cover_every_knob_on_units() {
        let r = RelayActuator::new();
        let caps = r.caps();
        for k in [Knob::CpuWeight, Knob::IoMax("8:0".into()), Knob::MemoryHigh, Knob::Nice, Knob::IoPrio] {
            assert!(caps.supports_knob(&k), "{k}");
        }
        assert!(caps.supports_target(&Target::user_unit(1, "a.scope")));
        assert!(!caps.supports_target(&Target::Cgroup { path: "/x".into() }));
        assert_eq!(r.info().name, NAME);
    }

    #[test]
    fn other_targets_go_to_the_fallback() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut r = RelayActuator::new().with_fallback(Box::new(Fake(log.clone())));
        assert_eq!(r.fallback_name().as_deref(), Some("fake"));
        assert!(r.caps().supports_target(&Target::Cgroup { path: "/x".into() }));
        let sys = Target::system_unit("packagekit.service");
        assert_eq!(r.read(&sys, &Knob::CpuWeight).unwrap(), Some(Setting::CpuWeight { value: 100 }));
        r.apply(&sys, &Setting::CpuWeight { value: 10 }).unwrap();
        r.reset(&sys, &Knob::CpuWeight, None).unwrap();
        r.apply(&Target::user_unit(5, "a.scope"), &Setting::CpuWeight { value: 10 }).unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "read system:packagekit.service cpu.weight",
                "apply system:packagekit.service cpu.weight=10",
                "reset system:packagekit.service cpu.weight"
            ]
        );
        assert!(r.table().user(5).len() == 1);
        let mut bare = RelayActuator::new();
        assert!(matches!(bare.apply(&sys, &Setting::CpuWeight { value: 1 }), Err(PluginError::Unsupported(_))));
    }
}
