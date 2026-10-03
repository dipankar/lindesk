//! Tunables of the `threshold` policy.
//!
//! Everything has a default, so `config = {}` (or no config at all) is valid. Durations are
//! humantime strings (`"500ms"`, `"5s"`), sizes are bytesize strings (`"512MiB"`) or plain
//! byte counts, ratios are fractions in `0..=1` (a PSI "some" ratio of `0.30` means some task
//! was stalled 30% of the time).

use std::time::Duration;

use lindesk_core::config::{dur, size};
use lindesk_plugin_api::Resource;
use serde::{Deserialize, Serialize};

const MIB: u64 = 1024 * 1024;

/// Configuration of [`super::ThresholdPolicy`], read from `[plugins.threshold.config]`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ThresholdConfig {
    /// Length of an interaction epoch. A focus change or an input event for the foreground
    /// starts (or extends) an epoch lasting until `event time + epoch`.
    #[serde(with = "dur")]
    pub epoch: Duration,

    /// Half-life of the EWMA applied to system-wide PSI ratios. A sample covering `dt` gets
    /// weight `1 - 2^(-dt / half_life)`. Ignored when [`Self::ewma_alpha`] is set.
    #[serde(with = "dur")]
    pub ewma_half_life: Duration,
    /// Fixed EWMA weight (`0 < alpha <= 1`) per sample, overriding `ewma_half_life`.
    /// `1.0` disables smoothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ewma_alpha: Option<f64>,

    /// YELLOW enter thresholds for the smoothed system-wide PSI "some" ratios.
    pub enter: ResourceThresholds,
    /// Exit thresholds are `enter * exit_ratio` (also applied to the foreground and memory
    /// thresholds). Must be in `0..1`; lower means stickier levels.
    pub exit_ratio: f64,
    /// YELLOW when the foreground cgroup's worst "some" ratio (cpu, io or memory) exceeds this.
    pub foreground_yellow: f64,
    /// RED when the foreground cgroup's worst "some" ratio exceeds this for `red_after`.
    /// While RED, AIMD halves `io.max` as long as the foreground stays above it.
    pub foreground_red: f64,
    /// How long the foreground must stall above `foreground_red` before RED.
    #[serde(with = "dur")]
    pub red_after: Duration,
    /// RED (immediately) when the smoothed system-wide memory "full" ratio exceeds this.
    pub memory_full_red: f64,
    /// `memory.high` is put on batch targets only after memory "full" has stayed above
    /// `memory_full_red` for this long (and only while RED).
    #[serde(with = "dur")]
    pub mem_red_after: Duration,

    /// A level steps down (one level at a time) only after its exit condition has held
    /// continuously for this long.
    #[serde(with = "dur")]
    pub recover_after: Duration,
    /// Minimum time spent in YELLOW before stepping down to GREEN.
    #[serde(with = "dur")]
    pub yellow_min_dwell: Duration,
    /// Minimum time spent in RED before stepping down to YELLOW.
    #[serde(with = "dur")]
    pub red_min_dwell: Duration,

    /// `memory.low` protection for targets classed critical.
    #[serde(with = "size")]
    pub critical_memory_low: u64,
    /// Upper bound for the foreground's `memory.low` (which is its current usage).
    #[serde(with = "size")]
    pub fg_memory_low_max: u64,

    /// Lower bound for AIMD `io.max` limits (bytes/s, per direction). Also the minimum
    /// "observed" rate the initial limit is derived from.
    #[serde(with = "size")]
    pub aimd_floor: u64,
    /// Additive increase per `aimd_interval` while RED but the foreground no longer stalls.
    #[serde(with = "size")]
    pub aimd_step: u64,
    /// Multiplicative decrease factor per `aimd_interval` while the foreground stalls, and
    /// the factor applied to the observed rate when a limit is first set. In `0..1`.
    pub aimd_decrease: f64,
    /// Time between AIMD adjustments.
    #[serde(with = "dur")]
    pub aimd_interval: Duration,

    /// `memory.high` for batch targets under sustained memory pressure, as a fraction of
    /// their `memory.current` when it was first set. The value is latched (not recomputed
    /// every tick) so it cannot ratchet down.
    pub memory_high_ratio: f64,

    /// Priority of the emitted intents (built-ins use 0).
    pub priority: i32,

    /// The concrete values written at each level.
    pub values: ThresholdValues,
}

impl Default for ThresholdConfig {
    fn default() -> Self {
        ThresholdConfig {
            epoch: Duration::from_millis(500),
            ewma_half_life: Duration::from_secs(2),
            ewma_alpha: None,
            enter: ResourceThresholds::default(),
            exit_ratio: 0.5,
            foreground_yellow: 0.05,
            foreground_red: 0.10,
            red_after: Duration::from_secs(1),
            memory_full_red: 0.20,
            mem_red_after: Duration::from_secs(3),
            recover_after: Duration::from_secs(5),
            yellow_min_dwell: Duration::from_secs(3),
            red_min_dwell: Duration::from_secs(2),
            critical_memory_low: 512 * MIB,
            fg_memory_low_max: 2048 * MIB,
            aimd_floor: 16 * MIB,
            aimd_step: 32 * MIB,
            aimd_decrease: 0.5,
            aimd_interval: Duration::from_secs(1),
            memory_high_ratio: 0.9,
            priority: 0,
            values: ThresholdValues::default(),
        }
    }
}

/// One threshold per PSI resource.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ResourceThresholds {
    /// CPU "some" ratio.
    pub cpu: f64,
    /// I/O "some" ratio.
    pub io: f64,
    /// Memory "some" ratio.
    pub memory: f64,
}

impl Default for ResourceThresholds {
    fn default() -> Self {
        ResourceThresholds { cpu: 0.30, io: 0.20, memory: 0.10 }
    }
}

impl ResourceThresholds {
    /// Threshold for `r`.
    pub fn get(&self, r: Resource) -> f64 {
        match r {
            Resource::Cpu => self.cpu,
            Resource::Io => self.io,
            Resource::Memory => self.memory,
        }
    }
}

/// The values the policy writes. Weights apply to both `cpu.weight` and `io.weight` unless
/// the name says otherwise (100 is the kernel default). Percentages are uclamp percentages.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ThresholdValues {
    /// Critical targets, always.
    pub critical_weight: u64,
    /// Foreground during an interaction epoch.
    pub fg_epoch_weight: u64,
    /// Foreground `cpu.uclamp.min` during an interaction epoch.
    pub fg_epoch_uclamp_min: f32,
    /// Foreground outside an epoch.
    pub fg_weight: u64,
    /// Background targets, always.
    pub background_weight: u64,
    /// Batch `io.weight`, always (batch also gets `cpu.idle=1`).
    pub batch_io_weight: u64,
    /// Background targets at YELLOW and above.
    pub yellow_background_weight: u64,
    /// Background `cpu.uclamp.max` at YELLOW and above.
    pub yellow_background_uclamp_max: f32,
    /// Batch `io.weight` at YELLOW and above.
    pub yellow_batch_io_weight: u64,
    /// Batch `cpu.uclamp.max` at YELLOW and above.
    pub yellow_batch_uclamp_max: f32,
}

impl Default for ThresholdValues {
    fn default() -> Self {
        ThresholdValues {
            critical_weight: 200,
            fg_epoch_weight: 500,
            fg_epoch_uclamp_min: 30.0,
            fg_weight: 200,
            background_weight: 50,
            batch_io_weight: 10,
            yellow_background_weight: 20,
            yellow_background_uclamp_max: 60.0,
            yellow_batch_io_weight: 1,
            yellow_batch_uclamp_max: 40.0,
        }
    }
}

impl ThresholdConfig {
    /// Parse the plugin's JSON config; `null` yields the defaults.
    pub fn from_json(v: &serde_json::Value) -> Result<Self, String> {
        let cfg: ThresholdConfig = if v.is_null() {
            ThresholdConfig::default()
        } else {
            serde_json::from_value(v.clone()).map_err(|e| e.to_string())?
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reject values that would make the controller misbehave.
    pub fn validate(&self) -> Result<(), String> {
        let ratio = |name: &str, v: f64, lo_open: bool| {
            let ok = if lo_open { v > 0.0 && v <= 1.0 } else { (0.0..=1.0).contains(&v) };
            if ok { Ok(()) } else { Err(format!("{name} = {v} is out of range")) }
        };
        for r in Resource::ALL {
            ratio(&format!("enter.{r}"), self.enter.get(r), false)?;
        }
        ratio("foreground_yellow", self.foreground_yellow, false)?;
        ratio("foreground_red", self.foreground_red, false)?;
        ratio("memory_full_red", self.memory_full_red, false)?;
        ratio("memory_high_ratio", self.memory_high_ratio, true)?;
        if !(self.exit_ratio > 0.0 && self.exit_ratio < 1.0) {
            return Err(format!("exit_ratio = {} must be in (0, 1)", self.exit_ratio));
        }
        if !(self.aimd_decrease > 0.0 && self.aimd_decrease < 1.0) {
            return Err(format!("aimd_decrease = {} must be in (0, 1)", self.aimd_decrease));
        }
        if let Some(a) = self.ewma_alpha {
            ratio("ewma_alpha", a, true)?;
        } else if self.ewma_half_life.is_zero() {
            return Err("ewma_half_life must be > 0 (or set ewma_alpha = 1.0 to disable smoothing)".into());
        }
        if self.foreground_red < self.foreground_yellow {
            return Err("foreground_red must be >= foreground_yellow".into());
        }
        if self.aimd_floor == 0 {
            return Err("aimd_floor must be > 0".into());
        }
        if self.aimd_interval.is_zero() {
            return Err("aimd_interval must be > 0".into());
        }
        Ok(())
    }

    /// EWMA weight for a sample covering `dt_ms`.
    pub(crate) fn alpha(&self, dt_ms: u64) -> f64 {
        match self.ewma_alpha {
            Some(a) => a,
            None => {
                let hl = self.ewma_half_life.as_millis().max(1) as f64;
                1.0 - (-(dt_ms.max(1) as f64) / hl).exp2()
            }
        }
    }
}
