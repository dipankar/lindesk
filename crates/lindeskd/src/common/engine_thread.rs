//! The engine thread shared by `lindeskd` and `lindesk-session`.
//!
//! The [`Engine`] and everything it owns (blocking zbus clients of the systemd actuator and
//! the environment, PSI trigger fds, external plugin pipes) live on one dedicated
//! `std::thread`. The D-Bus servers run on a tokio runtime and talk to it through:
//!
//! * a command queue ([`Command`]) plus an eventfd that wakes the thread's `poll(2)`;
//! * an `Arc<RwLock<Published>>` snapshot written after every step (status JSON, level,
//!   per-uid session states for the broker);
//! * an unbounded tokio channel of [`Notification`]s (level changes, user states, events).
//!
//! Every streamed event has an [`Audience`] ([`Hooks::audience`]): the broker sends events
//! about one user's units only to that user (and root), never broadcasts them.
//!
//! The loop: `poll` on the command eventfd plus [`Engine::wake_fds`] with a timeout of
//! `[general] tick` → drain commands → [`Engine::step`] → publish → notify. Steps happen on
//! every tick, when a wake fd fires, or right after an event-like command, but never closer
//! together than [`MIN_STEP_SPACING`].

use std::collections::{BTreeMap, VecDeque};
use std::os::fd::{BorrowedFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lindesk_core::clock::now_ms;
use lindesk_host::{Engine, StepReport};
use lindesk_plugin_api::{Event, Hardware, LatencyMap, Level, Metric, QosClass, Resource, Sample, Target, TimestampMs};
use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec};
use serde_json::{Value, json};
use tokio::sync::{mpsc as tmpsc, oneshot};

use super::what::{self, What};

/// Minimum time between two steps, whatever wakes the loop.
pub const MIN_STEP_SPACING: Duration = Duration::from_millis(20);

/// Steps slower than this are logged (with the per-stage timings).
const SLOW_STEP_US: u64 = 1_000_000;

/// How often [`Hooks::maintenance`] runs.
pub const MAINTENANCE_EVERY: Duration = Duration::from_secs(30);

/// Events kept for `RecentEvents`.
pub const EVENT_RING: usize = 500;

/// Most events carried by one `Events` signal (a step with more emits several signals).
pub const EVENTS_PER_SIGNAL: usize = 100;

/// Reply channel of a command.
pub type Reply<T> = oneshot::Sender<T>;

/// Who may see an event: `None` = everyone (system scope); `Some(uid)` = that user and root.
pub type Audience = Option<u32>;

/// Whether `viewer` (a uid) may see an event for `audience`. Root sees everything.
pub fn visible_to(audience: Audience, viewer: u32) -> bool {
    viewer == 0 || audience.is_none_or(|u| u == viewer)
}

/// The profiles as the engine thread sees them (for guardrails and `ListProfiles`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProfilesView {
    /// Every known profile name.
    pub names: Vec<String>,
    /// `ListProfiles` JSON: an array of `lindesk_core::profile::ProfileInfo`.
    pub list: Value,
}

/// Requests to the engine thread. `X` carries daemon-specific commands.
pub enum Command<X> {
    /// Queue an event (Focus, Interaction, …) and step soon.
    PushEvent(Event),
    /// Record a free-form metric (D-Bus `ReportMetric`).
    Metric(Metric),
    /// Pin a class on the unit owning `pid`. Replies with that unit, or why not.
    SetClass { pid: i32, class: QosClass, source: String, reply: Reply<Result<Target, String>> },
    /// Explain a pid or target string (see [`what::parse`]). Replies with JSON.
    Explain { what: String, reply: Reply<Result<String, String>> },
    /// Fresh status JSON.
    Status { reply: Reply<String> },
    /// Re-read the config (and the profiles), apply it and rediscover plugins. Replies with
    /// a summary.
    Reload { reply: Reply<Result<String, String>> },
    /// The host's hardware profile (`HostCaps.hardware`, with the latency map).
    Hardware { reply: Reply<Hardware> },
    /// Known profiles and the `ListProfiles` rows.
    Profiles { reply: Reply<ProfilesView> },
    /// A calibration finished and was saved to `path`: put it into the caps and announce it.
    Calibrated { map: Box<LatencyMap>, path: PathBuf },
    /// Restore everything, stop plugins and end the thread.
    Shutdown,
    /// Daemon-specific.
    Ext(X),
}

/// Things the tokio side should tell the world about.
#[derive(Clone, Debug, PartialEq)]
pub enum Notification {
    LevelChanged(Level),
    /// A user's desired set changed (broker only): `SessionState` JSON.
    UserState {
        uid: u32,
        json: String,
    },
    /// Events of one step for one audience, as a JSON array (at most [`EVENTS_PER_SIGNAL`]).
    Events {
        json: String,
        audience: Audience,
    },
    /// The engine thread has shut down.
    Stopped,
}

/// The latest state, readable without a round trip to the engine thread.
#[derive(Clone, Debug, Default)]
pub struct Published {
    /// Status JSON (same as [`Command::Status`]).
    pub status: String,
    pub level: Level,
    pub foreground: Option<Target>,
    /// Broker: `SessionState` JSON per registered uid.
    pub users: BTreeMap<u32, String>,
    pub steps: u64,
    pub last_step: TimestampMs,
}

/// Per-iteration context handed to [`Hooks`].
pub struct LoopCx<'a> {
    pub published: &'a RwLock<Published>,
    pub notify: &'a tmpsc::UnboundedSender<Notification>,
    /// Set to true to step right after this command batch.
    pub want_step: bool,
}

impl LoopCx<'_> {
    pub fn notify(&self, n: Notification) {
        let _ = self.notify.send(n);
    }

    pub fn publish(&self, f: impl FnOnce(&mut Published)) {
        let mut p = self.published.write().unwrap_or_else(|e| e.into_inner());
        f(&mut p);
    }
}

/// What a daemon adds to the shared loop.
pub trait Hooks: Send + 'static {
    type Ext: Send + 'static;

    /// Daemon-specific fields merged into the status JSON's `"daemon"` object (must include
    /// `"mode"`).
    fn status_extra(&self, _engine: &Engine) -> serde_json::Map<String, Value> {
        serde_json::Map::new()
    }

    /// Whether an interaction epoch is active and who says so (`None` = unknown).
    fn epoch(&self, _engine: &Engine) -> (Option<bool>, &'static str) {
        (None, "unknown")
    }

    /// Who may see `event` (default: everyone).
    fn audience(&self, _event: &Event) -> Audience {
        None
    }

    /// Known profiles (default: none).
    fn profiles(&self, _engine: &Engine) -> ProfilesView {
        ProfilesView { names: Vec::new(), list: Value::Array(Vec::new()) }
    }

    /// A calibration result arrived ([`Command::Calibrated`]).
    fn calibrated(&mut self, engine: &mut Engine, map: LatencyMap, path: &std::path::Path) {
        super::calibration::attach(engine, map, path, now_ms());
    }

    /// Candidate targets for a bare unit name in `explain`, most likely first.
    fn unit_candidates(&self, engine: &Engine, name: &str) -> Vec<Target>;

    /// A daemon-specific command.
    fn ext(&mut self, engine: &mut Engine, ext: Self::Ext, cx: &mut LoopCx<'_>);

    /// After every step.
    fn after_step(&mut self, _engine: &mut Engine, _report: &StepReport, _cx: &mut LoopCx<'_>) {}

    /// [`Command::Reload`].
    fn reload(&mut self, engine: &mut Engine) -> Result<String, String>;

    /// Warnings of the start-up requirements check, for the status JSON.
    fn requirements(&self) -> Value {
        Value::Array(Vec::new())
    }

    /// Every [`MAINTENANCE_EVERY`] (e.g. re-probe the power source).
    fn maintenance(&mut self, _engine: &mut Engine) {}

    /// After [`Engine::shutdown`] restored everything.
    fn shutdown(&mut self, _engine: &mut Engine) {}
}

/// The engine thread is gone (shut down or crashed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineGone;

impl std::fmt::Display for EngineGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the engine is not running")
    }
}

impl std::error::Error for EngineGone {}

/// Cloneable handle used by D-Bus handlers, adapters and signal handlers.
pub struct EngineHandle<X> {
    tx: mpsc::Sender<Command<X>>,
    waker: Arc<OwnedFd>,
    published: Arc<RwLock<Published>>,
    alive: Arc<AtomicI64>,
    recent: Arc<Mutex<VecDeque<(Audience, String)>>>,
}

impl<X> Clone for EngineHandle<X> {
    fn clone(&self) -> Self {
        EngineHandle {
            tx: self.tx.clone(),
            waker: self.waker.clone(),
            published: self.published.clone(),
            alive: self.alive.clone(),
            recent: self.recent.clone(),
        }
    }
}

impl<X: Send + 'static> EngineHandle<X> {
    /// Queue a command and wake the loop.
    pub fn send(&self, cmd: Command<X>) -> Result<(), EngineGone> {
        self.tx.send(cmd).map_err(|_| EngineGone)?;
        self.wake();
        Ok(())
    }

    /// Wake the loop without a command (e.g. after pushing into the host queues).
    pub fn wake(&self) {
        let _ = rustix::io::write(&*self.waker, &1u64.to_ne_bytes());
    }

    pub fn push_event(&self, e: Event) -> Result<(), EngineGone> {
        self.send(Command::PushEvent(e))
    }

    pub fn ext(&self, x: X) -> Result<(), EngineGone> {
        self.send(Command::Ext(x))
    }

    /// Send a command carrying a reply channel and await the reply.
    pub async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Command<X>) -> Result<T, EngineGone> {
        let (tx, rx) = oneshot::channel();
        self.send(make(tx))?;
        rx.await.map_err(|_| EngineGone)
    }

    pub async fn status(&self) -> Result<String, EngineGone> {
        self.call(|reply| Command::Status { reply }).await
    }

    pub async fn explain(&self, what: String) -> Result<Result<String, String>, EngineGone> {
        self.call(|reply| Command::Explain { what, reply }).await
    }

    pub async fn set_class(
        &self,
        pid: i32,
        class: QosClass,
        source: String,
    ) -> Result<Result<Target, String>, EngineGone> {
        self.call(|reply| Command::SetClass { pid, class, source, reply }).await
    }

    pub async fn reload(&self) -> Result<Result<String, String>, EngineGone> {
        self.call(|reply| Command::Reload { reply }).await
    }

    pub fn shutdown(&self) {
        let _ = self.send(Command::Shutdown);
    }

    /// A copy of the latest published state.
    pub fn published(&self) -> Published {
        self.published.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The last `n` events (oldest first) as a JSON array (`RecentEvents`).
    pub fn recent_events(&self, n: usize) -> String {
        self.recent_events_for(n, None)
    }

    /// The last `n` events `viewer` may see (all when `None`), oldest first, as a JSON array.
    pub fn recent_events_for(&self, n: usize, viewer: Option<u32>) -> String {
        let ring = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<&str> = ring
            .iter()
            .rev()
            .filter(|(a, _)| viewer.is_none_or(|v| visible_to(*a, v)))
            .take(n)
            .map(|(_, s)| s.as_str())
            .collect();
        out.reverse();
        format!("[{}]", out.join(","))
    }

    /// The host's hardware profile.
    pub async fn hardware(&self) -> Result<Hardware, EngineGone> {
        self.call(|reply| Command::Hardware { reply }).await
    }

    /// Known profiles.
    pub async fn profiles(&self) -> Result<ProfilesView, EngineGone> {
        self.call(|reply| Command::Profiles { reply }).await
    }

    /// Wall-clock ms of the loop's latest iteration (for the systemd watchdog).
    pub fn last_alive(&self) -> TimestampMs {
        self.alive.load(Ordering::Relaxed)
    }
}

/// The receiving half, moved into the thread by [`spawn`].
pub struct Inbox<X> {
    rx: mpsc::Receiver<Command<X>>,
    waker: Arc<OwnedFd>,
    published: Arc<RwLock<Published>>,
    alive: Arc<AtomicI64>,
    recent: Arc<Mutex<VecDeque<(Audience, String)>>>,
}

/// A command queue with its wake-up eventfd. Create it first when something (adapters)
/// needs the handle before the engine thread starts.
pub fn channel<X: Send + 'static>() -> std::io::Result<(EngineHandle<X>, Inbox<X>)> {
    let fd = rustix::event::eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;
    let waker = Arc::new(fd);
    let (tx, rx) = mpsc::channel();
    let published = Arc::new(RwLock::new(Published::default()));
    let alive = Arc::new(AtomicI64::new(now_ms()));
    let recent = Arc::new(Mutex::new(VecDeque::with_capacity(EVENT_RING)));
    Ok((
        EngineHandle {
            tx,
            waker: waker.clone(),
            published: published.clone(),
            alive: alive.clone(),
            recent: recent.clone(),
        },
        Inbox { rx, waker, published, alive, recent },
    ))
}

/// A running engine thread.
pub struct EngineThread {
    pub notifications: tmpsc::UnboundedReceiver<Notification>,
    join: JoinHandle<()>,
}

impl EngineThread {
    /// Wait for the thread to finish (after [`Command::Shutdown`]).
    pub fn join(self) {
        if self.join.join().is_err() {
            tracing::error!("the engine thread panicked");
        }
    }

    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }

    /// Split into the notification receiver and a joiner.
    pub fn split(self) -> (tmpsc::UnboundedReceiver<Notification>, JoinHandle<()>) {
        (self.notifications, self.join)
    }
}

/// Start the engine thread. Blocks until the first step completed, so the daemon can
/// announce readiness (sd_notify, bus name) afterwards.
pub fn spawn<H: Hooks>(engine: Engine, hooks: H, inbox: Inbox<H::Ext>) -> std::io::Result<EngineThread> {
    let (ntx, nrx) = tmpsc::unbounded_channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let join = std::thread::Builder::new().name("lindesk-engine".into()).spawn(move || {
        let mut l = Loop {
            engine,
            hooks,
            rx: inbox.rx,
            waker: inbox.waker,
            published: inbox.published,
            alive: inbox.alive,
            recent: inbox.recent,
            notify: ntx,
            last_level: None,
            steps: 0,
            seq: 0,
            started: now_ms(),
        };
        l.run(ready_tx);
    })?;
    // A closed channel means the thread died before its first step; the join reports it.
    let _ = ready_rx.recv();
    Ok(EngineThread { notifications: nrx, join })
}

struct Loop<H: Hooks> {
    engine: Engine,
    hooks: H,
    rx: mpsc::Receiver<Command<H::Ext>>,
    waker: Arc<OwnedFd>,
    published: Arc<RwLock<Published>>,
    alive: Arc<AtomicI64>,
    recent: Arc<Mutex<VecDeque<(Audience, String)>>>,
    notify: tmpsc::UnboundedSender<Notification>,
    last_level: Option<Level>,
    steps: u64,
    /// Sequence number of the last streamed event.
    seq: u64,
    started: TimestampMs,
}

impl<H: Hooks> Loop<H> {
    fn run(&mut self, ready: mpsc::Sender<()>) {
        let tick = self.engine.config().general.tick.max(MIN_STEP_SPACING);
        self.step();
        let _ = ready.send(());
        let mut last_step = Instant::now();
        let mut last_maintenance = Instant::now();
        let mut pending = false;
        loop {
            self.alive.store(now_ms(), Ordering::Relaxed);
            let since = last_step.elapsed();
            // While a step is pending but too close to the last one, only wait for commands
            // (a wake fd stuck in an error state must not make us spin).
            let (timeout, watch_fds) = if pending {
                (MIN_STEP_SPACING.saturating_sub(since), false)
            } else {
                (tick.saturating_sub(since), true)
            };
            let fds = if watch_fds { self.engine.wake_fds() } else { Vec::new() };
            let (cmd_ready, fd_fired) = self.wait(&fds, timeout);
            if cmd_ready {
                let mut buf = [0u8; 8];
                let _ = rustix::io::read(&*self.waker, &mut buf);
            }
            pending |= fd_fired || last_step.elapsed() >= tick;
            let mut cx_want = false;
            let mut stop = false;
            let mut commands = 0;
            loop {
                match self.rx.try_recv() {
                    Ok(Command::Shutdown) => {
                        stop = true;
                        break;
                    }
                    Ok(cmd) => {
                        commands += 1;
                        cx_want |= self.handle(cmd);
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        tracing::info!("every engine handle is gone; shutting down");
                        stop = true;
                        break;
                    }
                }
            }
            if stop {
                self.shutdown();
                return;
            }
            // A bare wake-up (no command) means someone fed the host queues directly
            // (adapters): step soon.
            pending |= cx_want || (cmd_ready && commands == 0);
            if last_maintenance.elapsed() >= MAINTENANCE_EVERY {
                last_maintenance = Instant::now();
                self.hooks.maintenance(&mut self.engine);
                if last_maintenance.elapsed() > Duration::from_secs(1) {
                    tracing::warn!("slow maintenance: {:?}", last_maintenance.elapsed());
                }
            }
            if pending && last_step.elapsed() >= MIN_STEP_SPACING {
                self.step();
                last_step = Instant::now();
                pending = false;
            }
        }
    }

    /// `poll(2)` on the command eventfd and the wake fds. Returns (command ready, wake fd fired).
    fn wait(&self, fds: &[RawFd], timeout: Duration) -> (bool, bool) {
        let mut pfds = Vec::with_capacity(fds.len() + 1);
        pfds.push(PollFd::new(&*self.waker, PollFlags::IN));
        for fd in fds {
            // SAFETY: the fds belong to sensors owned by the engine, which outlives this call
            // (nothing else runs on this thread while we poll).
            let b = unsafe { BorrowedFd::borrow_raw(*fd) };
            pfds.push(PollFd::from_borrowed_fd(b, PollFlags::PRI | PollFlags::IN));
        }
        let ts = Timespec::try_from(timeout).unwrap_or(Timespec { tv_sec: 1, tv_nsec: 0 });
        match rustix::event::poll(&mut pfds, Some(&ts)) {
            Ok(0) => (false, false),
            Ok(_) => {
                let cmd = !pfds[0].revents().is_empty();
                let fired = pfds[1..].iter().any(|p| !p.revents().is_empty());
                (cmd, fired)
            }
            Err(rustix::io::Errno::INTR) => (false, false),
            Err(e) => {
                tracing::warn!("poll failed: {e}");
                std::thread::sleep(MIN_STEP_SPACING);
                (true, false)
            }
        }
    }

    /// Returns true when the command calls for a step.
    fn handle(&mut self, cmd: Command<H::Ext>) -> bool {
        match cmd {
            Command::PushEvent(e) => {
                self.engine.push_event(e);
                true
            }
            Command::Metric(m) => {
                self.engine.host_sender().metric(m);
                false
            }
            Command::SetClass { pid, class, source, reply } => {
                let r = self
                    .engine
                    .set_class_for_pid(pid, class, source)
                    .ok_or_else(|| format!("pid {pid}: no such process, or not in a unit this host manages"));
                let _ = reply.send(r);
                true
            }
            Command::Explain { what, reply } => {
                let _ = reply.send(self.explain(&what));
                false
            }
            Command::Status { reply } => {
                let _ = reply.send(self.status_json());
                false
            }
            Command::Reload { reply } => {
                let r = self.hooks.reload(&mut self.engine);
                match &r {
                    Ok(s) => tracing::info!("reloaded: {s}"),
                    Err(e) => tracing::warn!("reload failed: {e}"),
                }
                let _ = reply.send(r);
                true
            }
            Command::Hardware { reply } => {
                let _ = reply.send(self.engine.options().caps.hardware.clone());
                false
            }
            Command::Profiles { reply } => {
                let _ = reply.send(self.hooks.profiles(&self.engine));
                false
            }
            Command::Calibrated { map, path } => {
                self.hooks.calibrated(&mut self.engine, *map, &path);
                true
            }
            Command::Ext(x) => {
                let mut cx = LoopCx { published: &self.published, notify: &self.notify, want_step: false };
                self.hooks.ext(&mut self.engine, x, &mut cx);
                cx.want_step
            }
            Command::Shutdown => unreachable!("handled by the loop"),
        }
    }

    fn step(&mut self) {
        let t0 = Instant::now();
        let report = self.engine.step(now_ms());
        self.steps += 1;
        if report.timings.total_us > SLOW_STEP_US {
            tracing::warn!("slow engine step: {:?}", report.timings);
        }
        let t1 = Instant::now();
        for a in &report.actions {
            tracing::info!(target: "lindesk::action", "{a}");
        }
        for n in &report.notes {
            tracing::debug!(target: "lindesk::note", "{n}");
        }
        let level = self.engine.level();
        if self.last_level.is_some_and(|l| l != level) {
            tracing::info!("level {} -> {}", self.last_level.unwrap_or_default(), level);
            let _ = self.notify.send(Notification::LevelChanged(level));
        }
        self.last_level = Some(level);
        let mut cx = LoopCx { published: &self.published, notify: &self.notify, want_step: false };
        self.hooks.after_step(&mut self.engine, &report, &mut cx);
        self.stream_events(&report.events);
        let status = self.status_json();
        let fg = self.engine.foreground().cloned();
        let steps = self.steps;
        let mut p = self.published.write().unwrap_or_else(|e| e.into_inner());
        p.status = status;
        p.level = level;
        p.foreground = fg;
        p.steps = steps;
        p.last_step = report.now;
        drop(p);
        if t1.elapsed() > Duration::from_micros(SLOW_STEP_US) {
            tracing::warn!("slow step bookkeeping: {:?} (engine step {:?})", t1.elapsed(), t1 - t0);
        }
    }

    /// Append events to the ring and emit them in `Events` chunks, one series per audience.
    fn stream_events(&mut self, events: &[Event]) {
        if events.is_empty() {
            return;
        }
        let mut lines: Vec<(Audience, String)> = Vec::with_capacity(events.len());
        for e in events {
            self.seq += 1;
            lines.push((self.hooks.audience(e), event_json(self.seq, e)));
        }
        {
            let mut ring = self.recent.lock().unwrap_or_else(|e| e.into_inner());
            for l in &lines {
                if ring.len() == EVENT_RING {
                    ring.pop_front();
                }
                ring.push_back(l.clone());
            }
        }
        let mut by_audience: BTreeMap<Audience, Vec<&str>> = BTreeMap::new();
        for (a, l) in &lines {
            by_audience.entry(*a).or_default().push(l.as_str());
        }
        for (audience, lines) in by_audience {
            for chunk in lines.chunks(EVENTS_PER_SIGNAL) {
                let json = format!("[{}]", chunk.join(","));
                let _ = self.notify.send(Notification::Events { json, audience });
            }
        }
    }

    fn status_json(&self) -> String {
        let status = self.engine.status();
        let now = now_ms();
        let mut daemon = serde_json::Map::new();
        daemon.insert("name".into(), json!(self.engine.options().host));
        daemon.insert("version".into(), json!(env!("CARGO_PKG_VERSION")));
        daemon.insert("pid".into(), json!(std::process::id()));
        daemon.insert("uid".into(), json!(rustix::process::getuid().as_raw()));
        daemon.insert("dry_run".into(), json!(self.engine.dry_run()));
        daemon.insert("started".into(), json!(self.started));
        daemon.insert("uptime_s".into(), json!(((now - self.started).max(0) as f64) / 1000.0));
        daemon.insert("steps".into(), json!(self.steps));
        for (k, v) in self.hooks.status_extra(&self.engine) {
            daemon.insert(k, v);
        }
        let telemetry = super::setup::telemetry_status(
            &self.engine,
            &super::setup::default_telemetry_dir(self.engine.options().privilege),
        );
        let system = pressure_json(&self.engine.system_pressure());
        let foreground = self.engine.foreground().and_then(|t| {
            let (cg, samples) = self.engine.target_pressure(t)?;
            let mut o = pressure_json(&samples);
            o.insert("target".into(), json!(t.to_string()));
            o.insert("cgroup".into(), json!(cg));
            Some(Value::Object(o))
        });
        let (active, source) = self.hooks.epoch(&self.engine);
        let mode = daemon.get("mode").and_then(Value::as_str).unwrap_or("?").to_string();
        let mut text = status.to_string();
        text.push_str(&format!(
            "mode: {mode}{}, up {}s\n",
            if self.engine.dry_run() { " (dry run)" } else { "" },
            (now - self.started).max(0) / 1000
        ));
        text.push_str(&format!("pressure:   {}\n", pressure_text(&self.engine.system_pressure())));
        text.push_str(&super::setup::telemetry_text(&telemetry));
        text.push('\n');
        daemon.insert("telemetry".into(), telemetry);
        json!({
            "status": status,
            "daemon": daemon,
            "pressure": { "system": system, "foreground": foreground },
            "epoch": { "active": active, "source": source },
            "hardware": self.engine.options().caps.hardware,
            "requirements": self.hooks.requirements(),
            "text": text,
        })
        .to_string()
    }

    fn explain(&self, query: &str) -> Result<String, String> {
        let w = what::parse(query)?;
        let explanation = match &w {
            What::Pid(pid) => {
                self.engine.explain_pid(*pid).unwrap_or_else(|| self.engine.explain(&Target::Process { pid: *pid }))
            }
            What::Target(t) => self.engine.explain(t),
            What::BareUnit(name) => {
                let candidates = self.hooks.unit_candidates(&self.engine, name);
                let explained: Vec<_> = candidates.iter().map(|t| self.engine.explain(t)).collect();
                match explained.iter().position(|e| e.known) {
                    Some(i) => explained[i].clone(),
                    None => explained
                        .into_iter()
                        .next()
                        .ok_or_else(|| format!("'{name}': no unit of that name on this host"))?,
                }
            }
        };
        Ok(what::explanation_json(query, &explanation))
    }

    fn shutdown(&mut self) {
        tracing::info!("shutting down: restoring everything lindesk changed");
        let actions = self.engine.shutdown();
        for a in &actions {
            tracing::info!(target: "lindesk::action", "{a}");
        }
        self.hooks.shutdown(&mut self.engine);
        let _ = self.notify.send(Notification::Stopped);
    }
}

/// One streamed event: the `Event` JSON plus a `"seq"` number (monotonic per daemon run,
/// for de-duplicating `RecentEvents` against the live `Events` signal).
pub fn event_json(seq: u64, e: &Event) -> String {
    let mut v = serde_json::to_value(e).unwrap_or_else(|_| json!({}));
    if let Some(o) = v.as_object_mut() {
        o.insert("seq".into(), json!(seq));
    }
    v.to_string()
}

/// `{"cpu": {...}, "io": {...}, "memory": {...}}`: per resource the stall ratios over the
/// sample's interval (0..1) and the kernel's avg10 percentages.
pub fn pressure_json(samples: &[Sample]) -> serde_json::Map<String, Value> {
    let mut o = serde_json::Map::new();
    for r in Resource::ALL {
        if let Some(s) = samples.iter().find(|s| s.resource == r) {
            o.insert(
                r.as_str().into(),
                json!({
                    "some": s.some_ratio(),
                    "full": s.full_ratio(),
                    "some_avg10": s.some_avg10,
                    "full_avg10": s.full_avg10,
                    "interval_ms": s.interval_ms,
                    "ts": s.ts,
                }),
            );
        }
    }
    o
}

fn pressure_text(samples: &[Sample]) -> String {
    if samples.is_empty() {
        return "no samples".into();
    }
    Resource::ALL
        .iter()
        .filter_map(|r| samples.iter().find(|s| s.resource == *r))
        .map(|s| format!("{} {:.0}%", s.resource, s.some_ratio() * 100.0))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_plugin_api::Scope;

    #[test]
    fn events_carry_seq() {
        let e = Event::Focus { ts: 5, pid: Some(1), app_id: None, target: None, source: "dbus".into() };
        let v: Value = serde_json::from_str(&event_json(7, &e)).unwrap();
        assert_eq!((v["kind"].as_str(), v["seq"].as_u64(), v["pid"].as_i64()), (Some("focus"), Some(7), Some(1)));
    }

    #[test]
    fn audience_visibility() {
        assert!(visible_to(None, 1000));
        assert!(visible_to(Some(1000), 1000));
        assert!(!visible_to(Some(1001), 1000));
        assert!(visible_to(Some(1001), 0), "root sees everything");
    }

    #[test]
    fn pressure_shape() {
        let s = Sample {
            ts: 1,
            scope: Scope::System,
            resource: Resource::Io,
            some_avg10: 12.5,
            full_avg10: Some(3.0),
            some_delta_us: 250_000,
            full_delta_us: Some(0),
            interval_ms: 1000,
        };
        let o = pressure_json(std::slice::from_ref(&s));
        assert_eq!(o["io"]["some"].as_f64(), Some(0.25));
        assert_eq!(o["io"]["full"].as_f64(), Some(0.0));
        assert!(!o.contains_key("cpu"));
        assert_eq!(pressure_text(&[s]), "io 25%");
        assert_eq!(pressure_text(&[]), "no samples");
    }
}
