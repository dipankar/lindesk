//! The pipeline engine: deterministic and synchronous.
//!
//! `lindeskd` and `lindesk-session` call [`Engine::step`] from their own loops with the real
//! clock; `lindeskctl simulate` and tests call it with a `SimClock` and a
//! [`crate::MemoryEnvironment`]. Each step:
//!
//! 1. polls due sensors and stores the latest samples and per-target usage;
//! 2. drains events (adapters, external plugins, [`Engine::push_event`]) and resolves
//!    Focus/Interaction pids and app ids to targets, tracking the foreground;
//! 3. classifies new or changed targets (explicit class → config `[[override]]` →
//!    classifiers by priority with prefilter and `min_confidence` → `normal`);
//! 4. builds a [`Snapshot`] and evaluates every healthy policy;
//! 5. resolves intents plus admin overrides with the safety caps;
//! 6. plans and dispatches actions to actuators (or only records them in `dry_run`);
//! 7. fans records out to sinks;
//! 8. returns a [`StepReport`].

use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use globset::{Glob, GlobMatcher};
use lindesk_core::cgroup_path;
use lindesk_core::config::Config;
use lindesk_core::reconcile::{Action, AppliedState};
use lindesk_core::resolve::{self, ADMIN, NoteKind, Proposal};
use lindesk_core::safety::{self, ClampContext};
use lindesk_plugin_api::{
    Event, HostCaps, Intent, Knob, Level, Manager, PluginError, Privilege, ProcInfo, QosClass, Record, Resource,
    Sample, Scope, SensorInput, Setting, Snapshot, Target, TimestampMs, UnitState, Usage, WatchedTarget,
};

use crate::env::Environment;
use crate::external::{HostReceiver, HostSender, host_queues};
use crate::registry::{HostContext, Registry, SlotMeta};
use crate::report::*;
use crate::supervisor::SupervisorConfig;

/// How often a known target's representative process is re-read (to notice exec changes).
const PROC_REFRESH_MS: i64 = 5_000;
/// How long a target whose classification hit a plugin error waits before retrying.
const CLASSIFY_RETRY_MS: i64 = 5_000;
/// Time one step may spend applying settings. An actuator can be slow (systemd under heavy
/// load takes seconds per call), and a step that applies dozens of changes must not stall
/// sensing and the daemon's liveness; what doesn't fit is planned again on the next step.
const ACTUATE_BUDGET: Duration = Duration::from_millis(400);
/// Actions always attempted per step, however slow, so a slow actuator still makes progress.
const ACTUATE_MIN_ACTIONS: usize = 4;
/// Called by [`Engine::step`] with what it is about to do (a stage, or one action).
pub type ProgressHook = Box<dyn Fn(&str) + Send>;

/// A source's list of visible or audible apps is dropped when it hasn't been sent again for
/// this long. Adapters resend at least every [`PRESENCE_RESEND_MS`], changed or not.
pub const PRESENCE_TTL_MS: i64 = 15 * 60 * 1000;
/// How often an adapter should resend its lists even when nothing changed.
pub const PRESENCE_RESEND_MS: u64 = 5 * 60 * 1000;
/// How often the focused application's other units are looked up again.
const FAMILY_REFRESH_MS: i64 = 5_000;
/// Every target is reclassified this often, so classifiers that look at what a unit is doing
/// right now (a build starting or finishing) are heard without the unit's main process changing.
const RECLASSIFY_MS: i64 = 15_000;
/// Notes not seen for this long are dropped from `explain`.
const NOTE_TTL_MS: i64 = 60_000;
const MAX_NOTES_PER_TARGET: usize = 32;
/// Smoothing for usage rates in `status`.
const RATE_ALPHA: f64 = 0.5;
/// Cap on events waiting for the next sink fan-out.
const MAX_UNSENT: usize = 10_000;

/// Static facts about the host running the engine.
#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// "lindeskd", "lindesk-session" or "simulate"; passed to plugins in `initialize`.
    pub host: String,
    pub privilege: Privilege,
    /// The session user (user hosts). Policies on a user host may only target this
    /// user's units.
    pub uid: Option<u32>,
    pub caps: HostCaps,
    /// When set, the applied state is saved here whenever it changes (crash recovery).
    pub state_path: Option<PathBuf>,
    pub supervisor: SupervisorConfig,
}

impl EngineOptions {
    pub fn new(host: impl Into<String>, privilege: Privilege) -> Self {
        EngineOptions {
            host: host.into(),
            privilege,
            uid: None,
            caps: HostCaps::default(),
            state_path: None,
            supervisor: SupervisorConfig::default(),
        }
    }
}

/// Configuration problems found by [`Engine::new`] / [`Engine::reload_config`].
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("config [[override]] #{}: {message}", index + 1)]
    Override { index: usize, message: String },
}

/// Errors reading or writing the applied-state file.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{path}: {source}")]
    Parse { path: PathBuf, source: serde_json::Error },
}

struct CompiledOverride {
    index: usize,
    unit: Option<GlobMatcher>,
    comm: Option<GlobMatcher>,
    class: Option<QosClass>,
    settings: Vec<Setting>,
}

impl CompiledOverride {
    fn matches(&self, target: &Target, proc: Option<&ProcInfo>) -> bool {
        if self.unit.is_none() && self.comm.is_none() {
            return false;
        }
        let unit_ok = self.unit.as_ref().is_none_or(|g| {
            let name = match target {
                Target::Unit { name, .. } => Some(name.as_str()),
                _ => None,
            };
            name.into_iter().chain(proc.and_then(|p| p.unit.as_deref())).any(|n| g.is_match(n))
        });
        let comm_ok =
            self.comm.as_ref().is_none_or(|g| proc.is_some_and(|p| !p.comm.is_empty() && g.is_match(&p.comm)));
        unit_ok && comm_ok
    }
}

/// Check that a config's `[[override]]` entries compile (globs valid, `unit` and/or `comm`
/// given), exactly as [`Engine::new`] and [`Engine::reload_config`] would. Used to validate a
/// config before a change to it is applied.
pub fn validate_overrides(config: &Config) -> Result<(), EngineError> {
    compile_overrides(config).map(|_| ())
}

fn compile_overrides(config: &Config) -> Result<Vec<CompiledOverride>, EngineError> {
    let glob = |index: usize, g: &Option<String>| -> Result<Option<GlobMatcher>, EngineError> {
        g.as_ref()
            .map(|g| Glob::new(g).map(|g| g.compile_matcher()))
            .transpose()
            .map_err(|e| EngineError::Override { index, message: e.to_string() })
    };
    config
        .overrides
        .iter()
        .enumerate()
        .map(|(index, o)| {
            if o.unit.is_none() && o.comm.is_none() {
                return Err(EngineError::Override { index, message: "needs `unit` and/or `comm`".into() });
            }
            Ok(CompiledOverride {
                index,
                unit: glob(index, &o.unit)?,
                comm: glob(index, &o.comm)?,
                class: o.class,
                settings: o.settings.clone(),
            })
        })
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Rates {
    /// CPUs' worth of time (1.0 = one full CPU).
    cpu: Option<f64>,
    /// Read + write bytes per second.
    io: Option<f64>,
}

impl Rates {
    fn update(&mut self, u: &Usage) {
        if u.interval_ms == 0 {
            return;
        }
        let secs = u.interval_ms as f64 / 1000.0;
        let cpu = u.cpu_usage_us as f64 / 1e6 / secs;
        let io = (u.io_rbytes + u.io_wbytes) as f64 / secs;
        let mix = |old: Option<f64>, new: f64| Some(old.map_or(new, |o| (1.0 - RATE_ALPHA) * o + RATE_ALPHA * new));
        self.cpu = mix(self.cpu, cpu);
        self.io = mix(self.io, io);
    }
}

fn pct_delta(base: Option<f64>, now: Option<f64>, floor: f64) -> Option<f64> {
    let (b, n) = (base?, now?);
    (b > floor).then(|| (n - b) / b * 100.0)
}

#[derive(Debug, Default)]
struct TargetState {
    watched: bool,
    cgroup: Option<String>,
    proc: Option<ProcInfo>,
    proc_checked: Option<TimestampMs>,
    decision: Option<ClassDecision>,
    dirty: bool,
    retry_at: Option<TimestampMs>,
    /// When the class was last decided.
    classified_at: Option<TimestampMs>,
    usage: Option<Usage>,
    rates: Rates,
    /// Rates just before the first throttle, and when it happened.
    baseline: Option<(TimestampMs, Rates)>,
}

impl TargetState {
    fn class(&self) -> QosClass {
        self.decision.as_ref().map_or(QosClass::Normal, |d| d.class)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct NoteKey {
    kind: &'static str,
    plugin: String,
    knob: Option<Knob>,
}

/// Per-step scratch space.
struct Ctx {
    now: TimestampMs,
    host_events: Vec<Event>,
    records: Vec<Record>,
    notes: Vec<String>,
    actions: Vec<ActionReport>,
    /// Targets an actuator reported as gone; forgotten after actuation.
    gone: Vec<Target>,
}

impl Ctx {
    fn new(now: TimestampMs) -> Self {
        Ctx {
            now,
            host_events: Vec::new(),
            records: Vec::new(),
            notes: Vec::new(),
            actions: Vec::new(),
            gone: Vec::new(),
        }
    }
}

/// The plugin pipeline. See the module docs for what a step does.
pub struct Engine {
    config: Config,
    opts: EngineOptions,
    env: Box<dyn Environment>,
    registry: Registry,
    host_tx: HostSender,
    queues: HostReceiver,
    overrides: Vec<CompiledOverride>,
    applied: AppliedState,
    targets: BTreeMap<Target, TargetState>,
    explicit: BTreeMap<Target, (QosClass, String)>,
    foreground: Option<Target>,
    /// The focused window's process, and the other units its descendants run in (an app
    /// split over several scopes, e.g. a browser's renderers): foreground too.
    foreground_pid: Option<i32>,
    foreground_family: Vec<Target>,
    family_checked: TimestampMs,
    /// The last `Event::Visible` of each source (an adapter, or one user's session), as
    /// reported: when, pids, app ids. Empty until an adapter sends one.
    visible_apps: BTreeMap<String, (TimestampMs, Vec<i32>, Vec<String>)>,
    /// The units of those apps. `None`: nobody can tell what is hidden.
    visible: Option<BTreeSet<Target>>,
    /// The last `Event::Audible` of each source, and the units of those apps.
    audible_pids: BTreeMap<String, (TimestampMs, Vec<i32>)>,
    audible: BTreeSet<Target>,
    /// Units the app in use has an open local connection to (set by the host daemon).
    serving: BTreeSet<Target>,
    pending: Vec<Event>,
    carry: Vec<Event>,
    unsent: Vec<Event>,
    system_pressure: BTreeMap<Resource, Sample>,
    cgroup_pressure: BTreeMap<String, BTreeMap<Resource, Sample>>,
    level: Level,
    notes: BTreeMap<Target, BTreeMap<NoteKey, ExplainNote>>,
    global_notes: BTreeMap<NoteKey, ExplainNote>,
    last_units_record: Option<TimestampMs>,
    prev_step: Option<TimestampMs>,
    last_now: TimestampMs,
    state_dirty: bool,
    /// Called after every actuator call and plugin poll: "the engine is alive, just busy".
    progress: Option<ProgressHook>,
    /// Observe-only (`set_paused`): sensing and policy run, nothing is applied.
    paused: bool,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("host", &self.opts.host)
            .field("level", &self.level)
            .field("targets", &self.targets.len())
            .field("applied", &self.applied.len())
            .finish()
    }
}

impl Engine {
    /// Build an engine. Register plugins through [`Engine::registry_mut`], then call
    /// [`Engine::start`] (adapters) and drive [`Engine::step`].
    pub fn new(config: Config, env: Box<dyn Environment>, opts: EngineOptions) -> Result<Engine, EngineError> {
        let overrides = compile_overrides(&config)?;
        let (host_tx, queues) = host_queues();
        let registry = Registry::new(
            HostContext { host: opts.host.clone(), privilege: opts.privilege, uid: opts.uid, caps: opts.caps.clone() },
            &config,
            host_tx.clone(),
            opts.supervisor.clone(),
        );
        Ok(Engine {
            config,
            opts,
            env,
            registry,
            host_tx,
            queues,
            overrides,
            applied: AppliedState::default(),
            paused: false,
            targets: BTreeMap::new(),
            explicit: BTreeMap::new(),
            foreground: None,
            foreground_pid: None,
            foreground_family: Vec::new(),
            family_checked: 0,
            visible_apps: BTreeMap::new(),
            visible: None,
            audible_pids: BTreeMap::new(),
            audible: BTreeSet::new(),
            serving: BTreeSet::new(),
            pending: Vec::new(),
            carry: Vec::new(),
            unsent: Vec::new(),
            system_pressure: BTreeMap::new(),
            cgroup_pressure: BTreeMap::new(),
            level: Level::Green,
            notes: BTreeMap::new(),
            global_notes: BTreeMap::new(),
            last_units_record: None,
            prev_step: None,
            last_now: 0,
            state_dirty: false,
            progress: None,
        })
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn registry_mut(&mut self) -> &mut Registry {
        &mut self.registry
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn options(&self) -> &EngineOptions {
        &self.opts
    }

    /// Swap in a new config: overrides, safety caps, dry-run and telemetry settings apply
    /// immediately and every target is reclassified. Built-in sensors, classifiers and
    /// policies whose `[plugins.<name>] config` changed are re-initialised with it (profile
    /// switches rely on this). Plugin enable/priority changes apply to plugins registered
    /// afterwards.
    pub fn reload_config(&mut self, config: Config) -> Result<(), EngineError> {
        self.overrides = compile_overrides(&config)?;
        self.registry.set_plugin_config(&config);
        for (name, r) in self.registry.reconfigure_builtins(&self.config, &config) {
            match r {
                Ok(()) => tracing::info!(plugin = %name, "re-initialised with its new config"),
                Err(e) => tracing::warn!(plugin = %name, "re-initialising with the new config failed: {e}"),
            }
        }
        self.config = config;
        for st in self.targets.values_mut() {
            st.dirty = true;
        }
        Ok(())
    }

    /// True when actions are recorded but never applied (`[general] dry_run`, or paused).
    pub fn dry_run(&self) -> bool {
        self.config.general.dry_run || self.paused
    }

    /// True while paused (see [`Engine::set_paused`]).
    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Pause: revert every setting lindesk applied, then keep sensing, classifying and
    /// deciding while applying nothing (as in dry-run; action events say "paused: ").
    /// Resume: forget what was only recorded while paused, so the next step applies the
    /// current decisions for real. Returns the restore actions of a pause.
    pub fn set_paused(&mut self, paused: bool) -> Vec<ActionReport> {
        if paused == self.paused {
            return Vec::new();
        }
        let mut reports = Vec::new();
        if paused {
            reports = self.restore_all();
            self.paused = true;
        } else {
            self.paused = false;
            if !self.config.general.dry_run {
                self.applied = AppliedState::default();
                self.state_dirty = true;
                self.autosave();
            }
        }
        for st in self.targets.values_mut() {
            st.dirty = true;
        }
        reports
    }

    /// A handle for pushing events from other threads (D-Bus handlers, adapters).
    pub fn host_sender(&self) -> HostSender {
        self.host_tx.clone()
    }

    /// Start every adapter. Returns the ones that failed (they are retried by supervision
    /// for externals; built-ins stay stopped).
    pub fn start(&mut self) -> Vec<(String, PluginError)> {
        let mut failed = Vec::new();
        for slot in &mut self.registry.adapters {
            let sink = self.host_tx.event_sink();
            if let Err(e) = slot.meta.guard(|| slot.plugin.start(sink)) {
                tracing::warn!(plugin = %slot.meta.name, "adapter failed to start: {e}");
                failed.push((slot.meta.name.clone(), e));
            }
        }
        failed
    }

    /// Queue an event for the next step (Focus, Interaction, …).
    pub fn push_event(&mut self, e: Event) {
        self.pending.push(e);
    }

    /// Pin a class on a target (highest precedence). `source` is shown by `explain`.
    pub fn set_class(&mut self, target: Target, class: QosClass, source: impl Into<String>) {
        self.explicit.insert(target.clone(), (class, source.into()));
        self.targets.entry(target).or_default().dirty = true;
    }

    /// [`Engine::set_class`] for the target owning `pid`; returns it.
    pub fn set_class_for_pid(&mut self, pid: i32, class: QosClass, source: impl Into<String>) -> Option<Target> {
        let t = self.env.target_for_pid(pid)?;
        self.set_class(t.clone(), class, source);
        Some(t)
    }

    /// Remove an explicit class; the target is reclassified on the next step.
    pub fn clear_class(&mut self, target: &Target) {
        self.explicit.remove(target);
        if let Some(st) = self.targets.get_mut(target) {
            st.dirty = true;
        }
    }

    pub fn level(&self) -> Level {
        self.level
    }

    pub fn foreground(&self) -> Option<&Target> {
        self.foreground.as_ref()
    }

    /// The other units of the focused application (a browser's renderers), not including
    /// [`Engine::foreground`] itself.
    pub fn foreground_family(&self) -> &[Target] {
        &self.foreground_family
    }

    /// The process that owns the focused window, when the adapter reported one.
    pub fn foreground_pid(&self) -> Option<i32> {
        self.foreground_pid
    }

    /// The cgroup of a known target, relative to the cgroup mount.
    pub fn target_cgroup(&self, target: &Target) -> Option<String> {
        self.targets.get(target)?.cgroup.clone()
    }

    /// The units with a window on screen, when an adapter reports visibility.
    pub fn visible_targets(&self) -> Option<&BTreeSet<Target>> {
        self.visible.as_ref()
    }

    /// Units the app in use is connected to and waiting on; policies leave them unthrottled.
    /// Replaces the previous set.
    pub fn set_serving(&mut self, serving: BTreeSet<Target>) {
        if serving != self.serving {
            self.serving = serving;
        }
    }

    /// The units that are throttled now, or would be if they weren't serving the app in
    /// use, with their cgroups: the only ones worth checking for a connection to it.
    pub fn serving_candidates(&self) -> Vec<(Target, String)> {
        self.targets
            .iter()
            .filter(|(t, st)| !self.is_foreground(t) && st.class() != QosClass::Critical)
            .filter(|(t, st)| {
                st.class().yields()
                    || self.serving.contains(*t)
                    || self.applied.for_target(t).any(|e| lindesk_core::safety::is_throttle(&e.current))
            })
            .filter_map(|(t, st)| Some((t.clone(), st.cgroup.clone()?)))
            .collect()
    }

    /// The units of apps that are playing or recording sound.
    pub fn audible_targets(&self) -> &BTreeSet<Target> {
        &self.audible
    }

    /// What lindesk currently has applied (simulated in `dry_run`).
    pub fn applied(&self) -> &AppliedState {
        &self.applied
    }

    /// Replace the host capabilities (e.g. the power source changed). Policies see the new
    /// caps in the next snapshot; plugins registered afterwards get them in `initialize`.
    pub fn set_caps(&mut self, caps: HostCaps) {
        self.registry.set_caps(caps.clone());
        self.opts.caps = caps;
    }

    /// Latest system-wide PSI sample per resource (what policies saw on the last step).
    pub fn system_pressure(&self) -> Vec<Sample> {
        self.system_pressure.values().cloned().collect()
    }

    /// Latest PSI samples measured inside a known target's own cgroup, and that cgroup.
    pub fn target_pressure(&self, target: &Target) -> Option<(String, Vec<Sample>)> {
        let cg = self.targets.get(target)?.cgroup.clone()?;
        let samples = self.cgroup_pressure.get(&cg).map(|m| m.values().cloned().collect()).unwrap_or_default();
        Some((cg, samples))
    }

    /// The class of a known target.
    pub fn class_of(&self, target: &Target) -> Option<QosClass> {
        self.targets.get(target).and_then(|s| s.decision.as_ref()).map(|d| d.class)
    }

    /// File descriptors (PSI triggers) that should wake the daemon loop early.
    /// Install a liveness callback. It is called with a short description *before* each
    /// stage of a step and each actuator call, so a daemon can tell "slow because the system
    /// is slow" from "stuck" for its watchdog, and say what the engine was doing if it is stuck.
    pub fn set_progress_hook(&mut self, hook: ProgressHook) {
        self.progress = Some(hook);
    }

    fn tick_progress(&self, doing: &str) {
        if let Some(p) = &self.progress {
            p(doing);
        }
    }

    pub fn wake_fds(&self) -> Vec<RawFd> {
        self.registry.sensors.iter().filter(|s| s.meta.is_builtin()).flat_map(|s| s.plugin.wake_fds()).collect()
    }

    /// The engine no longer needs to track `target` (unit stopped): its applied state is
    /// dropped without restoring (there is nothing left to restore).
    pub fn forget_target(&mut self, target: &Target) {
        self.applied.forget(target);
        self.targets.remove(target);
        self.explicit.remove(target);
        self.notes.remove(target);
        if self.foreground.as_ref() == Some(target) {
            self.foreground = None;
            self.foreground_pid = None;
            self.foreground_family.clear();
        }
        self.state_dirty = true;
        self.autosave();
    }

    // ------------------------------------------------------------------------------ step

    /// Run the pipeline once at time `now`.
    pub fn step(&mut self, now: TimestampMs) -> StepReport {
        let t_total = Instant::now();
        let mut timings = StepTimings::default();
        let mut cx = Ctx::new(now);
        self.last_now = now;

        // 0. Targets the host manages.
        self.tick_progress("listing units");
        let watched = self.env.watched_targets();
        self.sync_targets(&watched);

        // 1. Sensors.
        let t = Instant::now();
        self.tick_progress("polling sensors");
        self.poll_sensors(&mut cx);
        timings.sensors_us = elapsed_us(t);

        // 2. Events.
        let t = Instant::now();
        self.tick_progress("resolving events");
        let mut input = std::mem::take(&mut self.pending);
        input.extend(self.queues.events.try_iter());
        for e in &mut input {
            self.resolve_event(e);
        }
        self.prune_targets();
        timings.events_us = elapsed_us(t);

        // 3. Classification.
        let t = Instant::now();
        self.tick_progress("classifying units");
        self.classify(&mut cx);
        timings.classify_us = elapsed_us(t);

        // 4. Snapshot + policies.
        let t = Instant::now();
        let mut snapshot_events = std::mem::take(&mut self.carry);
        snapshot_events.extend(input.iter().cloned());
        snapshot_events.extend(cx.host_events.iter().cloned());
        let snapshot = self.snapshot(now, snapshot_events);
        let pre_policy_events = cx.host_events.len();
        self.tick_progress("evaluating policies");
        let proposals = self.evaluate_policies(&snapshot, &mut cx);
        timings.policies_us = elapsed_us(t);

        // 5. Resolve.
        let t = Instant::now();
        let resolution = self.resolve(proposals, &mut cx);
        timings.resolve_us = elapsed_us(t);

        // 6. Actuate.
        let t = Instant::now();
        self.applied.refresh_provenance(&resolution.desired);
        let plan = self.applied.plan(&resolution.desired);
        let planned = plan.len();
        let mut done = 0;
        for action in plan {
            // Reverts first (the plan lists them first) and a minimum of progress always; past
            // the budget the rest waits for the next step, which plans from the same state.
            if done >= ACTUATE_MIN_ACTIONS && t.elapsed() >= ACTUATE_BUDGET {
                break;
            }
            self.tick_progress(&describe(&action));
            self.execute(action, "no longer requested by any policy", &mut cx);
            done += 1;
        }
        if done < planned {
            tracing::debug!(done, planned, "actuation budget reached; the rest is applied on the next step");
        }
        for t in std::mem::take(&mut cx.gone) {
            self.forget_target(&t);
        }
        timings.actuate_us = elapsed_us(t);

        // 7. Sinks.
        let t = Instant::now();
        self.tick_progress("recording to sinks");
        let mut sink_events = std::mem::take(&mut self.unsent);
        sink_events.extend(input.iter().cloned());
        sink_events.extend(cx.host_events.iter().cloned());
        let fanned = cx.host_events.len();
        self.fan_out(&snapshot, &sink_events, &mut cx);
        timings.sinks_us = elapsed_us(t);

        // Bookkeeping: host events after the snapshot reach policies next step; events
        // produced after the fan-out reach sinks next step.
        self.unsent.extend(cx.host_events[fanned..].iter().cloned());
        self.unsent.truncate(MAX_UNSENT);
        self.carry = cx.host_events[pre_policy_events..].to_vec();
        self.prune_notes(now);
        self.prev_step = Some(now);
        self.autosave();
        timings.total_us = elapsed_us(t_total);

        let mut events = input;
        events.extend(cx.host_events);
        StepReport {
            now,
            level: self.level,
            foreground: self.foreground.clone(),
            actions: cx.actions,
            notes: cx.notes,
            events,
            timings,
        }
    }

    fn sync_targets(&mut self, watched: &[WatchedTarget]) {
        for st in self.targets.values_mut() {
            st.watched = false;
        }
        for w in watched {
            let st = self.targets.entry(w.target.clone()).or_default();
            st.watched = true;
            if w.cgroup.is_some() {
                st.cgroup = w.cgroup.clone();
            }
        }
    }

    /// Drop targets nobody cares about any more: not watched, not foreground, no explicit
    /// class and nothing applied.
    fn prune_targets(&mut self) {
        let applied: BTreeSet<&Target> = self.applied.entries().map(|e| &e.target).collect();
        let fg = self.foreground.as_ref();
        self.targets
            .retain(|t, st| st.watched || Some(t) == fg || self.explicit.contains_key(t) || applied.contains(t));
    }

    fn watched_inputs(&self) -> Vec<WatchedTarget> {
        self.targets
            .iter()
            .map(|(t, st)| WatchedTarget {
                target: t.clone(),
                cgroup: st
                    .cgroup
                    .clone()
                    .or_else(|| st.proc.as_ref().map(|p| p.cgroup.clone()).filter(|c| !c.is_empty())),
            })
            .collect()
    }

    fn poll_sensors(&mut self, cx: &mut Ctx) {
        let now = cx.now;
        let input = SensorInput {
            now,
            targets: self.watched_inputs(),
            foreground: self.foreground.clone(),
            foreground_family: self.foreground_family.clone(),
        };
        let mut outputs = Vec::new();
        let mut errors = Vec::new();
        for slot in &mut self.registry.sensors {
            if slot.meta.next_due.is_some_and(|due| now < due) || !slot.meta.is_available() {
                continue;
            }
            slot.meta.next_due = Some(now + slot.meta.interval.as_millis() as i64);
            match slot.meta.guard(|| slot.plugin.poll(&input)) {
                Ok(out) => outputs.push((slot.meta.name.clone(), out)),
                Err(e) => errors.push((slot.meta.name.clone(), e)),
            }
        }
        for (plugin, e) in errors {
            self.note(None, "plugin_error", &plugin, None, format!("sensor.poll failed: {e}"), true, cx);
        }
        let pushed: Vec<Sample> = self.queues.samples.try_iter().collect();
        for (plugin, out) in outputs {
            for s in out.samples {
                self.ingest_sample(s, cx);
            }
            for u in out.usage {
                if let Some(st) = self.targets.get_mut(&u.target) {
                    st.rates.update(&u.usage);
                    st.usage = Some(u.usage);
                }
            }
            for mut m in out.metrics {
                m.plugin = plugin.clone();
                cx.records.push(Record::Metric(m));
            }
        }
        for s in pushed {
            self.ingest_sample(s, cx);
        }
        cx.records.extend(self.queues.metrics.try_iter().map(Record::Metric));
    }

    fn ingest_sample(&mut self, s: Sample, cx: &mut Ctx) {
        match &s.scope {
            Scope::System => {
                self.system_pressure.insert(s.resource, s.clone());
            }
            Scope::Cgroup(path) => {
                self.cgroup_pressure.entry(path.clone()).or_default().insert(s.resource, s.clone());
            }
        }
        cx.records.push(Record::Pressure(s));
    }

    fn resolve_event(&mut self, e: &mut Event) {
        let (pid, target) = match e {
            Event::Focus { pid, app_id, target, .. } => {
                if target.is_none() {
                    *target = pid
                        .and_then(|p| self.env.target_for_pid(p))
                        .or_else(|| app_id.as_deref().and_then(|a| self.env.target_for_app_id(a)));
                }
                self.foreground = target.clone();
                self.foreground_pid = *pid;
                self.refresh_family();
                (*pid, target.clone())
            }
            Event::Interaction { pid, target, .. } => {
                if target.is_none() {
                    *target = pid.and_then(|p| self.env.target_for_pid(p));
                }
                (*pid, target.clone())
            }
            Event::Visible { ts, pids, app_ids, source } => {
                self.visible_apps.insert(source.clone(), (*ts, pids.clone(), app_ids.clone()));
                self.refresh_presence();
                return;
            }
            Event::Audible { ts, pids, source } => {
                self.audible_pids.insert(source.clone(), (*ts, pids.clone()));
                self.refresh_presence();
                return;
            }
            _ => return,
        };
        if let Some(t) = target {
            let st = self.targets.entry(t).or_default();
            if st.proc.is_none()
                && let Some(p) = pid.and_then(|p| self.env.proc_info(p))
            {
                st.proc = Some(p);
            }
        }
    }

    /// Recompute which other units belong to the focused application.
    fn refresh_family(&mut self) {
        self.family_checked = self.last_now;
        let primary = self.foreground.clone();
        self.foreground_family = match (self.foreground_pid, &primary) {
            (Some(pid), Some(_)) => {
                self.env.related_targets(pid).into_iter().filter(|t| Some(t) != primary.as_ref()).collect()
            }
            _ => Vec::new(),
        };
        for t in self.foreground_family.clone() {
            self.targets.entry(t).or_default();
        }
    }

    /// Recompute which units belong to an app with a visible window, and which to one that
    /// is making sound, from what the adapters last reported. Apps gain and lose processes
    /// (and units) all the time, so this runs again with the family refresh.
    fn refresh_presence(&mut self) {
        // A source that stopped reporting (its adapter or session went away) is forgotten:
        // a list nobody keeps up to date would make every new window look hidden.
        let fresh = self.last_now - PRESENCE_TTL_MS;
        self.visible_apps.retain(|_, (at, ..)| *at >= fresh);
        self.audible_pids.retain(|_, (at, _)| *at >= fresh);
        self.visible = (!self.visible_apps.is_empty()).then(|| {
            let pids: Vec<i32> = self.visible_apps.values().flat_map(|(_, pids, _)| pids.iter().copied()).collect();
            let mut set: BTreeSet<Target> = self.env.app_targets(&pids).into_iter().collect();
            let app_ids = self.visible_apps.values().flat_map(|(_, _, ids)| ids);
            set.extend(app_ids.filter_map(|a| self.env.target_for_app_id(a)));
            set
        });
        let pids: Vec<i32> = self.audible_pids.values().flat_map(|(_, pids)| pids.iter().copied()).collect();
        self.audible = self.env.app_targets(&pids).into_iter().collect();
    }

    /// Whether `t` has a window the person can see, when an adapter can tell.
    fn is_visible(&self, t: &Target) -> Option<bool> {
        self.visible.as_ref().map(|v| v.contains(t) || self.is_foreground(t))
    }

    fn is_foreground(&self, t: &Target) -> bool {
        self.foreground.as_ref() == Some(t) || self.foreground_family.contains(t)
    }

    fn classify(&mut self, cx: &mut Ctx) {
        let now = cx.now;
        // A browser spawns renderers into other scopes as you open tabs.
        if now - self.family_checked >= FAMILY_REFRESH_MS {
            if self.foreground_pid.is_some() {
                self.refresh_family();
            }
            self.family_checked = now;
            if !self.visible_apps.is_empty() || !self.audible_pids.is_empty() {
                self.refresh_presence();
            }
        }
        let keys: Vec<Target> = self.targets.keys().cloned().collect();
        let mut errors: Vec<(Target, String, PluginError)> = Vec::new();
        for t in keys {
            let Some(st) = self.targets.get_mut(&t) else { continue };
            if st.proc_checked.is_none_or(|c| now - c >= PROC_REFRESH_MS) {
                st.proc_checked = Some(now);
                if let Some(p) = self.env.representative_proc(&t) {
                    let changed = st.proc.as_ref().is_none_or(|old| {
                        (old.pid, &old.comm, &old.exe, &old.app_id) != (p.pid, &p.comm, &p.exe, &p.app_id)
                    });
                    if changed {
                        if st.decision.is_some() {
                            st.dirty = true;
                        }
                        if st.cgroup.is_none() && !p.cgroup.is_empty() {
                            st.cgroup = Some(p.cgroup.clone());
                        }
                        st.proc = Some(p);
                    }
                }
            }
            let retry =
                st.retry_at.is_some_and(|r| now >= r) || st.classified_at.is_none_or(|c| now - c >= RECLASSIFY_MS);
            if st.decision.is_some() && !st.dirty && !retry {
                continue;
            }
            let proc = st.proc.clone().unwrap_or_else(|| pseudo_proc(&t, st.cgroup.as_deref()));
            let mut errs = Vec::new();
            let (decision, provisional) =
                decide(&mut self.registry, &self.overrides, self.explicit.get(&t), &t, &proc, &mut errs);
            errors.extend(errs.into_iter().map(|(p, e)| (t.clone(), p, e)));
            let st = self.targets.get_mut(&t).expect("present");
            st.dirty = false;
            st.classified_at = Some(now);
            st.retry_at = provisional.then_some(now + CLASSIFY_RETRY_MS);
            if st.decision.as_ref().map(|d| d.class) != Some(decision.class) {
                cx.host_events.push(Event::ClassSet {
                    ts: now,
                    target: t.clone(),
                    class: decision.class,
                    source: match &decision.source {
                        ClassSource::Explicit { source } => format!("explicit:{source}"),
                        ClassSource::Override { index } => format!("override#{}", index + 1),
                        ClassSource::Classifier { plugin, .. } => format!("classifier:{plugin}"),
                        ClassSource::Default => "default".into(),
                    },
                });
            }
            st.decision = Some(decision);
        }
        for (t, plugin, e) in errors {
            self.note(Some(&t), "plugin_error", &plugin, None, format!("classify failed: {e}"), true, cx);
        }
    }

    fn snapshot(&self, now: TimestampMs, events: Vec<Event>) -> Snapshot {
        let units = self
            .targets
            .iter()
            .map(|(t, st)| UnitState {
                target: t.clone(),
                class: st.class(),
                foreground: self.is_foreground(t),
                pressure: st
                    .cgroup
                    .as_ref()
                    .and_then(|c| self.cgroup_pressure.get(c))
                    .map(|m| m.values().cloned().collect())
                    .unwrap_or_default(),
                usage: st.usage.clone(),
                applied: self.applied.for_target(t).map(|e| e.current.clone()).collect(),
                top_comm: st.proc.as_ref().map(|p| p.comm.clone()).filter(|c| !c.is_empty()),
                visible: self.is_visible(t),
                audible: (!self.audible_pids.is_empty()).then(|| self.audible.contains(t)),
                serving: self.serving.contains(t),
            })
            .collect();
        Snapshot {
            now,
            level: self.level(),
            pressure: self.system_pressure.values().cloned().collect(),
            units,
            events,
            foreground: self.foreground.clone(),
            caps: self.opts.caps.clone(),
        }
    }

    /// May a policy on this host touch `target`? User hosts are limited to their user.
    fn target_allowed(&self, target: &Target) -> bool {
        if self.opts.privilege == Privilege::System {
            return true;
        }
        let Some(uid) = self.opts.uid else { return true };
        match target {
            Target::Unit { manager: Manager::User(u), .. } => *u == uid,
            Target::Unit { manager: Manager::System, .. } => false,
            Target::Cgroup { path } => cgroup_path::parse(path).user == Some(uid),
            Target::Process { pid } => self.env.proc_info(*pid).is_some_and(|p| p.uid == uid),
        }
    }

    fn evaluate_policies(&mut self, snapshot: &Snapshot, cx: &mut Ctx) -> Vec<Proposal> {
        let mut proposals = Vec::new();
        let mut levels: Vec<(Level, String)> = Vec::new();
        let mut errors = Vec::new();
        for slot in &mut self.registry.policies {
            if !slot.meta.is_available() {
                continue;
            }
            match slot.meta.guard(|| slot.plugin.evaluate(snapshot)) {
                Ok(out) => {
                    for i in out.intents {
                        let priority = effective_priority(&slot.meta, i.priority);
                        proposals.push(Proposal::new(slot.meta.name.clone(), Intent { priority, ..i }));
                    }
                    if let Some(l) = out.level {
                        levels.push((l, slot.meta.name.clone()));
                    }
                    for mut m in out.metrics {
                        m.plugin = slot.meta.name.clone();
                        cx.records.push(Record::Metric(m));
                    }
                }
                Err(e) => errors.push((slot.meta.name.clone(), e)),
            }
        }
        for (plugin, e) in errors {
            self.note(None, "plugin_error", &plugin, None, format!("policy.evaluate failed: {e}"), true, cx);
        }
        // Access control for user hosts.
        let (allowed, denied): (Vec<_>, Vec<_>) =
            proposals.into_iter().partition(|p| self.target_allowed(&p.intent.target));
        for p in denied {
            let knob = p.intent.setting.knob();
            let msg = format!("{} is outside this user's units; ignored", p.intent.target);
            self.note(Some(&p.intent.target), "rejected", &p.plugin, Some(&knob), msg, true, cx);
        }
        let mut proposals = allowed;
        // Admin overrides from the config.
        for (t, st) in &self.targets {
            for o in &self.overrides {
                if o.settings.is_empty() || !o.matches(t, st.proc.as_ref()) {
                    continue;
                }
                for s in &o.settings {
                    proposals.push(Proposal::new(
                        ADMIN,
                        Intent {
                            target: t.clone(),
                            setting: s.clone(),
                            priority: 0,
                            reason: format!("config [[override]] #{}", o.index + 1),
                        },
                    ));
                }
            }
        }
        if let Some((new, by)) = levels.iter().max_by_key(|(l, _)| *l).cloned()
            && new != self.level
        {
            cx.host_events.push(Event::LevelChanged { ts: cx.now, from: self.level, to: new, policy: by });
            self.level = new;
        }
        proposals
    }

    fn resolve(&mut self, proposals: Vec<Proposal>, cx: &mut Ctx) -> resolve::Resolution {
        let mem_total = self.env.mem_total();
        let targets = &self.targets;
        // The cgroups of the app in use. A slice above one of them contains it: throttling
        // the slice throttles the app, whatever a policy meant.
        let fg_cgroups: Vec<&str> = self
            .foreground
            .iter()
            .chain(&self.foreground_family)
            .filter_map(|t| targets.get(t)?.cgroup.as_deref())
            .collect();
        let contains_foreground = |t: &Target| {
            targets.get(t).and_then(|s| s.cgroup.as_deref()).is_some_and(|cg| {
                let prefix = format!("{}/", cg.trim_end_matches('/'));
                fg_cgroups.iter().any(|f| f.starts_with(&prefix))
            })
        };
        let resolution = resolve::resolve(proposals, &self.config.safety, |t| ClampContext {
            // lindesk's own units, and anything the app in use runs inside, are judged like
            // critical ones: boosts allowed, throttles rejected, whoever asks. (They aren't
            // *classed* critical: their unit files already give them more than the class
            // would, or they hold far more than the app.)
            class: if is_own_unit(t) || contains_foreground(t) {
                Some(QosClass::Critical)
            } else {
                targets.get(t).map(TargetState::class)
            },
            mem_current: targets.get(t).and_then(|s| s.usage.as_ref()).map(|u| u.mem_current),
            mem_total,
        });
        for n in &resolution.notes {
            let (kind, event) = match n.kind {
                NoteKind::Clamped => ("clamped", true),
                NoteKind::Rejected => ("rejected", true),
                NoteKind::Overruled => ("overruled", false),
            };
            self.note(Some(&n.target), kind, &n.plugin, Some(&n.knob), n.message.clone(), event, cx);
        }
        resolution
    }

    /// Available actuators whose caps cover the target's kind and the knob, highest
    /// priority first. Caps only describe target *kinds*; an actuator that can't handle a
    /// particular target (e.g. a unit of another systemd manager) answers `Unsupported` and
    /// the engine moves on to the next candidate.
    fn actuator_candidates(&self, target: &Target, knob: &Knob) -> Vec<usize> {
        self.registry
            .actuators
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.meta.is_available() && {
                    let caps = s.plugin.caps();
                    caps.supports_target(target) && caps.supports_knob(knob)
                }
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn no_actuator_message(what: String, unsupported: &[String]) -> String {
        if unsupported.is_empty() { what } else { format!("{what} ({})", unsupported.join("; ")) }
    }

    fn execute(&mut self, action: Action, revert_reason: &str, cx: &mut Ctx) {
        let now = cx.now;
        let dry = self.dry_run();
        match action {
            Action::Apply { target, setting, mut provenance, first } => {
                let knob = setting.knob();
                let mut report = ActionReport {
                    target: target.clone(),
                    knob: knob.clone(),
                    setting: Some(setting.clone()),
                    original: None,
                    provenance: Some(provenance.clone()),
                    actuator: None,
                    outcome: ActionOutcome::Applied,
                };
                if dry {
                    report.outcome = ActionOutcome::DryRun;
                    self.applied.record_apply(&target, &setting, None, &provenance, now);
                    // Telemetry must not claim a dry-run action really happened.
                    let why = if self.paused && !self.config.general.dry_run { "paused" } else { "dry run" };
                    provenance.reason = format!("{why}: {}", provenance.reason);
                    cx.host_events.push(Event::ActionApplied { ts: now, target: target.clone(), setting, provenance });
                    self.after_change(&target, now);
                    cx.actions.push(report);
                    return;
                }
                // The first candidate that doesn't answer `Unsupported` handles it.
                let mut unsupported = Vec::new();
                let mut handled = None;
                for idx in self.actuator_candidates(&target, &knob) {
                    let slot = &mut self.registry.actuators[idx];
                    let name = slot.meta.name.clone();
                    let mut original = None;
                    let mut read_error = None;
                    if first {
                        match slot.meta.guard(|| slot.plugin.read(&target, &knob)) {
                            Ok(v) => original = v,
                            Err(PluginError::Unsupported(why)) => {
                                unsupported.push(format!("{name}: {why}"));
                                continue;
                            }
                            Err(e) => read_error = Some(format!("reading original {knob}: {e}")),
                        }
                    }
                    match slot.meta.guard(|| slot.plugin.apply(&target, &setting)) {
                        Err(PluginError::Unsupported(why)) => unsupported.push(format!("{name}: {why}")),
                        result => {
                            handled = Some((name, original, read_error, result));
                            break;
                        }
                    }
                }
                let Some((actuator, original, read_error, result)) = handled else {
                    report.outcome = ActionOutcome::NoActuator;
                    let msg = Self::no_actuator_message(
                        format!("no available actuator supports {knob} on {target}"),
                        &unsupported,
                    );
                    self.note(Some(&target), "no_actuator", &provenance.plugin, Some(&knob), msg, true, cx);
                    cx.actions.push(report);
                    return;
                };
                report.actuator = Some(actuator.clone());
                if let Some(msg) = read_error {
                    self.note(Some(&target), "actuator_error", &actuator, Some(&knob), msg, true, cx);
                }
                match result {
                    Ok(()) => {
                        report.original = original
                            .clone()
                            .or_else(|| self.applied.get(&target, &knob).and_then(|e| e.original.clone()));
                        self.applied.record_apply(&target, &setting, original, &provenance, now);
                        self.state_dirty = true;
                        cx.host_events.push(Event::ActionApplied {
                            ts: now,
                            target: target.clone(),
                            setting,
                            provenance,
                        });
                        self.after_change(&target, now);
                    }
                    Err(PluginError::Gone(_)) => {
                        report.outcome = ActionOutcome::Failed { error: "target gone".into() };
                        if !cx.gone.contains(&target) {
                            cx.gone.push(target.clone());
                        }
                    }
                    Err(e) => {
                        report.outcome = ActionOutcome::Failed { error: e.to_string() };
                        let msg = format!("applying {setting} failed: {e}");
                        self.note(Some(&target), "actuator_error", &actuator, Some(&knob), msg, true, cx);
                    }
                }
                cx.actions.push(report);
            }
            Action::Reset { target, knob, original } => {
                let mut report = ActionReport {
                    target: target.clone(),
                    knob: knob.clone(),
                    setting: None,
                    original: original.clone(),
                    provenance: None,
                    actuator: None,
                    outcome: ActionOutcome::Reverted,
                };
                let ok = if dry {
                    report.outcome = ActionOutcome::DryRun;
                    true
                } else {
                    let mut unsupported = Vec::new();
                    let mut handled = None;
                    for idx in self.actuator_candidates(&target, &knob) {
                        let slot = &mut self.registry.actuators[idx];
                        let name = slot.meta.name.clone();
                        match slot.meta.guard(|| slot.plugin.reset(&target, &knob, original.as_ref())) {
                            Err(PluginError::Unsupported(why)) => unsupported.push(format!("{name}: {why}")),
                            result => {
                                handled = Some((name, result));
                                break;
                            }
                        }
                    }
                    match handled {
                        Some((name, result)) => {
                            report.actuator = Some(name.clone());
                            match result {
                                Ok(()) => true,
                                // Nothing left to restore.
                                Err(PluginError::Gone(_)) => true,
                                Err(e) => {
                                    report.outcome = ActionOutcome::Failed { error: e.to_string() };
                                    let msg = format!("reverting {knob} failed (will retry): {e}");
                                    self.note(Some(&target), "actuator_error", &name, Some(&knob), msg, true, cx);
                                    false
                                }
                            }
                        }
                        None => {
                            report.outcome = ActionOutcome::NoActuator;
                            let msg = Self::no_actuator_message(
                                format!("no available actuator can revert {knob} on {target} (will retry)"),
                                &unsupported,
                            );
                            self.note(Some(&target), "no_actuator", "host", Some(&knob), msg, true, cx);
                            false
                        }
                    }
                };
                if ok {
                    self.applied.record_reset(&target, &knob);
                    self.state_dirty = true;
                    cx.host_events.push(Event::ActionReverted {
                        ts: now,
                        target: target.clone(),
                        knob,
                        reason: revert_reason.to_string(),
                    });
                    self.after_change(&target, now);
                }
                cx.actions.push(report);
            }
        }
    }

    /// Track the pre-throttle usage baseline for `status`.
    fn after_change(&mut self, target: &Target, now: TimestampMs) {
        let throttled = self.applied.for_target(target).any(|e| safety::is_throttle(&e.current));
        if let Some(st) = self.targets.get_mut(target) {
            if !throttled {
                st.baseline = None;
            } else if st.baseline.is_none() {
                st.baseline = Some((now, st.rates.clone()));
            }
        }
    }

    fn fan_out(&mut self, snapshot: &Snapshot, events: &[Event], cx: &mut Ctx) {
        if !self.config.telemetry.enabled || self.registry.sinks.is_empty() {
            cx.records.clear();
            return;
        }
        let now = cx.now;
        let interval = self.config.telemetry.interval.as_millis() as i64;
        if self.last_units_record.is_none_or(|l| now - l >= interval) {
            self.last_units_record = Some(now);
            for u in &snapshot.units {
                // `applied` reflects this step's actions, not the snapshot's.
                cx.records.push(Record::Unit {
                    ts: now,
                    target: u.target.clone(),
                    class: u.class,
                    foreground: u.foreground,
                    usage: u.usage.clone().unwrap_or_default(),
                    applied: self.applied.for_target(&u.target).map(|e| e.current.clone()).collect(),
                });
            }
        }
        cx.records.extend(events.iter().cloned().map(Record::Event));
        let records = std::mem::take(&mut cx.records);
        let mut errors = Vec::new();
        for slot in &mut self.registry.sinks {
            if !slot.meta.is_available() {
                continue;
            }
            let tables = slot.plugin.tables();
            let batch: Vec<Record> = if tables.is_empty() {
                records.clone()
            } else {
                records.iter().filter(|r| tables.iter().any(|t| t == r.table())).cloned().collect()
            };
            if batch.is_empty() {
                continue;
            }
            if let Err(e) = slot.meta.guard(|| slot.plugin.record(&batch)) {
                errors.push((slot.meta.name.clone(), e));
            }
        }
        for (plugin, e) in errors {
            // Recorded now, delivered to sinks with the next step.
            self.note(None, "plugin_error", &plugin, None, format!("sink.record failed: {e}"), true, cx);
        }
    }

    /// Store a note for `explain`; emit a `PluginError` event when it's new or changed.
    #[allow(clippy::too_many_arguments)]
    fn note(
        &mut self,
        target: Option<&Target>,
        kind: &'static str,
        plugin: &str,
        knob: Option<&Knob>,
        message: String,
        event: bool,
        cx: &mut Ctx,
    ) {
        let now = cx.now;
        let prev = self.prev_step.unwrap_or(i64::MIN);
        let key = NoteKey { kind, plugin: plugin.to_string(), knob: knob.cloned() };
        let map = match target {
            Some(t) => self.notes.entry(t.clone()).or_default(),
            None => &mut self.global_notes,
        };
        let fresh = map.get(&key).is_none_or(|n| n.message != message || n.last_seen < prev);
        let entry = map.entry(key).or_insert_with(|| ExplainNote {
            ts: now,
            last_seen: now,
            kind: kind.to_string(),
            plugin: plugin.to_string(),
            knob: knob.cloned(),
            message: message.clone(),
        });
        if fresh {
            entry.ts = now;
            entry.message = message.clone();
        }
        entry.last_seen = now;
        if map.len() > MAX_NOTES_PER_TARGET
            && let Some(oldest) = map.iter().min_by_key(|(_, n)| n.last_seen).map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
        if !fresh {
            return;
        }
        let what = match (target, knob) {
            (Some(t), Some(k)) => format!("{t} {k}: "),
            (Some(t), None) => format!("{t}: "),
            _ => String::new(),
        };
        cx.notes.push(format!("[{kind}] {plugin}: {what}{message}"));
        if event {
            cx.host_events.push(Event::PluginError {
                ts: now,
                plugin: plugin.to_string(),
                message: format!("{what}{message}"),
            });
        }
    }

    fn prune_notes(&mut self, now: TimestampMs) {
        let fresh = |n: &ExplainNote| now - n.last_seen <= NOTE_TTL_MS;
        for m in self.notes.values_mut() {
            m.retain(|_, n| fresh(n));
        }
        self.notes.retain(|_, m| !m.is_empty());
        self.global_notes.retain(|_, n| fresh(n));
    }

    // ------------------------------------------------------------------------------ lifecycle

    /// Revert everything lindesk applied (shutdown, `lindeskctl restore`).
    pub fn restore_all(&mut self) -> Vec<ActionReport> {
        let mut cx = Ctx::new(self.last_now.max(lindesk_core::clock::now_ms()));
        for action in self.applied.restore_all() {
            self.tick_progress(&describe(&action));
            self.execute(action, "restored", &mut cx);
        }
        self.state_dirty = true;
        self.autosave();
        // Sinks see the revert events with the next step (or at shutdown).
        self.unsent.extend(cx.host_events);
        cx.actions
    }

    /// Restore everything, flush sinks and stop all plugins.
    pub fn shutdown(&mut self) -> Vec<ActionReport> {
        let actions = self.restore_all();
        if self.config.telemetry.enabled && !self.unsent.is_empty() {
            let records: Vec<Record> = self.unsent.drain(..).map(Record::Event).collect();
            for slot in &mut self.registry.sinks {
                let tables = slot.plugin.tables();
                let batch: Vec<Record> = records
                    .iter()
                    .filter(|r| tables.is_empty() || tables.iter().any(|t| t == r.table()))
                    .cloned()
                    .collect();
                if !batch.is_empty() && slot.meta.is_available() {
                    let _ = slot.meta.guard(|| slot.plugin.record(&batch));
                }
            }
        }
        for slot in &mut self.registry.sinks {
            if slot.meta.is_available() {
                let _ = slot.meta.guard(|| slot.plugin.flush());
            }
        }
        self.registry.shutdown();
        actions
    }

    /// Adopt leftover state from a previous run (crash recovery). Entries that the next
    /// step's policies don't want are reverted then, keeping the *original* originals; the
    /// rest are kept without flapping. In `dry_run` leftovers are restored immediately.
    /// A missing file is not an error. Returns the number of entries recovered.
    pub fn load_state(&mut self, path: &Path) -> Result<usize, StateError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(source) => return Err(StateError::Io { path: path.into(), source }),
        };
        let loaded =
            AppliedState::from_json(&text).map_err(|source| StateError::Parse { path: path.into(), source })?;
        let n = loaded.len();
        if self.dry_run() {
            // Nothing in dry-run state is real, so put the real leftovers back right away.
            let saved = std::mem::replace(&mut self.applied, loaded);
            self.config.general.dry_run = false;
            self.restore_all();
            self.config.general.dry_run = true;
            let leftovers = std::mem::replace(&mut self.applied, saved);
            for e in leftovers.entries() {
                tracing::warn!(target = %e.target, knob = %e.knob, "could not restore leftover setting");
            }
        } else {
            for e in loaded.entries() {
                if self.applied.get(&e.target, &e.knob).is_none() {
                    self.applied.record_apply(&e.target, &e.current, e.original.clone(), &e.provenance, e.since);
                }
            }
            self.state_dirty = true;
        }
        Ok(n)
    }

    /// Write the applied state as JSON (atomically: temp file + rename). In `dry_run`
    /// nothing real is applied, so an empty state is written.
    pub fn save_state(&self, path: &Path) -> Result<(), StateError> {
        let json = if self.dry_run() { "[]".to_string() } else { self.applied.to_json() };
        let io = |source| StateError::Io { path: path.into(), source };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(io)?;
        std::fs::rename(&tmp, path).map_err(io)
    }

    fn autosave(&mut self) {
        if !self.state_dirty {
            return;
        }
        if let Some(p) = self.opts.state_path.clone() {
            match self.save_state(&p) {
                Ok(()) => self.state_dirty = false,
                Err(e) => tracing::error!("saving applied state: {e}"),
            }
        } else {
            self.state_dirty = false;
        }
    }

    // ------------------------------------------------------------------------------ DX

    fn summary(&self, t: &Target) -> TargetSummary {
        let st = self.targets.get(t);
        TargetSummary {
            target: t.clone(),
            comm: st.and_then(|s| s.proc.as_ref()).map(|p| p.comm.clone()).filter(|c| !c.is_empty()),
            class: st.and_then(|s| s.decision.as_ref()).map(|d| d.class),
            foreground: self.is_foreground(t),
            settings: self.applied.for_target(t).map(|e| e.current.clone()).collect(),
        }
    }

    /// Level, foreground, protected and yielding targets, plugin health.
    pub fn status(&self) -> Status {
        let protected = self
            .targets
            .iter()
            .filter(|(t, st)| {
                self.foreground.as_ref() == Some(*t) || matches!(st.class(), QosClass::Critical | QosClass::Interactive)
            })
            .map(|(t, _)| self.summary(t))
            .collect();
        let mut throttled: BTreeMap<&Target, TimestampMs> = BTreeMap::new();
        for e in self.applied.entries().filter(|e| safety::is_throttle(&e.current)) {
            let since = throttled.entry(&e.target).or_insert(e.since);
            *since = (*since).min(e.since);
        }
        let yielding = throttled
            .into_iter()
            .map(|(t, since)| {
                let st = self.targets.get(t);
                let (base, cur) = match st.and_then(|s| s.baseline.as_ref().map(|b| (&b.1, &s.rates))) {
                    Some((b, c)) => (b.clone(), c.clone()),
                    None => (Rates::default(), Rates::default()),
                };
                YieldingTarget {
                    summary: self.summary(t),
                    since,
                    cpu_delta_pct: pct_delta(base.cpu, cur.cpu, 1e-4),
                    io_delta_pct: pct_delta(base.io, cur.io, 1.0),
                }
            })
            .collect();
        Status {
            now: self.last_now,
            host: self.opts.host.clone(),
            level: self.level,
            dry_run: self.dry_run(),
            foreground: self.foreground.as_ref().map(|t| self.summary(t)),
            protected,
            yielding,
            plugins: self.registry.health(),
        }
    }

    /// Class and who decided it, effective settings with provenance and originals, and
    /// recent notes (clamps, rejections, errors) for one target.
    pub fn explain(&self, target: &Target) -> Explanation {
        let st = self.targets.get(target);
        let mut notes: Vec<ExplainNote> =
            self.notes.get(target).map(|m| m.values().cloned().collect()).unwrap_or_default();
        notes.sort_by_key(|n| std::cmp::Reverse(n.last_seen));
        Explanation {
            target: target.clone(),
            known: st.is_some() || self.applied.for_target(target).next().is_some(),
            proc: st.and_then(|s| s.proc.clone()),
            class: st.and_then(|s| s.decision.clone()),
            foreground: self.foreground.as_ref() == Some(target),
            dry_run: self.dry_run(),
            settings: self
                .applied
                .for_target(target)
                .map(|e| ExplainedSetting {
                    knob: e.knob.clone(),
                    value: e.current.clone(),
                    original: e.original.clone(),
                    provenance: e.provenance.clone(),
                    since: e.since,
                })
                .collect(),
            notes,
        }
    }

    /// [`Engine::explain`] for the target owning `pid`.
    pub fn explain_pid(&self, pid: i32) -> Option<Explanation> {
        let t = self.env.target_for_pid(pid)?;
        let mut e = self.explain(&t);
        if e.proc.as_ref().is_none_or(|p| p.pid != pid) {
            e.proc = self.env.proc_info(pid).or(e.proc);
        }
        Some(e)
    }

    /// Plugin-level notes (policy/sensor/sink failures) still within their TTL.
    pub fn global_notes(&self) -> Vec<ExplainNote> {
        self.global_notes.values().cloned().collect()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Persist whatever is applied so a restart can recover it; never restore here
        // (the daemon decides whether a drop is a shutdown).
        self.autosave();
    }
}

fn elapsed_us(t: Instant) -> u64 {
    t.elapsed().as_micros() as u64
}

/// Effective intent priority: an admin `[plugins.<name>] priority` overrides everything;
/// otherwise the intent's own priority, with `0` meaning "the plugin's priority".
fn effective_priority(meta: &SlotMeta, intent_priority: i32) -> i32 {
    if meta.priority_overridden || intent_priority == 0 { meta.priority } else { intent_priority }
}

fn pseudo_proc(t: &Target, cgroup: Option<&str>) -> ProcInfo {
    match t {
        Target::Unit { name, .. } => ProcInfo {
            pid: 0,
            uid: t.user().unwrap_or(0),
            comm: String::new(),
            exe: None,
            cgroup: cgroup.unwrap_or_default().to_string(),
            unit: Some(name.clone()),
            slice: None,
            app_id: cgroup_path::app_id_from_unit(name),
        },
        Target::Cgroup { path } => {
            let info = cgroup_path::parse(path);
            ProcInfo {
                cgroup: path.clone(),
                unit: info.unit,
                slice: info.slice,
                uid: info.user.unwrap_or(0),
                ..Default::default()
            }
        }
        Target::Process { pid } => ProcInfo { pid: *pid, ..Default::default() },
    }
}

/// What an action is about to do, for the liveness hook.
fn describe(a: &Action) -> String {
    match a {
        Action::Apply { target, setting, .. } => format!("applying {} on {target}", setting.knob()),
        Action::Reset { target, knob, .. } => format!("reverting {knob} on {target}"),
    }
}

/// lindesk's own services (`lindeskd`, `lindesk-session`, `lindesk-mcp`).
fn is_own_unit(t: &Target) -> bool {
    matches!(t, Target::Unit { name, .. }
        if matches!(name.as_str(), "lindeskd.service" | "lindesk-session.service" | "lindesk-mcp.service"))
}

fn decide(
    registry: &mut Registry,
    overrides: &[CompiledOverride],
    explicit: Option<&(QosClass, String)>,
    target: &Target,
    proc: &ProcInfo,
    errors: &mut Vec<(String, PluginError)>,
) -> (ClassDecision, bool) {
    if let Some((class, source)) = explicit {
        return (
            ClassDecision {
                class: *class,
                source: ClassSource::Explicit { source: source.clone() },
                reason: String::new(),
            },
            false,
        );
    }
    for o in overrides {
        if let Some(class) = o.class
            && o.matches(target, Some(proc))
        {
            return (
                ClassDecision { class, source: ClassSource::Override { index: o.index }, reason: String::new() },
                false,
            );
        }
    }
    let mut provisional = false;
    for slot in &mut registry.classifiers {
        if slot.meta.prefilter.as_ref().is_some_and(|f| !f.matches(proc)) {
            continue;
        }
        if !slot.meta.is_available() {
            // Backing off: a better answer may come later.
            provisional |=
                !matches!(crate::supervisor::lock(&slot.meta.sup).health(), crate::supervisor::Health::Disabled { .. });
            continue;
        }
        match slot.meta.guard(|| slot.plugin.classify(proc)) {
            Ok(Some(c)) if c.confidence >= slot.meta.min_confidence => {
                return (
                    ClassDecision {
                        class: c.class,
                        source: ClassSource::Classifier { plugin: slot.meta.name.clone(), confidence: c.confidence },
                        reason: c.reason,
                    },
                    provisional,
                );
            }
            Ok(_) => {}
            Err(e) => {
                provisional = true;
                errors.push((slot.meta.name.clone(), e));
            }
        }
    }
    (ClassDecision { class: QosClass::Normal, source: ClassSource::Default, reason: String::new() }, provisional)
}

#[cfg(test)]
const _: () = {
    const fn is_send<T: Send>() {}
    is_send::<Engine>();
};
