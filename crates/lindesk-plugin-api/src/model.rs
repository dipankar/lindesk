//! The lindesk data model shared by the host, built-in plugins and external plugins.
//!
//! Everything here is plain data: serialisable with serde, describable with JSON Schema,
//! and stable within an [`crate::API_VERSION`].

use std::collections::BTreeMap;
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Milliseconds since the Unix epoch. Simulated time in tests and `simulate` uses the same unit.
pub type TimestampMs = i64;

/// The semantic QoS class of a workload. Ordered from most to least protected.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum QosClass {
    /// Compositor, input stack, audio server, desktop shell.
    Critical,
    /// The application the human is using right now.
    Interactive,
    /// Visible/open applications.
    Normal,
    /// Sync, indexers, inactive applications.
    Background,
    /// Builds, downloads, backups, package upgrades.
    Batch,
}

impl QosClass {
    pub const ALL: [QosClass; 5] = [
        QosClass::Critical,
        QosClass::Interactive,
        QosClass::Normal,
        QosClass::Background,
        QosClass::Batch,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            QosClass::Critical => "critical",
            QosClass::Interactive => "interactive",
            QosClass::Normal => "normal",
            QosClass::Background => "background",
            QosClass::Batch => "batch",
        }
    }

    /// True for classes that are expected to yield under pressure.
    pub fn yields(self) -> bool {
        matches!(self, QosClass::Background | QosClass::Batch)
    }
}

impl fmt::Display for QosClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for QosClass {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        QosClass::ALL
            .into_iter()
            .find(|c| c.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| format!("unknown class '{s}' (expected critical|interactive|normal|background|batch)"))
    }
}

/// Health level computed by a policy. GREEN means "do nothing".
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    #[default]
    Green,
    Yellow,
    Red,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Green => "green",
            Level::Yellow => "yellow",
            Level::Red => "red",
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A pressure-stall resource as reported by PSI.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Resource {
    Cpu,
    Io,
    Memory,
}

impl Resource {
    pub const ALL: [Resource; 3] = [Resource::Cpu, Resource::Io, Resource::Memory];

    pub fn as_str(self) -> &'static str {
        match self {
            Resource::Cpu => "cpu",
            Resource::Io => "io",
            Resource::Memory => "memory",
        }
    }
}

impl fmt::Display for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which systemd manager owns a unit.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case", tag = "kind", content = "uid")]
pub enum Manager {
    System,
    User(u32),
}

/// Something that can be tuned.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Target {
    /// A systemd unit (slice, scope or service).
    Unit { manager: Manager, name: String },
    /// A cgroup v2/v1 path relative to the cgroup mount, e.g. `/user.slice/...`.
    Cgroup { path: String },
    /// A single process (per-process fallback backend).
    Process { pid: i32 },
}

impl Target {
    pub fn system_unit(name: impl Into<String>) -> Self {
        Target::Unit { manager: Manager::System, name: name.into() }
    }

    pub fn user_unit(uid: u32, name: impl Into<String>) -> Self {
        Target::Unit { manager: Manager::User(uid), name: name.into() }
    }

    /// The uid owning this target when it lives inside a user manager.
    pub fn user(&self) -> Option<u32> {
        match self {
            Target::Unit { manager: Manager::User(uid), .. } => Some(*uid),
            _ => None,
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Unit { manager: Manager::System, name } => write!(f, "system:{name}"),
            Target::Unit { manager: Manager::User(uid), name } => write!(f, "user/{uid}:{name}"),
            Target::Cgroup { path } => write!(f, "cgroup:{path}"),
            Target::Process { pid } => write!(f, "pid:{pid}"),
        }
    }
}

/// Where a pressure sample was measured.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case", tag = "type", content = "path")]
pub enum Scope {
    /// `/proc/pressure/*`.
    System,
    /// A cgroup's `*.pressure` file; path relative to the cgroup mount.
    Cgroup(String),
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::System => f.write_str("system"),
            Scope::Cgroup(p) => f.write_str(p),
        }
    }
}

/// One PSI reading. `*_delta_us` are stall time accrued since the previous sample of
/// the same (scope, resource), which is what controllers should usually look at.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Sample {
    pub ts: TimestampMs,
    pub scope: Scope,
    pub resource: Resource,
    pub some_avg10: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_avg10: Option<f32>,
    pub some_delta_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_delta_us: Option<u64>,
    /// Wall time covered by the deltas.
    pub interval_ms: u32,
}

impl Sample {
    /// Fraction (0..1) of the interval during which some task stalled.
    pub fn some_ratio(&self) -> f32 {
        if self.interval_ms == 0 {
            return 0.0;
        }
        (self.some_delta_us as f32 / (self.interval_ms as f32 * 1000.0)).clamp(0.0, 1.0)
    }

    pub fn full_ratio(&self) -> Option<f32> {
        let d = self.full_delta_us?;
        if self.interval_ms == 0 {
            return Some(0.0);
        }
        Some((d as f32 / (self.interval_ms as f32 * 1000.0)).clamp(0.0, 1.0))
    }
}

/// Resource consumption of one target over an interval (from cpu.stat/io.stat/memory.current).
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub cpu_usage_us: u64,
    pub io_rbytes: u64,
    pub io_wbytes: u64,
    pub io_rios: u64,
    pub io_wios: u64,
    pub mem_current: u64,
    pub interval_ms: u32,
}

/// I/O priority class. Realtime is deliberately not representable.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum IoPrioClass {
    BestEffort,
    Idle,
}

/// `io.max` limits for one block device. `None` means unlimited.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IoLimit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rbps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wbps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub riops: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wiops: Option<u64>,
}

impl IoLimit {
    pub fn is_unlimited(&self) -> bool {
        self.rbps.is_none() && self.wbps.is_none() && self.riops.is_none() && self.wiops.is_none()
    }
}

/// Identifies a tunable on a target, independent of its value.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case", tag = "knob", content = "device")]
pub enum Knob {
    CpuWeight,
    CpuIdle,
    UclampMin,
    UclampMax,
    IoWeight,
    /// Block device as `MAJ:MIN`.
    IoMax(String),
    MemoryLow,
    MemoryHigh,
    Nice,
    IoPrio,
}

impl fmt::Display for Knob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Knob::CpuWeight => f.write_str("cpu.weight"),
            Knob::CpuIdle => f.write_str("cpu.idle"),
            Knob::UclampMin => f.write_str("cpu.uclamp.min"),
            Knob::UclampMax => f.write_str("cpu.uclamp.max"),
            Knob::IoWeight => f.write_str("io.weight"),
            Knob::IoMax(d) => write!(f, "io.max[{d}]"),
            Knob::MemoryLow => f.write_str("memory.low"),
            Knob::MemoryHigh => f.write_str("memory.high"),
            Knob::Nice => f.write_str("nice"),
            Knob::IoPrio => f.write_str("ioprio"),
        }
    }
}

/// A concrete value for one knob.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case", tag = "knob")]
pub enum Setting {
    /// cgroup `cpu.weight`, 1..=10000 (default 100).
    CpuWeight { value: u64 },
    /// cgroup `cpu.idle` (SCHED_IDLE for the whole group).
    CpuIdle { value: bool },
    /// cgroup `cpu.uclamp.min` in percent (0..=100).
    UclampMin { percent: f32 },
    /// cgroup `cpu.uclamp.max` in percent (0..=100).
    UclampMax { percent: f32 },
    /// cgroup `io.weight`, 1..=10000 (default 100).
    IoWeight { value: u64 },
    /// cgroup `io.max` for one device.
    IoMax { device: String, limit: IoLimit },
    /// cgroup `memory.low` in bytes.
    MemoryLow { bytes: u64 },
    /// cgroup `memory.high` in bytes; `None` = `max`.
    MemoryHigh { bytes: Option<u64> },
    /// Process niceness (per-process backend).
    Nice { value: i8 },
    /// Process I/O priority (per-process backend). Never realtime.
    IoPrio { class: IoPrioClass, level: u8 },
}

impl Setting {
    pub fn knob(&self) -> Knob {
        match self {
            Setting::CpuWeight { .. } => Knob::CpuWeight,
            Setting::CpuIdle { .. } => Knob::CpuIdle,
            Setting::UclampMin { .. } => Knob::UclampMin,
            Setting::UclampMax { .. } => Knob::UclampMax,
            Setting::IoWeight { .. } => Knob::IoWeight,
            Setting::IoMax { device, .. } => Knob::IoMax(device.clone()),
            Setting::MemoryLow { .. } => Knob::MemoryLow,
            Setting::MemoryHigh { .. } => Knob::MemoryHigh,
            Setting::Nice { .. } => Knob::Nice,
            Setting::IoPrio { .. } => Knob::IoPrio,
        }
    }
}

impl fmt::Display for Setting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Setting::CpuWeight { value } => write!(f, "cpu.weight={value}"),
            Setting::CpuIdle { value } => write!(f, "cpu.idle={}", *value as u8),
            Setting::UclampMin { percent } => write!(f, "cpu.uclamp.min={percent:.2}"),
            Setting::UclampMax { percent } => write!(f, "cpu.uclamp.max={percent:.2}"),
            Setting::IoWeight { value } => write!(f, "io.weight={value}"),
            Setting::IoMax { device, limit } => {
                let v = |o: Option<u64>| o.map_or("max".to_string(), |v| v.to_string());
                write!(
                    f,
                    "io.max[{device}]=rbps={} wbps={} riops={} wiops={}",
                    v(limit.rbps),
                    v(limit.wbps),
                    v(limit.riops),
                    v(limit.wiops)
                )
            }
            Setting::MemoryLow { bytes } => write!(f, "memory.low={bytes}"),
            Setting::MemoryHigh { bytes } => match bytes {
                Some(b) => write!(f, "memory.high={b}"),
                None => f.write_str("memory.high=max"),
            },
            Setting::Nice { value } => write!(f, "nice={value}"),
            Setting::IoPrio { class, level } => write!(f, "ioprio={class:?}/{level}"),
        }
    }
}

/// A policy's wish for one knob on one target. The host resolves competing intents,
/// clamps them with safety caps and turns the winners into actuator calls.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Intent {
    pub target: Target,
    pub setting: Setting,
    /// Higher wins. Built-ins use 0; plugins default to 10.
    #[serde(default)]
    pub priority: i32,
    /// Human-readable explanation, shown by `lindeskctl explain`.
    #[serde(default)]
    pub reason: String,
}

/// Who set a value and why.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default, PartialEq)]
pub struct Provenance {
    pub plugin: String,
    pub reason: String,
    pub priority: i32,
    /// Set when safety caps altered the requested value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clamped_from: Option<String>,
}

/// Information about a process, passed to classifiers.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default, PartialEq)]
pub struct ProcInfo {
    pub pid: i32,
    pub uid: u32,
    pub comm: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    /// cgroup path relative to the cgroup mount.
    pub cgroup: String,
    /// Owning systemd unit, if any (scope/service).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Slice the unit lives in, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slice: Option<String>,
    /// Desktop application id when known (Wayland app_id / .desktop id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
}

/// A classifier's verdict.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Classification {
    pub class: QosClass,
    /// 0..1; the host takes the highest-priority classifier with confidence >= its threshold.
    #[serde(default = "one")]
    pub confidence: f32,
    #[serde(default)]
    pub reason: String,
}

fn one() -> f32 {
    1.0
}

/// Things that happen. Adapters produce Focus/Interaction; the host produces the rest.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Event {
    /// The user focused a window. Adapters supply pid and/or app_id; the host fills `target`.
    Focus {
        ts: TimestampMs,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Target>,
        #[serde(default)]
        source: String,
    },
    /// The user is actively interacting (input, explicit BeginInteraction).
    Interaction {
        ts: TimestampMs,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Target>,
        /// Explicit end of an interaction (EndInteraction).
        #[serde(default)]
        end: bool,
        #[serde(default)]
        source: String,
    },
    /// A target's class was set or changed.
    ClassSet { ts: TimestampMs, target: Target, class: QosClass, source: String },
    /// A policy changed health level.
    LevelChanged { ts: TimestampMs, from: Level, to: Level, policy: String },
    /// A setting was applied to the system.
    ActionApplied { ts: TimestampMs, target: Target, setting: Setting, provenance: Provenance },
    /// A setting was reverted to its original value.
    ActionReverted { ts: TimestampMs, target: Target, knob: Knob, reason: String },
    /// A plugin misbehaved (crash, timeout, protocol error, clamped intent).
    PluginError { ts: TimestampMs, plugin: String, message: String },
}

impl Event {
    pub fn ts(&self) -> TimestampMs {
        match self {
            Event::Focus { ts, .. }
            | Event::Interaction { ts, .. }
            | Event::ClassSet { ts, .. }
            | Event::LevelChanged { ts, .. }
            | Event::ActionApplied { ts, .. }
            | Event::ActionReverted { ts, .. }
            | Event::PluginError { ts, .. } => *ts,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Event::Focus { .. } => "focus",
            Event::Interaction { .. } => "interaction",
            Event::ClassSet { .. } => "class_set",
            Event::LevelChanged { .. } => "level",
            Event::ActionApplied { .. } => "action_applied",
            Event::ActionReverted { .. } => "action_reverted",
            Event::PluginError { .. } => "plugin_error",
        }
    }
}

/// A free-form numeric metric emitted by any plugin; lands in the `metrics` table.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Metric {
    pub ts: TimestampMs,
    pub name: String,
    pub value: f64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Filled in by the host.
    #[serde(default)]
    pub plugin: String,
}

/// Per-target state handed to policies.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct UnitState {
    pub target: Target,
    pub class: QosClass,
    /// The target currently holds input focus.
    #[serde(default)]
    pub foreground: bool,
    /// Pressure measured inside this target's cgroup, if a sensor provides it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pressure: Vec<Sample>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Values lindesk currently has applied (not the originals).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied: Vec<Setting>,
    /// Representative process name for display (e.g. "rustc").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_comm: Option<String>,
}

/// What the host can do on this machine (a summary of the probe).
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default, PartialEq)]
pub struct HostCaps {
    pub cgroup_v2: bool,
    pub systemd: bool,
    pub psi: bool,
    pub uclamp: bool,
    pub io_latency: bool,
    pub io_cost: bool,
    pub io_weight_effective: bool,
    pub io_max: bool,
    pub sched_ext: bool,
    pub bpf: bool,
    /// Block devices (`MAJ:MIN`) that back the filesystems lindesk cares about.
    #[serde(default)]
    pub devices: Vec<String>,
}

/// Everything a policy sees on each evaluation.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub now: TimestampMs,
    /// System-wide pressure (latest sample per resource).
    pub pressure: Vec<Sample>,
    pub units: Vec<UnitState>,
    /// Events since the previous evaluation.
    pub events: Vec<Event>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<Target>,
    pub caps: HostCaps,
}

impl Snapshot {
    pub fn system_pressure(&self, r: Resource) -> Option<&Sample> {
        self.pressure.iter().find(|s| s.resource == r && s.scope == Scope::System)
    }

    pub fn foreground_unit(&self) -> Option<&UnitState> {
        self.units.iter().find(|u| u.foreground)
    }
}

/// A policy's answer.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default, PartialEq)]
pub struct PolicyOutput {
    #[serde(default)]
    pub intents: Vec<Intent>,
    /// The policy's view of system health, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metrics: Vec<Metric>,
}

/// What an actuator can do.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default, PartialEq)]
pub struct ActuatorCaps {
    /// Knob kinds supported (IoMax is listed with an empty device string).
    pub knobs: Vec<Knob>,
    /// Target kinds supported: "unit", "cgroup", "process".
    pub targets: Vec<String>,
}

impl ActuatorCaps {
    pub fn supports_knob(&self, k: &Knob) -> bool {
        self.knobs.iter().any(|s| std::mem::discriminant(s) == std::mem::discriminant(k))
    }

    pub fn supports_target(&self, t: &Target) -> bool {
        let kind = match t {
            Target::Unit { .. } => "unit",
            Target::Cgroup { .. } => "cgroup",
            Target::Process { .. } => "process",
        };
        self.targets.iter().any(|s| s == kind)
    }
}

/// A row for sinks. One enum so plugins subscribe once and match on `table`.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case", tag = "table")]
pub enum Record {
    Pressure(Sample),
    Unit { ts: TimestampMs, target: Target, class: QosClass, foreground: bool, usage: Usage, applied: Vec<Setting> },
    Event(Event),
    Metric(Metric),
}

impl Record {
    pub fn table(&self) -> &'static str {
        match self {
            Record::Pressure(_) => "pressure",
            Record::Unit { .. } => "units",
            Record::Event(_) => "events",
            Record::Metric(_) => "metrics",
        }
    }
}
