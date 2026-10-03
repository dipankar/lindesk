//! `/etc/lindesk/lindesk.toml` (system) and `~/.config/lindesk/lindesk.toml` (user).
//! Every field has a default, so an empty file is a valid configuration.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::safety::SafetyCaps;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("parsing {path}: {source}")]
    Parse { path: PathBuf, source: toml::de::Error },
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub io: IoConfig,
    pub safety: SafetyCaps,
    pub telemetry: Telemetry,
    /// Per-plugin settings, keyed by plugin name (built-in or external).
    pub plugins: BTreeMap<String, PluginConfig>,
    /// Admin overrides: these always win over any plugin.
    #[serde(rename = "override")]
    pub overrides: Vec<Override>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    /// Pipeline tick. Sensors with wake fds can wake the loop earlier.
    #[serde(with = "dur")]
    pub tick: Duration,
    /// Extra plugin directories, searched after the defaults.
    pub plugin_dirs: Vec<PathBuf>,
    /// Start in observe-only mode: compute and record intents but never apply them.
    pub dry_run: bool,
}

impl Default for General {
    fn default() -> Self {
        General { tick: Duration::from_millis(250), plugin_dirs: Vec::new(), dry_run: false }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Toggle {
    #[default]
    Auto,
    On,
    Off,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct IoConfig {
    /// Enable the iocost controller on devices with no proportional I/O scheduler.
    pub iocost: Toggle,
    /// Devices (`MAJ:MIN`) to manage. Empty = those backing `/` and `/home`.
    pub devices: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Telemetry {
    pub enabled: bool,
    #[serde(with = "dur")]
    pub interval: Duration,
    #[serde(with = "dur")]
    pub flush: Duration,
    /// Store root. Empty = `/var/lib/lindesk/telemetry` (system) or
    /// `$XDG_STATE_HOME/lindesk/telemetry` (user).
    pub dir: Option<PathBuf>,
    pub retention: Retention,
}

impl Default for Telemetry {
    fn default() -> Self {
        Telemetry {
            enabled: true,
            interval: Duration::from_secs(1),
            flush: Duration::from_secs(60),
            dir: None,
            retention: Retention::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Retention {
    #[serde(with = "dur")]
    pub raw: Duration,
    #[serde(with = "dur")]
    pub rollup_1m: Duration,
    #[serde(with = "dur")]
    pub rollup_1h: Duration,
    #[serde(with = "size")]
    pub max_size: u64,
}

impl Default for Retention {
    fn default() -> Self {
        const DAY: u64 = 86_400;
        Retention {
            raw: Duration::from_secs(7 * DAY),
            rollup_1m: Duration::from_secs(90 * DAY),
            rollup_1h: Duration::from_secs(365 * DAY),
            max_size: 2 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct PluginConfig {
    pub enabled: bool,
    /// Overrides the manifest priority.
    pub priority: Option<i32>,
    /// Merged over the plugin's own `[config]`.
    pub config: serde_json::Value,
}

impl Default for PluginConfig {
    fn default() -> Self {
        PluginConfig { enabled: true, priority: None, config: serde_json::Value::Null }
    }
}

/// Pin a class (and optionally explicit settings) onto matching units.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Override {
    /// Unit name glob, e.g. `docker-*.scope`.
    #[serde(default)]
    pub unit: Option<String>,
    /// Process comm glob.
    #[serde(default)]
    pub comm: Option<String>,
    #[serde(default)]
    pub class: Option<lindesk_plugin_api::QosClass>,
    #[serde(default)]
    pub settings: Vec<lindesk_plugin_api::Setting>,
}

impl Config {
    pub fn from_toml(s: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(s)
    }

    /// Load a file; a missing file yields the defaults.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(s) => Config::from_toml(&s).map_err(|source| ConfigError::Parse { path: path.into(), source }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(source) => Err(ConfigError::Read { path: path.into(), source }),
        }
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    pub fn plugin(&self, name: &str) -> PluginConfig {
        self.plugins.get(name).cloned().unwrap_or_default()
    }
}

/// Merge `over` onto `base` (objects recursively, everything else replaced).
pub fn merge_json(base: &mut serde_json::Value, over: &serde_json::Value) {
    use serde_json::Value;
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                merge_json(b.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (_, Value::Null) => {}
        (b, o) => *b = o.clone(),
    }
}

/// `"250ms"`, `"7d"`, `"1y"` via humantime.
pub mod dur {
    use super::*;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&humantime::format_duration(*d).to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let s = String::deserialize(d)?;
        humantime::parse_duration(&s).map_err(serde::de::Error::custom)
    }
}

/// `"2GiB"`, `"512MB"` or a plain integer of bytes.
pub mod size {
    use super::*;

    pub fn serialize<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&bytesize::ByteSize::b(*v).display().iec().to_string().replace(' ', ""))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            N(u64),
            S(String),
        }
        match Either::deserialize(d)? {
            Either::N(n) => Ok(n),
            Either::S(s) => s.parse::<bytesize::ByteSize>().map(|b| b.as_u64()).map_err(serde::de::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_default() {
        assert_eq!(Config::from_toml("").unwrap(), Config::default());
    }

    #[test]
    fn parses_human_units_and_overrides() {
        let c = Config::from_toml(
            r#"
            [general]
            tick = "100ms"
            [telemetry.retention]
            raw = "3d"
            max_size = "512MiB"
            [plugins.threshold]
            priority = 5
            config = { epoch = "300ms" }
            [[override]]
            unit = "docker-*.scope"
            class = "batch"
            "#,
        )
        .unwrap();
        assert_eq!(c.general.tick, Duration::from_millis(100));
        assert_eq!(c.telemetry.retention.raw, Duration::from_secs(3 * 86400));
        assert_eq!(c.telemetry.retention.max_size, 512 * 1024 * 1024);
        assert_eq!(c.plugin("threshold").priority, Some(5));
        assert_eq!(c.overrides[0].class, Some(lindesk_plugin_api::QosClass::Batch));
        // round-trips
        let again = Config::from_toml(&c.to_toml()).unwrap();
        assert_eq!(again, c);
    }

    #[test]
    fn rejects_typos() {
        assert!(Config::from_toml("[generl]\ntick='1s'").is_err());
    }

    #[test]
    fn merge() {
        let mut a = serde_json::json!({"a": 1, "b": {"c": 2, "d": 3}});
        merge_json(&mut a, &serde_json::json!({"b": {"c": 9}, "e": true}));
        assert_eq!(a, serde_json::json!({"a": 1, "b": {"c": 9, "d": 3}, "e": true}));
    }
}
