//! The changeset data model: ops, authorship, records, plans.

use std::fmt;

use lindesk_plugin_api::{Event, QosClass, TimestampMs};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Which config file (and which daemon) a change belongs to.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// `$XDG_CONFIG_HOME/lindesk/lindesk.toml`, applied by lindesk-session.
    User,
    /// `/etc/lindesk/lindesk.toml` and kernel tunables, applied by lindeskd.
    System,
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Scope::User => "user",
            Scope::System => "system",
        })
    }
}

/// How much damage a mistaken op can do. Ordered: `Low < Medium < High`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// lindesk's own config: config keys, profile, class overrides.
    #[default]
    Low,
    /// VM sysctls and zswap.
    Medium,
    /// I/O scheduler, cpufreq governor and EPP.
    High,
}

/// Who (which front end) asked for a change.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// `lindeskctl`.
    Cli,
    /// An AI agent through lindesk-mcp.
    Mcp,
    /// Any other D-Bus client.
    Dbus,
    /// The daemon itself (dead-man reverts, recovery).
    Daemon,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Origin::Cli => "cli",
            Origin::Mcp => "mcp",
            Origin::Dbus => "dbus",
            Origin::Daemon => "daemon",
        })
    }
}

/// Recorded authorship of a changeset.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Author {
    pub uid: u32,
    pub origin: Origin,
    /// Free-form client hint from the ops envelope, e.g. `"claude-code"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// D-Bus unique name of the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bus_name: Option<String>,
}

impl Author {
    pub fn new(uid: u32, origin: Origin) -> Self {
        Author { uid, origin, client: None, bus_name: None }
    }

    pub fn with_client(mut self, client: impl Into<String>) -> Self {
        self.client = Some(client.into());
        self
    }

    pub fn with_bus_name(mut self, bus_name: impl Into<String>) -> Self {
        self.bus_name = Some(bus_name.into());
        self
    }

    /// The running process as [`Origin::Daemon`] (used for dead-man reverts).
    pub fn daemon(client: &str) -> Self {
        Author::new(rustix::process::getuid().as_raw(), Origin::Daemon).with_client(client)
    }
}

/// One change. Serialized with an `"op"` tag, e.g.
/// `{"op": "sysctl", "key": "vm.swappiness", "value": "10"}`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ChangeOp {
    /// Set a dotted key in lindesk.toml, e.g. `plugins.threshold.config.epoch`.
    ConfigSet { scope: Scope, key: String, value: Value },
    /// Remove a dotted key (back to its default).
    ConfigUnset { scope: Scope, key: String },
    /// Set `general.profile` (`"auto"` or a profile name).
    ProfileUse { scope: Scope, name: String },
    /// Pin a class on units matching a glob (an `[[override]]` entry).
    ClassOverride { scope: Scope, unit: String, class: QosClass },
    /// A kernel sysctl (allow-listed, system scope).
    Sysctl {
        key: String,
        value: String,
        /// Also write it to `/etc/sysctl.d/90-lindesk.conf`.
        #[serde(default, skip_serializing_if = "is_false")]
        persist: bool,
    },
    /// The I/O scheduler of a block device (`MAJ:MIN` or name).
    IoScheduler {
        device: String,
        scheduler: String,
        /// Also write a udev rule to `/etc/udev/rules.d/90-lindesk-iosched.rules`.
        #[serde(default, skip_serializing_if = "is_false")]
        persist: bool,
    },
    /// zswap module parameters; unset fields are left alone.
    Zswap {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compressor: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_pool_percent: Option<u8>,
    },
    /// `energy_performance_preference` on every cpufreq policy.
    CpuEpp { preference: String },
    /// `scaling_governor` on every cpufreq policy.
    CpuGovernor { governor: String },
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl ChangeOp {
    /// The `"op"` tag.
    pub fn kind(&self) -> &'static str {
        match self {
            ChangeOp::ConfigSet { .. } => "config_set",
            ChangeOp::ConfigUnset { .. } => "config_unset",
            ChangeOp::ProfileUse { .. } => "profile_use",
            ChangeOp::ClassOverride { .. } => "class_override",
            ChangeOp::Sysctl { .. } => "sysctl",
            ChangeOp::IoScheduler { .. } => "io_scheduler",
            ChangeOp::Zswap { .. } => "zswap",
            ChangeOp::CpuEpp { .. } => "cpu_epp",
            ChangeOp::CpuGovernor { .. } => "cpu_governor",
        }
    }

    /// Runtime (kernel) ops are always system scope.
    pub fn scope(&self) -> Scope {
        match self {
            ChangeOp::ConfigSet { scope, .. }
            | ChangeOp::ConfigUnset { scope, .. }
            | ChangeOp::ProfileUse { scope, .. }
            | ChangeOp::ClassOverride { scope, .. } => *scope,
            _ => Scope::System,
        }
    }

    /// Edits lindesk.toml (as opposed to a kernel tunable).
    pub fn is_config(&self) -> bool {
        matches!(
            self,
            ChangeOp::ConfigSet { .. }
                | ChangeOp::ConfigUnset { .. }
                | ChangeOp::ProfileUse { .. }
                | ChangeOp::ClassOverride { .. }
        )
    }

    /// Low: config/profile/class. Medium: sysctl/zswap. High: scheduler/governor/EPP.
    pub fn risk(&self) -> Risk {
        match self {
            ChangeOp::ConfigSet { .. }
            | ChangeOp::ConfigUnset { .. }
            | ChangeOp::ProfileUse { .. }
            | ChangeOp::ClassOverride { .. } => Risk::Low,
            ChangeOp::Sysctl { .. } | ChangeOp::Zswap { .. } => Risk::Medium,
            ChangeOp::IoScheduler { .. } | ChangeOp::CpuEpp { .. } | ChangeOp::CpuGovernor { .. } => Risk::High,
        }
    }

    /// The dotted config key an op edits (config ops only).
    pub fn config_key(&self) -> Option<String> {
        match self {
            ChangeOp::ConfigSet { key, .. } | ChangeOp::ConfigUnset { key, .. } => Some(key.clone()),
            ChangeOp::ProfileUse { .. } => Some("general.profile".into()),
            ChangeOp::ClassOverride { unit, .. } => Some(format!("override[unit={unit}]")),
            _ => None,
        }
    }

    /// What the op touches, for conflict detection. Two keys conflict when equal or when
    /// one is a dotted prefix of the other.
    pub fn keys(&self) -> Vec<String> {
        match self {
            ChangeOp::ConfigSet { scope, .. }
            | ChangeOp::ConfigUnset { scope, .. }
            | ChangeOp::ProfileUse { scope, .. }
            | ChangeOp::ClassOverride { scope, .. } => {
                vec![format!("config.{scope}:{}", self.config_key().unwrap_or_default())]
            }
            ChangeOp::Sysctl { key, .. } => vec![format!("sysctl:{key}")],
            ChangeOp::IoScheduler { device, .. } => vec![format!("io_scheduler:{device}")],
            ChangeOp::Zswap { enabled, compressor, max_pool_percent } => {
                let mut k = Vec::new();
                if enabled.is_some() {
                    k.push("zswap:enabled".into());
                }
                if compressor.is_some() {
                    k.push("zswap:compressor".into());
                }
                if max_pool_percent.is_some() {
                    k.push("zswap:max_pool_percent".into());
                }
                k
            }
            ChangeOp::CpuEpp { .. } => vec!["cpufreq:energy_performance_preference".into()],
            ChangeOp::CpuGovernor { .. } => vec!["cpufreq:scaling_governor".into()],
        }
    }

    /// The value an [`crate::Applier`] writes to apply this op. `Null` means "remove".
    /// With `persist`, sysctl/scheduler values are `{"runtime": v, "persisted": v}`.
    pub fn target_value(&self) -> Value {
        match self {
            ChangeOp::ConfigSet { value, .. } => value.clone(),
            ChangeOp::ConfigUnset { .. } => Value::Null,
            ChangeOp::ProfileUse { name, .. } => json!(name),
            ChangeOp::ClassOverride { class, .. } => json!(class),
            ChangeOp::Sysctl { value, persist, .. } => runtime_value(value, *persist),
            ChangeOp::IoScheduler { scheduler, persist, .. } => runtime_value(scheduler, *persist),
            ChangeOp::Zswap { enabled, compressor, max_pool_percent } => {
                let mut m = serde_json::Map::new();
                if let Some(e) = enabled {
                    m.insert("enabled".into(), json!(e));
                }
                if let Some(c) = compressor {
                    m.insert("compressor".into(), json!(c));
                }
                if let Some(p) = max_pool_percent {
                    m.insert("max_pool_percent".into(), json!(p));
                }
                Value::Object(m)
            }
            ChangeOp::CpuEpp { preference } => json!(preference),
            ChangeOp::CpuGovernor { governor } => json!(governor),
        }
    }

    /// Best-effort op that describes writing `v` to the same place (used for the ops of
    /// revert changesets). The record's `applied` value stays authoritative.
    pub fn with_value(&self, v: &Value) -> ChangeOp {
        let s = |v: &Value| match v {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o.get("runtime").and_then(|r| r.as_str()).map(str::to_string),
            _ => None,
        };
        match self {
            ChangeOp::ConfigSet { scope, key, .. } | ChangeOp::ConfigUnset { scope, key } => match v {
                Value::Null => ChangeOp::ConfigUnset { scope: *scope, key: key.clone() },
                v => ChangeOp::ConfigSet { scope: *scope, key: key.clone(), value: v.clone() },
            },
            ChangeOp::ProfileUse { scope, .. } => match v.as_str() {
                Some(name) => ChangeOp::ProfileUse { scope: *scope, name: name.to_string() },
                None => ChangeOp::ConfigUnset { scope: *scope, key: "general.profile".into() },
            },
            ChangeOp::ClassOverride { scope, unit, class } => ChangeOp::ClassOverride {
                scope: *scope,
                unit: unit.clone(),
                class: v.as_str().and_then(|c| c.parse().ok()).unwrap_or(*class),
            },
            ChangeOp::Sysctl { key, value, .. } => ChangeOp::Sysctl {
                key: key.clone(),
                value: s(v).unwrap_or_else(|| value.clone()),
                persist: v.is_object(),
            },
            ChangeOp::IoScheduler { device, scheduler, .. } => ChangeOp::IoScheduler {
                device: device.clone(),
                scheduler: s(v).unwrap_or_else(|| scheduler.clone()),
                persist: v.is_object(),
            },
            ChangeOp::Zswap { .. } => ChangeOp::Zswap {
                enabled: v.get("enabled").and_then(Value::as_bool),
                compressor: v.get("compressor").and_then(Value::as_str).map(str::to_string),
                max_pool_percent: v.get("max_pool_percent").and_then(Value::as_u64).map(|p| p.min(100) as u8),
            },
            ChangeOp::CpuEpp { preference } => {
                ChangeOp::CpuEpp { preference: uniform(v).unwrap_or_else(|| preference.clone()) }
            }
            ChangeOp::CpuGovernor { governor } => {
                ChangeOp::CpuGovernor { governor: uniform(v).unwrap_or_else(|| governor.clone()) }
            }
        }
    }
}

fn runtime_value(v: &str, persist: bool) -> Value {
    if persist { json!({"runtime": v, "persisted": v}) } else { json!(v) }
}

/// A string, or a per-policy map whose values are all the same string.
fn uniform(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(m) => {
            let mut it = m.values().filter_map(Value::as_str);
            let first = it.next()?;
            it.all(|x| x == first).then(|| first.to_string())
        }
        _ => None,
    }
}

impl fmt::Display for ChangeOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChangeOp::ConfigSet { scope, key, value } => write!(f, "set {key} = {value} ({scope} config)"),
            ChangeOp::ConfigUnset { scope, key } => write!(f, "unset {key} ({scope} config)"),
            ChangeOp::ProfileUse { scope, name } => write!(f, "use profile {name} ({scope})"),
            ChangeOp::ClassOverride { scope, unit, class } => write!(f, "pin class {class} on {unit} ({scope})"),
            ChangeOp::Sysctl { key, value, persist } => {
                write!(f, "sysctl {key} = {value}{}", if *persist { " (persistent)" } else { "" })
            }
            ChangeOp::IoScheduler { device, scheduler, persist } => {
                write!(f, "I/O scheduler of {device} = {scheduler}{}", if *persist { " (persistent)" } else { "" })
            }
            ChangeOp::Zswap { .. } => write!(f, "zswap {}", self.target_value()),
            ChangeOp::CpuEpp { preference } => write!(f, "cpufreq EPP = {preference}"),
            ChangeOp::CpuGovernor { governor } => write!(f, "cpufreq governor = {governor}"),
        }
    }
}

/// Changeset lifecycle: `planned → applying → applied → confirmed | reverted | failed`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Planned,
    /// Write-ahead state: recorded before the first write. Rolled back by `recover()`.
    Applying,
    Applied,
    Confirmed,
    Reverted,
    Failed,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = serde_json::to_value(self).unwrap_or_default();
        f.write_str(v.as_str().unwrap_or("?"))
    }
}

/// An op as recorded in a changeset.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct OpRecord {
    #[serde(flatten)]
    pub op: ChangeOp,
    /// The value before the change (what a revert writes back). `null` = absent.
    pub previous: Value,
    /// The value written (authoritative over the op's own fields).
    pub applied: Value,
    /// Written to the system. Recovery rolls back done ops plus the first not-done one.
    #[serde(default)]
    pub done: bool,
}

/// A versioned changeset (`NNNNNN.json`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ChangeSet {
    pub id: u64,
    /// HEAD when this changeset was created.
    pub parent: Option<u64>,
    pub ts: TimestampMs,
    /// Last status transition.
    pub updated: TimestampMs,
    pub author: Author,
    pub origin: Origin,
    pub scope: Scope,
    pub description: String,
    pub risk: Risk,
    pub ops: Vec<OpRecord>,
    pub status: Status,
    /// Dead-man deadline: auto-reverted at this time unless confirmed.
    pub ttl_until: Option<TimestampMs>,
    /// This changeset reverts that one.
    pub reverts: Option<u64>,
    /// This changeset returned the system to that version (`revert_to`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revert_to: Option<u64>,
    /// This changeset was reverted by that one.
    #[serde(default)]
    pub reverted_by: Option<u64>,
    /// Why it failed (or what went wrong while rolling back).
    #[serde(default)]
    pub error: Option<String>,
    /// `config/NNNNNN.toml`, when this changeset edited the config file.
    #[serde(default)]
    pub config_snapshot: Option<String>,
}

impl ChangeSet {
    /// Created by `revert`/`revert_to`/expiry rather than by a caller's plan.
    pub fn is_revert(&self) -> bool {
        self.reverts.is_some() || self.revert_to.is_some()
    }

    /// Applied or confirmed: its values are (still) in effect.
    pub fn is_effective(&self) -> bool {
        matches!(self.status, Status::Applied | Status::Confirmed)
    }

    pub fn keys(&self) -> Vec<String> {
        let mut k: Vec<String> = self.ops.iter().flat_map(|o| o.op.keys()).collect();
        k.sort();
        k.dedup();
        k
    }

    /// `Event::Notice { name, data: <this changeset> }` for the daemons to forward
    /// (`change.applied`, `change.confirmed`, `change.reverted`, `change.failed`,
    /// `change.expired`).
    pub fn notice(&self, name: &str) -> Event {
        let verb = name.strip_prefix("change.").unwrap_or(name);
        let mut message = format!("changeset {} {verb}", self.id);
        if !self.description.is_empty() {
            message.push_str(&format!(": {}", self.description));
        }
        if let Some(e) = &self.error {
            message.push_str(&format!(" ({e})"));
        }
        Event::Notice {
            ts: self.updated,
            name: name.to_string(),
            source: "changes".into(),
            message,
            data: serde_json::to_value(self).unwrap_or_default(),
        }
    }
}

/// Which guardrail refused an op.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    Empty,
    TooManyOps,
    RateLimited,
    Scope,
    ConfigKey,
    ConfigInvalid,
    SafetyLoosened,
    ProfileUnknown,
    Glob,
    SysctlNotAllowed,
    OutOfRange,
    DeviceUnknown,
    SchedulerUnavailable,
    EppUnavailable,
    GovernorUnavailable,
    Zswap,
    NotAvailable,
    TtlRequired,
}

/// A guardrail refusal.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Rejection {
    /// Index of the offending op; `None` for changeset-wide rules.
    pub index: Option<usize>,
    pub rule: Rule,
    pub message: String,
}

impl Rejection {
    pub fn new(index: Option<usize>, rule: Rule, message: impl Into<String>) -> Self {
        Rejection { index, rule, message: message.into() }
    }
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.index {
            Some(i) => write!(f, "op {i}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// An op in a [`Plan`] with its current and target values.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PlannedOp {
    #[serde(flatten)]
    pub op: ChangeOp,
    pub risk: Risk,
    /// Value now (`null` when absent or unreadable).
    pub current: Value,
    /// Value after applying.
    pub target: Value,
}

/// The result of [`crate::ChangeStore::plan`]: what would change, how risky it is, and
/// what the guardrails refused. Has no side effects.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Plan {
    pub scope: Scope,
    pub ops: Vec<PlannedOp>,
    /// The highest op risk.
    pub risk: Risk,
    /// Human-readable unified diff (a real TOML diff for config edits).
    pub diff: String,
    pub rejections: Vec<Rejection>,
    /// Applying needs a ttl (High risk from MCP).
    pub ttl_required: bool,
    /// Set only by `ChangeStore::plan`, so a deserialized or hand-built plan can't be applied.
    #[serde(skip)]
    pub(crate) validated: bool,
}

impl Plan {
    /// No rejections.
    pub fn is_ok(&self) -> bool {
        self.rejections.is_empty()
    }

    pub fn change_ops(&self) -> Vec<ChangeOp> {
        self.ops.iter().map(|p| p.op.clone()).collect()
    }
}

/// True when two conflict keys overlap (equal, or one is a dotted prefix of the other).
pub fn keys_overlap(a: &str, b: &str) -> bool {
    let prefix = |short: &str, long: &str| long.starts_with(short) && long[short.len()..].starts_with('.');
    a == b || prefix(a, b) || prefix(b, a)
}
