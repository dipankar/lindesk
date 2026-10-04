//! Tunables of the `threshold` policy.
//!
//! Everything has a default, so `config = {}` (or no config at all) is valid. Durations are
//! humantime strings (`"500ms"`, `"5s"`), sizes are bytesize strings (`"512MiB"`) or plain
//! byte counts, ratios are fractions in `0..=1` (a PSI "some" ratio of `0.30` means some task
//! was stalled 30% of the time).
//!
//! # Hardware-scaled tunables (`"auto"`)
//!
//! The tunables marked *auto* below default to `"auto"` and are resolved against
//! [`HostCaps::hardware`] on every evaluation (cheaply: the result is cached until the
//! hardware or power source changes). An explicit value always wins. With unknown hardware
//! (nothing detected) every `"auto"` resolves to the historical constant, so behaviour is
//! unchanged there. [`ThresholdConfig::resolve`] (and
//! [`super::ThresholdPolicy::effective_config`]) return the resolved values together with
//! the reason for each, for `lindeskctl explain`/`doctor`.
//!
//! | Tunable | Unknown hardware | Auto rule |
//! |---|---|---|
//! | `epoch` | 500ms | 300ms on battery (shorter boosts save energy), else 500ms |
//! | `enter.cpu` | 0.30 | 0.40 with ≤ 4 logical CPUs (one runnable task already shows as CPU pressure) |
//! | `enter.io` | 0.20 | 0.15 when a managed disk is an HDD (rotational disks feel stalls sooner) |
//! | `enter.memory` | 0.10 | 0.15 with zram/zswap (compressed reclaim is cheap, so "some" stalls start early and are benign) |
//! | `critical_memory_low` | 512MiB | min(512MiB, 4% of RAM), at least 128MiB |
//! | `fg_memory_low_max` | 2GiB | min(2GiB, 10% of RAM) |
//! | `aimd_floor` | 16MiB/s | per device: 2% of its nominal bandwidth, at least 4MiB/s |
//! | `aimd_step` | 32MiB/s | per device: 4% of its nominal bandwidth, at least 2MiB/s |
//! | `values.fg_epoch_uclamp_min` | 30% | 15% on battery |
//! | `values.yellow_background_uclamp_max` | 60% | 40% on battery |
//!
//! The AIMD bandwidth is the **measured** sequential bandwidth from a calibration
//! (`hardware.latency.storage[*].seq_{read,write}_bps`, the lower of the two) when one
//! exists and wasn't noisy, else the nominal bandwidth of the device kind; the "why" text
//! says which (`NVMe measured 1850MiB/s × 2%` or `NVMe nominal ~2000MiB/s × 2%`).
//! Nominal bandwidths come from [`lindesk_sys::hardware::nominal_bps`]: NVMe 2000MiB/s,
//! SSD 500, HDD 150, eMMC 250, SD 50, virtual 500, network 100. So an NVMe disk gets a
//! 40MiB/s floor and an 80MiB/s step, an HDD 4MiB/s and 6MiB/s. A device of unknown kind
//! keeps 16/32MiB/s. The initial AIMD limit derives from the floor, so it scales too.
//!
//! Hybrid CPUs (Intel P/E cores, ARM big.LITTLE) need no extra knob: the epoch
//! `uclamp.min` boost makes the scheduler (EAS on ARM, ITMT/capacity-aware placement on
//! Intel) favour performance cores for the foreground, and the YELLOW/RED `uclamp.max` on
//! batch keeps it on efficiency cores. Pinning with cpusets is deliberately not done here.
//! Virtualized hosts get no special scaling (PSI and I/O reflect the guest only); the
//! policy reports it in its metrics (`threshold.virtualized`) and in the resolution notes.

use std::fmt;
use std::time::Duration;

use lindesk_core::config::{dur, size};
use lindesk_plugin_api::{Hardware, HostCaps, PowerSource, Resource, StorageDevice, StorageKind};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// A tunable resolved from the hardware (`"auto"`, the default) or set explicitly.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Auto<T> {
    #[default]
    Auto,
    Value(T),
}

impl<T: Copy> Auto<T> {
    /// The explicit value, if any.
    pub fn explicit(self) -> Option<T> {
        match self {
            Auto::Auto => None,
            Auto::Value(v) => Some(v),
        }
    }
}

impl<T> From<T> for Auto<T> {
    fn from(v: T) -> Self {
        Auto::Value(v)
    }
}

fn is_auto(v: &serde_json::Value) -> bool {
    v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("auto"))
}

impl<T: Serialize> Serialize for Auto<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Auto::Auto => s.serialize_str("auto"),
            Auto::Value(v) => v.serialize(s),
        }
    }
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for Auto<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        if is_auto(&v) {
            return Ok(Auto::Auto);
        }
        T::deserialize(v).map(Auto::Value).map_err(serde::de::Error::custom)
    }
}

/// `"auto"` or a size (`"512MiB"`, bytes).
pub(crate) mod auto_size {
    use super::*;

    pub fn serialize<S: Serializer>(v: &Auto<u64>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Auto::Auto => s.serialize_str("auto"),
            Auto::Value(b) => size::serialize(b, s),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Auto<u64>, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        if is_auto(&v) {
            return Ok(Auto::Auto);
        }
        size::deserialize(v).map(Auto::Value).map_err(serde::de::Error::custom)
    }
}

/// `"auto"` or a duration (`"500ms"`).
pub(crate) mod auto_dur {
    use super::*;

    pub fn serialize<S: Serializer>(v: &Auto<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Auto::Auto => s.serialize_str("auto"),
            Auto::Value(d) => dur::serialize(d, s),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Auto<Duration>, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        if is_auto(&v) {
            return Ok(Auto::Auto);
        }
        dur::deserialize(v).map(Auto::Value).map_err(serde::de::Error::custom)
    }
}

/// Configuration of [`super::ThresholdPolicy`], read from `[plugins.threshold.config]`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ThresholdConfig {
    /// Length of an interaction epoch. A focus change or an input event for the foreground
    /// starts (or extends) an epoch lasting until `event time + epoch`.
    /// *auto*: 300ms on battery, else 500ms.
    #[serde(with = "auto_dur")]
    pub epoch: Auto<Duration>,

    /// Half-life of the EWMA applied to system-wide PSI ratios. A sample covering `dt` gets
    /// weight `1 - 2^(-dt / half_life)`. Ignored when [`Self::ewma_alpha`] is set.
    #[serde(with = "dur")]
    pub ewma_half_life: Duration,
    /// Fixed EWMA weight (`0 < alpha <= 1`) per sample, overriding `ewma_half_life`.
    /// `1.0` disables smoothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ewma_alpha: Option<f64>,

    /// YELLOW enter thresholds for the smoothed system-wide PSI "some" ratios (each *auto*).
    pub enter: EnterThresholds,
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
    /// *auto*: min(512MiB, 4% of RAM), at least 128MiB.
    #[serde(with = "auto_size")]
    pub critical_memory_low: Auto<u64>,
    /// Upper bound for the foreground's `memory.low` (which is its current usage).
    /// *auto*: min(2GiB, 10% of RAM).
    #[serde(with = "auto_size")]
    pub fg_memory_low_max: Auto<u64>,

    /// Lower bound for AIMD `io.max` limits (bytes/s, per direction). Also the minimum
    /// "observed" rate the initial limit is derived from.
    /// *auto*: per device, 2% of its nominal bandwidth, at least 4MiB/s.
    #[serde(with = "auto_size")]
    pub aimd_floor: Auto<u64>,
    /// Additive increase per `aimd_interval` while RED but the foreground no longer stalls.
    /// *auto*: per device, 4% of its nominal bandwidth, at least 2MiB/s.
    #[serde(with = "auto_size")]
    pub aimd_step: Auto<u64>,
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
            epoch: Auto::Auto,
            ewma_half_life: Duration::from_secs(2),
            ewma_alpha: None,
            enter: EnterThresholds::default(),
            exit_ratio: 0.5,
            foreground_yellow: 0.05,
            foreground_red: 0.10,
            red_after: Duration::from_secs(1),
            memory_full_red: 0.20,
            mem_red_after: Duration::from_secs(3),
            recover_after: Duration::from_secs(5),
            yellow_min_dwell: Duration::from_secs(3),
            red_min_dwell: Duration::from_secs(2),
            critical_memory_low: Auto::Auto,
            fg_memory_low_max: Auto::Auto,
            aimd_floor: Auto::Auto,
            aimd_step: Auto::Auto,
            aimd_decrease: 0.5,
            aimd_interval: Duration::from_secs(1),
            memory_high_ratio: 0.9,
            priority: 0,
            values: ThresholdValues::default(),
        }
    }
}

/// Configured YELLOW enter thresholds, each `"auto"` (the default) or a ratio.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct EnterThresholds {
    /// CPU "some" ratio. *auto*: 0.40 with ≤ 4 logical CPUs, else 0.30.
    pub cpu: Auto<f64>,
    /// I/O "some" ratio. *auto*: 0.15 when a managed disk is an HDD, else 0.20.
    pub io: Auto<f64>,
    /// Memory "some" ratio. *auto*: 0.15 with zram/zswap, else 0.10.
    pub memory: Auto<f64>,
}

impl EnterThresholds {
    /// The configured threshold for `r`.
    pub fn get(&self, r: Resource) -> Auto<f64> {
        match r {
            Resource::Cpu => self.cpu,
            Resource::Io => self.io,
            Resource::Memory => self.memory,
        }
    }
}

/// One resolved threshold per PSI resource.
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
    /// Foreground `cpu.uclamp.min` during an interaction epoch. *auto*: 15% on battery,
    /// else 30%. On hybrid CPUs this is also what steers the foreground onto P-cores.
    pub fg_epoch_uclamp_min: Auto<f32>,
    /// Foreground outside an epoch.
    pub fg_weight: u64,
    /// Background targets, always.
    pub background_weight: u64,
    /// Batch `io.weight`, always (batch also gets `cpu.idle=1`).
    pub batch_io_weight: u64,
    /// Background targets at YELLOW and above.
    pub yellow_background_weight: u64,
    /// Background `cpu.uclamp.max` at YELLOW and above. *auto*: 40% on battery, else 60%.
    pub yellow_background_uclamp_max: Auto<f32>,
    /// Batch `io.weight` at YELLOW and above.
    pub yellow_batch_io_weight: u64,
    /// Batch `cpu.uclamp.max` at YELLOW and above. On hybrid CPUs this keeps batch work on
    /// the efficiency cores.
    pub yellow_batch_uclamp_max: f32,
}

impl Default for ThresholdValues {
    fn default() -> Self {
        ThresholdValues {
            critical_weight: 200,
            fg_epoch_weight: 500,
            fg_epoch_uclamp_min: Auto::Auto,
            fg_weight: 200,
            background_weight: 50,
            batch_io_weight: 10,
            yellow_background_weight: 20,
            yellow_background_uclamp_max: Auto::Auto,
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
            if let Some(v) = self.enter.get(r).explicit() {
                ratio(&format!("enter.{r}"), v, false)?;
            }
        }
        let percent = |name: &str, v: Auto<f32>| match v.explicit() {
            Some(p) if !(0.0..=100.0).contains(&p) => Err(format!("{name} = {p} must be a percentage in 0..=100")),
            _ => Ok(()),
        };
        percent("values.fg_epoch_uclamp_min", self.values.fg_epoch_uclamp_min)?;
        percent("values.yellow_background_uclamp_max", self.values.yellow_background_uclamp_max)?;
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
        if self.aimd_floor == Auto::Value(0) {
            return Err("aimd_floor must be > 0".into());
        }
        if self.epoch == Auto::Value(Duration::ZERO) {
            return Err("epoch must be > 0".into());
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

// ── resolution against the hardware ───────────────────────────────────────────────────────

/// Values used when the hardware is unknown (and the historical fixed defaults).
pub mod legacy {
    use super::*;

    pub const EPOCH: Duration = Duration::from_millis(500);
    pub const ENTER_CPU: f64 = 0.30;
    pub const ENTER_IO: f64 = 0.20;
    pub const ENTER_MEMORY: f64 = 0.10;
    pub const CRITICAL_MEMORY_LOW: u64 = 512 * MIB;
    pub const FG_MEMORY_LOW_MAX: u64 = 2 * GIB;
    pub const AIMD_FLOOR: u64 = 16 * MIB;
    pub const AIMD_STEP: u64 = 32 * MIB;
    pub const FG_EPOCH_UCLAMP_MIN: f32 = 30.0;
    pub const YELLOW_BACKGROUND_UCLAMP_MAX: f32 = 60.0;
}

/// Battery: shorter epochs and a smaller foreground boost.
const BATTERY_EPOCH: Duration = Duration::from_millis(300);
const BATTERY_FG_UCLAMP_MIN: f32 = 15.0;
const BATTERY_BG_UCLAMP_MAX: f32 = 40.0;
const SMALL_CPU_COUNT: u32 = 4;
const SMALL_CPU_ENTER: f64 = 0.40;
const HDD_ENTER_IO: f64 = 0.15;
const COMPRESSED_SWAP_ENTER_MEMORY: f64 = 0.15;
const AIMD_FLOOR_PCT: u64 = 2;
const AIMD_STEP_PCT: u64 = 4;
const AIMD_MIN_FLOOR: u64 = 4 * MIB;
const AIMD_MIN_STEP: u64 = 2 * MIB;
const MIN_CRITICAL_MEMORY_LOW: u64 = 128 * MIB;

/// Where a resolved value came from.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Set explicitly in the configuration.
    Explicit,
    /// `"auto"`, scaled from the detected hardware.
    Auto,
    /// `"auto"`, but the hardware is unknown: the historical default.
    Default,
}

/// One resolved tunable and why it has its value.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Resolution {
    /// Config key, e.g. `aimd_floor[nvme0n1]` or `enter.io`.
    pub key: String,
    /// The value, formatted (`40MiB/s`, `0.15`, `300ms`).
    pub value: String,
    pub source: Source,
    /// Why, e.g. `NVMe ~2000MiB/s × 2%`.
    pub why: String,
}

impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let src = match self.source {
            Source::Explicit => "explicit",
            Source::Auto => "auto",
            Source::Default => "default",
        };
        write!(f, "{} {} ({src}: {})", self.key, self.value, self.why)
    }
}

/// AIMD parameters for one device.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct DeviceAimd {
    /// `MAJ:MIN`.
    pub dev: String,
    /// Kernel name (`nvme0n1`), or the device number when the hardware is unknown.
    pub name: String,
    pub kind: StorageKind,
    /// Bytes/s.
    pub floor: u64,
    /// Bytes/s.
    pub step: u64,
}

/// The configuration after resolving every `"auto"` against the hardware.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ResolvedThreshold {
    pub epoch_ms: u64,
    pub enter: ResourceThresholds,
    /// Bytes.
    pub critical_memory_low: u64,
    /// Bytes.
    pub fg_memory_low_max: u64,
    /// Percent.
    pub fg_epoch_uclamp_min: f32,
    /// Percent.
    pub yellow_background_uclamp_max: f32,
    /// Per managed device, in [`HostCaps::devices`] order.
    pub aimd: Vec<DeviceAimd>,
    /// Floor and step for a device missing from [`Self::aimd`] (bytes/s).
    pub aimd_default: (u64, u64),
    /// A hybrid CPU was detected: the epoch boost favours P-cores, batch caps keep E-cores busy.
    pub hybrid: bool,
    pub on_battery: bool,
    /// Hypervisor/container name; no scaling is applied for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtualization: Option<String>,
    /// Every resolved value with its reason, in config order.
    pub why: Vec<Resolution>,
}

impl Default for ResolvedThreshold {
    fn default() -> Self {
        ThresholdConfig::default().resolve(&HostCaps::default())
    }
}

impl ResolvedThreshold {
    /// The epoch length.
    pub fn epoch(&self) -> Duration {
        Duration::from_millis(self.epoch_ms)
    }

    /// AIMD `(floor, step)` for a device (`MAJ:MIN`), bytes/s.
    pub fn aimd_for(&self, dev: &str) -> (u64, u64) {
        self.aimd.iter().find(|a| a.dev == dev).map_or(self.aimd_default, |a| (a.floor, a.step))
    }
}

fn round_mib(b: u64) -> u64 {
    (b + MIB / 2) / MIB * MIB
}

fn fmt_size(b: u64) -> String {
    if b >= GIB && b.is_multiple_of(GIB) { format!("{}GiB", b / GIB) } else { format!("{}MiB", (b + MIB / 2) / MIB) }
}

fn fmt_ms(d: Duration) -> String {
    format!("{}ms", d.as_millis())
}

fn kind_name(k: StorageKind) -> &'static str {
    lindesk_sys::hardware::kind_label(k)
}

struct Resolver {
    why: Vec<Resolution>,
}

impl Resolver {
    /// Record and return a value: explicit wins, else `auto` (Some((value, why))) or the
    /// legacy default.
    fn pick<T: Copy>(
        &mut self,
        key: &str,
        explicit: Option<T>,
        auto: Option<(T, String)>,
        default: (T, &str),
        fmt: impl Fn(T) -> String,
    ) -> T {
        let (v, source, why) = match (explicit, auto) {
            (Some(v), _) => (v, Source::Explicit, "set in the configuration".to_string()),
            (None, Some((v, why))) => (v, Source::Auto, why),
            (None, None) => (default.0, Source::Default, default.1.to_string()),
        };
        self.why.push(Resolution { key: key.into(), value: fmt(v), source, why });
        v
    }
}

/// Measured sequential bandwidth of `dev` from a calibration that wasn't noisy: the lower of
/// read and write (I/O limits apply to both directions), ignoring implausible values.
fn measured_bps(hw: &Hardware, dev: &str) -> Option<u64> {
    let lat = hw.latency.as_ref().filter(|l| !l.noisy)?;
    let s = lat.storage.iter().find(|s| s.dev == dev)?;
    [s.seq_read_bps, s.seq_write_bps].into_iter().filter(|b| *b >= MIB).min()
}

fn aimd_auto(s: &StorageDevice, measured: Option<u64>) -> Option<(u64, u64, String, String)> {
    let (bw, base) = match measured {
        Some(m) => (m, format!("{} measured {}/s", kind_name(s.kind), fmt_size(m))),
        None if s.kind == StorageKind::Unknown || s.nominal_bps == 0 => return None,
        None => (s.nominal_bps, format!("{} nominal ~{}/s", kind_name(s.kind), fmt_size(s.nominal_bps))),
    };
    let nominal = bw;
    let floor = round_mib(nominal * AIMD_FLOOR_PCT / 100).max(AIMD_MIN_FLOOR);
    let step = round_mib(nominal * AIMD_STEP_PCT / 100).max(AIMD_MIN_STEP);
    let clamp = |v: u64, min: u64| if v == min && nominal * AIMD_FLOOR_PCT / 100 < min { ", clamped" } else { "" };
    let fwhy = format!("{base} × {AIMD_FLOOR_PCT}%{}", clamp(floor, AIMD_MIN_FLOOR));
    let swhy = format!("{base} × {AIMD_STEP_PCT}%");
    Some((floor, step, fwhy, swhy))
}

impl ThresholdConfig {
    /// Resolve every `"auto"` tunable against `caps.hardware` (see the module docs for the
    /// rules). Unknown hardware yields the historical defaults.
    pub fn resolve(&self, caps: &HostCaps) -> ResolvedThreshold {
        let hw = &caps.hardware;
        let battery = hw.power == PowerSource::Battery;
        let mut r = Resolver { why: Vec::new() };
        let ratio = |v: f64| format!("{v:.2}");
        let pct = |v: f32| format!("{v:.0}%");

        let epoch = r.pick(
            "epoch",
            self.epoch.explicit(),
            (hw.power != PowerSource::Unknown).then(|| {
                if battery {
                    (BATTERY_EPOCH, "on battery: shorter interaction boosts save energy".to_string())
                } else {
                    (legacy::EPOCH, "on AC".to_string())
                }
            }),
            (legacy::EPOCH, "power source unknown"),
            fmt_ms,
        );

        // Managed disks (caps.devices order), falling back to everything detected.
        let managed: Vec<&StorageDevice> = if caps.devices.is_empty() {
            hw.storage.iter().collect()
        } else {
            caps.devices.iter().filter_map(|d| hw.storage_for(d)).collect()
        };

        let cpu = r.pick(
            "enter.cpu",
            self.enter.cpu.explicit(),
            (hw.cpus > 0).then(|| {
                if hw.cpus <= SMALL_CPU_COUNT {
                    (SMALL_CPU_ENTER, format!("{} logical CPUs: single runnable spikes are normal", hw.cpus))
                } else {
                    (legacy::ENTER_CPU, format!("{} logical CPUs", hw.cpus))
                }
            }),
            (legacy::ENTER_CPU, "CPU count unknown"),
            ratio,
        );
        let io = r.pick(
            "enter.io",
            self.enter.io.explicit(),
            (!managed.is_empty()).then(|| match managed.iter().find(|s| s.kind == StorageKind::Hdd) {
                Some(s) => (HDD_ENTER_IO, format!("{} is a rotational disk: stalls are felt sooner", s.name)),
                None => (legacy::ENTER_IO, "no rotational disk".to_string()),
            }),
            (legacy::ENTER_IO, "disks unknown"),
            ratio,
        );
        let memory = r.pick(
            "enter.memory",
            self.enter.memory.explicit(),
            (hw.mem_total > 0).then(|| {
                if hw.compressed_swap {
                    (COMPRESSED_SWAP_ENTER_MEMORY, "compressed swap (zram/zswap): reclaim is cheap".to_string())
                } else {
                    (legacy::ENTER_MEMORY, "no compressed swap".to_string())
                }
            }),
            (legacy::ENTER_MEMORY, "memory unknown"),
            ratio,
        );

        let ram = lindesk_sys::hardware::fmt_bytes(hw.mem_total);
        let critical_memory_low = r.pick(
            "critical_memory_low",
            self.critical_memory_low.explicit(),
            (hw.mem_total > 0).then(|| {
                let v =
                    round_mib((hw.mem_total * 4 / 100).min(legacy::CRITICAL_MEMORY_LOW)).max(MIN_CRITICAL_MEMORY_LOW);
                (v, format!("min(512MiB, 4% of {ram}), at least 128MiB"))
            }),
            (legacy::CRITICAL_MEMORY_LOW, "RAM unknown"),
            fmt_size,
        );
        let fg_memory_low_max = r.pick(
            "fg_memory_low_max",
            self.fg_memory_low_max.explicit(),
            (hw.mem_total > 0).then(|| {
                let v = round_mib((hw.mem_total / 10).min(legacy::FG_MEMORY_LOW_MAX));
                (v, format!("min(2GiB, 10% of {ram})"))
            }),
            (legacy::FG_MEMORY_LOW_MAX, "RAM unknown"),
            fmt_size,
        );

        let mut aimd = Vec::new();
        let bps = |v: u64| format!("{}/s", fmt_size(v));
        for dev in &caps.devices {
            let s = hw.storage_for(dev);
            let name = s.map_or_else(|| dev.clone(), |s| s.name.clone());
            let auto = s.and_then(|s| aimd_auto(s, measured_bps(hw, dev)));
            let floor = r.pick(
                &format!("aimd_floor[{name}]"),
                self.aimd_floor.explicit(),
                auto.as_ref().map(|a| (a.0, a.2.clone())),
                (legacy::AIMD_FLOOR, "device kind unknown"),
                bps,
            );
            let step = r.pick(
                &format!("aimd_step[{name}]"),
                self.aimd_step.explicit(),
                auto.as_ref().map(|a| (a.1, a.3.clone())),
                (legacy::AIMD_STEP, "device kind unknown"),
                bps,
            );
            aimd.push(DeviceAimd {
                dev: dev.clone(),
                name,
                kind: s.map_or(StorageKind::Unknown, |s| s.kind),
                floor,
                step,
            });
        }
        let aimd_default = (
            self.aimd_floor.explicit().unwrap_or(legacy::AIMD_FLOOR),
            self.aimd_step.explicit().unwrap_or(legacy::AIMD_STEP),
        );

        let power_auto = |on_batt: f32, on_ac: f32| {
            (hw.power != PowerSource::Unknown).then(|| {
                if battery { (on_batt, "on battery: energy first".to_string()) } else { (on_ac, "on AC".to_string()) }
            })
        };
        let fg_epoch_uclamp_min = r.pick(
            "values.fg_epoch_uclamp_min",
            self.values.fg_epoch_uclamp_min.explicit(),
            power_auto(BATTERY_FG_UCLAMP_MIN, legacy::FG_EPOCH_UCLAMP_MIN),
            (legacy::FG_EPOCH_UCLAMP_MIN, "power source unknown"),
            pct,
        );
        let yellow_background_uclamp_max = r.pick(
            "values.yellow_background_uclamp_max",
            self.values.yellow_background_uclamp_max.explicit(),
            power_auto(BATTERY_BG_UCLAMP_MAX, legacy::YELLOW_BACKGROUND_UCLAMP_MAX),
            (legacy::YELLOW_BACKGROUND_UCLAMP_MAX, "power source unknown"),
            pct,
        );

        let hybrid = hw.is_hybrid();
        if hybrid {
            r.why.push(Resolution {
                key: "hybrid".into(),
                value: format!("{} P + {} E logical CPUs", hw.performance_cpus.len(), hw.efficiency_cpus.len()),
                source: Source::Auto,
                why: "epoch uclamp.min steers the foreground to P-cores (EAS/ITMT); YELLOW/RED uclamp.max on batch \
                      keeps it on E-cores"
                    .into(),
            });
        }
        if let Some(v) = &hw.virtualization {
            r.why.push(Resolution {
                key: "virtualization".into(),
                value: v.clone(),
                source: Source::Auto,
                why: "no scaling: PSI and I/O control reflect the guest only".into(),
            });
        }

        ResolvedThreshold {
            epoch_ms: u64::try_from(epoch.as_millis()).unwrap_or(u64::MAX),
            enter: ResourceThresholds { cpu, io, memory },
            critical_memory_low,
            fg_memory_low_max,
            fg_epoch_uclamp_min,
            yellow_background_uclamp_max,
            aimd,
            aimd_default,
            hybrid,
            on_battery: battery,
            virtualization: hw.virtualization.clone(),
            why: r.why,
        }
    }
}
