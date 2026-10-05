//! Out-of-process plugins: newline-delimited JSON-RPC 2.0 over a child's stdio.
//!
//! [`ExternalPlugin`] is a cheap, cloneable handle to one supervised plugin process. It
//! implements every plugin trait, so the registry can hand the same process out as, say, a
//! `Box<dyn Classifier>` and a `Box<dyn Policy>`, and the engine can't tell it apart from a
//! built-in.
//!
//! Per process there are three threads: a writer that owns the child's stdin, a reader that
//! parses stdout (responses go to the waiting caller by id; `event`/`log`/`metric`/`samples`
//! notifications are routed to the host queues, the adapter sink or `tracing`) and a stderr
//! forwarder. Nothing the host sends ever blocks on the pipe: lines go through a bounded
//! queue ([`STDIN_QUEUE`]) to the writer thread, so a plugin that stops reading its stdin
//! can't stall the engine. A full queue fails a request like a timeout and drops a
//! notification (counted in [`PluginStats::dropped`]). Calls block the caller until the
//! reply or the per-method timeout. Crashes, timeouts and malformed output feed the
//! [`Supervisor`]; the process is restarted lazily on the first call after backoff.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError};
use lindesk_core::clock::now_ms;
use lindesk_plugin_api::protocol::{
    InitializeResult, LogLevel, LogParams, Message, Notification, Request, Response, codes, methods,
};
use lindesk_plugin_api::{
    API_VERSION, Actuator, ActuatorCaps, Adapter, Classification, Classifier, Event, EventSink, InitContext, Knob,
    Manifest, Metric, Plugin, PluginError, PluginInfo, PluginKind, PluginResult, Policy, PolicyOutput, Privilege,
    ProcInfo, Record, Sample, Sensor, SensorInput, SensorOutput, Setting, Sink, Snapshot, Target, TimestampMs,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::discovery::DiscoveredPlugin;
use crate::supervisor::{self, FailureKind, Health, PluginStats, SharedSupervisor, Supervisor, SupervisorConfig};

/// Capacity of each host queue; a plugin flooding notifications beyond it loses the excess.
pub const QUEUE_CAPACITY: usize = 16_384;

/// How many log lines (stderr + `log` notifications) are kept per plugin.
const LOG_RING: usize = 200;

/// Lines waiting for a plugin's stdin. When a plugin stops reading, the pipe fills, the
/// writer thread blocks, and this queue fills: requests then fail as timeouts and
/// notifications (`sink.record` batches) are dropped instead of blocking the caller.
pub const STDIN_QUEUE: usize = 64;

/// Environment variables passed through to plugins; everything else is cleared.
const ENV_KEEP: [&str; 4] = ["PATH", "HOME", "LANG", "LINDESK_SDK_PATH"];
/// Additionally passed to user-privilege plugins so they can reach the desktop session.
const ENV_KEEP_USER: [&str; 5] =
    ["DBUS_SESSION_BUS_ADDRESS", "WAYLAND_DISPLAY", "DISPLAY", "SWAYSOCK", "HYPRLAND_INSTANCE_SIGNATURE"];

/// Sending half of the queues external plugins (and adapters) feed. Cheap to clone.
#[derive(Clone, Debug)]
pub struct HostSender {
    events: Sender<Event>,
    metrics: Sender<Metric>,
    samples: Sender<Sample>,
    dropped: Arc<AtomicU64>,
}

/// Receiving half, drained by the engine on every step.
#[derive(Debug)]
pub struct HostReceiver {
    pub events: Receiver<Event>,
    pub metrics: Receiver<Metric>,
    pub samples: Receiver<Sample>,
}

/// Bounded queues for plugin → host traffic.
pub fn host_queues() -> (HostSender, HostReceiver) {
    let (et, er) = crossbeam_channel::bounded(QUEUE_CAPACITY);
    let (mt, mr) = crossbeam_channel::bounded(QUEUE_CAPACITY);
    let (st, sr) = crossbeam_channel::bounded(QUEUE_CAPACITY);
    (
        HostSender { events: et, metrics: mt, samples: st, dropped: Arc::default() },
        HostReceiver { events: er, metrics: mr, samples: sr },
    )
}

impl HostSender {
    /// A sender whose receiver is gone: everything is discarded (conformance, tools).
    pub fn detached() -> Self {
        host_queues().0
    }

    fn put<T>(&self, tx: &Sender<T>, v: T) {
        if let Err(TrySendError::Full(_)) = tx.try_send(v) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Queue an event (Focus, Interaction, …) for the next engine step.
    pub fn event(&self, e: Event) {
        self.put(&self.events, e);
    }

    /// Queue a metric.
    pub fn metric(&self, m: Metric) {
        self.put(&self.metrics, m);
    }

    /// Queue a pressure sample.
    pub fn sample(&self, s: Sample) {
        self.put(&self.samples, s);
    }

    /// Items dropped because a queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// An [`EventSink`] feeding the event queue (what the engine hands to adapters).
    pub fn event_sink(&self) -> EventSink {
        let me = self.clone();
        Arc::new(move |e| me.event(e))
    }
}

/// Where a plugin log line came from.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogSource {
    /// A `log` notification.
    Plugin,
    /// A line on the plugin's stderr.
    Stderr,
    /// The host, about the plugin (crashes, restarts, protocol errors).
    Host,
}

/// One remembered log line (`lindeskctl plugin logs`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LogLine {
    pub ts: TimestampMs,
    pub level: LogLevel,
    pub source: LogSource,
    pub message: String,
}

/// How a plugin process ended on [`ExternalPlugin::shutdown`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShutdownOutcome {
    /// No process was running.
    NotRunning,
    /// Exited by itself after `shutdown` + stdin close, within the 500 ms grace period.
    Exited { code: Option<i32>, after: Duration },
    /// Needed SIGTERM.
    Terminated,
    /// Needed SIGKILL.
    Killed,
}

impl ShutdownOutcome {
    /// True when the plugin honoured the shutdown protocol.
    pub fn is_clean(&self) -> bool {
        matches!(self, ShutdownOutcome::Exited { code: Some(0), .. } | ShutdownOutcome::NotRunning)
    }
}

/// Default per-call timeout for a method when the manifest has no `[timeouts]`.
pub fn default_timeout(method: &str) -> Duration {
    let ms = match method {
        methods::INITIALIZE => 2000,
        m if m.starts_with("classifier.") => 50,
        m if m.starts_with("policy.") => 100,
        m if m.starts_with("sensor.") => 200,
        _ => 1000,
    };
    Duration::from_millis(ms)
}

/// Timeout for `method` honouring manifest `[timeouts]` (`init_ms` for initialize,
/// `call_ms` for everything else).
pub fn timeout_for(manifest: &Manifest, method: &str) -> Duration {
    let t = manifest.timeouts.as_ref();
    let ms = if method == methods::INITIALIZE { t.and_then(|t| t.init_ms) } else { t.and_then(|t| t.call_ms) };
    ms.map_or_else(|| default_timeout(method), Duration::from_millis)
}

/// Options shared by all external plugins of a host.
#[derive(Clone, Debug)]
pub struct ExternalOptions {
    pub host: HostSender,
    pub supervisor: SupervisorConfig,
    /// Multiplies every timeout (1.0 in production; tools may relax it).
    pub timeout_scale: f64,
}

impl Default for ExternalOptions {
    fn default() -> Self {
        ExternalOptions { host: HostSender::detached(), supervisor: SupervisorConfig::default(), timeout_scale: 1.0 }
    }
}

/// A supervised plugin process. Clones share the process.
#[derive(Clone)]
pub struct ExternalPlugin {
    inner: Arc<Inner>,
}

struct Inner {
    name: String,
    manifest: Manifest,
    dir: PathBuf,
    exec: PathBuf,
    ctx: InitContext,
    host: HostSender,
    timeout_scale: f64,
    sup: SharedSupervisor,
    conn: Mutex<Option<Arc<Conn>>>,
    init: Mutex<Option<InitializeResult>>,
    adapter_sink: Mutex<Option<EventSink>>,
    adapter_started: AtomicBool,
    caps: Mutex<Option<ActuatorCaps>>,
    logs: Mutex<VecDeque<LogLine>>,
    stopped: AtomicBool,
    ever_started: AtomicBool,
    malformed: AtomicU64,
}

struct Conn {
    child: Mutex<Child>,
    /// Queue to the writer thread, which owns the child's stdin. Dropping the sender closes
    /// stdin once the queued lines are written.
    stdin: Mutex<Option<Sender<String>>>,
    pending: Mutex<HashMap<u64, Sender<Response>>>,
    next_id: AtomicU64,
    alive: AtomicBool,
    /// We initiated the exit, so EOF is not a crash.
    closing: AtomicBool,
    exit: Mutex<Option<String>>,
}

fn lk<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Conn {
    /// Queue one line for the writer thread; never blocks. `WouldBlock` when the plugin
    /// isn't keeping up with its stdin.
    fn write_line(&self, line: String) -> std::io::Result<()> {
        let g = lk(&self.stdin);
        let tx = g.as_ref().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdin closed"))?;
        tx.try_send(line).map_err(|e| match e {
            TrySendError::Full(_) => std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!("the plugin is not reading its stdin ({STDIN_QUEUE} lines queued)"),
            ),
            TrySendError::Disconnected(_) => std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdin closed"),
        })
    }

    fn send(&self, v: &impl Serialize) -> std::io::Result<()> {
        let line = serde_json::to_string(v).map_err(std::io::Error::other)?;
        self.write_line(line)
    }

    fn wait_exit(&self, deadline: Instant) -> Option<ExitStatus> {
        loop {
            if let Ok(Some(st)) = lk(&self.child).try_wait() {
                return Some(st);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn signal(&self, sig: libc::c_int) {
        let pid = lk(&self.child).id() as libc::pid_t;
        // SAFETY: plain syscall on a pid we spawned and haven't reaped yet.
        unsafe {
            libc::kill(pid, sig);
        }
    }

    fn kill_now(&self) {
        self.closing.store(true, Ordering::SeqCst);
        lk(&self.stdin).take();
        let mut c = lk(&self.child);
        let _ = c.kill();
        let _ = c.wait();
    }
}

impl std::fmt::Debug for ExternalPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalPlugin")
            .field("name", &self.inner.name)
            .field("exec", &self.inner.exec)
            .field("health", &self.health())
            .finish()
    }
}

#[derive(Serialize)]
struct ClassifyRef<'a> {
    proc: &'a ProcInfo,
}
#[derive(Serialize)]
struct EvaluateRef<'a> {
    snapshot: &'a Snapshot,
}
#[derive(Serialize)]
struct ReadRef<'a> {
    target: &'a Target,
    knob: &'a Knob,
}
#[derive(Serialize)]
struct ApplyRef<'a> {
    target: &'a Target,
    setting: &'a Setting,
}
#[derive(Serialize)]
struct ResetRef<'a> {
    target: &'a Target,
    knob: &'a Knob,
    original: Option<&'a Setting>,
}
#[derive(Serialize)]
struct RecordRef<'a> {
    records: &'a [Record],
}

impl ExternalPlugin {
    /// Prepare a plugin without starting it; the first call (or [`ExternalPlugin::start`])
    /// spawns the process. `dir` is the working directory; `manifest.plugin.exec` is
    /// resolved against it.
    pub fn new(manifest: Manifest, dir: impl Into<PathBuf>, ctx: InitContext, opts: ExternalOptions) -> Self {
        // The process runs inside `dir`: a relative program path would be resolved against it
        // a second time (`steam-games/steam-games/steam_games.py`), so make `dir` absolute.
        let dir = dir.into();
        let dir = if dir.is_absolute() { dir } else { std::path::absolute(&dir).unwrap_or(dir) };
        let exec = {
            let e = Path::new(&manifest.plugin.exec);
            if e.is_absolute() { e.to_path_buf() } else { dir.join(e) }
        };
        ExternalPlugin {
            inner: Arc::new(Inner {
                name: manifest.plugin.name.clone(),
                manifest,
                dir,
                exec,
                ctx,
                host: opts.host,
                timeout_scale: opts.timeout_scale.max(0.01),
                sup: Supervisor::shared(opts.supervisor, true),
                conn: Mutex::new(None),
                init: Mutex::new(None),
                adapter_sink: Mutex::new(None),
                adapter_started: AtomicBool::new(false),
                caps: Mutex::new(None),
                logs: Mutex::new(VecDeque::new()),
                stopped: AtomicBool::new(false),
                ever_started: AtomicBool::new(false),
                malformed: AtomicU64::new(0),
            }),
        }
    }

    /// [`ExternalPlugin::new`] + [`ExternalPlugin::start`]: spawn and complete the handshake.
    pub fn spawn(
        manifest: Manifest,
        dir: impl Into<PathBuf>,
        ctx: InitContext,
        opts: ExternalOptions,
    ) -> PluginResult<Self> {
        let p = ExternalPlugin::new(manifest, dir, ctx, opts);
        p.start()?;
        Ok(p)
    }

    /// Build from a discovery result; `ctx.config` is replaced by the plugin's merged config.
    pub fn from_discovered(d: &DiscoveredPlugin, mut ctx: InitContext, opts: ExternalOptions) -> Self {
        ctx.config = d.config.clone();
        let mut manifest = d.manifest.clone();
        manifest.plugin.exec = d.exec.to_string_lossy().into_owned();
        ExternalPlugin::new(manifest, d.dir.clone(), ctx, opts)
    }

    /// Ensure the process is running and initialised.
    pub fn start(&self) -> PluginResult<()> {
        self.ensure_running().map(|_| ())
    }

    /// Plugin name from the manifest.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn manifest(&self) -> &Manifest {
        &self.inner.manifest
    }

    /// Kinds this plugin serves, from the manifest.
    pub fn kinds(&self) -> &[PluginKind] {
        &self.inner.manifest.plugin.kinds
    }

    /// Working directory of the plugin process.
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// The handshake result of the current (or last) process.
    pub fn init_result(&self) -> Option<InitializeResult> {
        lk(&self.inner.init).clone()
    }

    /// The shared supervisor (health + stats).
    pub fn supervisor(&self) -> SharedSupervisor {
        self.inner.sup.clone()
    }

    pub fn health(&self) -> Health {
        supervisor::lock(&self.inner.sup).health().clone()
    }

    pub fn stats(&self) -> PluginStats {
        supervisor::lock(&self.inner.sup).stats()
    }

    /// True when a call right now would be attempted (not disabled or backing off).
    pub fn is_available(&self) -> bool {
        !self.inner.stopped.load(Ordering::SeqCst) && supervisor::lock(&self.inner.sup).is_available(Instant::now())
    }

    /// Pid of the running process.
    pub fn pid(&self) -> Option<u32> {
        let c = lk(&self.inner.conn).clone()?;
        c.alive.load(Ordering::SeqCst).then(|| lk(&c.child).id())
    }

    /// Recent stderr lines, `log` notifications and host messages about this plugin.
    pub fn recent_logs(&self) -> Vec<LogLine> {
        lk(&self.inner.logs).iter().cloned().collect()
    }

    /// Lines on stdout that were not valid JSON-RPC.
    pub fn malformed_lines(&self) -> u64 {
        self.inner.malformed.load(Ordering::Relaxed)
    }

    /// Disable the plugin (operator action); it is stopped.
    pub fn disable(&self, reason: &str) {
        supervisor::lock(&self.inner.sup).disable(reason);
        if let Some(c) = lk(&self.inner.conn).take() {
            graceful_stop(&c, self.inner.timeout_scale);
        }
    }

    /// Clear failures and allow restarts again (e.g. `lindeskctl plugin enable`).
    pub fn reset(&self) {
        self.inner.stopped.store(false, Ordering::SeqCst);
        supervisor::lock(&self.inner.sup).reset();
    }

    /// Timeout for `method`, including the configured scale.
    pub fn timeout(&self, method: &str) -> Duration {
        timeout_for(&self.inner.manifest, method).mul_f64(self.inner.timeout_scale)
    }

    fn ensure_running(&self) -> PluginResult<Arc<Conn>> {
        let inner = &self.inner;
        if inner.stopped.load(Ordering::SeqCst) {
            return Err(PluginError::Failed(format!("plugin {} has been shut down", inner.name)));
        }
        {
            let s = supervisor::lock(&inner.sup);
            match s.health() {
                Health::Disabled { reason } => {
                    return Err(PluginError::Unavailable(format!("plugin {} is disabled: {reason}", inner.name)));
                }
                _ => {
                    if let Some(left) = s.backoff_remaining(Instant::now()) {
                        return Err(PluginError::Unavailable(format!(
                            "plugin {} is backing off for another {}ms",
                            inner.name,
                            left.as_millis()
                        )));
                    }
                }
            }
        }
        let mut slot = lk(&inner.conn);
        if let Some(c) = slot.as_ref()
            && c.alive.load(Ordering::SeqCst)
        {
            return Ok(c.clone());
        }
        if let Some(old) = slot.take() {
            old.kill_now();
        }
        let restart = inner.ever_started.swap(true, Ordering::SeqCst);
        supervisor::lock(&inner.sup).on_starting(restart);
        if restart {
            self.host_log(LogLevel::Info, "restarting".into());
        }
        match self.start_process() {
            Ok(c) => {
                *slot = Some(c.clone());
                Ok(c)
            }
            Err((e, already_recorded)) => {
                let msg = format!("start failed: {e}");
                self.host_log(LogLevel::Error, msg.clone());
                if !already_recorded {
                    let kind = supervisor::is_failure(&e).unwrap_or(FailureKind::Protocol);
                    supervisor::lock(&inner.sup).on_failure(kind, &msg, Instant::now());
                }
                Err(e)
            }
        }
    }

    fn env(&self) -> Vec<(OsString, OsString)> {
        let user = self.inner.manifest.plugin.privilege == Privilege::User;
        let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(k, _)| {
                let k = k.to_string_lossy();
                ENV_KEEP.contains(&k.as_ref()) || k.starts_with("XDG_") || (user && ENV_KEEP_USER.contains(&k.as_ref()))
            })
            .collect();
        if !env.iter().any(|(k, _)| k == "PATH") {
            env.push(("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()));
        }
        env.push(("LINDESK_API_VERSION".into(), API_VERSION.to_string().into()));
        env
    }

    /// Spawn and handshake. On error, the flag says whether the reader thread already
    /// recorded the failure (the process died on its own).
    fn start_process(&self) -> Result<Arc<Conn>, (PluginError, bool)> {
        let inner = &self.inner;
        let mut child = Command::new(&inner.exec)
            .args(&inner.manifest.plugin.args)
            .current_dir(&inner.dir)
            .env_clear()
            .envs(self.env())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| (PluginError::Io(format!("spawning {}: {e}", inner.exec.display())), false))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (stdin_tx, stdin_rx) = crossbeam_channel::bounded::<String>(STDIN_QUEUE);
        let conn = Arc::new(Conn {
            child: Mutex::new(child),
            stdin: Mutex::new(Some(stdin_tx)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            closing: AtomicBool::new(false),
            exit: Mutex::new(None),
        });
        let weak = Arc::downgrade(inner);
        let thread_name = format!("plugin-{}", inner.name);
        std::thread::Builder::new()
            .name(format!("{thread_name}-in"))
            .spawn(move || writer_loop(stdin_rx, stdin))
            .map_err(|e| (PluginError::from(e), false))?;
        {
            let (weak, conn) = (weak.clone(), conn.clone());
            std::thread::Builder::new()
                .name(format!("{thread_name}-out"))
                .spawn(move || reader_loop(weak, conn, stdout))
                .map_err(|e| (PluginError::from(e), false))?;
        }
        std::thread::Builder::new()
            .name(format!("{thread_name}-err"))
            .spawn(move || stderr_loop(weak, stderr))
            .map_err(|e| (PluginError::from(e), false))?;

        let handshake = (|| {
            let resp = self.transact(&conn, methods::INITIALIZE, &inner.ctx, self.timeout(methods::INITIALIZE))?;
            let res: InitializeResult = resp.into_result()?;
            if res.api_version != API_VERSION {
                return Err(PluginError::Protocol(format!(
                    "plugin speaks API v{}, host speaks v{API_VERSION}",
                    res.api_version
                )));
            }
            let missing: Vec<String> =
                inner.manifest.plugin.kinds.iter().filter(|k| !res.kinds.contains(k)).map(|k| k.to_string()).collect();
            if !missing.is_empty() {
                return Err(PluginError::Protocol(format!(
                    "plugin.toml declares kinds [{}] that initialize did not report",
                    missing.join(", ")
                )));
            }
            if res.name != inner.name {
                self.host_log(
                    LogLevel::Warn,
                    format!("initialize reported name '{}', plugin.toml says '{}'", res.name, inner.name),
                );
            }
            Ok(res)
        })();
        let res = match handshake {
            Ok(r) => r,
            Err(e) => {
                let already_recorded = lk(&conn.exit).is_some();
                conn.kill_now();
                let e = match e {
                    PluginError::Failed(m) | PluginError::Unsupported(m) => {
                        PluginError::Protocol(format!("initialize refused: {m}"))
                    }
                    e => e,
                };
                return Err((e, already_recorded));
            }
        };
        if let Some(c) = &res.actuator {
            *lk(&inner.caps) = Some(c.clone());
        }
        *lk(&inner.init) = Some(res);
        supervisor::lock(&inner.sup).on_started();
        if inner.adapter_started.load(Ordering::SeqCst) {
            let r = self
                .transact(&conn, methods::ADAPTER_START, Value::Null, self.timeout(methods::ADAPTER_START))
                .and_then(|r| r.into_result::<Value>());
            if let Err(e) = r {
                self.host_log(LogLevel::Warn, format!("adapter.start after restart failed: {e}"));
            }
        }
        Ok(conn)
    }

    /// Send a request on `conn` and wait for its response. Records nothing.
    fn transact(&self, conn: &Conn, method: &str, params: impl Serialize, timeout: Duration) -> PluginResult<Response> {
        let id = conn.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = crossbeam_channel::bounded(1);
        lk(&conn.pending).insert(id, tx);
        if !conn.alive.load(Ordering::SeqCst) {
            lk(&conn.pending).remove(&id);
            return Err(PluginError::Io(self.exit_message(conn)));
        }
        if let Err(e) = conn.send(&Request::new(id, method, params)) {
            lk(&conn.pending).remove(&id);
            if e.kind() == std::io::ErrorKind::WouldBlock {
                // Not reading its stdin: as unresponsive as a timeout, and supervised as one.
                return Err(PluginError::Timeout);
            }
            return Err(PluginError::Io(format!("writing to plugin: {e}")));
        }
        match rx.recv_timeout(timeout) {
            Ok(r) => Ok(r),
            Err(RecvTimeoutError::Timeout) => {
                lk(&conn.pending).remove(&id);
                Err(PluginError::Timeout)
            }
            Err(RecvTimeoutError::Disconnected) => Err(PluginError::Io(self.exit_message(conn))),
        }
    }

    fn exit_message(&self, conn: &Conn) -> String {
        lk(&conn.exit).clone().unwrap_or_else(|| "plugin exited".into())
    }

    /// Send a request and return the raw response (no statistics, no error mapping).
    /// Used by conformance tests to check error codes.
    pub fn call_raw(&self, method: &str, params: impl Serialize, timeout: Duration) -> PluginResult<Response> {
        let conn = self.ensure_running()?;
        self.transact(&conn, method, params, timeout)
    }

    /// Typed request with the method's timeout, supervised: timeouts and bad replies count
    /// as failures; crashes are counted once by the reader thread.
    pub fn call<T: DeserializeOwned>(&self, method: &str, params: impl Serialize) -> PluginResult<T> {
        let conn = self.ensure_running()?;
        let started = Instant::now();
        let raw = self.transact(&conn, method, params, self.timeout(method));
        let latency = started.elapsed();
        let mut s = supervisor::lock(&self.inner.sup);
        match raw {
            Ok(resp) => {
                let r = resp.into_result::<T>();
                s.on_call(latency, r.as_ref().map(|_| ()));
                drop(s);
                if let Err(e @ PluginError::Protocol(_)) = &r {
                    self.host_log(LogLevel::Error, format!("{method}: {e}"));
                }
                r
            }
            Err(e @ PluginError::Timeout) => {
                s.on_call(latency, Err(&e));
                drop(s);
                self.host_log(LogLevel::Warn, format!("{method} timed out after {}ms", latency.as_millis()));
                Err(e)
            }
            Err(e) => {
                // Write errors and disconnects: the reader thread records the crash itself.
                s.on_call(latency, Err(&PluginError::Failed(e.to_string())));
                Err(e)
            }
        }
    }

    /// Send a notification (e.g. `sink.record`). Starts the process if needed. Never
    /// blocks on the plugin: when it isn't reading its stdin the notification is dropped,
    /// counted in [`PluginStats::dropped`] and logged (the first time, then every 100th).
    pub fn notify(&self, method: &str, params: impl Serialize) -> PluginResult<()> {
        let conn = self.ensure_running()?;
        match conn.send(&Notification::new(method, params)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let n = supervisor::lock(&self.inner.sup).on_dropped();
                if n == 1 || n.is_multiple_of(100) {
                    self.host_log(LogLevel::Warn, format!("{method} dropped ({n} so far): {e}"));
                }
                Ok(())
            }
            Err(e) => Err(PluginError::Io(format!("writing to plugin: {e}"))),
        }
    }

    /// Stop the process: `shutdown` notification and stdin close, then SIGTERM after
    /// 500 ms and SIGKILL after 2 s. The plugin is not restarted afterwards.
    pub fn shutdown(&self) -> ShutdownOutcome {
        self.inner.stopped.store(true, Ordering::SeqCst);
        self.inner.adapter_started.store(false, Ordering::SeqCst);
        match lk(&self.inner.conn).take() {
            Some(c) => graceful_stop(&c, self.inner.timeout_scale),
            None => ShutdownOutcome::NotRunning,
        }
    }

    fn host_log(&self, level: LogLevel, message: String) {
        self.inner.log(LogSource::Host, level, message);
    }
}

/// Owns the child's stdin: writes queued lines until the queue's senders are gone (then
/// closes stdin, which the plugin sees as EOF) or the pipe breaks.
fn writer_loop(rx: Receiver<String>, stdin: ChildStdin) {
    let mut w = std::io::BufWriter::new(stdin);
    while let Ok(line) = rx.recv() {
        if w.write_all(line.as_bytes()).and_then(|()| w.write_all(b"\n")).is_err() {
            return;
        }
        // Batch what is already queued, then flush so the plugin sees every line promptly.
        if rx.is_empty() && w.flush().is_err() {
            return;
        }
    }
    let _ = w.flush();
}

fn graceful_stop(c: &Conn, scale: f64) -> ShutdownOutcome {
    c.closing.store(true, Ordering::SeqCst);
    let start = Instant::now();
    if c.alive.load(Ordering::SeqCst) {
        let _ = c.send(&Notification::new(methods::SHUTDOWN, Value::Null));
    }
    lk(&c.stdin).take();
    if let Some(st) = c.wait_exit(start + Duration::from_millis(500).mul_f64(scale.max(1.0))) {
        return ShutdownOutcome::Exited { code: st.code(), after: start.elapsed() };
    }
    c.signal(libc::SIGTERM);
    if c.wait_exit(start + Duration::from_secs(2).mul_f64(scale.max(1.0))).is_some() {
        return ShutdownOutcome::Terminated;
    }
    c.kill_now();
    ShutdownOutcome::Killed
}

impl Inner {
    fn log(&self, source: LogSource, level: LogLevel, message: String) {
        let plugin = self.name.as_str();
        match level {
            LogLevel::Trace => tracing::trace!(plugin, ?source, "{message}"),
            LogLevel::Debug => tracing::debug!(plugin, ?source, "{message}"),
            LogLevel::Info => tracing::info!(plugin, ?source, "{message}"),
            LogLevel::Warn => tracing::warn!(plugin, ?source, "{message}"),
            LogLevel::Error => tracing::error!(plugin, ?source, "{message}"),
        }
        let mut logs = lk(&self.logs);
        if logs.len() == LOG_RING {
            logs.pop_front();
        }
        logs.push_back(LogLine { ts: now_ms(), level, source, message });
    }

    fn last_stderr(&self) -> Option<String> {
        lk(&self.logs).iter().rev().find(|l| l.source == LogSource::Stderr).map(|l| l.message.clone())
    }

    fn protocol_error(&self, msg: String) {
        self.malformed.fetch_add(1, Ordering::Relaxed);
        self.log(LogSource::Host, LogLevel::Error, msg.clone());
        supervisor::lock(&self.sup).on_failure(FailureKind::Protocol, &msg, Instant::now());
    }

    fn route(&self, n: Notification) {
        match n.method.as_str() {
            methods::EVENT => match serde_json::from_value::<Event>(n.params) {
                Ok(e) => match lk(&self.adapter_sink).clone() {
                    Some(sink) => sink(e),
                    None => self.host.event(e),
                },
                Err(e) => self.protocol_error(format!("invalid event notification: {e}")),
            },
            methods::LOG => match serde_json::from_value::<LogParams>(n.params) {
                Ok(l) => self.log(LogSource::Plugin, l.level, l.message),
                Err(e) => self.protocol_error(format!("invalid log notification: {e}")),
            },
            methods::METRIC => match serde_json::from_value::<Metric>(n.params) {
                Ok(mut m) => {
                    m.plugin = self.name.clone();
                    self.host.metric(m);
                }
                Err(e) => self.protocol_error(format!("invalid metric notification: {e}")),
            },
            methods::SAMPLES => {
                #[derive(Deserialize)]
                #[serde(untagged)]
                enum Samples {
                    Wrapped { samples: Vec<Sample> },
                    Bare(Vec<Sample>),
                }
                match serde_json::from_value::<Samples>(n.params) {
                    Ok(Samples::Wrapped { samples } | Samples::Bare(samples)) => {
                        samples.into_iter().for_each(|s| self.host.sample(s))
                    }
                    Err(e) => self.protocol_error(format!("invalid samples notification: {e}")),
                }
            }
            other => self.log(LogSource::Host, LogLevel::Debug, format!("ignoring unknown notification '{other}'")),
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(c) = lk(&self.conn).take() {
            graceful_stop(&c, self.timeout_scale);
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn reader_loop(weak: Weak<Inner>, conn: Arc<Conn>, stdout: impl Read) {
    let reader = BufReader::new(stdout);
    for line in reader.split(b'\n') {
        let Ok(bytes) = line else { break };
        let text = String::from_utf8_lossy(&bytes);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let Some(inner) = weak.upgrade() else { break };
        match Message::parse(text) {
            Ok(Message::Response(r)) => {
                let tx = lk(&conn.pending).remove(&r.id);
                match tx {
                    Some(tx) => {
                        let _ = tx.send(r);
                    }
                    None => {
                        inner.log(LogSource::Host, LogLevel::Debug, format!("late or unknown response id {}", r.id))
                    }
                }
            }
            Ok(Message::Notification(n)) => inner.route(n),
            Ok(Message::Request(r)) => {
                let _ = conn.send(&Response::err(r.id, codes::METHOD_NOT_FOUND, "the host does not serve requests"));
            }
            Err(e) => inner.protocol_error(format!("malformed output on stdout ({e}): {}", truncate(text, 200))),
        }
    }
    conn.alive.store(false, Ordering::SeqCst);
    if let Some(inner) = weak.upgrade()
        && !conn.closing.load(Ordering::SeqCst)
    {
        let status = conn.wait_exit(Instant::now() + Duration::from_millis(200));
        // Give the stderr thread a moment to catch the last words.
        std::thread::sleep(Duration::from_millis(10));
        let mut msg = match status {
            Some(st) => format!("plugin exited unexpectedly ({st})"),
            None => "plugin closed stdout unexpectedly".to_string(),
        };
        if let Some(last) = inner.last_stderr() {
            msg.push_str(&format!("; last stderr: {}", truncate(&last, 300)));
        }
        *lk(&conn.exit) = Some(msg.clone());
        inner.log(LogSource::Host, LogLevel::Error, msg.clone());
        supervisor::lock(&inner.sup).on_failure(FailureKind::Crash, &msg, Instant::now());
    }
    // Wake every waiting caller with `Disconnected`.
    lk(&conn.pending).clear();
}

fn stderr_loop(weak: Weak<Inner>, stderr: impl Read) {
    let reader = BufReader::new(stderr);
    for line in reader.split(b'\n') {
        let Ok(bytes) = line else { break };
        let text = String::from_utf8_lossy(&bytes).trim_end().to_string();
        if text.is_empty() {
            continue;
        }
        let Some(inner) = weak.upgrade() else { break };
        inner.log(LogSource::Stderr, LogLevel::Info, text);
    }
}

// ---------------------------------------------------------------------------------------
// Trait proxies: the engine treats external plugins exactly like built-ins.

impl Plugin for ExternalPlugin {
    fn info(&self) -> PluginInfo {
        let p = &self.inner.manifest.plugin;
        PluginInfo {
            name: p.name.clone(),
            version: p.version.clone(),
            kinds: p.kinds.clone(),
            description: p.description.clone(),
        }
    }

    /// External plugins are initialised by the handshake when the process starts.
    fn init(&mut self, _ctx: &InitContext) -> PluginResult<()> {
        Ok(())
    }

    fn shutdown(&mut self) {
        ExternalPlugin::shutdown(self);
    }
}

impl Sensor for ExternalPlugin {
    fn poll(&mut self, input: &SensorInput) -> PluginResult<SensorOutput> {
        self.call(methods::SENSOR_POLL, input)
    }
}

impl Adapter for ExternalPlugin {
    fn start(&mut self, sink: EventSink) -> PluginResult<()> {
        *lk(&self.inner.adapter_sink) = Some(sink);
        self.inner.adapter_started.store(true, Ordering::SeqCst);
        self.call::<Value>(methods::ADAPTER_START, Value::Null).map(|_| ())
    }

    fn stop(&mut self) {
        self.inner.adapter_started.store(false, Ordering::SeqCst);
        lk(&self.inner.adapter_sink).take();
    }

    fn connected(&self) -> bool {
        self.inner.adapter_started.load(Ordering::SeqCst) && self.health() == Health::Running
    }
}

impl Classifier for ExternalPlugin {
    fn classify(&mut self, proc_info: &ProcInfo) -> PluginResult<Option<Classification>> {
        self.call(methods::CLASSIFY, ClassifyRef { proc: proc_info })
    }
}

impl Policy for ExternalPlugin {
    fn evaluate(&mut self, snapshot: &Snapshot) -> PluginResult<PolicyOutput> {
        self.call(methods::EVALUATE, EvaluateRef { snapshot })
    }
}

impl Actuator for ExternalPlugin {
    fn caps(&self) -> ActuatorCaps {
        if let Some(c) = lk(&self.inner.caps).clone() {
            return c;
        }
        match self.call::<ActuatorCaps>(methods::ACTUATOR_CAPS, Value::Null) {
            Ok(c) => {
                *lk(&self.inner.caps) = Some(c.clone());
                c
            }
            Err(_) => ActuatorCaps::default(),
        }
    }

    fn read(&mut self, target: &Target, knob: &Knob) -> PluginResult<Option<Setting>> {
        self.call(methods::ACTUATOR_READ, ReadRef { target, knob })
    }

    fn apply(&mut self, target: &Target, setting: &Setting) -> PluginResult<()> {
        self.call::<Value>(methods::ACTUATOR_APPLY, ApplyRef { target, setting }).map(|_| ())
    }

    fn reset(&mut self, target: &Target, knob: &Knob, original: Option<&Setting>) -> PluginResult<()> {
        self.call::<Value>(methods::ACTUATOR_RESET, ResetRef { target, knob, original }).map(|_| ())
    }
}

impl Sink for ExternalPlugin {
    fn tables(&self) -> Vec<String> {
        match lk(&self.inner.init).as_ref() {
            Some(r) if !r.tables.is_empty() => r.tables.clone(),
            _ => self.inner.manifest.sink.as_ref().map(|s| s.tables.clone()).unwrap_or_default(),
        }
    }

    /// One `sink.record` notification per batch (no reply, so a slow sink never blocks a step).
    fn record(&mut self, records: &[Record]) -> PluginResult<()> {
        if records.is_empty() {
            return Ok(());
        }
        self.notify(methods::SINK_RECORD, RecordRef { records })
    }

    fn flush(&mut self) -> PluginResult<()> {
        self.call::<Value>(methods::SINK_FLUSH, Value::Null).map(|_| ())
    }
}
