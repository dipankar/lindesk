//! Plugin traits. Built-in plugins implement these directly; external plugins speak the
//! stdio protocol in [`crate::protocol`] and the host wraps them in proxies that implement
//! the very same traits. A plugin may implement several kinds.

use std::os::fd::RawFd;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::*;

/// The six extension points.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    /// Produces pressure samples and per-target usage.
    Sensor,
    /// Reports foreground/interaction events from a desktop.
    Adapter,
    /// Assigns a QoS class to processes.
    Classifier,
    /// Turns snapshots into intents.
    Policy,
    /// Applies settings to the system. System privilege only.
    Actuator,
    /// Receives records (telemetry, events).
    Sink,
}

impl PluginKind {
    pub const ALL: [PluginKind; 6] = [
        PluginKind::Sensor,
        PluginKind::Adapter,
        PluginKind::Classifier,
        PluginKind::Policy,
        PluginKind::Actuator,
        PluginKind::Sink,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            PluginKind::Sensor => "sensor",
            PluginKind::Adapter => "adapter",
            PluginKind::Classifier => "classifier",
            PluginKind::Policy => "policy",
            PluginKind::Actuator => "actuator",
            PluginKind::Sink => "sink",
        }
    }
}

impl std::fmt::Display for PluginKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for PluginKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PluginKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| format!("unknown plugin kind '{s}'"))
    }
}

/// Where a plugin runs and what it may touch.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Privilege {
    /// Runs in `lindesk-session`; may only target the user's own units.
    #[default]
    User,
    /// Runs in `lindeskd` as root; only loaded from root-owned directories.
    System,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    pub kinds: Vec<PluginKind>,
    #[serde(default)]
    pub description: String,
}

/// Handed to every plugin once, before any other call.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct InitContext {
    pub api_version: u32,
    /// "lindeskd", "lindesk-session" or "simulate".
    pub host: String,
    pub privilege: Privilege,
    /// The session user for `lindesk-session`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    pub caps: HostCaps,
    /// The plugin's `[config]` table from its manifest merged with admin overrides.
    #[serde(default)]
    pub config: serde_json::Value,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum PluginError {
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("{0}")]
    Failed(String),
    #[error("timed out")]
    Timeout,
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("i/o error: {0}")]
    Io(String),
}

impl PluginError {
    pub fn failed(e: impl std::fmt::Display) -> Self {
        PluginError::Failed(e.to_string())
    }
}

impl From<std::io::Error> for PluginError {
    fn from(e: std::io::Error) -> Self {
        PluginError::Io(e.to_string())
    }
}

pub type PluginResult<T> = Result<T, PluginError>;

/// Common lifecycle for all plugin kinds.
pub trait Plugin: Send {
    fn info(&self) -> PluginInfo;

    fn init(&mut self, _ctx: &InitContext) -> PluginResult<()> {
        Ok(())
    }

    fn shutdown(&mut self) {}
}

/// A target the host wants usage/pressure for.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct WatchedTarget {
    pub target: Target,
    /// cgroup path relative to the cgroup mount, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct SensorInput {
    pub now: TimestampMs,
    #[serde(default)]
    pub targets: Vec<WatchedTarget>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct TargetUsage {
    pub target: Target,
    pub usage: Usage,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default, PartialEq)]
pub struct SensorOutput {
    /// System scope and/or per-cgroup scope samples.
    #[serde(default)]
    pub samples: Vec<Sample>,
    #[serde(default)]
    pub usage: Vec<TargetUsage>,
    #[serde(default)]
    pub metrics: Vec<Metric>,
}

pub trait Sensor: Plugin {
    /// Called on every host tick and whenever one of [`Sensor::wake_fds`] becomes readable.
    fn poll(&mut self, input: &SensorInput) -> PluginResult<SensorOutput>;

    /// File descriptors (e.g. PSI triggers) whose `POLLPRI`/`POLLIN` should wake the host early.
    fn wake_fds(&self) -> Vec<RawFd> {
        Vec::new()
    }
}

/// Callback used by adapters to emit events. Cheap to clone.
pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

pub trait Adapter: Plugin {
    /// Start watching. Implementations usually spawn a thread and call `sink` for each event.
    fn start(&mut self, sink: EventSink) -> PluginResult<()>;

    fn stop(&mut self) {}

    /// True when the adapter is connected to its desktop and producing events.
    fn connected(&self) -> bool {
        true
    }
}

pub trait Classifier: Plugin {
    fn classify(&mut self, proc_info: &ProcInfo) -> PluginResult<Option<Classification>>;
}

pub trait Policy: Plugin {
    fn evaluate(&mut self, snapshot: &Snapshot) -> PluginResult<PolicyOutput>;
}

pub trait Actuator: Plugin {
    fn caps(&self) -> ActuatorCaps;

    /// Current value of a knob, used to remember originals before the first change.
    fn read(&mut self, target: &Target, knob: &Knob) -> PluginResult<Option<Setting>>;

    fn apply(&mut self, target: &Target, setting: &Setting) -> PluginResult<()>;

    /// Undo: write `original` back, or the kernel/systemd default when `None`.
    fn reset(&mut self, target: &Target, knob: &Knob, original: Option<&Setting>) -> PluginResult<()>;
}

pub trait Sink: Plugin {
    /// Table names this sink wants ("pressure", "units", "events", "metrics"); empty = all.
    fn tables(&self) -> Vec<String> {
        Vec::new()
    }

    fn record(&mut self, records: &[Record]) -> PluginResult<()>;

    fn flush(&mut self) -> PluginResult<()> {
        Ok(())
    }
}
