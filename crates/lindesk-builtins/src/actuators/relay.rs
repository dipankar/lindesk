//! The `relay` actuator: the broker's (`lindeskd`) stand-in for user units.
//!
//! The root broker never writes user units itself: each user's session agent applies them
//! through its own user manager. Instead, `apply` and `reset` on a
//! `Target::Unit { manager: Manager::User(uid), .. }` record the change in a shared
//! [`RelayTable`]; the broker turns the table (with provenance from the engine's applied
//! state) into the per-uid `SessionState` it sends to that user's agent. `read` returns
//! `Ok(None)`: only the session agent knows (and keeps) the real originals.
//!
//! # Other targets
//!
//! Every other target (system units, cgroups, processes) is `Unsupported`. The engine then
//! asks the next capable actuator (normally `systemd` on the system bus), so the broker
//! registers this actuator ahead of the backend's actuators.

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
#[derive(Debug)]
pub struct RelayActuator {
    table: RelayTable,
}

impl Default for RelayActuator {
    fn default() -> Self {
        Self::new()
    }
}

impl RelayActuator {
    /// A relay with a fresh table.
    pub fn new() -> Self {
        RelayActuator { table: RelayTable::new() }
    }

    /// A relay recording into `table`.
    pub fn with_table(table: RelayTable) -> Self {
        RelayActuator { table }
    }

    /// The shared table.
    pub fn table(&self) -> RelayTable {
        self.table.clone()
    }

    fn user_of(target: &Target) -> Option<u32> {
        match target {
            Target::Unit { manager: Manager::User(uid), .. } => Some(*uid),
            _ => None,
        }
    }

    /// Only user units are relayed.
    fn uid(target: &Target) -> PluginResult<u32> {
        Self::user_of(target).ok_or_else(|| unsupported_target(NAME, target))
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
        PluginInfo {
            name: NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Actuator],
            description: "Relays user-unit settings to session agents".into(),
        }
    }

    fn init(&mut self, _ctx: &InitContext) -> PluginResult<()> {
        Ok(())
    }
}

impl Actuator for RelayActuator {
    fn caps(&self) -> ActuatorCaps {
        ActuatorCaps { knobs: all_knobs(), targets: vec!["unit".into()] }
    }

    fn read(&mut self, target: &Target, _knob: &Knob) -> PluginResult<Option<Setting>> {
        // The session agent reads and keeps the real original.
        Self::uid(target).map(|_| None)
    }

    fn apply(&mut self, target: &Target, setting: &Setting) -> PluginResult<()> {
        let uid = Self::uid(target)?;
        self.table.record(uid, target, setting, None);
        Ok(())
    }

    fn reset(&mut self, target: &Target, knob: &Knob, _original: Option<&Setting>) -> PluginResult<()> {
        let uid = Self::uid(target)?;
        self.table.remove(uid, target, knob);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_plugin_api::PluginError;

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
    fn other_targets_are_unsupported() {
        let mut r = RelayActuator::new();
        let sys = Target::system_unit("packagekit.service");
        assert!(matches!(r.read(&sys, &Knob::CpuWeight), Err(PluginError::Unsupported(_))));
        assert!(matches!(r.apply(&sys, &Setting::CpuWeight { value: 1 }), Err(PluginError::Unsupported(_))));
        assert!(matches!(r.reset(&sys, &Knob::CpuWeight, None), Err(PluginError::Unsupported(_))));
        assert!(r.table().snapshot().is_empty());
    }
}
