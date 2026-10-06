//! Input activity on any Wayland compositor (plugin `"wayland-input"`).
//!
//! Wayland shows a client no input but its own, and most compositors' IPC says nothing about
//! it either. `ext-idle-notify-v1` does, indirectly: with a short timeout, `idled` means "no
//! input for that long" and `resumed` means "input again". The adapter turns that into
//! `Event::Interaction { source: "wayland-input" }`: one when input resumes, then one per
//! [`INTERACTION_THROTTLE`] for as long as the compositor has not reported idle. Interactions
//! carry no pid; the engine applies them to whatever has focus, so this adapter runs next to
//! one that reports focus ([`hyprland`](super::hyprland), [`cosmic`](super::cosmic)).
//!
//! Version 2 of the protocol offers `get_input_idle_notification`, which ignores idle
//! inhibitors. On a version 1 compositor a playing video (which inhibits idle) would look
//! like endless input, so there the run of interactions after each `resumed` is capped at
//! [`UNCONFIRMED_RUN`].
//!
//! Hyprland, COSMIC, sway and other wlroots compositors, niri and KWin implement the
//! protocol. Where it is missing, [`WlInputAdapter::start`] returns
//! [`PluginError::Unsupported`] so the host skips the adapter.
//!
//! Config (`InitContext::config`):
//! ```toml
//! display = "wayland-1"   # socket name in $XDG_RUNTIME_DIR, or an absolute path; default: $WAYLAND_DISPLAY
//! ```

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use lindesk_core::clock::now_ms;
use lindesk_plugin_api::{
    Adapter, Event, EventSink, InitContext, Plugin, PluginError, PluginInfo, PluginKind, PluginResult,
};
use serde::Deserialize;
use wayland_client::backend::WaylandError;
use wayland_client::globals::{BindError, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::{ext_idle_notification_v1, ext_idle_notifier_v1};

use super::worker::{Backoff, Control, Wake, Worker};
use super::{INTERACTION_THROTTLE, Throttle, WAYLAND_INPUT};

const SOURCE: &str = "wayland-input";
/// Longest run of interactions after one `resumed` when the compositor's idle state may be
/// held by an inhibitor rather than by input (protocol version 1).
pub const UNCONFIRMED_RUN: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------------------------
// Pure state: idle/resumed in, "report an interaction now" out.
// ---------------------------------------------------------------------------------------------

/// Whether the person is giving input, as far as the compositor tells.
#[derive(Clone, Debug)]
pub struct Activity {
    active: bool,
    /// Idle means "no input", whatever idle inhibitors say (protocol version 2).
    input_only: bool,
    resumed_at: Instant,
    throttle: Throttle,
}

impl Activity {
    /// A notification object starts out not idle: the compositor reports `idled` first.
    pub fn new(input_only: bool, now: Instant) -> Self {
        Activity { active: true, input_only, resumed_at: now, throttle: Throttle::new(INTERACTION_THROTTLE) }
    }

    /// `ext_idle_notification_v1.idled`
    pub fn idled(&mut self) {
        self.active = false;
    }

    /// `ext_idle_notification_v1.resumed`. True when an interaction should be reported.
    pub fn resumed(&mut self, now: Instant) -> bool {
        self.active = true;
        self.resumed_at = now;
        self.throttle.ready(now)
    }

    /// True while interactions are still being reported without further events.
    pub fn running(&self, now: Instant) -> bool {
        self.active && (self.input_only || now.saturating_duration_since(self.resumed_at) < UNCONFIRMED_RUN)
    }

    /// Call when [`Activity::wait`] elapsed. True when an interaction should be reported.
    pub fn tick(&mut self, now: Instant) -> bool {
        self.running(now) && self.throttle.ready(now)
    }

    /// How long to wait for compositor events before the next [`Activity::tick`]; `None`
    /// while idle (only an event changes anything).
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        self.running(now).then_some(INTERACTION_THROTTLE)
    }
}

// ---------------------------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WlInputConfig {
    /// Wayland socket: a name relative to `$XDG_RUNTIME_DIR` or an absolute path.
    /// Defaults to `$WAYLAND_DISPLAY`, else the first `wayland-N` socket in `$XDG_RUNTIME_DIR`.
    pub display: Option<String>,
}

/// See the [module docs](self).
#[derive(Default)]
pub struct WlInputAdapter {
    config: WlInputConfig,
    worker: Option<Worker>,
}

impl WlInputAdapter {
    pub fn new(config: WlInputConfig) -> Self {
        WlInputAdapter { config, worker: None }
    }
}

impl Plugin for WlInputAdapter {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: WAYLAND_INPUT.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Adapter],
            description: "Input activity on Wayland compositors, from the ext-idle-notify protocol".into(),
        }
    }

    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
        if !ctx.config.is_null() {
            self.config = WlInputConfig::deserialize(&ctx.config)
                .map_err(|e| PluginError::Failed(format!("invalid wayland-input adapter config: {e}")))?;
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.stop();
    }
}

impl Adapter for WlInputAdapter {
    fn start(&mut self, sink: EventSink) -> PluginResult<()> {
        if self.worker.as_ref().is_some_and(Worker::is_running) {
            return Ok(());
        }
        let display = self.config.display.clone().filter(|d| !d.trim().is_empty());
        let session = Session::connect(display.as_deref(), sink.clone()).map_err(|e| match e {
            SessionError::Unsupported(m) => PluginError::Unsupported(m),
            SessionError::Connect(m) => PluginError::Unsupported(format!("cannot connect to Wayland display: {m}")),
            SessionError::Wayland(m) => PluginError::Failed(m),
        })?;
        let worker = Worker::spawn("lindesk-wl-input", true, move |ctl| run(ctl, session, display.as_deref(), &sink))?;
        self.worker = Some(worker);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(mut w) = self.worker.take() {
            w.stop();
        }
    }

    fn connected(&self) -> bool {
        self.worker.as_ref().is_some_and(Worker::connected)
    }
}

fn run(ctl: &Control, first: Session, display: Option<&str>, sink: &EventSink) {
    let mut backoff = Backoff::default();
    let mut session = Some(first);
    loop {
        let s = match session.take() {
            Some(s) => s,
            None => match Session::connect(display, sink.clone()) {
                Ok(s) => {
                    tracing::info!("wayland-input adapter reconnected");
                    s
                }
                Err(e) => {
                    let delay = backoff.next_delay();
                    tracing::debug!(error = %e, ?delay, "wayland-input reconnect failed");
                    if ctl.sleep(delay) {
                        return;
                    }
                    continue;
                }
            },
        };
        backoff.reset();
        ctl.set_connected(true);
        let result = s.run(ctl);
        ctl.set_connected(false);
        match result {
            Ok(()) => return,
            Err(e) => tracing::warn!(error = %e, "wayland-input: compositor connection lost; reconnecting"),
        }
        if ctl.is_stopped() {
            return;
        }
    }
}

#[derive(Debug)]
enum SessionError {
    /// The compositor lacks the protocol.
    Unsupported(String),
    /// No socket / cannot connect.
    Connect(String),
    /// Protocol or I/O error on an established connection.
    Wayland(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Unsupported(m) | SessionError::Connect(m) | SessionError::Wayland(m) => f.write_str(m),
        }
    }
}

fn wl_err(e: impl std::fmt::Display) -> SessionError {
    SessionError::Wayland(e.to_string())
}

/// The compositor's socket: `display` (a name in `$XDG_RUNTIME_DIR` or an absolute path), else
/// `$WAYLAND_DISPLAY`, else the first `wayland-N` socket in `$XDG_RUNTIME_DIR` (a user service
/// started before the session imported its environment has no `WAYLAND_DISPLAY`).
fn socket_path(display: Option<&str>) -> Result<PathBuf, SessionError> {
    let runtime = || {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| SessionError::Connect("XDG_RUNTIME_DIR is unset".into()))
    };
    let named = display.map(str::to_owned).or_else(|| std::env::var("WAYLAND_DISPLAY").ok().filter(|d| !d.is_empty()));
    if let Some(name) = named {
        let p = PathBuf::from(name);
        return if p.is_absolute() { Ok(p) } else { Ok(runtime()?.join(p)) };
    }
    let dir = runtime()?;
    let mut sockets: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| SessionError::Connect(format!("{}: {e}", dir.display())))?
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.strip_prefix("wayland-")
                    .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
            })
        })
        .collect();
    sockets.sort();
    sockets
        .into_iter()
        .next()
        .ok_or_else(|| SessionError::Connect("not a Wayland session: no compositor socket".into()))
}

/// Wayland dispatch state.
struct State {
    activity: Activity,
    sink: EventSink,
}

impl State {
    fn interaction(&self) {
        (self.sink)(Event::Interaction { ts: now_ms(), pid: None, target: None, end: false, source: SOURCE.into() });
    }
}

struct Session {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    _notification: ext_idle_notification_v1::ExtIdleNotificationV1,
}

impl Session {
    fn connect(display: Option<&str>, sink: EventSink) -> Result<Session, SessionError> {
        let path = socket_path(display)?;
        let stream =
            UnixStream::connect(&path).map_err(|e| SessionError::Connect(format!("{}: {e}", path.display())))?;
        let conn = Connection::from_socket(stream).map_err(|e| SessionError::Connect(e.to_string()))?;
        let (globals, queue) = registry_queue_init::<State>(&conn).map_err(wl_err)?;
        let qh = queue.handle();
        let missing = |what: &str, e: BindError| {
            SessionError::Unsupported(match e {
                BindError::NotPresent => format!("compositor does not advertise {what}"),
                BindError::UnsupportedVersion => format!("compositor's {what} is too old"),
            })
        };
        let notifier = globals
            .bind::<ext_idle_notifier_v1::ExtIdleNotifierV1, State, ()>(&qh, 1..=2, ())
            .map_err(|e| missing("ext_idle_notifier_v1", e))?;
        let seat = globals.bind::<wl_seat::WlSeat, State, ()>(&qh, 1..=1, ()).map_err(|e| missing("wl_seat", e))?;
        let timeout = u32::try_from(INTERACTION_THROTTLE.as_millis()).unwrap_or(250);
        let input_only = wayland_client::Proxy::version(&notifier) >= 2;
        let notification = if input_only {
            notifier.get_input_idle_notification(timeout, &seat, &qh, ())
        } else {
            notifier.get_idle_notification(timeout, &seat, &qh, ())
        };
        tracing::debug!(input_only, "wayland-input: watching for input");
        let state = State { activity: Activity::new(input_only, Instant::now()), sink };
        Ok(Session { conn, queue, state, _notification: notification })
    }

    /// Event loop. `Ok(())` when stopped, `Err` when the connection broke.
    fn run(mut self, ctl: &Control) -> Result<(), SessionError> {
        loop {
            self.queue.dispatch_pending(&mut self.state).map_err(wl_err)?;
            match self.conn.flush() {
                Ok(()) => {}
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(wl_err(e)),
            }
            // `None` means events are already queued: dispatch them first.
            let Some(guard) = self.queue.prepare_read() else { continue };
            let wait = self.state.activity.wait(Instant::now());
            match ctl.wait(Some(guard.connection_fd()), wait).map_err(wl_err)? {
                Wake::Stopped => return Ok(()),
                Wake::Timeout => {
                    drop(guard);
                    if self.state.activity.tick(Instant::now()) {
                        self.state.interaction();
                    }
                }
                Wake::Ready => match guard.read() {
                    Ok(_) => {}
                    Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(wl_err(e)),
                },
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // If the notifier or the seat goes away the notification dies with it and we reconnect.
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ext_idle_notifier_v1::ExtIdleNotifierV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ext_idle_notifier_v1::ExtIdleNotifierV1,
        _: ext_idle_notifier_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ext_idle_notification_v1::ExtIdleNotificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ext_idle_notification_v1::ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => state.activity.idled(),
            ext_idle_notification_v1::Event::Resumed => {
                let report = state.activity.resumed(Instant::now());
                if report {
                    state.interaction();
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    #[test]
    fn interactions_run_from_resumed_to_idled() {
        let t0 = Instant::now();
        let mut a = Activity::new(true, t0);
        // Typing when the adapter started: no `resumed` will come, the ticks report it.
        assert_eq!(a.wait(t0), Some(INTERACTION_THROTTLE));
        assert!(a.tick(at(t0, 250)));
        assert!(!a.tick(at(t0, 300)), "at most one per throttle interval");
        assert!(a.tick(at(t0, 500)));
        // Hours of typing keep reporting: idle here means no input, nothing else.
        assert!(a.tick(at(t0, 3_600_000)));

        a.idled();
        assert_eq!(a.wait(at(t0, 3_600_250)), None, "idle: sleep until the compositor says otherwise");
        assert!(!a.tick(at(t0, 3_700_000)));

        // The first key after a pause is reported at once, not a tick later.
        assert!(a.resumed(at(t0, 3_800_000)));
        assert!(!a.resumed(at(t0, 3_800_010)), "still throttled");
        assert!(a.tick(at(t0, 3_800_250)));
    }

    #[test]
    fn an_inhibitor_cannot_look_like_endless_input() {
        let t0 = Instant::now();
        // Protocol version 1: a video player's idle inhibitor holds `idled` back.
        let mut a = Activity::new(false, t0);
        assert!(a.tick(at(t0, 250)));
        assert!(a.tick(at(t0, 4_750)));
        assert!(!a.tick(at(t0, 5_000)));
        assert_eq!(a.wait(at(t0, 5_000)), None);
        // Real input after an idle period starts a new, equally bounded run.
        a.idled();
        assert!(a.resumed(at(t0, 60_000)));
        assert!(a.tick(at(t0, 60_250)));
        assert!(!a.tick(at(t0, 65_000)));
    }

    #[test]
    fn socket_selection_and_start_without_a_compositor() {
        assert_eq!(socket_path(Some("/tmp/x/wayland-7")).unwrap(), PathBuf::from("/tmp/x/wayland-7"));
        let tmp = crate::actuators::test_dir();
        let missing = tmp.path().join("wayland-9");
        let mut a = WlInputAdapter::new(WlInputConfig { display: Some(missing.to_string_lossy().into_owned()) });
        let sink: EventSink = std::sync::Arc::new(|_| {});
        match a.start(sink) {
            Err(PluginError::Unsupported(why)) => assert!(why.contains("cannot connect"), "{why}"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
        assert!(!a.connected());
        assert_eq!(a.info().name, WAYLAND_INPUT);
    }
}
