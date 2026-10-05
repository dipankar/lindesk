//! systemd units of conflicting priority daemons, for the `service_state` op and the
//! install-time snapshot. All access goes through a [`ServiceRunner`], so tests (and fixture
//! runs without systemd as PID 1) inject their own.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use crate::{ChangeError, Result};

/// Reads and changes a unit's enablement and activity.
pub trait ServiceRunner: Send + Sync {
    /// The unit file exists.
    fn installed(&self, unit: &str) -> Result<bool>;
    /// `systemctl is-enabled` says enabled (or enabled-runtime, alias).
    fn is_enabled(&self, unit: &str) -> Result<bool>;
    /// `systemctl is-active` says active (or activating, reloading).
    fn is_active(&self, unit: &str) -> Result<bool>;
    fn set_enabled(&self, unit: &str, enabled: bool) -> Result<()>;
    fn set_active(&self, unit: &str, active: bool) -> Result<()>;
}

/// Environment variable naming the `systemctl` to run (default: `systemctl` on PATH). Lets
/// a fixture stand in where systemd isn't PID 1 (containers, tests).
pub const SYSTEMCTL_ENV: &str = "LINDESK_SYSTEMCTL";

/// The real thing: runs `systemctl`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Systemctl {
    pub program: PathBuf,
}

impl Systemctl {
    /// `$LINDESK_SYSTEMCTL`, else `systemctl`.
    pub fn from_env() -> Self {
        let program = std::env::var_os(SYSTEMCTL_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("systemctl"));
        Systemctl { program }
    }

    fn query(&self, verb: &str, unit: &str) -> Result<String> {
        let o = Command::new(&self.program)
            .args([verb, unit])
            .output()
            .map_err(|e| ChangeError::Apply(format!("running {} {verb} {unit}: {e}", self.program.display())))?;
        // is-enabled/is-active exit non-zero for "disabled"/"inactive"; the word is on stdout.
        let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if out.is_empty() {
            let err = String::from_utf8_lossy(&o.stderr);
            if err.contains("No such file") || err.contains("not found") {
                return Ok("not-found".into());
            }
            return Err(ChangeError::Apply(format!("systemctl {verb} {unit}: {}", err.trim())));
        }
        Ok(out.lines().next().unwrap_or_default().to_string())
    }

    fn run(&self, verb: &str, unit: &str) -> Result<()> {
        let o = Command::new(&self.program)
            .args([verb, "--quiet", unit])
            .output()
            .map_err(|e| ChangeError::Apply(format!("running {} {verb} {unit}: {e}", self.program.display())))?;
        if o.status.success() {
            Ok(())
        } else {
            Err(ChangeError::Apply(format!(
                "systemctl {verb} {unit} failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            )))
        }
    }
}

impl ServiceRunner for Systemctl {
    fn installed(&self, unit: &str) -> Result<bool> {
        Ok(self.query("is-enabled", unit)? != "not-found")
    }

    fn is_enabled(&self, unit: &str) -> Result<bool> {
        Ok(matches!(self.query("is-enabled", unit)?.as_str(), "enabled" | "enabled-runtime" | "alias"))
    }

    fn is_active(&self, unit: &str) -> Result<bool> {
        Ok(matches!(self.query("is-active", unit)?.as_str(), "active" | "activating" | "reloading"))
    }

    fn set_enabled(&self, unit: &str, enabled: bool) -> Result<()> {
        self.run(if enabled { "enable" } else { "disable" }, unit)
    }

    fn set_active(&self, unit: &str, active: bool) -> Result<()> {
        self.run(if active { "start" } else { "stop" }, unit)
    }
}

/// A shared runner that compares by identity (so appliers stay `Clone + PartialEq`).
#[derive(Clone)]
pub struct Services(pub Arc<dyn ServiceRunner>);

impl Services {
    pub fn new(r: impl ServiceRunner + 'static) -> Self {
        Services(Arc::new(r))
    }
}

impl Default for Services {
    fn default() -> Self {
        Services::new(Systemctl::from_env())
    }
}

impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Services")
    }
}

impl PartialEq for Services {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::ops::Deref for Services {
    type Target = dyn ServiceRunner;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

/// An in-memory runner for tests: `(installed, enabled, active)` per unit.
#[derive(Default, Debug)]
pub struct FakeServices {
    pub units: std::sync::Mutex<std::collections::BTreeMap<String, (bool, bool)>>,
    /// Calls made, e.g. `"disable ananicy.service"`.
    pub calls: std::sync::Mutex<Vec<String>>,
}

impl FakeServices {
    /// A runner where `units` are installed with the given (enabled, active) state.
    pub fn with(units: &[(&str, bool, bool)]) -> Self {
        let f = FakeServices::default();
        for (u, e, a) in units {
            f.units.lock().unwrap().insert(u.to_string(), (*e, *a));
        }
        f
    }

    pub fn state(&self, unit: &str) -> Option<(bool, bool)> {
        self.units.lock().unwrap().get(unit).copied()
    }
}

impl ServiceRunner for FakeServices {
    fn installed(&self, unit: &str) -> Result<bool> {
        Ok(self.units.lock().unwrap().contains_key(unit))
    }
    fn is_enabled(&self, unit: &str) -> Result<bool> {
        Ok(self.state(unit).is_some_and(|s| s.0))
    }
    fn is_active(&self, unit: &str) -> Result<bool> {
        Ok(self.state(unit).is_some_and(|s| s.1))
    }
    fn set_enabled(&self, unit: &str, enabled: bool) -> Result<()> {
        self.calls.lock().unwrap().push(format!("{} {unit}", if enabled { "enable" } else { "disable" }));
        let mut m = self.units.lock().unwrap();
        let s = m.get_mut(unit).ok_or_else(|| ChangeError::Apply(format!("{unit} not found")))?;
        s.0 = enabled;
        Ok(())
    }
    fn set_active(&self, unit: &str, active: bool) -> Result<()> {
        self.calls.lock().unwrap().push(format!("{} {unit}", if active { "start" } else { "stop" }));
        let mut m = self.units.lock().unwrap();
        let s = m.get_mut(unit).ok_or_else(|| ChangeError::Apply(format!("{unit} not found")))?;
        s.1 = active;
        Ok(())
    }
}

impl ServiceRunner for Arc<FakeServices> {
    fn installed(&self, unit: &str) -> Result<bool> {
        (**self).installed(unit)
    }
    fn is_enabled(&self, unit: &str) -> Result<bool> {
        (**self).is_enabled(unit)
    }
    fn is_active(&self, unit: &str) -> Result<bool> {
        (**self).is_active(unit)
    }
    fn set_enabled(&self, unit: &str, enabled: bool) -> Result<()> {
        (**self).set_enabled(unit, enabled)
    }
    fn set_active(&self, unit: &str, active: bool) -> Result<()> {
        (**self).set_active(unit, active)
    }
}
