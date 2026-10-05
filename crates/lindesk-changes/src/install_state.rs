//! The install-time snapshot (`/var/lib/lindesk/install-state.json`): how lindesk was
//! installed and the state every conflicting priority daemon's unit was in *before*
//! lindesk, so uninstall can put back anything that changed, including changes made by hand
//! outside the versioned change system.
//!
//! Written by `install.sh` and the package scripts (`lindeskctl setup snapshot`), and by
//! `lindeskd` on its first start when it is missing. Never overwritten: a later install or
//! upgrade only appends to `upgrades`, so the original pre-lindesk state survives.

use std::path::Path;

use lindesk_plugin_api::TimestampMs;
use serde::{Deserialize, Serialize};

use crate::roots::{atomic_write, read_opt};
use crate::service::ServiceRunner;
use crate::{ChangeError, Result};

/// Where the system snapshot lives.
pub const DEFAULT_PATH: &str = "/var/lib/lindesk/install-state.json";

/// One conflicting unit as observed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UnitState {
    pub unit: String,
    pub installed: bool,
    pub enabled: bool,
    pub active: bool,
}

/// A later install or upgrade over an existing snapshot.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Upgrade {
    pub at_ms: TimestampMs,
    pub version: String,
    pub method: String,
}

/// The snapshot file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct InstallState {
    pub schema: u32,
    /// `script`, `deb`, `rpm`, `arch` or `daemon` (written by lindeskd on first start).
    pub method: String,
    pub version: String,
    pub created_ms: TimestampMs,
    /// Every conflicting unit (`lindesk_core::conflicts::conflicting_units`).
    pub units: Vec<UnitState>,
    #[serde(default)]
    pub upgrades: Vec<Upgrade>,
}

/// A unit whose current state differs from the snapshot.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Difference {
    pub unit: String,
    pub snapshot: UnitState,
    pub current: UnitState,
}

/// The current state of every conflicting unit.
pub fn observe(services: &dyn ServiceRunner) -> Vec<UnitState> {
    lindesk_core::conflicts::conflicting_units()
        .into_iter()
        .map(|u| {
            let installed = services.installed(u).unwrap_or(false);
            UnitState {
                unit: u.to_string(),
                installed,
                enabled: installed && services.is_enabled(u).unwrap_or(false),
                active: installed && services.is_active(u).unwrap_or(false),
            }
        })
        .collect()
}

/// Reads the snapshot; `None` when there is none.
pub fn load(path: &Path) -> Result<Option<InstallState>> {
    match read_opt(path)? {
        None => Ok(None),
        Some(text) => {
            serde_json::from_str(&text).map(Some).map_err(|e| ChangeError::Apply(format!("{}: {e}", path.display())))
        }
    }
}

fn save(path: &Path, s: &InstallState) -> Result<()> {
    let mut text = serde_json::to_string_pretty(s).map_err(|e| ChangeError::Apply(e.to_string()))?;
    text.push('\n');
    atomic_write(path, text.as_bytes(), 0o644)
}

/// An install or upgrade by `method`: creates the snapshot from the current state, or, when
/// one exists, keeps it and appends to `upgrades`. Returns the state and whether it was
/// created now.
pub fn record_install(
    path: &Path,
    method: &str,
    version: &str,
    services: &dyn ServiceRunner,
    now: TimestampMs,
) -> Result<(InstallState, bool)> {
    if let Some(mut s) = load(path)? {
        s.upgrades.push(Upgrade { at_ms: now, version: version.into(), method: method.into() });
        save(path, &s)?;
        return Ok((s, false));
    }
    let s = InstallState {
        schema: 1,
        method: method.into(),
        version: version.into(),
        created_ms: now,
        units: observe(services),
        upgrades: Vec::new(),
    };
    save(path, &s)?;
    Ok((s, true))
}

/// Creates the snapshot only when it is missing (lindeskd at start-up). Returns whether it
/// was created.
pub fn ensure(
    path: &Path,
    method: &str,
    version: &str,
    services: &dyn ServiceRunner,
    now: TimestampMs,
) -> Result<bool> {
    if load(path)?.is_some() {
        return Ok(false);
    }
    record_install(path, method, version, services, now).map(|(_, created)| created)
}

/// Units whose current state differs from the snapshot (installed units only).
pub fn differences(state: &InstallState, services: &dyn ServiceRunner) -> Vec<Difference> {
    let now = observe(services);
    state
        .units
        .iter()
        .filter_map(|snap| {
            let cur = now.iter().find(|c| c.unit == snap.unit)?;
            (cur.installed && (cur.enabled != snap.enabled || cur.active != snap.active)).then(|| Difference {
                unit: snap.unit.clone(),
                snapshot: snap.clone(),
                current: cur.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::FakeServices;

    #[test]
    fn created_once_then_upgrades_append() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("install-state.json");
        let svc = FakeServices::with(&[("com.system76.Scheduler.service", true, true)]);
        let (s, created) = record_install(&p, "deb", "0.1.0", &svc, 1000).unwrap();
        assert!(created);
        let s76 = s.units.iter().find(|u| u.unit == "com.system76.Scheduler.service").unwrap();
        assert!(s76.installed && s76.enabled && s76.active);
        assert!(s.units.iter().any(|u| u.unit == "ananicy.service" && !u.installed));
        assert!(!s.units.iter().any(|u| u.unit == "gamemoded.service"), "gamemode is compatible");

        // Someone (lindesk) disables it; an upgrade must not overwrite the original state.
        svc.set_enabled("com.system76.Scheduler.service", false).unwrap();
        svc.set_active("com.system76.Scheduler.service", false).unwrap();
        let (s2, created) = record_install(&p, "deb", "0.2.0", &svc, 2000).unwrap();
        assert!(!created);
        assert_eq!(s2.units, s.units, "the pre-lindesk state is kept");
        assert_eq!(s2.upgrades, vec![Upgrade { at_ms: 2000, version: "0.2.0".into(), method: "deb".into() }]);
        assert!(!ensure(&p, "daemon", "0.2.0", &svc, 3000).unwrap(), "lindeskd never overwrites it");
        assert_eq!(load(&p).unwrap().unwrap().upgrades.len(), 1);

        let diff = differences(&s2, &svc);
        assert_eq!(diff.len(), 1);
        assert_eq!((diff[0].snapshot.enabled, diff[0].current.enabled), (true, false));
    }

    #[test]
    fn ensure_creates_when_missing() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/install-state.json");
        assert!(ensure(&p, "daemon", "0.1.0", &FakeServices::default(), 5).unwrap());
        assert_eq!(load(&p).unwrap().unwrap().method, "daemon");
    }
}
