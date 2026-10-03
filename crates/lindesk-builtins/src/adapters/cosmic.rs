//! COSMIC foreground adapter (plugin `"cosmic"`).
//!
//! A Wayland client that binds `ext_foreign_toplevel_list_v1` (the toplevel list and each
//! toplevel's `app_id`) and `zcosmic_toplevel_info_v1` version 2 or later (whose
//! `get_cosmic_toplevel` request attaches a `zcosmic_toplevel_handle_v1` to each ext handle and
//! reports its `state`, including `activated` = 2). When a toplevel becomes the activated one it
//! emits `Event::Focus { pid: None, app_id: Some(..), source: "cosmic" }`; the engine maps the
//! app_id to the app's `app-*-<app_id>-*.scope`. Wayland gives clients no pids for other
//! clients' windows, and COSMIC exposes no input activity, so there are no Interaction events.
//!
//! On compositors without these globals (GNOME, KDE, wlroots) [`CosmicAdapter::start`] returns
//! [`PluginError::Unsupported`] so the host can skip it.
//!
//! Config (`InitContext::config`):
//! ```toml
//! display = "wayland-1"   # socket name in $XDG_RUNTIME_DIR, or an absolute path; default: $WAYLAND_DISPLAY
//! ```

use std::collections::HashMap;
use std::hash::Hash;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use cosmic_protocols::toplevel_info::v1::client::{zcosmic_toplevel_handle_v1, zcosmic_toplevel_info_v1};
use lindesk_core::clock::now_ms;
use lindesk_plugin_api::{
    Adapter, Event, EventSink, InitContext, Plugin, PluginError, PluginInfo, PluginKind, PluginResult,
};
use serde::Deserialize;
use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::globals::{BindError, GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1, ext_foreign_toplevel_list_v1,
};

use super::worker::{Backoff, Control, Wake, Worker};
use super::{COSMIC, Debouncer};

const SOURCE: &str = "cosmic";
/// `zcosmic_toplevel_handle_v1.state` entry for "the toplevel is active".
pub const STATE_ACTIVATED: u32 = 2;
/// First `zcosmic_toplevel_info_v1` version with `get_cosmic_toplevel` (ext handle extension).
const COSMIC_INFO_MIN_VERSION: u32 = 2;
const COSMIC_INFO_MAX_VERSION: u32 = 3;

// ---------------------------------------------------------------------------------------------
// Pure state tracking (no Wayland types): toplevel updates in, focus changes out.
// ---------------------------------------------------------------------------------------------

/// One protocol-level change to a toplevel, keyed by an opaque handle id `K`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToplevelUpdate<K> {
    /// `ext_foreign_toplevel_handle_v1.app_id`
    AppId(K, String),
    /// `zcosmic_toplevel_handle_v1.state`, decoded into whether `activated` is set.
    Activated(K, bool),
    /// `ext_foreign_toplevel_handle_v1.closed`
    Closed(K),
    /// `ext_foreign_toplevel_handle_v1.done` or `zcosmic_toplevel_info_v1.done`: the preceding
    /// updates form a consistent state.
    Done,
}

#[derive(Clone, Debug, Default)]
struct Toplevel {
    app_id: Option<String>,
    activated: bool,
    /// When it last became activated; the most recent activation wins if several toplevels
    /// claim `activated` (multi-seat, or transiently between events).
    activated_seq: u64,
}

/// Tracks toplevels and decides when focus changed.
#[derive(Clone, Debug)]
pub struct ToplevelTracker<K> {
    toplevels: HashMap<K, Toplevel>,
    seq: u64,
    focus: Debouncer<(K, String)>,
}

impl<K> Default for ToplevelTracker<K> {
    fn default() -> Self {
        ToplevelTracker { toplevels: HashMap::new(), seq: 0, focus: Debouncer::default() }
    }
}

impl<K: Eq + Hash + Clone> ToplevelTracker<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one update. Returns the app_id to report when this update completes a state in
    /// which a different toplevel (or the same one under a new app_id) is activated.
    pub fn apply(&mut self, update: ToplevelUpdate<K>) -> Option<String> {
        match update {
            ToplevelUpdate::AppId(k, app_id) => {
                let app_id = app_id.trim();
                self.toplevels.entry(k).or_default().app_id = (!app_id.is_empty()).then(|| app_id.to_owned());
            }
            ToplevelUpdate::Activated(k, activated) => {
                let t = self.toplevels.entry(k).or_default();
                if activated && !t.activated {
                    self.seq += 1;
                    t.activated_seq = self.seq;
                }
                t.activated = activated;
            }
            ToplevelUpdate::Closed(k) => {
                self.toplevels.remove(&k);
            }
            ToplevelUpdate::Done => return self.commit(),
        }
        None
    }

    /// The toplevel that currently has focus, if any.
    fn active(&self) -> Option<(&K, &Toplevel)> {
        self.toplevels.iter().filter(|(_, t)| t.activated).max_by_key(|(_, t)| t.activated_seq)
    }

    /// App id of the focused toplevel, if known.
    pub fn active_app_id(&self) -> Option<&str> {
        self.active().and_then(|(_, t)| t.app_id.as_deref())
    }

    pub fn len(&self) -> usize {
        self.toplevels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.toplevels.is_empty()
    }

    fn commit(&mut self) -> Option<String> {
        let Some((k, t)) = self.active() else {
            // Nothing focused (e.g. the desktop): report the next focus even if it is the
            // same toplevel as before.
            self.focus.reset();
            return None;
        };
        // Wait for the app_id before reporting; it arrives in a later batch at the latest.
        let app_id = t.app_id.clone()?;
        let key = (k.clone(), app_id.clone());
        self.focus.accept(key).then_some(app_id)
    }
}

/// Decodes a `zcosmic_toplevel_handle_v1.state` array (native-endian `u32`s).
pub fn decode_states(raw: &[u8]) -> impl Iterator<Item = u32> + '_ {
    raw.chunks_exact(4).map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
}

/// True if a `state` array contains `activated`.
pub fn is_activated(raw: &[u8]) -> bool {
    decode_states(raw).any(|s| s == STATE_ACTIVATED)
}

// ---------------------------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CosmicConfig {
    /// Wayland socket: a name relative to `$XDG_RUNTIME_DIR` or an absolute path.
    /// Defaults to `$WAYLAND_DISPLAY` (or `$WAYLAND_SOCKET`).
    pub display: Option<String>,
}

/// See the [module docs](self).
#[derive(Default)]
pub struct CosmicAdapter {
    config: CosmicConfig,
    worker: Option<Worker>,
}

impl CosmicAdapter {
    pub fn new(config: CosmicConfig) -> Self {
        CosmicAdapter { config, worker: None }
    }
}

impl Plugin for CosmicAdapter {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: COSMIC.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Adapter],
            description: "Focus (app_id) from COSMIC's ext-foreign-toplevel-list and cosmic-toplevel-info protocols"
                .into(),
        }
    }

    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
        if !ctx.config.is_null() {
            self.config = CosmicConfig::deserialize(&ctx.config)
                .map_err(|e| PluginError::Failed(format!("invalid cosmic adapter config: {e}")))?;
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.stop();
    }
}

impl Adapter for CosmicAdapter {
    fn start(&mut self, sink: EventSink) -> PluginResult<()> {
        if self.worker.as_ref().is_some_and(Worker::is_running) {
            return Ok(());
        }
        let display = self.config.display.clone().filter(|d| !d.trim().is_empty());
        if display.is_none()
            && std::env::var_os("WAYLAND_DISPLAY").is_none()
            && std::env::var_os("WAYLAND_SOCKET").is_none()
        {
            return Err(PluginError::Unsupported("not a Wayland session: WAYLAND_DISPLAY is unset".into()));
        }
        let session = Session::connect(display.as_deref(), sink.clone()).map_err(|e| match e {
            SessionError::Unsupported(m) => PluginError::Unsupported(m),
            SessionError::Connect(m) => PluginError::Unsupported(format!("cannot connect to Wayland display: {m}")),
            SessionError::Wayland(m) => PluginError::Failed(m),
        })?;
        // The session is already connected; report that before the thread gets going.
        let worker = Worker::spawn("lindesk-cosmic", true, move |ctl| run(ctl, session, display.as_deref(), &sink))?;
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
                    tracing::info!("cosmic adapter reconnected");
                    s
                }
                Err(e) => {
                    let delay = backoff.next_delay();
                    tracing::debug!(error = %e, ?delay, "cosmic reconnect failed");
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
            Err(e) => tracing::warn!(error = %e, "cosmic compositor connection lost; reconnecting"),
        }
        if ctl.is_stopped() {
            return;
        }
    }
}

#[derive(Debug)]
enum SessionError {
    /// The compositor lacks the protocols.
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

fn socket_path(display: &str) -> Result<PathBuf, SessionError> {
    let p = PathBuf::from(display);
    if p.is_absolute() {
        return Ok(p);
    }
    let runtime =
        std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| SessionError::Connect("XDG_RUNTIME_DIR is unset".into()))?;
    Ok(PathBuf::from(runtime).join(p))
}

/// Wayland dispatch state.
struct State {
    info: zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1,
    tracker: ToplevelTracker<ObjectId>,
    /// cosmic extension object per ext handle, destroyed with it.
    cosmic_handles: HashMap<ObjectId, zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1>,
    sink: EventSink,
    /// The compositor stopped the toplevel list (`finished`): reconnect.
    finished: bool,
}

impl State {
    fn update(&mut self, u: ToplevelUpdate<ObjectId>) {
        if let Some(app_id) = self.tracker.apply(u) {
            tracing::trace!(%app_id, "cosmic focus");
            (self.sink)(Event::Focus {
                ts: now_ms(),
                pid: None,
                app_id: Some(app_id),
                target: None,
                source: SOURCE.into(),
            });
        }
    }
}

struct Session {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    _list: ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
}

impl Session {
    fn connect(display: Option<&str>, sink: EventSink) -> Result<Session, SessionError> {
        let conn = match display {
            Some(d) => {
                let path = socket_path(d)?;
                let stream = UnixStream::connect(&path)
                    .map_err(|e| SessionError::Connect(format!("{}: {e}", path.display())))?;
                Connection::from_socket(stream).map_err(|e| SessionError::Connect(e.to_string()))?
            }
            None => Connection::connect_to_env().map_err(|e| SessionError::Connect(e.to_string()))?,
        };
        let (globals, queue) = registry_queue_init::<State>(&conn).map_err(wl_err)?;
        let qh = queue.handle();
        let missing = |what: &str, e: BindError| {
            SessionError::Unsupported(match e {
                BindError::NotPresent => format!("compositor does not advertise {what} (not a COSMIC session?)"),
                BindError::UnsupportedVersion => format!("compositor's {what} is too old"),
            })
        };
        // Bind the cosmic extension first so that, by the time the list announces toplevels,
        // `info` exists to attach to them.
        let info = globals
            .bind::<zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1, State, ()>(
                &qh,
                COSMIC_INFO_MIN_VERSION..=COSMIC_INFO_MAX_VERSION,
                (),
            )
            .map_err(|e| missing("zcosmic_toplevel_info_v1 (version 2 or later)", e))?;
        let list = globals
            .bind::<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, State, ()>(&qh, 1..=1, ())
            .map_err(|e| missing("ext_foreign_toplevel_list_v1", e))?;
        let state =
            State { info, tracker: ToplevelTracker::new(), cosmic_handles: HashMap::new(), sink, finished: false };
        Ok(Session { conn, queue, state, _list: list })
    }

    /// Event loop. `Ok(())` when stopped, `Err` when the connection broke.
    fn run(mut self, ctl: &Control) -> Result<(), SessionError> {
        loop {
            self.queue.dispatch_pending(&mut self.state).map_err(wl_err)?;
            if self.state.finished {
                return Err(SessionError::Wayland("compositor finished the toplevel list".into()));
            }
            match self.conn.flush() {
                Ok(()) => {}
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(wl_err(e)),
            }
            // `None` means events are already queued: dispatch them first.
            let Some(guard) = self.queue.prepare_read() else { continue };
            match ctl.wait(Some(guard.connection_fd()), None).map_err(wl_err)? {
                Wake::Stopped => return Ok(()),
                Wake::Timeout => continue,
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
        // Globals appearing/disappearing later don't matter: if ours go away, the objects die
        // and we reconnect.
    }
}

impl Dispatch<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } => {
                let id = toplevel.id();
                let cosmic = state.info.get_cosmic_toplevel(&toplevel, qh, id.clone());
                state.cosmic_handles.insert(id, cosmic);
            }
            ext_foreign_toplevel_list_v1::Event::Finished => state.finished = true,
            _ => {}
        }
    }

    event_created_child!(State, ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = handle.id();
        match event {
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => state.update(ToplevelUpdate::AppId(id, app_id)),
            ext_foreign_toplevel_handle_v1::Event::Done => state.update(ToplevelUpdate::Done),
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                state.update(ToplevelUpdate::Closed(id.clone()));
                if let Some(cosmic) = state.cosmic_handles.remove(&id) {
                    cosmic.destroy();
                }
                handle.destroy();
                state.update(ToplevelUpdate::Done);
            }
            _ => {}
        }
    }
}

impl Dispatch<zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1,
        event: zcosmic_toplevel_info_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zcosmic_toplevel_info_v1::Event::Done = event {
            state.update(ToplevelUpdate::Done);
        }
    }
}

/// User data: the id of the ext handle this cosmic handle extends.
impl Dispatch<zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1, ObjectId> for State {
    fn event(
        state: &mut Self,
        _: &zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1,
        event: zcosmic_toplevel_handle_v1::Event,
        ext_id: &ObjectId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zcosmic_toplevel_handle_v1::Event::State { state: raw } = event {
            state.update(ToplevelUpdate::Activated(ext_id.clone(), is_activated(&raw)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ToplevelUpdate::*;

    fn run(t: &mut ToplevelTracker<u32>, updates: impl IntoIterator<Item = ToplevelUpdate<u32>>) -> Vec<String> {
        updates.into_iter().filter_map(|u| t.apply(u)).collect()
    }

    fn states(v: &[u32]) -> Vec<u8> {
        v.iter().flat_map(|s| s.to_ne_bytes()).collect()
    }

    #[test]
    fn decodes_state_arrays() {
        assert!(is_activated(&states(&[0, 2])));
        assert!(!is_activated(&states(&[0, 1, 3, 4])));
        assert!(!is_activated(&[]));
        assert_eq!(decode_states(&states(&[2, 3])).collect::<Vec<_>>(), [2, 3]);
        // Trailing garbage is ignored, not misread.
        assert!(!is_activated(&[2, 0, 0]));
    }

    #[test]
    fn initial_burst_reports_the_activated_toplevel_once() {
        let mut t = ToplevelTracker::new();
        // Compositor sends existing toplevels: ext app_id + done, then cosmic states, then info done.
        let out = run(
            &mut t,
            [
                AppId(1, "org.mozilla.firefox".into()),
                Done,
                AppId(2, "com.system76.CosmicTerm".into()),
                Done,
                Activated(1, false),
                Activated(2, true),
                Done,
            ],
        );
        assert_eq!(out, ["com.system76.CosmicTerm"]);
        assert_eq!(t.len(), 2);
        assert_eq!(t.active_app_id(), Some("com.system76.CosmicTerm"));
    }

    #[test]
    fn focus_switch_and_duplicates() {
        let mut t = ToplevelTracker::new();
        run(&mut t, [AppId(1, "a".into()), AppId(2, "b".into()), Activated(1, true), Done]);
        // Switch: 1 loses, 2 gains, in one batch.
        assert_eq!(run(&mut t, [Activated(1, false), Activated(2, true), Done]), ["b"]);
        // Unrelated state change (e.g. maximize) on the focused window: no new event.
        assert!(run(&mut t, [Activated(2, true), Done]).is_empty());
        // Several done events in a row: no duplicates.
        assert!(run(&mut t, [Done, Done]).is_empty());
        assert_eq!(run(&mut t, [Activated(2, false), Activated(1, true), Done]), ["a"]);
    }

    #[test]
    fn nothing_is_reported_before_done() {
        let mut t = ToplevelTracker::new();
        assert!(run(&mut t, [AppId(1, "a".into()), Activated(1, true)]).is_empty());
        assert_eq!(run(&mut t, [Done]), ["a"]);
    }

    #[test]
    fn waits_for_app_id() {
        let mut t = ToplevelTracker::new();
        assert!(run(&mut t, [Activated(7, true), Done]).is_empty());
        assert!(run(&mut t, [AppId(7, "  ".into()), Done]).is_empty(), "blank app_id is no app_id");
        assert_eq!(run(&mut t, [AppId(7, "late.app".into()), Done]), ["late.app"]);
        // app_id changes on the focused toplevel (e.g. a launcher that execs): reported.
        assert_eq!(run(&mut t, [AppId(7, "real.app".into()), Done]), ["real.app"]);
    }

    #[test]
    fn same_app_in_another_window_is_reported() {
        let mut t = ToplevelTracker::new();
        run(&mut t, [AppId(1, "term".into()), AppId(2, "term".into()), Activated(1, true), Done]);
        assert_eq!(run(&mut t, [Activated(1, false), Activated(2, true), Done]), ["term"]);
    }

    #[test]
    fn unfocus_then_refocus_reports_again() {
        let mut t = ToplevelTracker::new();
        assert_eq!(run(&mut t, [AppId(1, "a".into()), Activated(1, true), Done]), ["a"]);
        // Focus to the desktop / nothing.
        assert!(run(&mut t, [Activated(1, false), Done]).is_empty());
        assert_eq!(t.active_app_id(), None);
        assert_eq!(run(&mut t, [Activated(1, true), Done]), ["a"]);
    }

    #[test]
    fn closing_the_focused_toplevel() {
        let mut t = ToplevelTracker::new();
        run(&mut t, [AppId(1, "a".into()), AppId(2, "b".into()), Activated(1, true), Done]);
        // 1 closes; the compositor activates 2.
        assert_eq!(run(&mut t, [Closed(1), Done, Activated(2, true), Done]), ["b"]);
        assert_eq!(t.len(), 1);
        // A reopened "a" (new handle, new process) is reported even though "a" was seen before.
        assert_eq!(run(&mut t, [AppId(3, "a".into()), Activated(2, false), Activated(3, true), Done]), ["a"]);
    }

    #[test]
    fn most_recent_activation_wins() {
        let mut t = ToplevelTracker::new();
        run(&mut t, [AppId(1, "a".into()), AppId(2, "b".into()), Activated(1, true), Done]);
        // 2 activated before the compositor clears 1 (or a second seat): newest wins.
        assert_eq!(run(&mut t, [Activated(2, true), Done]), ["b"]);
        assert!(run(&mut t, [Activated(1, false), Done]).is_empty());
    }

    #[test]
    fn start_without_wayland_is_unsupported() {
        let mut a = CosmicAdapter::new(CosmicConfig { display: Some("/nonexistent/lindesk-test-wayland".into()) });
        let err = a.start(std::sync::Arc::new(|_| {})).unwrap_err();
        assert!(matches!(err, PluginError::Unsupported(_)), "{err:?}");
        assert!(!a.connected());
    }

    /// Manual check against the session this runs in:
    /// `cargo test -p lindesk-builtins cosmic::tests::live -- --ignored --nocapture`.
    /// Prints whether the compositor is supported and, if so, focus events for
    /// `LINDESK_LIVE_SECS` seconds (default 10).
    #[test]
    #[ignore = "talks to the live Wayland session"]
    fn live() {
        let secs = std::env::var("LINDESK_LIVE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(10);
        let mut a = CosmicAdapter::default();
        let sink: EventSink = std::sync::Arc::new(|e| println!("event: {}", serde_json::to_string(&e).unwrap()));
        match a.start(sink) {
            Ok(()) => {
                println!("cosmic adapter started; connected={}", a.connected());
                std::thread::sleep(std::time::Duration::from_secs(secs));
                a.stop();
                println!("stopped; connected={}", a.connected());
            }
            Err(e) => println!("cosmic adapter start: {e:?}"),
        }
    }

    #[test]
    fn config_parses() {
        let c: CosmicConfig = serde_json::from_value(serde_json::json!({"display": "wayland-1"})).unwrap();
        assert_eq!(c.display.as_deref(), Some("wayland-1"));
        assert!(serde_json::from_value::<CosmicConfig>(serde_json::json!({"wayland": "x"})).is_err());
    }
}
