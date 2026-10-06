//! `lindeskctl simulate`: replay recorded telemetry through a policy on simulated time.
//!
//! The recorded `pressure`, `units` and `events` rows of a window are read from the lake
//! with `lindesk-data export` (the optional lindesk-telemetry package).
//! A [`MemoryEnvironment`] is rebuilt from the recorded units, a replay sensor feeds the
//! samples and usage back in timestamp order, focus/interaction events are pushed at their
//! recorded times, recorded classes are pinned, and a recording actuator accepts every
//! action. The real [`Engine`] is stepped once per recorded timestamp, once with the
//! candidate (alongside the baseline, as in production) and once with the baseline alone.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use lindesk_builtins::policy::ThresholdPolicy;
use lindesk_core::config::{Config, merge_json};
use lindesk_host::{ActionOutcome, Engine, EngineOptions, MemoryEnvironment};
use lindesk_plugin_api::{
    Actuator, ActuatorCaps, Event, HostCaps, IoLimit, Knob, Level, Manager, Plugin, PluginInfo, PluginKind,
    PluginResult, Privilege, QosClass, Resource, Sample, Scope, Sensor, SensorInput, SensorOutput, Setting, Target,
    TargetUsage, TimestampMs, Usage,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::cli::SimulateArgs;
use crate::cmd::data::{Lake, open_lake, parse_time};
use crate::cmd::doctor::tilde;
use crate::paths;
use crate::ui::{self, Ui, badge, duration_ms, out, setting_short};

// ------------------------------------------------------------------------- recorded data

/// Everything read from the lake for one window.
#[derive(Default, Debug)]
pub struct Recording {
    pub from: TimestampMs,
    pub to: TimestampMs,
    pub samples: Vec<Sample>,
    /// (ts, target, class, foreground, usage)
    pub units: Vec<(TimestampMs, Target, Option<QosClass>, bool, Usage)>,
    /// Focus / interaction events to push.
    pub inputs: Vec<Event>,
    /// Recorded level changes, for comparison.
    pub levels: Vec<(TimestampMs, Level, Level)>,
    /// cgroup path of each target, from per-cgroup pressure rows.
    pub cgroups: BTreeMap<Target, String>,
}

fn i(v: &Value, k: &str) -> i64 {
    v.get(k)
        .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)).or_else(|| x.as_str()?.parse().ok()))
        .unwrap_or(0)
}

fn f(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64().or_else(|| x.as_str()?.parse().ok()))
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// Rebuild a target from its flattened `units`/`events` columns.
pub fn unflatten(manager: Option<&str>, uid: Option<i64>, unit: &str) -> Target {
    match manager {
        Some("system") => Target::system_unit(unit),
        Some("user") => Target::user_unit(uid.unwrap_or(0) as u32, unit),
        _ => {
            if let Some(p) = unit.strip_prefix("cgroup:") {
                Target::Cgroup { path: p.to_string() }
            } else if let Some(pid) = unit.strip_prefix("pid:").and_then(|p| p.parse().ok()) {
                Target::Process { pid }
            } else {
                Target::system_unit(unit)
            }
        }
    }
}

fn resource(s: &str) -> Option<Resource> {
    Resource::ALL.into_iter().find(|r| r.as_str() == s)
}

/// Read a window from the lake.
pub fn load(lake: &Lake, from: TimestampMs, to: TimestampMs) -> Result<Recording> {
    let export = |table: &str| lake.data.export(table, from, to).map_err(|e| anyhow!("reading the lake: {e}"));
    Ok(from_rows(&export("pressure")?, &export("units")?, &export("events")?, from, to))
}

/// Build a recording from exported rows (`ts` in epoch ms, ordered by `ts`).
pub fn from_rows(
    pressure: &[Value],
    units: &[Value],
    events: &[Value],
    from: TimestampMs,
    to: TimestampMs,
) -> Recording {
    let mut rec = Recording { from, to, ..Default::default() };
    for r in pressure {
        let Some(res) = s(r, "resource").and_then(resource) else { continue };
        let scope = match s(r, "scope") {
            Some("system") | None => Scope::System,
            Some(p) => Scope::Cgroup(p.to_string()),
        };
        if let Scope::Cgroup(p) = &scope {
            rec.cgroups.entry(lindesk_host::env::target_for_cgroup(p)).or_insert_with(|| p.clone());
        }
        rec.samples.push(Sample {
            ts: i(r, "ts"),
            scope,
            resource: res,
            some_avg10: f(r, "some_avg10").unwrap_or(0.0) as f32,
            full_avg10: f(r, "full_avg10").map(|x| x as f32),
            some_delta_us: i(r, "some_delta_us").max(0) as u64,
            full_delta_us: r.get("full_delta_us").filter(|x| !x.is_null()).map(|_| i(r, "full_delta_us").max(0) as u64),
            interval_ms: i(r, "interval_ms").clamp(0, u32::MAX as i64) as u32,
        });
    }
    for r in units {
        let target = unflatten(s(r, "manager"), r.get("uid").and_then(Value::as_i64), s(r, "unit").unwrap_or(""));
        let class = s(r, "class").and_then(|c| c.parse().ok());
        let usage = Usage {
            cpu_usage_us: i(r, "cpu_usage_us").max(0) as u64,
            io_rbytes: i(r, "io_rbytes").max(0) as u64,
            io_wbytes: i(r, "io_wbytes").max(0) as u64,
            io_rios: i(r, "io_rios").max(0) as u64,
            io_wios: i(r, "io_wios").max(0) as u64,
            mem_current: i(r, "mem_current").max(0) as u64,
            interval_ms: i(r, "interval_ms").clamp(0, u32::MAX as i64) as u32,
        };
        let fg = r.get("foreground").and_then(Value::as_bool).unwrap_or(false);
        rec.units.push((i(r, "ts"), target, class, fg, usage));
    }
    for r in events {
        if !matches!(s(r, "kind"), Some("focus" | "interaction" | "level")) {
            continue;
        }
        let Some(payload) = s(r, "payload").and_then(|p| serde_json::from_str::<Event>(p).ok()) else { continue };
        match payload {
            Event::LevelChanged { ts, from, to, .. } => rec.levels.push((ts, from, to)),
            e @ (Event::Focus { .. } | Event::Interaction { .. }) => rec.inputs.push(e),
            _ => {}
        }
    }
    rec
}

impl Recording {
    pub fn targets(&self) -> BTreeSet<Target> {
        self.units.iter().map(|u| u.1.clone()).collect()
    }

    /// Every distinct timestamp, in order: the simulated clock's ticks.
    pub fn ticks(&self) -> Vec<TimestampMs> {
        let mut t: BTreeSet<TimestampMs> = self.samples.iter().map(|s| s.ts).collect();
        t.extend(self.units.iter().map(|u| u.0));
        t.extend(self.inputs.iter().map(Event::ts));
        t.into_iter().collect()
    }
}

// ------------------------------------------------------------------- simulated plugins

/// Emits recorded samples and usage up to the engine's `now`.
struct ReplaySensor {
    samples: Vec<Sample>,
    usage: Vec<(TimestampMs, Target, Usage)>,
    si: usize,
    ui: usize,
}

impl Plugin for ReplaySensor {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: "replay".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Sensor],
            description: "recorded telemetry".into(),
        }
    }
}

impl Sensor for ReplaySensor {
    fn poll(&mut self, input: &SensorInput) -> PluginResult<SensorOutput> {
        let mut out = SensorOutput::default();
        while self.si < self.samples.len() && self.samples[self.si].ts <= input.now {
            out.samples.push(self.samples[self.si].clone());
            self.si += 1;
        }
        while self.ui < self.usage.len() && self.usage[self.ui].0 <= input.now {
            let (_, t, u) = &self.usage[self.ui];
            out.usage.push(TargetUsage { target: t.clone(), usage: u.clone() });
            self.ui += 1;
        }
        Ok(out)
    }
}

/// Accepts every action; nothing touches the system.
struct Recorder;

impl Plugin for Recorder {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: "simulate".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Actuator],
            description: "records actions".into(),
        }
    }
}

impl Actuator for Recorder {
    fn caps(&self) -> ActuatorCaps {
        ActuatorCaps {
            knobs: vec![
                Knob::CpuWeight,
                Knob::CpuIdle,
                Knob::UclampMin,
                Knob::UclampMax,
                Knob::IoWeight,
                Knob::IoMax(String::new()),
                Knob::MemoryLow,
                Knob::MemoryHigh,
                Knob::Nice,
                Knob::IoPrio,
                Knob::TimerSlack,
            ],
            targets: vec!["unit".into(), "cgroup".into(), "process".into()],
        }
    }
    fn read(&mut self, _: &Target, _: &Knob) -> PluginResult<Option<Setting>> {
        Ok(None)
    }
    fn apply(&mut self, _: &Target, _: &Setting) -> PluginResult<()> {
        Ok(())
    }
    fn reset(&mut self, _: &Target, _: &Knob, _: Option<&Setting>) -> PluginResult<()> {
        Ok(())
    }
}

// -------------------------------------------------------------------------------- runs

/// A policy to run.
#[derive(Clone, Debug, PartialEq)]
pub enum PolicySpec {
    Threshold,
    Dir(std::path::PathBuf),
    None,
}

impl PolicySpec {
    pub fn parse(s: &str) -> Result<PolicySpec> {
        match s {
            "builtin:threshold" | "threshold" => Ok(PolicySpec::Threshold),
            "none" => Ok(PolicySpec::None),
            other if other.starts_with("builtin:") => {
                bail!("unknown built-in policy '{other}' (available: builtin:threshold)")
            }
            dir => {
                let p = std::path::PathBuf::from(dir);
                // Plugins run in their own directory: use an absolute path.
                let p = std::fs::canonicalize(&p).unwrap_or(p);
                if !p.join("plugin.toml").is_file() {
                    bail!(
                        "{dir} isn't a plugin directory (no plugin.toml). Pass a policy plugin directory or builtin:threshold."
                    );
                }
                Ok(PolicySpec::Dir(p))
            }
        }
    }

    pub fn label(&self) -> String {
        match self {
            PolicySpec::Threshold => "threshold".into(),
            PolicySpec::Dir(p) => p.display().to_string(),
            PolicySpec::None => "none".into(),
        }
    }
}

/// One recorded action of a run.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Act {
    pub ts: TimestampMs,
    pub target: String,
    pub knob: String,
    /// The value set, or `None` for a revert.
    pub value: Option<String>,
    #[serde(skip)]
    pub setting: Option<Setting>,
    pub plugin: String,
    pub reason: String,
}

/// What one run did.
#[derive(Serialize, Clone, Debug, Default)]
pub struct RunResult {
    pub policies: Vec<String>,
    pub levels: Vec<(TimestampMs, Level, Level, String)>,
    pub level_time_ms: BTreeMap<String, i64>,
    pub actions: Vec<Act>,
    /// Target → ms spent with at least one throttle applied.
    pub throttled_ms: BTreeMap<String, i64>,
    pub notes: Vec<String>,
    /// Problems with the candidate plugin itself (crashes, timeouts, never called).
    pub warnings: Vec<String>,
}

fn is_throttle(s: &Setting) -> bool {
    match s {
        Setting::CpuWeight { value } | Setting::IoWeight { value } => *value < 100,
        Setting::CpuIdle { value } => *value,
        Setting::UclampMax { percent } => *percent < 100.0,
        Setting::IoMax { limit, .. } => !IoLimit::is_unlimited(limit),
        Setting::MemoryHigh { bytes } => bytes.is_some(),
        Setting::Nice { value } => *value > 0,
        Setting::IoPrio { class, .. } => *class == lindesk_plugin_api::IoPrioClass::Idle,
        Setting::TimerSlack { ns } => *ns > lindesk_core::safety::TIMER_SLACK_DEFAULT_NS,
        Setting::UclampMin { .. } | Setting::MemoryLow { .. } => false,
    }
}

fn host_caps() -> HostCaps {
    let mut c = lindesk_sys::probe(&lindesk_sys::Roots::host(), &lindesk_sys::Env::from_process()).host_caps();
    // Replays assume a capable host so policies show everything they would do.
    c.cgroup_v2 = true;
    c.systemd = true;
    c.psi = true;
    c.io_max = true;
    if c.devices.is_empty() {
        c.devices.push("259:0".into());
    }
    c
}

/// Options shared by both runs.
pub struct SimOptions {
    pub config: Config,
    pub privilege: Privilege,
    pub uid: Option<u32>,
    pub caps: HostCaps,
    pub timeout_scale: f64,
}

/// Run the engine over a recording with these policies.
pub fn run_once(rec: &Recording, policies: &[PolicySpec], o: &SimOptions) -> Result<RunResult> {
    let mut config = o.config.clone();
    config.general.dry_run = false;
    let env = MemoryEnvironment::new();
    for t in rec.targets() {
        env.watch(t.clone(), rec.cgroups.get(&t).map(String::as_str));
    }
    let mut opts = EngineOptions::new("simulate", o.privilege);
    opts.uid = o.uid;
    opts.caps = o.caps.clone();
    let mut result = RunResult::default();
    // Built-ins we register must be enabled even if the user's config disables them.
    if policies.contains(&PolicySpec::Threshold) {
        config.plugins.entry("threshold".into()).or_default().enabled = true;
    }
    let mut engine = Engine::new(config.clone(), Box::new(env), opts).map_err(|e| anyhow!("{e}"))?;
    let reg = engine.registry_mut();
    reg.set_timeout_scale(o.timeout_scale);
    reg.register_sensor(
        Box::new(ReplaySensor {
            samples: rec.samples.clone(),
            usage: rec.units.iter().map(|u| (u.0, u.1.clone(), u.4.clone())).collect(),
            si: 0,
            ui: 0,
        }),
        Duration::ZERO,
    )?;
    reg.register_actuator(Box::new(Recorder), 0)?;
    for p in policies {
        match p {
            PolicySpec::Threshold => {
                reg.register_policy(Box::new(ThresholdPolicy::default()), 0)?;
                result.policies.push("threshold".into());
            }
            PolicySpec::Dir(dir) => {
                let mut d = lindesk_host::discovery::inspect_dev(dir)
                    .map_err(|r| anyhow!("{}: {}", dir.display(), r.reason))?;
                if !d.manifest.plugin.kinds.contains(&PluginKind::Policy) {
                    bail!(
                        "{} is a {} plugin, not a policy",
                        d.name,
                        d.manifest.plugin.kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>().join("/")
                    );
                }
                if let Some(pc) = config.plugins.get(&d.name) {
                    d.priority_override = pc.priority;
                    merge_json(&mut d.config, &pc.config);
                }
                d.enabled = true;
                if o.privilege == Privilege::User && d.manifest.plugin.privilege == Privilege::System {
                    bail!(
                        "{} is a privilege = \"system\" plugin; replay it against the system lake with --system",
                        d.name
                    );
                }
                reg.load_external(&d).map_err(|e| anyhow!("{e}"))?;
                result.policies.push(d.name.clone());
            }
            PolicySpec::None => {}
        }
    }
    engine.start();
    let ticks = rec.ticks();
    let mut ui_i = 0;
    let mut ev_i = 0;
    let mut classes: BTreeMap<Target, QosClass> = BTreeMap::new();
    let mut level = Level::Green;
    let mut level_since = ticks.first().copied().unwrap_or(rec.from);
    let mut held: BTreeMap<(String, String), bool> = BTreeMap::new();
    let mut throttled_since: BTreeMap<String, TimestampMs> = BTreeMap::new();
    let mut seen_notes = BTreeSet::new();
    for &now in &ticks {
        while ui_i < rec.units.len() && rec.units[ui_i].0 <= now {
            let (_, t, class, _, _) = &rec.units[ui_i];
            if let Some(c) = class
                && classes.get(t) != Some(c)
            {
                classes.insert(t.clone(), *c);
                engine.set_class(t.clone(), *c, "recorded");
            }
            ui_i += 1;
        }
        while ev_i < rec.inputs.len() && rec.inputs[ev_i].ts() <= now {
            engine.push_event(rec.inputs[ev_i].clone());
            ev_i += 1;
        }
        let report = engine.step(now);
        for e in &report.events {
            if let Event::LevelChanged { ts, from, to, policy } = e {
                *result.level_time_ms.entry(level.as_str().into()).or_default() += ts - level_since;
                level = *to;
                level_since = *ts;
                result.levels.push((*ts, *from, *to, policy.clone()));
            }
        }
        for n in &report.notes {
            if seen_notes.insert(n.clone()) && seen_notes.len() <= 50 {
                result.notes.push(n.clone());
            }
        }
        for a in &report.actions {
            if matches!(a.outcome, ActionOutcome::Failed { .. } | ActionOutcome::NoActuator) {
                continue;
            }
            let target = match &a.target {
                Target::Unit { manager: Manager::User(uid), name } => format!("user/{uid}:{name}"),
                t => t.to_string(),
            };
            let (plugin, reason) =
                a.provenance.as_ref().map(|p| (p.plugin.clone(), p.reason.clone())).unwrap_or_default();
            let key = (target.clone(), a.knob.to_string());
            let throttle = a.setting.as_ref().is_some_and(is_throttle);
            held.insert(key, throttle);
            let any = held.iter().any(|((t, _), on)| *t == target && *on);
            match (any, throttled_since.get(&target).copied()) {
                (true, None) => {
                    throttled_since.insert(target.clone(), now);
                }
                (false, Some(since)) => {
                    *result.throttled_ms.entry(target.clone()).or_default() += now - since;
                    throttled_since.remove(&target);
                }
                _ => {}
            }
            result.actions.push(Act {
                ts: now,
                target,
                knob: a.knob.to_string(),
                value: a.setting.as_ref().map(setting_short),
                setting: a.setting.clone(),
                plugin,
                reason,
            });
        }
    }
    let end = rec.to.max(ticks.last().copied().unwrap_or(rec.to));
    *result.level_time_ms.entry(level.as_str().into()).or_default() += end - level_since;
    for (t, since) in throttled_since {
        *result.throttled_ms.entry(t).or_default() += end - since;
    }
    // A candidate that never answered makes the comparison meaningless: say so loudly.
    for name in result.policies.clone() {
        if let Some(h) = engine.registry().plugin_health(&name)
            && matches!(h.origin, lindesk_host::Origin::External { .. })
            && (h.state != "running" || h.stats.errors > 0 || h.stats.calls == 0)
        {
            let why = h.detail.clone().or(h.stats.last_error.clone()).unwrap_or_default();
            result.warnings.push(format!(
                "{name} is {} after {} calls ({} errors){}",
                h.state,
                h.stats.calls,
                h.stats.errors,
                if why.is_empty() { String::new() } else { format!(": {why}") }
            ));
        }
    }
    // Restores everything in the recorder; not part of the replay.
    engine.shutdown();
    Ok(result)
}

/// One row of the candidate-vs-baseline diff.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct DiffRow {
    pub ts: TimestampMs,
    pub target: String,
    pub knob: String,
    pub candidate: Option<String>,
    pub baseline: Option<String>,
    pub plugin: String,
    pub reason: String,
}

/// Actions that differ between two runs, keyed by (time, target, knob).
pub fn diff(cand: &RunResult, base: &RunResult) -> Vec<DiffRow> {
    type Key = (TimestampMs, String, String);
    fn index(r: &RunResult) -> BTreeMap<Key, &Act> {
        r.actions.iter().map(|a| ((a.ts, a.target.clone(), a.knob.clone()), a)).collect()
    }
    let c = index(cand);
    let b = index(base);
    let keys: BTreeSet<&Key> = c.keys().chain(b.keys()).collect();
    let show = |a: Option<&&Act>| a.map(|a| a.value.clone().unwrap_or_else(|| format!("revert {}", a.knob)));
    keys.into_iter()
        .filter_map(|k| {
            let (ca, ba) = (c.get(k), b.get(k));
            let (cv, bv) = (show(ca), show(ba));
            if cv == bv {
                return None;
            }
            let src = ca.or(ba)?;
            Some(DiffRow {
                ts: k.0,
                target: k.1.clone(),
                knob: k.2.clone(),
                candidate: cv,
                baseline: bv,
                plugin: src.plugin.clone(),
                reason: src.reason.clone(),
            })
        })
        .collect()
}

/// The complete simulation result.
#[derive(Serialize, Debug)]
pub struct Simulation {
    pub from: TimestampMs,
    pub to: TimestampMs,
    pub ticks: usize,
    pub units: usize,
    pub samples: usize,
    pub input_events: usize,
    pub candidate: RunResult,
    pub baseline: Option<RunResult>,
    pub recorded_levels: Vec<(TimestampMs, Level, Level)>,
    pub diff: Vec<DiffRow>,
}

pub fn simulate(rec: &Recording, candidate: &PolicySpec, baseline: &PolicySpec, o: &SimOptions) -> Result<Simulation> {
    let mut cand_policies = vec![];
    if *baseline != PolicySpec::None && baseline != candidate {
        cand_policies.push(baseline.clone());
    }
    cand_policies.push(candidate.clone());
    let cand = run_once(rec, &cand_policies, o)?;
    let base = match baseline {
        PolicySpec::None => None,
        b => Some(run_once(rec, std::slice::from_ref(b), o)?),
    };
    let empty = RunResult::default();
    let diff = diff(&cand, base.as_ref().unwrap_or(&empty));
    Ok(Simulation {
        from: rec.from,
        to: rec.to,
        ticks: rec.ticks().len(),
        units: rec.targets().len(),
        samples: rec.samples.len(),
        input_events: rec.inputs.len(),
        candidate: cand,
        baseline: base,
        recorded_levels: rec.levels.clone(),
        diff,
    })
}

// ------------------------------------------------------------------------------ output

fn level_times(ui: &Ui, m: &BTreeMap<String, i64>) -> String {
    ["green", "yellow", "red"]
        .iter()
        .map(|l| {
            let ms = m.get(*l).copied().unwrap_or(0);
            let lv: Level = match *l {
                "green" => Level::Green,
                "yellow" => Level::Yellow,
                _ => Level::Red,
            };
            let txt = format!("{} {:<7}", l, if ms == 0 { "-".to_string() } else { duration_ms(ms) });
            if ms == 0 { ui.dim(txt) } else { ui.paint(ui::level_code(lv), txt) }
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn recorded_level_time(sim: &Simulation) -> BTreeMap<String, i64> {
    let mut m = BTreeMap::new();
    let mut level = Level::Green;
    let mut since = sim.from;
    for (ts, _, to) in &sim.recorded_levels {
        *m.entry(level.as_str().to_string()).or_default() += ts - since;
        level = *to;
        since = *ts;
    }
    *m.entry(level.as_str().to_string()).or_default() += sim.to - since;
    m
}

fn short_target(t: &str) -> &str {
    t.split_once(':').map_or(t, |(_, n)| n)
}

pub fn render(ui: &Ui, sim: &Simulation, cand_label: &str, base_label: &str, limit: usize) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "{} {} → {}  {}\n",
        ui.bold("replaying"),
        ui::datetime(sim.from),
        ui::datetime(sim.to),
        ui.dim(format!(
            "({} ticks, {} units, {} samples, {} focus/interaction events)",
            sim.ticks, sim.units, sim.samples, sim.input_events
        ))
    ));
    s.push_str(&format!(
        "{}  {} {}   {}  {}\n\n",
        ui.dim("candidate"),
        ui.bold(cand_label),
        ui.dim(if sim.candidate.policies.len() > 1 {
            format!("(with {})", sim.candidate.policies[..sim.candidate.policies.len() - 1].join(", "))
        } else {
            String::new()
        }),
        ui.dim("baseline"),
        ui.bold(base_label)
    ));
    for w in &sim.candidate.warnings {
        s.push_str(&format!("{} {}\n", ui.paint("1;31", "warning:"), w));
    }
    if !sim.candidate.warnings.is_empty() {
        s.push_str(&ui.dim("  check it with: lindeskctl plugin test <dir>\n\n"));
    }
    s.push_str(&format!("{}\n", ui.bold(ui.dim("LEVEL"))));
    s.push_str(&format!("  {:<10}{}\n", "candidate", level_times(ui, &sim.candidate.level_time_ms)));
    if let Some(b) = &sim.baseline {
        s.push_str(&format!("  {:<10}{}\n", "baseline", level_times(ui, &b.level_time_ms)));
    }
    s.push_str(&format!("  {:<10}{}\n", "recorded", level_times(ui, &recorded_level_time(sim))));
    if !sim.candidate.levels.is_empty() {
        s.push_str(&format!("\n{}\n", ui.bold(ui.dim("LEVEL CHANGES  candidate"))));
        for (ts, from, to, by) in sim.candidate.levels.iter().take(limit) {
            s.push_str(&format!(
                "  {}  {} → {}  {}\n",
                ui.dim(ui::clock(*ts)),
                badge(ui, *from),
                badge(ui, *to),
                ui.dim(by)
            ));
        }
        if sim.candidate.levels.len() > limit {
            s.push_str(&ui.dim(format!("  … {} more\n", sim.candidate.levels.len() - limit)));
        }
    }
    s.push_str(&format!(
        "\n{}\n",
        ui.bold(ui.dim(if sim.baseline.is_some() {
            "DIFF  candidate vs baseline (nothing is applied)"
        } else {
            "ACTIONS  candidate (nothing is applied)"
        }))
    ));
    if sim.diff.is_empty() {
        s.push_str(&format!(
            "  {}\n",
            ui.green("no differences: the candidate would have done exactly what the baseline did")
        ));
    } else {
        let w =
            sim.diff.iter().take(limit).map(|d| short_target(&d.target).chars().count()).max().unwrap_or(10).min(44);
        for d in sim.diff.iter().take(limit) {
            let c = d.candidate.clone().unwrap_or_else(|| format!("(no {} change)", d.knob));
            let b = d.baseline.clone().unwrap_or_else(|| "nothing".into());
            let value = if sim.baseline.is_some() {
                format!("{}  {} {}", ui.yellow(&c), ui.dim("was"), b)
            } else {
                ui.yellow(&c)
            };
            let why = if d.candidate.is_some() && !d.plugin.is_empty() {
                ui.dim(format!("  [{}: {}]", d.plugin, d.reason))
            } else {
                String::new()
            };
            s.push_str(&format!(
                "  {}  {}  {value}{why}\n",
                ui.dim(ui::clock(d.ts)),
                ui::pad(short_target(&d.target), w)
            ));
        }
        if sim.diff.len() > limit {
            s.push_str(&ui.dim(format!("  … {} more (--limit, or --json for all)\n", sim.diff.len() - limit)));
        }
    }
    s.push_str(&format!("\n{}\n", ui.bold(ui.dim("SUMMARY"))));
    let cand_n = sim.candidate.actions.len();
    match &sim.baseline {
        Some(b) => {
            let only_c = sim.diff.iter().filter(|d| d.baseline.is_none()).count();
            let only_b = sim.diff.iter().filter(|d| d.candidate.is_none()).count();
            let changed = sim.diff.len() - only_c - only_b;
            s.push_str(&format!(
                "  actions    candidate {cand_n}, baseline {}; {only_c} only in candidate, {only_b} only in baseline, {changed} with different values\n",
                b.actions.len()
            ));
        }
        None => s.push_str(&format!("  actions    {cand_n}\n")),
    }
    let mut thr: Vec<(&String, &i64)> = sim.candidate.throttled_ms.iter().filter(|(_, ms)| **ms > 0).collect();
    thr.sort_by_key(|(_, ms)| -**ms);
    if thr.is_empty() {
        s.push_str("  throttled  nothing\n");
    } else {
        for (i, (t, ms)) in thr.iter().take(8).enumerate() {
            let base = sim.baseline.as_ref().and_then(|b| b.throttled_ms.get(*t)).copied().unwrap_or(0);
            let vs = if sim.baseline.is_some() {
                ui.dim(format!(" (baseline {})", duration_ms(base)))
            } else {
                String::new()
            };
            s.push_str(&format!(
                "  {:<10} {} for {}{vs}\n",
                if i == 0 { "would have throttled" } else { "" },
                ui.bold(short_target(t)),
                ui.yellow(duration_ms(**ms))
            ));
        }
    }
    if !sim.candidate.notes.is_empty() {
        s.push_str(&format!("\n{}\n", ui.bold(ui.dim("NOTES"))));
        for n in sim.candidate.notes.iter().take(10) {
            s.push_str(&format!("  {}\n", ui.dim(n)));
        }
    }
    s
}

pub fn run(ui: &Ui, a: &SimulateArgs) -> Result<()> {
    let candidate = PolicySpec::parse(&a.policy)?;
    if candidate == PolicySpec::None {
        bail!("--policy none would replay nothing; pass a plugin directory or builtin:threshold");
    }
    let baseline = match a.baseline.as_str() {
        "none" => PolicySpec::None,
        "builtin:threshold" | "threshold" => PolicySpec::Threshold,
        other => bail!("--baseline must be builtin:threshold or none (got '{other}')"),
    };
    let now = lindesk_core::clock::now_ms();
    let from = parse_time(&a.from, now)?;
    let to = parse_time(&a.to, now)?;
    if from >= to {
        bail!("--from ({}) must be before --to ({})", ui::datetime(from), ui::datetime(to));
    }
    let config_path = a.config.clone().unwrap_or_else(|| paths::config_path(ui.system));
    let config = paths::load_config(&config_path)?;
    let lake = open_lake(ui, a.lake.as_deref().or(config.telemetry.dir.as_deref()))?;
    if !lake.root().is_dir() {
        bail!(
            "no telemetry at {} to replay. lindesk records it while running (telemetry.enabled = true).",
            tilde(lake.root())
        );
    }
    ui.info(ui.dim(format!("reading {} …", tilde(lake.root()))));
    let rec = load(&lake, from, to)?;
    if rec.ticks().is_empty() {
        bail!(
            "no telemetry between {} and {} in {}. Check what's there: lindeskctl data stats",
            ui::datetime(from),
            ui::datetime(to),
            tilde(lake.root())
        );
    }
    let opts = SimOptions {
        config,
        privilege: if ui.system { Privilege::System } else { Privilege::User },
        uid: if ui.system { None } else { Some(paths::uid()) },
        caps: host_caps(),
        timeout_scale: a.timeout_scale,
    };
    let sim = simulate(&rec, &candidate, &baseline, &opts)?;
    if ui.json {
        return ui.print_json(&json!({
            "candidate_policy": candidate.label(),
            "baseline_policy": baseline.label(),
            "simulation": sim,
        }));
    }
    let label = if matches!(candidate, PolicySpec::Dir(_)) { a.policy.clone() } else { candidate.label() };
    out(&render(ui, &sim, &label, &baseline.label(), a.limit));
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use lindesk_plugin_api::Provenance;

    const T0: i64 = 1_791_000_000_000;
    pub const BATCH: &str = "lindesk-run-make-1.scope";
    pub const FF: &str = "app-gnome-org.mozilla.firefox-7.scope";

    /// 2 minutes of telemetry as `lindesk-data export` prints it: calm for 30s, heavy I/O
    /// pressure for 60s, calm again. Returns (pressure, units, events) rows.
    pub fn rows() -> (Vec<Value>, Vec<Value>, Vec<Value>) {
        let (mut pressure, mut units, mut events) = (Vec::new(), Vec::new(), Vec::new());
        let event = |e: &Event| {
            let v = serde_json::to_value(e).unwrap();
            json!({"ts": v["ts"], "kind": v["kind"], "payload": v.to_string()})
        };
        events.push(event(&Event::Focus {
            ts: T0,
            pid: Some(4242),
            app_id: Some("org.mozilla.firefox".into()),
            target: Some(Target::user_unit(1000, FF)),
            source: "gnome".into(),
        }));
        for s in 0..120 {
            let ts = T0 + s * 1000;
            let hot = (30..90).contains(&s);
            for r in Resource::ALL {
                let pct = if hot && r == Resource::Io { 60.0 } else { 1.0 };
                let partial = r != Resource::Cpu;
                pressure.push(json!({
                    "ts": ts, "scope": "system", "resource": r.as_str(),
                    "some_avg10": pct, "full_avg10": partial.then_some(pct / 2.0),
                    "some_delta_us": (pct as u64) * 10_000, "full_delta_us": partial.then_some((pct as u64) * 5_000),
                    "interval_ms": 1000, "date": "2026-10-04", "hour": 10,
                }));
            }
            for (unit, class, fg, cpu, io) in
                [(FF, "interactive", true, 200_000u64, 1u64 << 20), (BATCH, "batch", false, 900_000, 50 << 20)]
            {
                units.push(json!({
                    "ts": ts, "manager": "user", "uid": 1000, "unit": unit, "class": class, "foreground": fg,
                    "cpu_usage_us": cpu, "io_rbytes": io, "io_wbytes": io, "io_rios": 10, "io_wios": 10,
                    "mem_current": 1u64 << 28, "interval_ms": 1000,
                }));
            }
        }
        events.push(event(&Event::LevelChanged {
            ts: T0 + 33_000,
            from: Level::Green,
            to: Level::Yellow,
            policy: "threshold".into(),
        }));
        events.push(event(&Event::ActionApplied {
            ts: T0 + 33_000,
            target: Target::user_unit(1000, BATCH),
            setting: Setting::IoWeight { value: 1 },
            provenance: Provenance { plugin: "threshold".into(), ..Default::default() },
        }));
        (pressure, units, events)
    }

    pub fn opts() -> SimOptions {
        SimOptions {
            config: Config::default(),
            privilege: Privilege::User,
            uid: Some(1000),
            caps: HostCaps {
                cgroup_v2: true,
                systemd: true,
                psi: true,
                uclamp: true,
                io_max: true,
                io_weight_effective: true,
                devices: vec!["259:0".into()],
                ..Default::default()
            },
            timeout_scale: 1.0,
        }
    }

    #[test]
    fn replays_a_synthetic_lake() {
        let (p, u, e) = rows();
        let rec = from_rows(&p, &u, &e, T0 - 1000, T0 + 130_000);
        assert_eq!(rec.samples.len(), 360);
        assert_eq!(rec.units.len(), 240);
        assert_eq!(rec.inputs.len(), 1);
        assert_eq!(rec.levels.len(), 1);
        assert_eq!(rec.targets().len(), 2);

        // Candidate = threshold alone, baseline none: the actions are the candidate's.
        let sim = simulate(&rec, &PolicySpec::Threshold, &PolicySpec::None, &opts()).unwrap();
        let levels: Vec<Level> = sim.candidate.levels.iter().map(|l| l.2).collect();
        assert!(levels.contains(&Level::Yellow), "I/O pressure must raise the level: {:?}", sim.candidate.levels);
        assert_eq!(levels.last(), Some(&Level::Green), "and recover: {:?}", sim.candidate.levels);
        let batch = format!("user/1000:{BATCH}");
        assert!(sim.candidate.actions.iter().any(|a| a.target == batch), "batch gets throttled");
        assert!(
            sim.candidate.throttled_ms.get(&batch).copied().unwrap_or(0) > 30_000,
            "{:?}",
            sim.candidate.throttled_ms
        );
        assert!(sim.candidate.level_time_ms.get("yellow").copied().unwrap_or(0) > 10_000);
        assert_eq!(sim.diff.len(), sim.candidate.actions.len(), "no baseline: every action is a diff");

        // Same policy as baseline: no differences.
        let same = simulate(&rec, &PolicySpec::Threshold, &PolicySpec::Threshold, &opts()).unwrap();
        assert!(same.diff.is_empty(), "{:?}", same.diff);
        let text = render(&Ui::plain(), &same, "threshold", "threshold", 10);
        assert!(text.contains("no differences"), "{text}");
        assert!(text.contains("would have throttled") && text.contains(BATCH), "{text}");
        assert!(text.contains("recorded"), "{text}");
    }

    #[test]
    fn policy_specs() {
        assert_eq!(PolicySpec::parse("builtin:threshold").unwrap(), PolicySpec::Threshold);
        assert!(PolicySpec::parse("builtin:nope").is_err());
        assert!(PolicySpec::parse("/definitely/not/a/plugin").is_err());
        assert_eq!(unflatten(Some("user"), Some(1000), "a.scope"), Target::user_unit(1000, "a.scope"));
        assert_eq!(unflatten(None, None, "cgroup:/x"), Target::Cgroup { path: "/x".into() });
        assert_eq!(unflatten(None, None, "pid:42"), Target::Process { pid: 42 });
    }

    #[test]
    fn diff_rows() {
        let act = |ts, v: Option<&str>| Act {
            ts,
            target: "t".into(),
            knob: "cpu.weight".into(),
            value: v.map(Into::into),
            setting: None,
            plugin: "p".into(),
            reason: "r".into(),
        };
        let c = RunResult {
            actions: vec![act(1, Some("cpu.weight 20")), act(2, Some("cpu.weight 50"))],
            ..Default::default()
        };
        let b = RunResult { actions: vec![act(1, Some("cpu.weight 20")), act(3, None)], ..Default::default() };
        let d = diff(&c, &b);
        assert_eq!(d.len(), 2);
        assert_eq!((d[0].ts, d[0].candidate.as_deref(), d[0].baseline.as_deref()), (2, Some("cpu.weight 50"), None));
        assert_eq!(
            (d[1].ts, d[1].candidate.as_deref(), d[1].baseline.as_deref()),
            (3, None, Some("revert cpu.weight"))
        );
    }
}
