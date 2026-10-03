//! Serialisable outputs of the engine: per-step reports, `status` and `explain`.
//! Each has a human-readable `Display` (what `lindeskctl` prints without `--json`).

use std::fmt;

use lindesk_plugin_api::{Event, Knob, Level, ProcInfo, Provenance, QosClass, Setting, Target, TimestampMs};
use serde::{Deserialize, Serialize};

use crate::registry::PluginHealth;

/// What happened to one planned action.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum ActionOutcome {
    Applied,
    Reverted,
    /// `dry_run`: recorded, not applied.
    DryRun,
    Failed {
        error: String,
    },
    /// No available actuator supports this target kind and knob.
    NoActuator,
}

/// One apply or revert.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ActionReport {
    pub target: Target,
    pub knob: Knob,
    /// The new value; `None` for a revert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setting: Option<Setting>,
    /// The value before lindesk touched the knob, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original: Option<Setting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actuator: Option<String>,
    #[serde(flatten)]
    pub outcome: ActionOutcome,
}

impl fmt::Display for ActionReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.setting {
            Some(s) => write!(f, "apply  {s} on {}", self.target)?,
            None => match &self.original {
                Some(o) => write!(f, "revert {} on {} (to {o})", self.knob, self.target)?,
                None => write!(f, "revert {} on {} (to default)", self.knob, self.target)?,
            },
        }
        if let Some(p) = &self.provenance {
            write!(f, " [{}", p.plugin)?;
            if !p.reason.is_empty() {
                write!(f, ": {}", p.reason)?;
            }
            if let Some(c) = &p.clamped_from {
                write!(f, "; clamped from {c}")?;
            }
            f.write_str("]")?;
        }
        match &self.outcome {
            ActionOutcome::Applied | ActionOutcome::Reverted => Ok(()),
            ActionOutcome::DryRun => f.write_str(" (dry run)"),
            ActionOutcome::Failed { error } => write!(f, " FAILED: {error}"),
            ActionOutcome::NoActuator => f.write_str(" (no actuator supports this)"),
        }
    }
}

/// Wall-clock cost of each pipeline stage, µs. Informational (not deterministic).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct StepTimings {
    pub sensors_us: u64,
    pub events_us: u64,
    pub classify_us: u64,
    pub policies_us: u64,
    pub resolve_us: u64,
    pub actuate_us: u64,
    pub sinks_us: u64,
    pub total_us: u64,
}

/// The result of one [`crate::Engine::step`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StepReport {
    pub now: TimestampMs,
    pub level: Level,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<Target>,
    pub actions: Vec<ActionReport>,
    /// Resolver and dispatch notes (clamps, rejections, plugin errors), human-readable.
    pub notes: Vec<String>,
    /// Every event recorded this step (input events plus host events).
    pub events: Vec<Event>,
    pub timings: StepTimings,
}

impl fmt::Display for StepReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "t={} level={}", self.now, self.level)?;
        if let Some(fg) = &self.foreground {
            write!(f, " foreground={fg}")?;
        }
        writeln!(f)?;
        for a in &self.actions {
            writeln!(f, "  {a}")?;
        }
        for n in &self.notes {
            writeln!(f, "  note: {n}")?;
        }
        Ok(())
    }
}

/// How a target's class was decided.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case", tag = "by")]
pub enum ClassSource {
    /// `lindeskctl set`, D-Bus `SetClass` or `lindeskctl run --batch`.
    Explicit { source: String },
    /// `[[override]]` number `index` (0-based) in the config.
    Override { index: usize },
    /// A classifier plugin.
    Classifier { plugin: String, confidence: f32 },
    /// Nobody had an opinion.
    Default,
}

impl fmt::Display for ClassSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClassSource::Explicit { source } => write!(f, "set explicitly by {source}"),
            ClassSource::Override { index } => write!(f, "config [[override]] #{}", index + 1),
            ClassSource::Classifier { plugin, confidence } => {
                write!(f, "classifier '{plugin}' (confidence {confidence:.2})")
            }
            ClassSource::Default => f.write_str("default (no classifier matched)"),
        }
    }
}

/// A class with its provenance.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ClassDecision {
    pub class: QosClass,
    pub source: ClassSource,
    #[serde(default)]
    pub reason: String,
}

/// A target in `status`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TargetSummary {
    pub target: Target,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<QosClass>,
    #[serde(default)]
    pub foreground: bool,
    /// Settings lindesk currently holds on it.
    #[serde(default)]
    pub settings: Vec<Setting>,
}

impl TargetSummary {
    /// The short name shown to humans: the process name if known, else the unit.
    pub fn label(&self) -> String {
        match (&self.comm, &self.target) {
            (Some(c), _) if !c.is_empty() => c.clone(),
            (_, Target::Unit { name, .. }) => name.clone(),
            (_, t) => t.to_string(),
        }
    }
}

/// A throttled target and how much it yielded compared to before the first throttle.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct YieldingTarget {
    #[serde(flatten)]
    pub summary: TargetSummary,
    /// When the first throttle was applied.
    pub since: TimestampMs,
    /// CPU usage change vs the pre-throttle baseline, percent (e.g. -31.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_delta_pct: Option<f64>,
    /// I/O bandwidth change vs the pre-throttle baseline, percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_delta_pct: Option<f64>,
}

impl fmt::Display for YieldingTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary.label())?;
        if let Some(c) = self.cpu_delta_pct {
            write!(f, " {c:+.0}% CPU")?;
        }
        if let Some(i) = self.io_delta_pct {
            write!(f, " {i:+.0}% IO")?;
        }
        Ok(())
    }
}

/// `lindeskctl status`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Status {
    pub now: TimestampMs,
    pub host: String,
    pub level: Level,
    pub dry_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<TargetSummary>,
    /// Critical and interactive targets (and the foreground).
    pub protected: Vec<TargetSummary>,
    /// Targets currently throttled.
    pub yielding: Vec<YieldingTarget>,
    pub plugins: Vec<PluginHealth>,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: level {}", self.host, self.level.as_str().to_uppercase())?;
        if self.dry_run {
            f.write_str(" (dry run: nothing is applied)")?;
        }
        writeln!(f)?;
        match &self.foreground {
            Some(fg) => writeln!(f, "foreground: {} ({})", fg.label(), fg.target)?,
            None => writeln!(f, "foreground: none")?,
        }
        if !self.protected.is_empty() {
            let names: Vec<String> = self.protected.iter().map(TargetSummary::label).collect();
            writeln!(f, "protected:  {}", names.join(", "))?;
        }
        if self.yielding.is_empty() {
            writeln!(f, "yielding:   nothing")?;
        } else {
            let names: Vec<String> = self.yielding.iter().map(ToString::to_string).collect();
            writeln!(f, "yielding:   {}", names.join(", "))?;
        }
        writeln!(f, "plugins:")?;
        for p in &self.plugins {
            let kinds: Vec<&str> = p.kinds.iter().map(|k| k.as_str()).collect();
            write!(
                f,
                "  {:<20} {:<9} {:<22} calls={} errors={} p99={}µs",
                p.name,
                p.state,
                kinds.join(","),
                p.stats.calls,
                p.stats.errors,
                p.stats.latency_p99_us
            )?;
            if let Some(d) = &p.detail {
                write!(f, " ({d})")?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

/// One knob in `explain`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ExplainedSetting {
    pub knob: Knob,
    pub value: Setting,
    /// Value before lindesk changed it (`None` = unknown, restored to the default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original: Option<Setting>,
    pub provenance: Provenance,
    pub since: TimestampMs,
}

/// Why something did or didn't happen to a target.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ExplainNote {
    pub ts: TimestampMs,
    pub last_seen: TimestampMs,
    /// `clamped`, `rejected`, `overruled`, `plugin_error`, `no_actuator`, `actuator_error`.
    pub kind: String,
    pub plugin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knob: Option<Knob>,
    pub message: String,
}

/// `lindeskctl explain <pid|unit>`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Explanation {
    pub target: Target,
    /// False when the engine has never seen this target.
    pub known: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proc: Option<ProcInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<ClassDecision>,
    pub foreground: bool,
    pub dry_run: bool,
    pub settings: Vec<ExplainedSetting>,
    pub notes: Vec<ExplainNote>,
}

impl fmt::Display for Explanation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}", self.target)?;
        if !self.known {
            return writeln!(f, "  not managed by lindesk (not watched, never focused, no explicit class)");
        }
        if let Some(p) = &self.proc
            && p.pid != 0
        {
            writeln!(f, "  process: {} (pid {}, uid {})", p.comm, p.pid, p.uid)?;
        }
        match &self.class {
            Some(c) => {
                write!(f, "  class:   {} ← {}", c.class, c.source)?;
                if !c.reason.is_empty() {
                    write!(f, ": {}", c.reason)?;
                }
                writeln!(f)?;
            }
            None => writeln!(f, "  class:   not yet classified")?,
        }
        if self.foreground {
            writeln!(f, "  foreground: yes (policies treat it as interactive)")?;
        }
        if self.settings.is_empty() {
            writeln!(f, "  settings: none (system defaults)")?;
        } else {
            writeln!(f, "  settings{}:", if self.dry_run { " (dry run, not applied)" } else { "" })?;
            for s in &self.settings {
                write!(f, "    {:<28} ← {}", s.value.to_string(), s.provenance.plugin)?;
                if !s.provenance.reason.is_empty() {
                    write!(f, ": {}", s.provenance.reason)?;
                }
                if let Some(c) = &s.provenance.clamped_from {
                    write!(f, " (clamped from {c})")?;
                }
                match &s.original {
                    Some(o) => write!(f, "; was {o}")?,
                    None => write!(f, "; was default")?,
                }
                writeln!(f)?;
            }
        }
        if !self.notes.is_empty() {
            writeln!(f, "  notes:")?;
            for n in &self.notes {
                write!(f, "    [{}] {}", n.kind, n.plugin)?;
                if let Some(k) = &n.knob {
                    write!(f, " {k}")?;
                }
                writeln!(f, ": {}", n.message)?;
            }
        }
        Ok(())
    }
}
