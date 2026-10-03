//! X11 foreground adapter (plugin `"x11"`).
//!
//! * Focus: watches `_NET_ACTIVE_WINDOW` on the root window (set by every EWMH window
//!   manager) and resolves the window's process through `_NET_WM_PID`, falling back to the
//!   `WM_CLIENT_LEADER` window and then to child windows. `WM_CLASS` (the class part) is sent
//!   as the app_id hint.
//! * Input: XInput2 raw key/button/touch events on the root window, throttled to one
//!   `Event::Interaction` per [`INTERACTION_THROTTLE`](super::INTERACTION_THROTTLE). Raw events
//!   are delivered regardless of which client has focus, without grabbing anything.
//!
//! Config (`InitContext::config`):
//! ```toml
//! display = ":0"   # default: $DISPLAY
//! input = true     # default: on when the server has XInput 2
//! ```
//!
//! `_NET_WM_PID` is set by the client and can be absent (some old toolkits) or refer to another
//! host for remote clients; such windows are reported by `WM_CLASS` only.

use std::os::fd::AsFd;
use std::time::Instant;

use lindesk_core::clock::now_ms;
use lindesk_plugin_api::{
    Adapter, Event, EventSink, InitContext, Plugin, PluginError, PluginInfo, PluginKind, PluginResult,
};
use serde::Deserialize;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::{ConnectionError, ReplyError};
use x11rb::protocol::Event as XEvent;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, Window};
use x11rb::rust_connection::RustConnection;

use super::worker::{Backoff, Control, Wake, Worker};
use super::{Debouncer, INTERACTION_THROTTLE, Throttle, X11};

const SOURCE: &str = "x11";
/// How many windows the child-window fallback inspects at most.
const MAX_CHILD_SCAN: usize = 64;

/// Adapter config. Unknown keys are rejected so typos surface at init.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct X11Config {
    /// X display to connect to, e.g. `":0"`. Defaults to `$DISPLAY`.
    pub display: Option<String>,
    /// Emit `Interaction` events from XInput2 raw input. `None` = on if XInput 2 is available.
    pub input: Option<bool>,
}

/// See the [module docs](self).
#[derive(Default)]
pub struct X11Adapter {
    config: X11Config,
    worker: Option<Worker>,
}

impl X11Adapter {
    pub fn new(config: X11Config) -> Self {
        X11Adapter { config, worker: None }
    }

    fn display(&self) -> PluginResult<String> {
        let non_empty = |d: &String| !d.trim().is_empty();
        self.config
            .display
            .clone()
            .filter(non_empty)
            .or_else(|| std::env::var("DISPLAY").ok().filter(non_empty))
            .ok_or_else(|| {
                PluginError::Unsupported("no X11 display: DISPLAY is unset and no `display` configured".into())
            })
    }
}

impl Plugin for X11Adapter {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: X11.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Adapter],
            description: "Focus from _NET_ACTIVE_WINDOW/_NET_WM_PID and input activity from XInput2 raw events".into(),
        }
    }

    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
        if !ctx.config.is_null() {
            self.config = X11Config::deserialize(&ctx.config)
                .map_err(|e| PluginError::Failed(format!("invalid x11 adapter config: {e}")))?;
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.stop();
    }
}

impl Adapter for X11Adapter {
    fn start(&mut self, sink: EventSink) -> PluginResult<()> {
        if self.worker.as_ref().is_some_and(Worker::is_running) {
            return Ok(());
        }
        let display = self.display()?;
        let input = self.config.input;
        // Connect synchronously so a missing X server is reported to the host right away.
        let session = Session::connect(&display, input)
            .map_err(|e| PluginError::Unsupported(format!("cannot use X display {display}: {e}")))?;
        // The session is already connected; report that before the thread gets going.
        let worker = Worker::spawn("lindesk-x11", true, move |ctl| run(ctl, session, &display, input, &sink))?;
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

/// Worker thread: run the session, reconnect with backoff when the server goes away.
fn run(ctl: &Control, first: Session, dpy: &str, input: Option<bool>, sink: &EventSink) {
    let mut backoff = Backoff::default();
    let mut session = Some(first);
    loop {
        let s = match session.take() {
            Some(s) => s,
            None => match Session::connect(dpy, input) {
                Ok(s) => {
                    tracing::info!(display = %dpy, "x11 adapter reconnected");
                    s
                }
                Err(e) => {
                    let delay = backoff.next_delay();
                    tracing::debug!(display = %dpy, error = %e, ?delay, "x11 reconnect failed");
                    if ctl.sleep(delay) {
                        return;
                    }
                    continue;
                }
            },
        };
        backoff.reset();
        ctl.set_connected(true);
        let result = s.run(ctl, sink);
        ctl.set_connected(false);
        match result {
            Ok(()) => return, // stopped
            Err(e) => tracing::warn!(display = %dpy, error = %e, "x11 connection lost; reconnecting"),
        }
        if ctl.is_stopped() {
            return;
        }
    }
}

#[derive(Debug)]
enum SessionError {
    Connect(x11rb::errors::ConnectError),
    Connection(ConnectionError),
    Reply(ReplyError),
    Io(std::io::Error),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Connect(e) => e.fmt(f),
            SessionError::Connection(e) => e.fmt(f),
            SessionError::Reply(e) => e.fmt(f),
            SessionError::Io(e) => e.fmt(f),
        }
    }
}

impl From<x11rb::errors::ConnectError> for SessionError {
    fn from(e: x11rb::errors::ConnectError) -> Self {
        SessionError::Connect(e)
    }
}
impl From<ConnectionError> for SessionError {
    fn from(e: ConnectionError) -> Self {
        SessionError::Connection(e)
    }
}
impl From<ReplyError> for SessionError {
    fn from(e: ReplyError) -> Self {
        SessionError::Reply(e)
    }
}
impl From<std::io::Error> for SessionError {
    fn from(e: std::io::Error) -> Self {
        SessionError::Io(e)
    }
}

struct Atoms {
    net_active_window: Atom,
    net_wm_pid: Atom,
    wm_client_leader: Atom,
}

/// One connection to the X server.
struct Session {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
    focus: Debouncer<(Option<i32>, Option<String>)>,
    throttle: Throttle,
}

impl Session {
    fn connect(dpy: &str, input: Option<bool>) -> Result<Session, SessionError> {
        // x11rb 0.14 panics (integer overflow) on absurd display numbers such as ":65000";
        // a bad config value must not take the host down.
        let (conn, screen) = std::panic::catch_unwind(|| x11rb::connect(Some(dpy)))
            .map_err(|_| SessionError::Io(std::io::Error::other(format!("invalid X display name {dpy:?}"))))??;
        let root = conn.setup().roots[screen].root;
        let c1 = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW")?;
        let c2 = conn.intern_atom(false, b"_NET_WM_PID")?;
        let c3 = conn.intern_atom(false, b"WM_CLIENT_LEADER")?;
        let atoms = Atoms {
            net_active_window: c1.reply()?.atom,
            net_wm_pid: c2.reply()?.atom,
            wm_client_leader: c3.reply()?.atom,
        };
        conn.change_window_attributes(root, &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE))?
            .check()?;
        if input != Some(false) {
            match select_raw_input(&conn, root) {
                Ok(true) => {}
                Ok(false) if input == Some(true) => {
                    tracing::warn!(display = %dpy, "XInput 2 not available; x11 adapter reports focus only")
                }
                Ok(false) => tracing::debug!(display = %dpy, "XInput 2 not available; input activity disabled"),
                Err(e) => tracing::warn!(display = %dpy, error = %e, "selecting XInput 2 raw events failed"),
            }
        }
        conn.flush()?;
        Ok(Session { conn, root, atoms, focus: Debouncer::new(), throttle: Throttle::new(INTERACTION_THROTTLE) })
    }

    /// Event loop. `Ok(())` when stopped, `Err` when the connection broke.
    fn run(mut self, ctl: &Control, sink: &EventSink) -> Result<(), SessionError> {
        self.on_active_window_changed(sink)?;
        loop {
            while let Some(event) = self.conn.poll_for_event()? {
                self.handle(event, sink)?;
            }
            self.conn.flush()?;
            if ctl.wait(Some(self.conn.stream().as_fd()), None)? == Wake::Stopped {
                return Ok(());
            }
        }
    }

    fn handle(&mut self, event: XEvent, sink: &EventSink) -> Result<(), SessionError> {
        match event {
            XEvent::PropertyNotify(e) if e.window == self.root && e.atom == self.atoms.net_active_window => {
                self.on_active_window_changed(sink)?;
            }
            XEvent::XinputRawKeyPress(_) | XEvent::XinputRawButtonPress(_) | XEvent::XinputRawTouchBegin(_)
                if self.throttle.ready(Instant::now()) =>
            {
                let pid = self.focus.last().and_then(|(pid, _)| *pid);
                sink(Event::Interaction { ts: now_ms(), pid, target: None, end: false, source: SOURCE.into() });
            }
            _ => {}
        }
        Ok(())
    }

    fn on_active_window_changed(&mut self, sink: &EventSink) -> Result<(), SessionError> {
        let active = self.window_prop(self.root, self.atoms.net_active_window)?.filter(|w| *w != 0);
        let Some(window) = active else {
            // Focus went to nothing (desktop, or the WM cleared it): re-report the next window
            // even if it is the same as before.
            self.focus.reset();
            return Ok(());
        };
        let (pid, holder) = self.resolve_pid(window)?;
        let mut app_id = self.wm_class(window)?;
        if app_id.is_none() && holder != window {
            app_id = self.wm_class(holder)?;
        }
        if pid.is_none() && app_id.is_none() {
            tracing::debug!(window, "active window has neither _NET_WM_PID nor WM_CLASS");
            return Ok(());
        }
        if self.focus.accept((pid, app_id.clone())) {
            sink(Event::Focus { ts: now_ms(), pid, app_id, target: None, source: SOURCE.into() });
        }
        Ok(())
    }

    /// `_NET_WM_PID` of `window`, else of its `WM_CLIENT_LEADER`, else of the first descendant
    /// that has one (for frame/reparented windows). Returns the pid and the window carrying it.
    fn resolve_pid(&self, window: Window) -> Result<(Option<i32>, Window), SessionError> {
        if let Some(pid) = self.pid_of(window)? {
            return Ok((Some(pid), window));
        }
        if let Some(leader) = self.window_prop(window, self.atoms.wm_client_leader)?.filter(|w| *w != 0 && *w != window)
            && let Some(pid) = self.pid_of(leader)?
        {
            return Ok((Some(pid), leader));
        }
        let mut queue = std::collections::VecDeque::from([window]);
        let mut scanned = 0;
        while let Some(w) = queue.pop_front() {
            let children = match self.conn.query_tree(w)?.reply() {
                Ok(tree) => tree.children,
                Err(ReplyError::X11Error(_)) => continue,
                Err(e) => return Err(e.into()),
            };
            for child in children {
                if let Some(pid) = self.pid_of(child)? {
                    return Ok((Some(pid), child));
                }
                scanned += 1;
                if scanned >= MAX_CHILD_SCAN {
                    return Ok((None, window));
                }
                queue.push_back(child);
            }
        }
        Ok((None, window))
    }

    fn pid_of(&self, window: Window) -> Result<Option<i32>, SessionError> {
        Ok(self
            .prop32(window, self.atoms.net_wm_pid, AtomEnum::CARDINAL.into())?
            .and_then(|pid| i32::try_from(pid).ok())
            .filter(|pid| *pid > 0))
    }

    fn window_prop(&self, window: Window, property: Atom) -> Result<Option<Window>, SessionError> {
        self.prop32(window, property, AtomEnum::WINDOW.into())
    }

    /// First 32-bit value of a property. `None` if unset, of another type, or the window is gone.
    fn prop32(&self, window: Window, property: Atom, ty: Atom) -> Result<Option<u32>, SessionError> {
        match self.conn.get_property(false, window, property, ty, 0, 1)?.reply() {
            Ok(reply) => Ok(reply.value32().and_then(|mut v| v.next())),
            Err(ReplyError::X11Error(_)) => Ok(None), // BadWindow: it went away meanwhile
            Err(e) => Err(e.into()),
        }
    }

    /// The class part of `WM_CLASS` ("instance\0class\0"), or the instance if there is no class.
    fn wm_class(&self, window: Window) -> Result<Option<String>, SessionError> {
        let reply = match self.conn.get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)?.reply() {
            Ok(r) => r,
            Err(ReplyError::X11Error(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(parse_wm_class(&reply.value))
    }
}

/// Parses a `WM_CLASS` value into the app_id hint: the class, else the instance.
pub fn parse_wm_class(raw: &[u8]) -> Option<String> {
    let mut parts = raw.split(|b| *b == 0).map(|p| String::from_utf8_lossy(p).trim().to_owned());
    let instance = parts.next().filter(|s| !s.is_empty());
    let class = parts.next().filter(|s| !s.is_empty());
    class.or(instance)
}

/// Selects XI2 raw key/button(/touch) events on the root window. `Ok(false)` if the server has
/// no XInput 2.
fn select_raw_input(conn: &RustConnection, root: Window) -> Result<bool, SessionError> {
    if conn.extension_information(xinput::X11_EXTENSION_NAME)?.is_none() {
        return Ok(false);
    }
    let version = conn.xinput_xi_query_version(2, 2)?.reply()?;
    if version.major_version < 2 {
        return Ok(false);
    }
    let mut mask = xinput::XIEventMask::RAW_KEY_PRESS | xinput::XIEventMask::RAW_BUTTON_PRESS;
    if version.minor_version >= 2 {
        mask |= xinput::XIEventMask::RAW_TOUCH_BEGIN;
    }
    let masks = [xinput::EventMask { deviceid: xinput::Device::ALL_MASTER.into(), mask: vec![mask] }];
    conn.xinput_xi_select_events(root, &masks)?.check()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wm_class_prefers_class() {
        assert_eq!(parse_wm_class(b"navigator\0firefox\0").as_deref(), Some("firefox"));
        assert_eq!(parse_wm_class(b"xterm\0").as_deref(), Some("xterm"));
        assert_eq!(parse_wm_class(b"\0Gimp\0").as_deref(), Some("Gimp"));
        assert_eq!(parse_wm_class(b""), None);
        assert_eq!(parse_wm_class(b"\0\0"), None);
    }

    #[test]
    fn config_parses_and_rejects_typos() {
        let c: X11Config = serde_json::from_value(serde_json::json!({"display": ":3", "input": false})).unwrap();
        assert_eq!(c, X11Config { display: Some(":3".into()), input: Some(false) });
        assert!(serde_json::from_value::<X11Config>(serde_json::json!({"dispaly": ":3"})).is_err());
        assert_eq!(serde_json::from_value::<X11Config>(serde_json::json!({})).unwrap(), X11Config::default());
    }

    #[test]
    fn start_without_display_is_unsupported() {
        let mut a = X11Adapter::new(X11Config { display: Some("   ".into()), input: None });
        // Whitespace display counts as unset; fall back to $DISPLAY only if present.
        if std::env::var_os("DISPLAY").is_none() {
            let err = a.start(std::sync::Arc::new(|_| {})).unwrap_err();
            assert!(matches!(err, PluginError::Unsupported(_)), "{err:?}");
        }
        // A display nobody serves, and one x11rb cannot even parse without panicking.
        for display in [":4999", ":65000"] {
            let mut a = X11Adapter::new(X11Config { display: Some(display.into()), input: None });
            let err = a.start(std::sync::Arc::new(|_| {})).unwrap_err();
            assert!(matches!(err, PluginError::Unsupported(_)), "{display}: {err:?}");
            assert!(!a.connected());
        }
    }
}
