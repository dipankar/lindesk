//! The `threshold` policy: GREEN ⇄ YELLOW ⇄ RED with hysteresis, interaction epochs and
//! `io.max` AIMD admission control.
//!
//! # Thesis
//!
//! Do nothing while the machine is healthy. When foreground latency deteriorates, intervene
//! progressively and with hysteresis. Never make interactive work dangerous; make
//! non-interactive work yield.
//!
//! # Signals
//!
//! * **System PSI**: the `some` ratio of each resource (and memory `full`) over the sample's
//!   delta interval, smoothed with an EWMA (half-life or fixed alpha).
//! * **Foreground stall**: the worst raw `some` ratio (cpu, io, memory) measured inside the
//!   foreground target's own cgroup. This is the primary "the human is suffering" signal.
//! * **Interaction epochs**: a `Focus` event, or an `Interaction { end: false }` event for the
//!   foreground, starts or extends an epoch lasting until `event ts + epoch`.
//!   `Interaction { end: true }` ends it early. Focusing a different target starts a new
//!   epoch for that target.
//!
//! # Levels
//!
//! | Level  | Enter                                                              | Step down after             |
//! |--------|--------------------------------------------------------------------|-----------------------------|
//! | YELLOW | any smoothed system `some` > `enter.*`, or foreground > `foreground_yellow` | everything below `exit_ratio × enter` for `recover_after`, and `yellow_min_dwell` elapsed |
//! | RED    | foreground > `foreground_red` for `red_after`, or memory `full` > `memory_full_red` | both below `exit_ratio ×` their thresholds for `recover_after`, and `red_min_dwell` elapsed |
//!
//! The level moves at most one step per evaluation in either direction.
//!
//! # Intents
//!
//! The complete set is returned on every evaluation (the host reverts anything omitted):
//!
//! * always (work-conserving): critical gets higher weights and `memory.low`; background gets
//!   lower weights; batch gets `cpu.idle` and a low `io.weight`; normal gets nothing.
//! * foreground: a strong boost (weights, `uclamp.min`, `memory.low`) during an epoch, a mild
//!   one otherwise. The foreground is never throttled, whatever its class.
//! * YELLOW: background and batch yield harder (weights, `uclamp.max`).
//! * RED: `io.max` AIMD on background and batch per device, and `memory.high` on batch under
//!   sustained memory `full` pressure.
//!
//! # Hardware awareness
//!
//! Tunables default to `"auto"` and are resolved against [`HostCaps::hardware`] (CPU count,
//! RAM, compressed swap, disk kinds, power source): see [`config`](ThresholdConfig) for the
//! rules and [`ThresholdPolicy::effective_config`] for the resolved values with reasons.
//! Unknown hardware resolves to the historical constants.

mod config;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use lindesk_plugin_api::{
    Event, Hardware, HostCaps, InitContext, Intent, IoLimit, Knob, Level, Metric, Plugin, PluginError, PluginInfo,
    PluginKind, PluginResult, Policy, PolicyOutput, QosClass, Resource, Sample, Setting, Snapshot, Target, TimestampMs,
    UnitState,
};

pub use config::{
    Auto, DeviceAimd, EnterThresholds, Resolution, ResolvedThreshold, ResourceThresholds, Source, ThresholdConfig,
    ThresholdValues,
};

/// Plugin name.
pub const NAME: &str = "threshold";

const MIB: f64 = 1024.0 * 1024.0;

/// The built-in `threshold` policy. See the [module docs](self).
///
/// Deterministic: all time comes from [`Snapshot::now`] and event timestamps, so replaying
/// the same snapshots yields the same output.
#[derive(Debug, Clone, Default)]
pub struct ThresholdPolicy {
    cfg: ThresholdConfig,
    st: State,
    /// `cfg` resolved against the hardware of the last snapshot.
    res: ResolvedThreshold,
    /// The (devices, hardware) `res` was resolved for; `None` = not yet resolved.
    res_key: Option<(Vec<String>, Hardware)>,
}

/// Exponentially weighted moving average fed by PSI samples.
#[derive(Debug, Clone, Copy, Default)]
struct Ewma {
    value: f64,
    /// Timestamp of the last sample consumed, so a repeated sample is not counted twice.
    last_ts: Option<TimestampMs>,
}

impl Ewma {
    fn update(&mut self, ts: TimestampMs, x: f64, alpha: f64) {
        if self.last_ts == Some(ts) {
            return;
        }
        self.last_ts = Some(ts);
        self.value += alpha * (x - self.value);
    }
}

/// An interaction epoch for one target.
#[derive(Debug, Clone, PartialEq)]
struct Epoch {
    target: Target,
    until: TimestampMs,
}

/// AIMD state for one (target, device).
#[derive(Debug, Clone, PartialEq)]
struct Aimd {
    rbps: u64,
    wbps: u64,
    last_step: TimestampMs,
    step: u32,
    action: &'static str,
}

#[derive(Debug, Clone, Default)]
struct State {
    started: bool,
    some: [Ewma; 3],
    mem_full: Ewma,
    level: Level,
    entered_at: TimestampMs,
    exit_since: Option<TimestampMs>,
    fg_red_since: Option<TimestampMs>,
    mem_full_since: Option<TimestampMs>,
    epoch: Option<Epoch>,
    aimd: BTreeMap<(Target, String), Aimd>,
    /// Latched `memory.high` per batch target.
    mem_high: BTreeMap<Target, u64>,
    /// Apps that have had a window on screen (or focus): the only ones that can nap.
    had_window: BTreeSet<Target>,
    /// Since when each of them has had no window on screen.
    hidden_since: BTreeMap<Target, TimestampMs>,
    /// When each app last had focus (for the warm set).
    last_focus: BTreeMap<Target, TimestampMs>,
    /// The app the last `Focus` event named (to tell a switch from a re-focus).
    last_focused: Option<Target>,
}

/// The inputs of one evaluation, after smoothing.
#[derive(Debug, Clone, Copy, Default)]
struct Signals {
    /// Smoothed system `some` ratio per resource (indexed like [`Resource::ALL`]).
    some: [f64; 3],
    /// Smoothed system memory `full` ratio.
    mem_full: f64,
    /// Worst raw `some` ratio inside the foreground cgroup.
    fg: Option<(Resource, f64)>,
}

impl Signals {
    fn fg_ratio(&self) -> f64 {
        self.fg.map_or(0.0, |(_, v)| v)
    }
}

fn idx(r: Resource) -> usize {
    match r {
        Resource::Cpu => 0,
        Resource::Io => 1,
        Resource::Memory => 2,
    }
}

/// `w`, or the weight already planned for this knob when that is higher.
fn weight_at_least(plan: &Plan, t: &Target, knob: Knob, w: u64) -> u64 {
    match plan.get(t, knob) {
        Some(Setting::CpuWeight { value } | Setting::IoWeight { value }) => w.max(*value),
        _ => w,
    }
}

/// `v` rounded up to a multiple of `step`.
fn round_up(v: u64, step: u64) -> u64 {
    if step == 0 { v } else { v.div_ceil(step).saturating_mul(step) }
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

fn pct(v: f64) -> String {
    format!("{:.0}%", v * 100.0)
}

fn mib(v: u64) -> String {
    format!("{:.0}MiB/s", v as f64 / MIB)
}

fn up(l: Level) -> Level {
    match l {
        Level::Green => Level::Yellow,
        _ => Level::Red,
    }
}

fn down(l: Level) -> Level {
    match l {
        Level::Red => Level::Yellow,
        _ => Level::Green,
    }
}

fn level_value(l: Level) -> f64 {
    match l {
        Level::Green => 0.0,
        Level::Yellow => 1.0,
        Level::Red => 2.0,
    }
}

/// Latest sample per resource from a list (by timestamp).
fn latest(samples: &[Sample], r: Resource) -> Option<&Sample> {
    samples.iter().filter(|s| s.resource == r).max_by_key(|s| s.ts)
}

/// Bytes per second from a byte delta over `interval_ms`.
/// I/O below this (bytes/s, reads plus writes) is not "doing I/O" for the adaptive limit.
const ACTIVE_IO: u64 = 1024 * 1024;
/// However many units share a device's floor, none goes below this (or the floor itself).
const SHARED_FLOOR_MIN: u64 = 4 * 1024 * 1024;

/// A unit's I/O rate over its last usage interval, bytes/s, both directions.
fn io_rate(u: &UnitState) -> u64 {
    u.usage.as_ref().map_or(0, |us| rate(us.io_rbytes.saturating_add(us.io_wbytes), us.interval_ms))
}

fn rate(bytes: u64, interval_ms: u32) -> u64 {
    if interval_ms == 0 { 0 } else { bytes.saturating_mul(1000) / u64::from(interval_ms) }
}

/// Collects intents keyed by (target, knob): a later `set` for the same knob replaces the
/// earlier one, which is how level layers override the static layer.
#[derive(Default)]
struct Plan {
    map: BTreeMap<(Target, Knob), (Setting, String)>,
}

impl Plan {
    fn set(&mut self, target: &Target, setting: Setting, reason: impl Into<String>) {
        self.map.insert((target.clone(), setting.knob()), (setting, reason.into()));
    }

    fn get(&self, target: &Target, knob: Knob) -> Option<&Setting> {
        self.map.get(&(target.clone(), knob)).map(|(s, _)| s)
    }

    fn into_intents(self, priority: i32) -> Vec<Intent> {
        self.map
            .into_iter()
            .map(|((target, _), (setting, reason))| Intent { target, setting, priority, reason })
            .collect()
    }
}

impl ThresholdPolicy {
    /// A policy with the given configuration (call [`ThresholdConfig::validate`] first if it
    /// comes from an untrusted source; [`Plugin::init`] does).
    pub fn new(cfg: ThresholdConfig) -> Self {
        ThresholdPolicy { cfg, ..Default::default() }
    }

    /// The active configuration.
    pub fn config(&self) -> &ThresholdConfig {
        &self.cfg
    }

    /// The configuration with every `"auto"` resolved against `caps.hardware`, with the
    /// reason for each value (for `lindeskctl explain` and `doctor`).
    pub fn effective_config(&self, caps: &HostCaps) -> ResolvedThreshold {
        self.cfg.resolve(caps)
    }

    /// Re-resolve the configuration when the devices or hardware (e.g. power source) changed.
    fn refresh(&mut self, caps: &HostCaps) {
        let fresh = self.res_key.as_ref().is_some_and(|(d, h)| d == &caps.devices && h == &caps.hardware);
        if !fresh {
            self.res = self.cfg.resolve(caps);
            self.res_key = Some((caps.devices.clone(), caps.hardware.clone()));
        }
    }

    /// The current health level.
    pub fn level(&self) -> Level {
        self.st.level
    }

    fn signals(&mut self, snap: &Snapshot, fg_units: &[&UnitState]) -> Signals {
        let cfg = &self.cfg;
        for r in Resource::ALL {
            if let Some(s) = snap.system_pressure(r) {
                let a = cfg.alpha(u64::from(s.interval_ms));
                self.st.some[idx(r)].update(s.ts, f64::from(s.some_ratio()), a);
                if r == Resource::Memory
                    && let Some(full) = s.full_ratio()
                {
                    self.st.mem_full.update(s.ts, f64::from(full), a);
                }
            }
        }
        // An application can span several units (a browser's renderers): the worst stall
        // in any of them is the one the person feels.
        let fg = fg_units
            .iter()
            .flat_map(|u| {
                Resource::ALL.into_iter().filter_map(|r| latest(&u.pressure, r).map(|s| (r, f64::from(s.some_ratio()))))
            })
            .max_by(|a, b| a.1.total_cmp(&b.1));
        Signals { some: self.st.some.map(|e| e.value), mem_full: self.st.mem_full.value, fg }
    }

    /// Apply focus/interaction events to the epoch state.
    fn process_events(&mut self, snap: &Snapshot, fg: Option<&Target>) {
        let epoch_ms = i64::try_from(self.res.epoch_ms).unwrap_or(i64::MAX);
        // On battery the switch boost is capped at one second: enough to repaint, not a habit.
        let switch_ms =
            if self.res.on_battery { ms(self.cfg.switch_epoch).min(1000) } else { ms(self.cfg.switch_epoch) };
        let mut events: Vec<&Event> = snap.events.iter().collect();
        events.sort_by_key(|e| e.ts());
        for e in events {
            match e {
                Event::Focus { ts, target, .. } => {
                    if let Some(t) = target.as_ref().or(fg) {
                        // Switching to another app earns the longer boost: it has to page
                        // itself back in and repaint. Re-focusing the same one is a keypress.
                        let switched = self.st.last_focused.as_ref() != Some(t);
                        self.st.last_focused = Some(t.clone());
                        let len = if switched { switch_ms.max(epoch_ms) } else { epoch_ms };
                        let until = ts.saturating_add(len);
                        let until = match &self.st.epoch {
                            Some(ep) if &ep.target == t => ep.until.max(until),
                            _ => until,
                        };
                        self.st.epoch = Some(Epoch { target: t.clone(), until });
                    }
                }
                Event::Interaction { ts, target, end, .. } => {
                    // Interactions without a resolved target refer to whatever has focus.
                    let Some(t) = target.as_ref().or(fg) else { continue };
                    if Some(t) != fg {
                        continue;
                    }
                    if *end {
                        if self.st.epoch.as_ref().is_some_and(|ep| &ep.target == t) {
                            self.st.epoch = None;
                        }
                    } else {
                        let until = ts.saturating_add(epoch_ms);
                        let until = match &self.st.epoch {
                            Some(ep) if &ep.target == t => ep.until.max(until),
                            _ => until,
                        };
                        self.st.epoch = Some(Epoch { target: t.clone(), until });
                    }
                }
                _ => {}
            }
        }
        if self.st.epoch.as_ref().is_some_and(|ep| ep.until <= snap.now) {
            self.st.epoch = None;
        }
    }

    fn epoch_active(&self, now: TimestampMs, fg: Option<&Target>) -> bool {
        self.st.epoch.as_ref().is_some_and(|ep| Some(&ep.target) == fg && now < ep.until)
    }

    /// Advance the GREEN/YELLOW/RED state machine by at most one step.
    fn update_level(&mut self, now: TimestampMs, sig: &Signals) {
        let cfg = &self.cfg;
        let enter = &self.res.enter;
        let st = &mut self.st;
        let fg = sig.fg_ratio();
        let x = cfg.exit_ratio;

        st.fg_red_since = if fg > cfg.foreground_red { Some(st.fg_red_since.unwrap_or(now)) } else { None };
        st.mem_full_since =
            if sig.mem_full > cfg.memory_full_red { Some(st.mem_full_since.unwrap_or(now)) } else { None };

        let red_enter =
            st.fg_red_since.is_some_and(|t| now - t >= ms(cfg.red_after)) || sig.mem_full > cfg.memory_full_red;
        let yellow_enter =
            Resource::ALL.into_iter().any(|r| sig.some[idx(r)] > enter.get(r)) || fg > cfg.foreground_yellow;
        let wanted = if red_enter {
            Level::Red
        } else if yellow_enter {
            Level::Yellow
        } else {
            Level::Green
        };

        if wanted > st.level {
            st.level = up(st.level);
            st.entered_at = now;
            st.exit_since = None;
            return;
        }

        let red_clear = fg <= cfg.foreground_red * x && sig.mem_full <= cfg.memory_full_red * x;
        let yellow_clear = red_clear
            && fg <= cfg.foreground_yellow * x
            && Resource::ALL.into_iter().all(|r| sig.some[idx(r)] <= enter.get(r) * x);
        let exit_ok = |l: Level| match l {
            Level::Green => false,
            Level::Red => red_clear,
            Level::Yellow => yellow_clear,
        };
        st.exit_since = if exit_ok(st.level) { Some(st.exit_since.unwrap_or(now)) } else { None };

        let dwell = match st.level {
            Level::Red => cfg.red_min_dwell,
            Level::Yellow => cfg.yellow_min_dwell,
            Level::Green => Duration::ZERO,
        };
        if let Some(since) = st.exit_since
            && now - since >= ms(cfg.recover_after)
            && now - st.entered_at >= ms(dwell)
        {
            st.level = down(st.level);
            st.entered_at = now;
            // The next step down needs its own full `recover_after`, counted from now.
            st.exit_since = exit_ok(st.level).then_some(now);
        }
    }

    /// Human-readable reason for the current level, e.g. `foreground io stall 14% > 10%`.
    fn cause(&self, sig: &Signals) -> String {
        let cfg = &self.cfg;
        let enter = &self.res.enter;
        let fg = sig.fg.map(|(r, v)| format!("foreground {r} stall {}", pct(v)));
        match self.st.level {
            Level::Red => {
                if let Some((r, v)) = sig.fg.filter(|(_, v)| *v > cfg.foreground_red) {
                    format!("foreground {r} stall {} > {}", pct(v), pct(cfg.foreground_red))
                } else if sig.mem_full > cfg.memory_full_red {
                    format!("memory full {} > {}", pct(sig.mem_full), pct(cfg.memory_full_red))
                } else {
                    format!("recovering ({})", fg.unwrap_or_else(|| "no foreground stall".into()))
                }
            }
            Level::Yellow => {
                if let Some(r) = Resource::ALL.into_iter().find(|r| sig.some[idx(*r)] > enter.get(*r)) {
                    format!("system {r} pressure {} > {}", pct(sig.some[idx(r)]), pct(enter.get(r)))
                } else if let Some((r, v)) = sig.fg.filter(|(_, v)| *v > cfg.foreground_yellow) {
                    format!("foreground {r} stall {} > {}", pct(v), pct(cfg.foreground_yellow))
                } else {
                    format!("recovering ({})", fg.unwrap_or_else(|| "no foreground stall".into()))
                }
            }
            Level::Green => "healthy".into(),
        }
    }

    /// Since when a unit that had a window has had none on screen. `None` while it is
    /// visible, while nobody can tell, and for units that never had a window (a service or
    /// a terminal tab is not an app that was put away).
    fn hidden_since(&mut self, now: TimestampMs, u: &UnitState) -> Option<TimestampMs> {
        match u.visible {
            Some(true) => {
                self.st.had_window.insert(u.target.clone());
                self.st.hidden_since.remove(&u.target);
                None
            }
            Some(false) if self.st.had_window.contains(&u.target) => {
                Some(*self.st.hidden_since.entry(u.target.clone()).or_insert(now))
            }
            _ => {
                self.st.hidden_since.remove(&u.target);
                None
            }
        }
    }

    /// Advance (or create) the AIMD state for one (target, device) and return it. `sharing`
    /// is the number of units doing I/O under a limit on the device: they share its floor.
    fn aimd_step(
        &mut self,
        now: TimestampMs,
        target: &Target,
        device: &str,
        unit: &UnitState,
        stalling: bool,
        sharing: usize,
    ) -> &Aimd {
        let cfg = &self.cfg;
        let (floor, step) = self.res.aimd_for(device);
        let floor = (floor / sharing.max(1) as u64).max(SHARED_FLOOR_MIN.min(floor));
        let key = (target.clone(), device.to_string());
        let active = io_rate(unit) >= ACTIVE_IO;
        let st = self.st.aimd.entry(key).or_insert_with(|| {
            let (r, w) = unit
                .usage
                .as_ref()
                .map_or((0, 0), |u| (rate(u.io_rbytes, u.interval_ms), rate(u.io_wbytes, u.interval_ms)));
            let init = |obs: u64| ((obs.max(floor) as f64 * cfg.aimd_decrease) as u64).max(floor);
            Aimd { rbps: init(r), wbps: init(w), last_step: now, step: 1, action: "initial" }
        });
        // A unit that isn't doing I/O keeps the limit it has: moving it every second changes
        // nothing for anyone and costs a write per unit.
        if active && now - st.last_step >= ms(cfg.aimd_interval) {
            let next = |v: u64| {
                if stalling {
                    ((v as f64 * cfg.aimd_decrease) as u64).max(floor)
                } else {
                    v.saturating_add(step).max(floor)
                }
            };
            st.rbps = next(st.rbps);
            st.wbps = next(st.wbps);
            st.last_step = now;
            st.step += 1;
            st.action = if stalling { "decrease" } else { "increase" };
        }
        st
    }

    fn metrics(&self, now: TimestampMs, sig: &Signals, epoch: bool) -> Vec<Metric> {
        let m = |name: &str, value: f64| Metric {
            ts: now,
            name: format!("{NAME}.{name}"),
            value,
            labels: BTreeMap::new(),
            plugin: String::new(),
        };
        let mut out: Vec<Metric> =
            Resource::ALL.into_iter().map(|r| m(&format!("score.{r}"), sig.some[idx(r)])).collect();
        out.push(m("score.memory_full", sig.mem_full));
        out.push(m("score.foreground", sig.fg_ratio()));
        out.push(m("level", level_value(self.st.level)));
        out.push(m("epoch", f64::from(u8::from(epoch))));
        out.push(m("aimd_limits", self.st.aimd.len() as f64));
        out.push(m("on_battery", f64::from(u8::from(self.res.on_battery))));
        if let Some(v) = &self.res.virtualization {
            let mut vm = m("virtualized", 1.0);
            vm.labels.insert("kind".into(), v.clone());
            out.push(vm);
        }
        out
    }
}

impl Plugin for ThresholdPolicy {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Policy],
            description: "GREEN/YELLOW/RED levels with hysteresis, interaction epochs and io.max AIMD".into(),
        }
    }

    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
        self.cfg = ThresholdConfig::from_json(&ctx.config).map_err(|e| PluginError::Failed(format!("{NAME}: {e}")))?;
        self.st = State::default();
        self.res_key = None;
        Ok(())
    }
}

impl Policy for ThresholdPolicy {
    fn evaluate(&mut self, snap: &Snapshot) -> PluginResult<PolicyOutput> {
        let now = snap.now;
        self.refresh(&snap.caps);
        if !self.st.started {
            self.st.started = true;
            self.st.entered_at = now;
        }

        let fg_target: Option<Target> =
            snap.foreground.clone().or_else(|| snap.foreground_unit().map(|u| u.target.clone()));
        let is_fg = |u: &UnitState| u.foreground || Some(&u.target) == fg_target.as_ref();
        // Every unit of the focused application: the window's own and those of its children.
        let fg_units: Vec<&UnitState> = snap.units.iter().filter(|u| is_fg(u)).collect();
        let fg_set: BTreeSet<&Target> = fg_units.iter().map(|u| &u.target).chain(fg_target.as_ref()).collect();

        let sig = self.signals(snap, &fg_units);
        self.process_events(snap, fg_target.as_ref());
        self.update_level(now, &sig);
        let epoch = self.epoch_active(now, fg_target.as_ref());
        let level = self.st.level;
        let cause = self.cause(&sig);
        let cfg = self.cfg.clone();
        let v = &cfg.values;
        let critical_memory_low = self.res.critical_memory_low;
        let fg_memory_low_max = self.res.fg_memory_low_max;
        let fg_epoch_uclamp_min = self.res.fg_epoch_uclamp_min;
        let yellow_background_uclamp_max = self.res.yellow_background_uclamp_max;
        let hybrid = self.res.hybrid;
        let stalling = sig.fg_ratio() > cfg.foreground_red;

        // RED-only state is dropped as soon as we leave RED.
        if level != Level::Red {
            self.st.aimd.clear();
        }
        let mem_sustained =
            level == Level::Red && self.st.mem_full_since.is_some_and(|t| now - t >= ms(cfg.mem_red_after));
        if level != Level::Red || sig.mem_full <= cfg.memory_full_red * cfg.exit_ratio {
            self.st.mem_high.clear();
        }

        let mut plan = Plan::default();
        let mut live_aimd: BTreeSet<(Target, String)> = BTreeSet::new();
        let mut live_batch: BTreeSet<Target> = BTreeSet::new();

        // Who the adaptive I/O limit applies to in RED: everything that yields by class, and
        // whatever else is doing real I/O next to the stalling foreground. Once limited, a
        // neighbour stays limited until RED ends (its rate drops because of the limit).
        let limited: BTreeSet<&Target> = if level == Level::Red && snap.caps.io_max {
            snap.units
                .iter()
                .filter(|u| !is_fg(u) && u.class != QosClass::Critical)
                .filter(|u| {
                    u.class.yields()
                        || (cfg.limit_heavy_neighbours
                            && (io_rate(u) >= ACTIVE_IO || self.st.aimd.keys().any(|(t, _)| t == &u.target)))
                })
                .map(|u| &u.target)
                .collect()
        } else {
            BTreeSet::new()
        };
        let sharing = snap.units.iter().filter(|u| limited.contains(&u.target) && io_rate(u) >= ACTIVE_IO).count();

        for u in &snap.units {
            let t = &u.target;
            if is_fg(u) {
                self.st.had_window.insert(t.clone());
                self.st.hidden_since.remove(t);
                // The foreground is boosted below and never throttled, whatever its class.
                if u.class == QosClass::Critical {
                    let w = v.critical_weight;
                    plan.set(t, Setting::CpuWeight { value: w }, "critical: protected (static)");
                    plan.set(t, Setting::IoWeight { value: w }, "critical: protected (static)");
                    plan.set(t, Setting::MemoryLow { bytes: critical_memory_low }, "critical: protected (static)");
                }
                continue;
            }
            match u.class {
                QosClass::Critical => {
                    let r = "critical: protected (static, work-conserving)";
                    plan.set(t, Setting::CpuWeight { value: v.critical_weight }, r);
                    plan.set(t, Setting::IoWeight { value: v.critical_weight }, r);
                    plan.set(t, Setting::MemoryLow { bytes: critical_memory_low }, r);
                }
                QosClass::Background => {
                    let r = "background: lower share (static, work-conserving)";
                    plan.set(t, Setting::CpuWeight { value: v.background_weight }, r);
                    plan.set(t, Setting::IoWeight { value: v.background_weight }, r);
                    if level >= Level::Yellow {
                        let r = format!("{}: {cause}; background yields", level.as_str().to_uppercase());
                        plan.set(t, Setting::CpuWeight { value: v.yellow_background_weight }, &r);
                        plan.set(t, Setting::IoWeight { value: v.yellow_background_weight }, &r);
                        plan.set(t, Setting::UclampMax { percent: yellow_background_uclamp_max }, &r);
                    }
                }
                QosClass::Batch => {
                    live_batch.insert(t.clone());
                    let r = "batch: idle CPU, low I/O share (static, work-conserving)";
                    plan.set(t, Setting::CpuIdle { value: true }, r);
                    plan.set(t, Setting::IoWeight { value: v.batch_io_weight }, r);
                    if level >= Level::Yellow {
                        let r = format!("{}: {cause}; batch yields", level.as_str().to_uppercase());
                        plan.set(t, Setting::IoWeight { value: v.yellow_batch_io_weight }, &r);
                        let r = if hybrid { format!("{r}; uclamp.max keeps it on E-cores") } else { r };
                        plan.set(t, Setting::UclampMax { percent: v.yellow_batch_uclamp_max }, r);
                    }
                    if mem_sustained && let Some(cur) = u.usage.as_ref().map(|u| u.mem_current).filter(|c| *c > 0) {
                        self.st.mem_high.entry(t.clone()).or_insert((cur as f64 * cfg.memory_high_ratio) as u64);
                    }
                    if let Some(&b) = self.st.mem_high.get(t) {
                        let r = format!(
                            "RED: memory full {} > {} for {}; memory.high at {}% of usage",
                            pct(sig.mem_full),
                            pct(cfg.memory_full_red),
                            humantime_short(cfg.mem_red_after),
                            (cfg.memory_high_ratio * 100.0).round()
                        );
                        plan.set(t, Setting::MemoryHigh { bytes: Some(b) }, r);
                    }
                }
                QosClass::Normal | QosClass::Interactive => {
                    if let Some(since) = self.hidden_since(now, u)
                        && cfg.nap
                        // Silent for sure: where nothing reports sound, nothing naps.
                        && u.audible == Some(false)
                        && now - since >= ms(cfg.nap_after)
                    {
                        let r = format!(
                            "nap: no window on screen for over {}, silent (timers coalesced, efficient cores)",
                            humantime_short(cfg.nap_after)
                        );
                        plan.set(t, Setting::UclampMax { percent: cfg.nap_uclamp_max }, &r);
                        let ns = u64::try_from(cfg.nap_timer_slack.as_nanos()).unwrap_or(u64::MAX);
                        plan.set(t, Setting::TimerSlack { ns }, &r);
                    }
                }
            }
            if limited.contains(t) {
                let who = if u.class.yields() { "" } else { " heavy I/O next to the app in use;" };
                for dev in &snap.caps.devices {
                    live_aimd.insert((t.clone(), dev.clone()));
                    let a = self.aimd_step(now, t, dev, u, stalling, sharing).clone();
                    let r = format!(
                        "RED: {cause};{who} io.max AIMD step {} ({}, r={} w={})",
                        a.step,
                        a.action,
                        mib(a.rbps),
                        mib(a.wbps)
                    );
                    let limit = IoLimit { rbps: Some(a.rbps), wbps: Some(a.wbps), riops: None, wiops: None };
                    plan.set(t, Setting::IoMax { device: dev.clone(), limit }, r);
                }
            }
        }

        // Garbage-collect state for targets that vanished, changed class or gained focus.
        self.st.aimd.retain(|k, _| live_aimd.contains(k));
        if self.st.had_window.len() + self.st.hidden_since.len() > 0 {
            let known: BTreeSet<&Target> = snap.units.iter().map(|u| &u.target).collect();
            self.st.had_window.retain(|t| known.contains(t));
            self.st.hidden_since.retain(|t, _| known.contains(t));
        }
        self.st.mem_high.retain(|t, _| live_batch.contains(t));

        for &t in &fg_set {
            let mem_low = snap
                .units
                .iter()
                .find(|u| &u.target == t)
                .and_then(|u| u.usage.as_ref())
                .filter(|u| u.mem_current > 0)
                .map(|u| round_up(u.mem_current, cfg.memory_low_step).min(fg_memory_low_max));
            let (w, r) = if epoch {
                (v.fg_epoch_weight, "foreground: interaction epoch boost")
            } else {
                (v.fg_weight, "foreground: focused, no recent input (mild boost)")
            };
            let keep_max = |plan: &Plan, knob: Knob, w: u64| match plan.get(t, knob) {
                Some(Setting::CpuWeight { value } | Setting::IoWeight { value }) => w.max(*value),
                _ => w,
            };
            let cw = keep_max(&plan, Knob::CpuWeight, w);
            let iw = keep_max(&plan, Knob::IoWeight, w);
            plan.set(t, Setting::CpuWeight { value: cw }, r);
            plan.set(t, Setting::IoWeight { value: iw }, r);
            if epoch {
                let r = if hybrid { format!("{r} (uclamp.min favours P-cores)") } else { r.to_string() };
                plan.set(t, Setting::UclampMin { percent: fg_epoch_uclamp_min }, r);
            }
            if let Some(b) = mem_low {
                let b = match plan.get(t, Knob::MemoryLow) {
                    Some(Setting::MemoryLow { bytes }) => b.max(*bytes),
                    _ => b,
                };
                plan.set(t, Setting::MemoryLow { bytes: b }, r);
            }
        }

        // Warm apps: used recently, not in front right now. Keep their memory resident and
        // give them a little more than the default share, so coming back to them is instant.
        for &t in &fg_set {
            self.st.last_focus.insert(t.clone(), now);
        }
        let warm_ms = ms(cfg.warm_for);
        let live: BTreeSet<&Target> = snap.units.iter().map(|u| &u.target).collect();
        self.st.last_focus.retain(|t, at| live.contains(t) && now - *at <= warm_ms);
        if warm_ms > 0 {
            let mut budget = (snap.caps.hardware.mem_total as f64 * cfg.warm_memory_budget) as u64;
            let unlimited = snap.caps.hardware.mem_total == 0;
            // Most recently used first: they get the memory budget before older ones.
            let mut warm: Vec<(&Target, TimestampMs)> =
                self.st.last_focus.iter().filter(|(t, _)| !fg_set.contains(*t)).map(|(t, at)| (t, *at)).collect();
            warm.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
            for (t, at) in warm {
                let Some(u) = snap.units.iter().find(|u| &u.target == t) else { continue };
                // Only ordinary apps: critical ones are protected already, and background or
                // batch work that once had focus (a build's terminal tab) still yields.
                if !matches!(u.class, QosClass::Normal | QosClass::Interactive) {
                    continue;
                }
                let r = format!("warm: in front {}s ago; kept ready to switch back to", (now - at) / 1000);
                plan.set(
                    t,
                    Setting::CpuWeight { value: weight_at_least(&plan, t, Knob::CpuWeight, cfg.warm_weight) },
                    r.clone(),
                );
                plan.set(
                    t,
                    Setting::IoWeight { value: weight_at_least(&plan, t, Knob::IoWeight, cfg.warm_weight) },
                    r.clone(),
                );
                if let Some(used) = u.usage.as_ref().map(|u| u.mem_current).filter(|b| *b > 0) {
                    let want = round_up(used, cfg.memory_low_step).min(cfg.warm_memory_low_max);
                    if unlimited || want <= budget {
                        budget = budget.saturating_sub(want);
                        plan.set(t, Setting::MemoryLow { bytes: want }, r);
                    }
                }
            }
        }

        let metrics = self.metrics(now, &sig, epoch);
        Ok(PolicyOutput { intents: plan.into_intents(cfg.priority), level: Some(level), metrics })
    }
}

/// `3s`, `500ms`: compact duration for reason strings.
fn humantime_short(d: Duration) -> String {
    if d.subsec_millis() == 0 { format!("{}s", d.as_secs()) } else { format!("{}ms", d.as_millis()) }
}
