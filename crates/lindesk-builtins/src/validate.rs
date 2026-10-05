//! Validation of a whole [`Config`] before it is written: what [`Config::from_toml`] can't
//! see. The daemons plug [`config`] into the change guardrails
//! (`lindesk_changes::GuardContext::with_config_validator`), so a change that would leave an
//! `[[override]]` glob that doesn't compile, or a built-in plugin config the plugin would
//! refuse at `init` (unknown keys, out-of-range values), is rejected up front instead of
//! failing at the next reload.

use lindesk_core::config::Config;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::classifier::rules::RulesConfig;
use crate::policy::ThresholdConfig;
use crate::sensors::PsiConfig;

/// Parse a plugin's `config` the way its `init` does (`null` = defaults).
fn parse<T: DeserializeOwned>(v: &Value) -> Result<(), String> {
    if v.is_null() {
        return Ok(());
    }
    serde_json::from_value::<T>(v.clone()).map(|_| ()).map_err(|e| e.to_string())
}

/// Checks one plugin's `config` value.
type Check = fn(&Value) -> Result<(), String>;

/// Every built-in plugin config validator, by plugin name.
fn plugin_validators() -> Vec<(&'static str, Check)> {
    #[allow(unused_mut)] // without the adapter features nothing is pushed
    let mut v: Vec<(&'static str, Check)> = vec![
        (crate::policy::threshold::NAME, |v| ThresholdConfig::from_json(v).map(|_| ())),
        (crate::sensors::PSI, |v| PsiConfig::from_json(v).map(|_| ())),
        (crate::classifier::rules::NAME, parse::<RulesConfig>),
        (crate::actuators::systemd::NAME, parse::<crate::actuators::systemd::SystemdConfig>),
    ];
    #[cfg(feature = "x11")]
    v.push(("x11", parse::<crate::adapters::x11::X11Config>));
    #[cfg(feature = "cosmic")]
    v.push(("cosmic", parse::<crate::adapters::cosmic::CosmicConfig>));
    v
}

/// Check `[[override]]` entries and the config of every built-in plugin that has one.
pub fn config(cfg: &Config) -> Result<(), String> {
    lindesk_host::validate_overrides(cfg).map_err(|e| e.to_string())?;
    for (name, check) in plugin_validators() {
        if let Some(pc) = cfg.plugins.get(name) {
            check(&pc.config).map_err(|e| format!("[plugins.{name}.config]: {e}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_defaults_and_valid_values() {
        config(&Config::default()).unwrap();
        let c = Config::from_toml(
            "[plugins.threshold.config]\nepoch = \"400ms\"\n[plugins.psi.config]\nwindow = \"2s\"\n\
             [[override]]\nunit = \"steam*.scope\"\nclass = \"interactive\"\n",
        )
        .unwrap();
        config(&c).unwrap();
    }

    #[test]
    fn rejects_bad_plugin_keys_values_and_globs() {
        let bad_key = Config::from_toml("[plugins.threshold.config]\nepohc = \"400ms\"\n").unwrap();
        let e = config(&bad_key).unwrap_err();
        assert!(e.contains("plugins.threshold.config") && e.contains("epohc"), "{e}");
        let bad_psi = Config::from_toml("[plugins.psi.config]\nnope = 1\n").unwrap();
        assert!(config(&bad_psi).unwrap_err().contains("plugins.psi.config"));
        let bad_rules = Config::from_toml("[plugins.rules.config]\ndir = [\"x\"]\n").unwrap();
        assert!(config(&bad_rules).unwrap_err().contains("plugins.rules.config"));
        let bad_glob = Config::from_toml("[[override]]\nunit = \"[x\"\nclass = \"batch\"\n").unwrap();
        assert!(config(&bad_glob).unwrap_err().contains("override"));
    }
}
