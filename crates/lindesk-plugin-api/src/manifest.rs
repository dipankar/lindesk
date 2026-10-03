//! `plugin.toml`: how the host discovers and launches an external plugin.
//!
//! ```toml
//! [plugin]
//! name = "steam-games"
//! version = "0.1.0"
//! api = 1
//! kinds = ["classifier"]
//! exec = "steam_games.py"      # relative to the manifest directory, or absolute
//! privilege = "user"           # user | system
//!
//! [classifier]
//! priority = 50
//! prefilter = { comm = ["*.exe", "steam*"] }
//!
//! [config]                     # free-form, passed to the plugin in `initialize`
//! boost_proton = true
//! ```

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::traits::{PluginKind, Privilege};

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub plugin: ManifestPlugin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier: Option<ClassifierSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<PolicySection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensor: Option<SensorSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sink: Option<SinkSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeouts: Option<Timeouts>,
    #[serde(default)]
    pub config: serde_json::Value,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ManifestPlugin {
    pub name: String,
    pub version: String,
    /// Plugin API version this plugin targets.
    pub api: u32,
    pub kinds: Vec<PluginKind>,
    /// Executable, relative to the manifest directory unless absolute.
    pub exec: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub privilege: Privilege,
    #[serde(default)]
    pub description: String,
    /// Disabled plugins are discovered but not started.
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

/// Glob lists; a process must match at least one glob in every non-empty list.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Prefilter {
    #[serde(default)]
    pub comm: Vec<String>,
    #[serde(default)]
    pub exe: Vec<String>,
    #[serde(default)]
    pub unit: Vec<String>,
    #[serde(default)]
    pub app_id: Vec<String>,
}

impl Prefilter {
    pub fn is_empty(&self) -> bool {
        self.comm.is_empty() && self.exe.is_empty() && self.unit.is_empty() && self.app_id.is_empty()
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ClassifierSection {
    #[serde(default = "default_priority")]
    pub priority: i32,
    #[serde(default = "default_confidence")]
    pub min_confidence: f32,
    #[serde(default)]
    pub prefilter: Prefilter,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PolicySection {
    #[serde(default = "default_priority")]
    pub priority: i32,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SensorSection {
    /// Minimum interval between polls.
    #[serde(default = "default_sensor_interval")]
    pub interval_ms: u64,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SinkSection {
    #[serde(default)]
    pub tables: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Timeouts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_ms: Option<u64>,
}

fn default_priority() -> i32 {
    10
}
fn default_confidence() -> f32 {
    0.5
}
fn default_sensor_interval() -> u64 {
    1000
}
