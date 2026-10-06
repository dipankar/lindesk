//! Hyprland foreground adapter (plugin `"hyprland"`).
//!
//! Hyprland publishes window events on a Unix socket
//! (`$XDG_RUNTIME_DIR/hypr/<instance>/.socket2.sock`, one `EVENT>>DATA` line each) and answers
//! queries on a second one (`.socket.sock`). On `activewindowv2` the adapter asks for
//! `j/activewindow`, which carries what Wayland itself withholds: the window's pid. It emits
//! `Event::Focus { pid, app_id: class, source: "hyprland" }`, so the engine finds the unit
//! from the process (apps started through `uwsm app` each have a scope) and falls back to the
//! app id.
//!
//! It also reports which apps have a window on screen (`Event::Visible`): the windows on
//! the workspaces the monitors are showing, from `j/monitors` and `j/clients`, whenever a
//! workspace, monitor or window event says that may have changed. That is what lets apps
//! on other workspaces nap.
//!
//! Hyprland's IPC says nothing about input; the [`wayland-input`](super::wl_input) adapter
//! runs next to this one for that.
//!
//! The instance is `$HYPRLAND_INSTANCE_SIGNATURE` when its socket exists, otherwise the most
//! recently started instance under `$XDG_RUNTIME_DIR/hypr`. The adapter keeps looking, so it
//! works when the agent starts before the compositor, when the variable never reached the
//! systemd user environment, and after Hyprland restarts under a new signature.
//!
//! Config (`InitContext::config`):
//! ```toml
//! instance = "..."            # instance signature; default: $HYPRLAND_INSTANCE_SIGNATURE, else the newest
//! runtime_dir = "/run/user/1000"   # default: $XDG_RUNTIME_DIR
//! ```

use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use lindesk_core::clock::now_ms;
use lindesk_plugin_api::{
    Adapter, Event, EventSink, InitContext, Plugin, PluginError, PluginInfo, PluginKind, PluginResult,
};
use serde::Deserialize;

use super::worker::{Backoff, Control, Wake, Worker};
use super::{Debouncer, HYPRLAND};

const SOURCE: &str = "hyprland";
/// The event socket in an instance directory.
pub const EVENT_SOCKET: &str = ".socket2.sock";
/// The request socket in an instance directory.
pub const REQUEST_SOCKET: &str = ".socket.sock";
/// How long a request may take. Hyprland answers from its main loop, so a stalled
/// compositor must not stall the adapter with it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
/// The list of visible apps is sent again this often even when nothing changed.
const RESEND: Duration = Duration::from_millis(lindesk_host::PRESENCE_RESEND_MS);
/// An event line longer than this is not one of ours (window titles are capped far below).
const MAX_LINE: usize = 64 * 1024;

// ---------------------------------------------------------------------------------------------
// Pure parts: event lines, the activewindow reply, picking an instance.
// ---------------------------------------------------------------------------------------------

/// True for an event line that says focus moved: `activewindowv2>>ADDRESS` (the address is
/// empty when nothing has focus). `activewindow>>class,title` precedes it and says less.
pub fn is_focus_event(line: &str) -> bool {
    line.split_once(">>").is_some_and(|(event, _)| event == "activewindowv2")
}

/// True for an event line after which a different set of windows may be on screen.
pub fn is_layout_event(line: &str) -> bool {
    const EVENTS: [&str; 12] = [
        "workspace",
        "workspacev2",
        "focusedmon",
        "focusedmonv2",
        "activespecial",
        "activespecialv2",
        "openwindow",
        "closewindow",
        "movewindow",
        "movewindowv2",
        "monitoradded",
        "monitorremoved",
    ];
    line.split_once(">>").is_some_and(|(event, _)| EVENTS.contains(&event))
}

#[derive(Deserialize, Default)]
struct RawWorkspace {
    #[serde(default)]
    id: i64,
}

#[derive(Deserialize)]
struct RawMonitor {
    #[serde(default, rename = "activeWorkspace")]
    active: RawWorkspace,
    #[serde(default, rename = "specialWorkspace")]
    special: RawWorkspace,
}

#[derive(Deserialize)]
struct RawClient {
    #[serde(default)]
    pid: i64,
    #[serde(default)]
    class: String,
    #[serde(default = "yes")]
    mapped: bool,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    workspace: RawWorkspace,
}

fn yes() -> bool {
    true
}

/// The apps with a window on screen, from the `j/monitors` and `j/clients` replies: mapped,
/// not hidden (a tab behind another in a group), on a workspace some monitor is showing
/// (its regular one, or a special workspace toggled over it). Returns their pids, and the
/// classes of windows Hyprland has no pid for.
pub fn visible_apps(monitors: &str, clients: &str) -> Result<(Vec<i32>, Vec<String>), String> {
    let monitors: Vec<RawMonitor> =
        serde_json::from_str(monitors.trim()).map_err(|e| format!("unexpected monitors reply: {e}"))?;
    let clients: Vec<RawClient> =
        serde_json::from_str(clients.trim()).map_err(|e| format!("unexpected clients reply: {e}"))?;
    // A monitor without a special workspace open reports id 0 for it, which no window is on.
    let shown: Vec<i64> = monitors.iter().flat_map(|m| [m.active.id, m.special.id]).filter(|id| *id != 0).collect();
    let (mut pids, mut classes) = (Vec::new(), Vec::new());
    for c in clients.iter().filter(|c| c.mapped && !c.hidden && shown.contains(&c.workspace.id)) {
        match i32::try_from(c.pid).ok().filter(|p| *p > 0) {
            Some(pid) => pids.push(pid),
            None if !c.class.trim().is_empty() => classes.push(c.class.trim().to_owned()),
            None => {}
        }
    }
    pids.sort_unstable();
    pids.dedup();
    classes.sort_unstable();
    classes.dedup();
    Ok((pids, classes))
}

/// The focused window, from a `j/activewindow` reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Focused {
    /// Hyprland's window address (`0x...`), stable for the life of the window.
    pub address: String,
    pub pid: Option<i32>,
    /// The window class (Wayland app id, or `WM_CLASS` for Xwayland windows).
    pub app_id: Option<String>,
}

#[derive(Deserialize)]
struct RawWindow {
    #[serde(default)]
    address: String,
    #[serde(default)]
    pid: i64,
    #[serde(default)]
    class: String,
    #[serde(default, rename = "initialClass")]
    initial_class: String,
}

/// Parses a `j/activewindow` reply. `Ok(None)` when nothing has focus (Hyprland answers `{}`).
pub fn parse_active_window(reply: &str) -> Result<Option<Focused>, String> {
    let raw: RawWindow =
        serde_json::from_str(reply.trim()).map_err(|e| format!("unexpected activewindow reply: {e}"))?;
    if raw.address.is_empty() {
        return Ok(None);
    }
    let class = [raw.class, raw.initial_class].into_iter().map(|c| c.trim().to_owned()).find(|c| !c.is_empty());
    Ok(Some(Focused {
        address: raw.address,
        // Hyprland reports -1 when it cannot tell (some Xwayland windows).
        pid: i32::try_from(raw.pid).ok().filter(|p| *p > 0),
        app_id: class,
    }))
}

/// The instance directory to use under `<runtime_dir>/hypr`: `wanted` when it has an event
/// socket, else the instance whose event socket is newest.
pub fn find_instance(runtime_dir: &Path, wanted: Option<&str>) -> Option<PathBuf> {
    let root = runtime_dir.join("hypr");
    if let Some(w) = wanted.map(str::trim).filter(|w| !w.is_empty() && !w.contains('/')) {
        let dir = root.join(w);
        if dir.join(EVENT_SOCKET).exists() {
            return Some(dir);
        }
    }
    std::fs::read_dir(&root)
        .ok()?
        .filter_map(|e| {
            let dir = e.ok()?.path();
            let started = std::fs::metadata(dir.join(EVENT_SOCKET)).ok()?.modified().ok()?;
            Some((started, dir))
        })
        .max()
        .map(|(_, dir)| dir)
}

/// True when a Hyprland instance under `runtime_dir` accepts connections. Sockets of a
/// compositor that is gone can linger, so this connects rather than looks.
pub fn is_live(runtime_dir: &Path, wanted: Option<&str>) -> bool {
    find_instance(runtime_dir, wanted).is_some_and(|dir| UnixStream::connect(dir.join(EVENT_SOCKET)).is_ok())
}

/// Splits complete lines off the front of `buf`, leaving a trailing partial line in place.
fn take_lines(buf: &mut Vec<u8>) -> Vec<String> {
    let Some(end) = buf.iter().rposition(|b| *b == b'\n') else {
        if buf.len() > MAX_LINE {
            buf.clear();
        }
        return Vec::new();
    };
    let rest = buf.split_off(end + 1);
    let lines = String::from_utf8_lossy(buf).lines().map(str::to_owned).collect();
    *buf = rest;
    lines
}

// ---------------------------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct HyprlandConfig {
    /// Instance signature. Defaults to `$HYPRLAND_INSTANCE_SIGNATURE`, else the newest instance.
    pub instance: Option<String>,
    /// Defaults to `$XDG_RUNTIME_DIR`.
    pub runtime_dir: Option<PathBuf>,
}

/// See the [module docs](self).
#[derive(Default)]
pub struct HyprlandAdapter {
    config: HyprlandConfig,
    worker: Option<Worker>,
}

impl HyprlandAdapter {
    pub fn new(config: HyprlandConfig) -> Self {
        HyprlandAdapter { config, worker: None }
    }
}

impl Plugin for HyprlandAdapter {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: HYPRLAND.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Adapter],
            description: "Focus (pid and class) from Hyprland's event and request sockets".into(),
        }
    }

    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
        if !ctx.config.is_null() {
            self.config = HyprlandConfig::deserialize(&ctx.config)
                .map_err(|e| PluginError::Failed(format!("invalid hyprland adapter config: {e}")))?;
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.stop();
    }
}

impl Adapter for HyprlandAdapter {
    fn start(&mut self, sink: EventSink) -> PluginResult<()> {
        if self.worker.as_ref().is_some_and(Worker::is_running) {
            return Ok(());
        }
        let runtime_dir = self
            .config
            .runtime_dir
            .clone()
            .or_else(|| std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))
            .ok_or_else(|| PluginError::Unsupported("XDG_RUNTIME_DIR is unset".into()))?;
        let wanted = self
            .config
            .instance
            .clone()
            .or_else(|| std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok())
            .filter(|s| !s.trim().is_empty());
        let worker = Worker::spawn("lindesk-hyprland", false, move |ctl| {
            run(ctl, &runtime_dir, wanted.as_deref(), &sink);
        })?;
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

fn run(ctl: &Control, runtime_dir: &Path, wanted: Option<&str>, sink: &EventSink) {
    // The compositor may simply not be up yet: keep the retries short.
    let mut backoff = Backoff::new(Duration::from_millis(500), Duration::from_secs(5));
    let mut announced = false;
    loop {
        match Session::connect(runtime_dir, wanted) {
            Ok(session) => {
                backoff.reset();
                announced = false;
                tracing::info!(instance = %session.dir.display(), "hyprland adapter connected");
                ctl.set_connected(true);
                let result = session.run(ctl, sink);
                ctl.set_connected(false);
                match result {
                    Ok(()) => return,
                    Err(e) => tracing::warn!(error = %e, "hyprland event socket closed; reconnecting"),
                }
            }
            Err(e) => {
                if !announced {
                    tracing::info!(error = %e, "hyprland adapter is waiting for the compositor");
                    announced = true;
                }
            }
        }
        if ctl.sleep(backoff.next_delay()) {
            return;
        }
    }
}

struct Session {
    dir: PathBuf,
    events: UnixStream,
    buf: Vec<u8>,
    focus: Debouncer<(String, Option<i32>)>,
    visible: Debouncer<(Vec<i32>, Vec<String>)>,
}

impl Session {
    fn connect(runtime_dir: &Path, wanted: Option<&str>) -> io::Result<Session> {
        let dir = find_instance(runtime_dir, wanted).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no Hyprland instance under {}/hypr", runtime_dir.display()),
            )
        })?;
        let events = UnixStream::connect(dir.join(EVENT_SOCKET))?;
        Ok(Session { dir, events, buf: Vec::new(), focus: Debouncer::new(), visible: Debouncer::new() })
    }

    /// `Ok(())` when stopped, `Err` when the event socket broke.
    fn run(mut self, ctl: &Control, sink: &EventSink) -> io::Result<()> {
        // Whatever has focus and is on screen now: there may be no event for a long while.
        self.report(sink);
        self.report_visible(sink);
        let mut chunk = [0u8; 4096];
        loop {
            match ctl.wait(Some(self.events.as_fd()), Some(RESEND))? {
                Wake::Stopped => return Ok(()),
                // The host forgets a list that isn't sent again now and then.
                Wake::Timeout => {
                    self.visible.reset();
                    self.report_visible(sink);
                    continue;
                }
                Wake::Ready => {}
            }
            let n = match self.events.read(&mut chunk) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Hyprland closed the event socket")),
                Ok(n) => n,
                Err(e) if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => continue,
                Err(e) => return Err(e),
            };
            self.buf.extend_from_slice(&chunk[..n]);
            // However many events arrived together, only where things ended up matters.
            let lines = take_lines(&mut self.buf);
            if lines.iter().any(|l| is_focus_event(l)) {
                self.report(sink);
            }
            if lines.iter().any(|l| is_layout_event(l)) {
                self.report_visible(sink);
            }
        }
    }

    fn report(&mut self, sink: &EventSink) {
        let focused =
            match request(&self.dir, "j/activewindow").map_err(|e| e.to_string()).and_then(|r| parse_active_window(&r))
            {
                Ok(f) => f,
                Err(e) => {
                    tracing::debug!(error = %e, "hyprland: activewindow request failed");
                    return;
                }
            };
        let Some(f) = focused else {
            // An empty workspace: report the next focus even if it is the same window.
            self.focus.reset();
            return;
        };
        if f.pid.is_none() && f.app_id.is_none() {
            return;
        }
        if self.focus.accept((f.address.clone(), f.pid)) {
            tracing::trace!(pid = ?f.pid, app_id = ?f.app_id, "hyprland focus");
            sink(Event::Focus { ts: now_ms(), pid: f.pid, app_id: f.app_id, target: None, source: SOURCE.into() });
        }
    }
}

impl Session {
    fn report_visible(&mut self, sink: &EventSink) {
        let apps = request(&self.dir, "j/monitors")
            .and_then(|m| Ok((m, request(&self.dir, "j/clients")?)))
            .map_err(|e| e.to_string())
            .and_then(|(m, c)| visible_apps(&m, &c));
        match apps {
            Ok(apps) => {
                if self.visible.accept(apps.clone()) {
                    tracing::trace!(pids = ?apps.0, "hyprland visible apps");
                    sink(Event::Visible { ts: now_ms(), pids: apps.0, app_ids: apps.1, source: SOURCE.into() });
                }
            }
            Err(e) => tracing::debug!(error = %e, "hyprland: listing windows failed"),
        }
    }
}

/// One request on the request socket: Hyprland answers and closes the connection.
fn request(dir: &Path, command: &str) -> io::Result<String> {
    let mut s = UnixStream::connect(dir.join(REQUEST_SOCKET))?;
    s.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    s.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    s.write_all(command.as_bytes())?;
    let mut reply = String::new();
    s.read_to_string(&mut reply)?;
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    const REPLY: &str = r#"{
        "address": "0x5589f0a1c2d0", "mapped": true, "hidden": false, "at": [8, 44], "size": [1904, 1028],
        "workspace": {"id": 1, "name": "1"}, "floating": false, "monitor": 0,
        "class": "chromium", "title": "lindesk - Chromium", "initialClass": "chromium", "initialTitle": "Chromium",
        "pid": 4242, "xwayland": false, "pinned": false, "fullscreen": 0, "focusHistoryID": 0
    }"#;

    const MONITORS: &str = r#"[
        {"id": 0, "name": "DP-1", "activeWorkspace": {"id": 1, "name": "1"}, "specialWorkspace": {"id": 0, "name": ""}},
        {"id": 1, "name": "DP-2", "activeWorkspace": {"id": 3, "name": "3"}, "specialWorkspace": {"id": -98, "name": "special:notes"}}
    ]"#;
    const CLIENTS: &str = r#"[
        {"address": "0x1", "mapped": true, "hidden": false, "workspace": {"id": 1, "name": "1"}, "class": "chromium", "pid": 4242},
        {"address": "0x2", "mapped": true, "hidden": false, "workspace": {"id": 1, "name": "1"}, "class": "chromium", "pid": 4242},
        {"address": "0x3", "mapped": true, "hidden": false, "workspace": {"id": 2, "name": "2"}, "class": "Slack", "pid": 500},
        {"address": "0x4", "mapped": true, "hidden": false, "workspace": {"id": 3, "name": "3"}, "class": "Alacritty", "pid": 77},
        {"address": "0x5", "mapped": true, "hidden": true, "workspace": {"id": 3, "name": "3"}, "class": "Alacritty", "pid": 78},
        {"address": "0x6", "mapped": true, "hidden": false, "workspace": {"id": -98, "name": "special:notes"}, "class": "obsidian", "pid": 900},
        {"address": "0x7", "mapped": true, "hidden": false, "workspace": {"id": -99, "name": "special:other"}, "class": "spotify", "pid": 901},
        {"address": "0x8", "mapped": true, "hidden": false, "workspace": {"id": 1, "name": "1"}, "class": "steam_app_1", "pid": -1},
        {"address": "0x9", "mapped": false, "hidden": false, "workspace": {"id": 1, "name": "1"}, "class": "ghost", "pid": 902}
    ]"#;

    #[test]
    fn windows_on_the_shown_workspaces_are_visible() {
        let (pids, classes) = visible_apps(MONITORS, CLIENTS).unwrap();
        // Workspaces 1 and 3 and the open special one; not workspace 2, the closed special
        // workspace, the tab hidden in a group, or the unmapped window.
        assert_eq!(pids, [77, 900, 4242]);
        assert_eq!(classes, ["steam_app_1"], "a window without a pid is known by its class");
        assert_eq!(visible_apps("[]", CLIENTS).unwrap(), (vec![], vec![]), "no monitor: nothing on screen");
        assert!(visible_apps("unknown request", "[]").is_err());
        for line in [
            "workspace>>2",
            "workspacev2>>2,2",
            "openwindow>>5589,1,kitty,~",
            "closewindow>>5589",
            "activespecial>>special:notes,DP-2",
        ] {
            assert!(is_layout_event(line), "{line}");
        }
        assert!(!is_layout_event("activewindowv2>>5589") && !is_layout_event("windowtitle>>5589"));
    }

    #[test]
    fn focus_events() {
        assert!(is_focus_event("activewindowv2>>5589f0a1c2d0"));
        assert!(is_focus_event("activewindowv2>>"), "nothing focused is a focus change too");
        assert!(!is_focus_event("activewindow>>chromium,lindesk - Chromium"));
        assert!(!is_focus_event("openwindow>>5589f0a1c2d0,1,kitty,activewindowv2>>x"));
        assert!(!is_focus_event("workspace>>2"));
        assert!(!is_focus_event(""));
    }

    #[test]
    fn active_window_replies() {
        let f = parse_active_window(REPLY).unwrap().unwrap();
        assert_eq!(f, Focused { address: "0x5589f0a1c2d0".into(), pid: Some(4242), app_id: Some("chromium".into()) });
        assert_eq!(parse_active_window("{}\n").unwrap(), None);
        // An Xwayland window Hyprland has no pid for, and one that has not set its class yet.
        let f =
            parse_active_window(r#"{"address":"0x1","pid":-1,"class":"","initialClass":"steam"}"#).unwrap().unwrap();
        assert_eq!((f.pid, f.app_id.as_deref()), (None, Some("steam")));
        assert!(parse_active_window("unknown request").is_err());
    }

    #[test]
    fn lines_are_split_across_reads() {
        let mut buf = b"workspace>>1\nactivewindowv2>>55".to_vec();
        assert_eq!(take_lines(&mut buf), ["workspace>>1"]);
        assert_eq!(buf, b"activewindowv2>>55");
        buf.extend_from_slice(b"89\n");
        assert_eq!(take_lines(&mut buf), ["activewindowv2>>5589"]);
        assert!(buf.is_empty());
        let mut junk = vec![b'x'; MAX_LINE + 1];
        assert!(take_lines(&mut junk).is_empty() && junk.is_empty(), "an endless line is dropped");
    }

    fn instance(runtime: &Path, name: &str) -> (PathBuf, UnixListener, UnixListener) {
        let dir = runtime.join("hypr").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let events = UnixListener::bind(dir.join(EVENT_SOCKET)).unwrap();
        let requests = UnixListener::bind(dir.join(REQUEST_SOCKET)).unwrap();
        (dir, events, requests)
    }

    #[test]
    fn instance_selection() {
        let tmp = crate::actuators::test_dir();
        assert_eq!(find_instance(tmp.path(), None), None);
        let (old, _e1, _r1) = instance(tmp.path(), "old_1_2");
        std::fs::create_dir_all(tmp.path().join("hypr/no-sockets")).unwrap();
        assert_eq!(find_instance(tmp.path(), None).as_deref(), Some(old.as_path()));
        std::thread::sleep(Duration::from_millis(20));
        let (new, e2, _r2) = instance(tmp.path(), "new_3_4");
        assert_eq!(find_instance(tmp.path(), None).as_deref(), Some(new.as_path()), "the newest instance");
        assert_eq!(find_instance(tmp.path(), Some("old_1_2")).as_deref(), Some(old.as_path()));
        assert_eq!(find_instance(tmp.path(), Some("gone")).as_deref(), Some(new.as_path()), "a stale signature");
        assert_eq!(find_instance(tmp.path(), Some("../x")).as_deref(), Some(new.as_path()));
        assert!(is_live(tmp.path(), None));
        // A compositor that is gone leaves its socket file behind.
        drop(e2);
        assert!(!is_live(tmp.path(), Some("new_3_4")));
    }

    /// Reported visible apps: pids, and classes of windows without one.
    type Apps = (Vec<i32>, Vec<String>);

    /// A reported focus: pid and app id.
    type Seen = (Option<i32>, Option<String>);

    /// A stand-in compositor: answers `j/activewindow` from `window`, noting each answer in
    /// `sent`. The test writes the event lines itself.
    fn serve(requests: UnixListener, window: Arc<Mutex<String>>, sent: Arc<Mutex<Vec<String>>>) {
        std::thread::spawn(move || {
            for conn in requests.incoming() {
                let Ok(mut c) = conn else { return };
                let mut cmd = [0u8; 64];
                let n = c.read(&mut cmd).unwrap_or(0);
                let reply = match &cmd[..n] {
                    b"j/activewindow" => window.lock().unwrap().clone(),
                    b"j/monitors" => MONITORS.to_owned(),
                    b"j/clients" => CLIENTS.to_owned(),
                    _ => "unknown request".to_owned(),
                };
                let _ = c.write_all(reply.as_bytes());
                if &cmd[..n] == b"j/activewindow" {
                    sent.lock().unwrap().push(reply);
                }
            }
        });
    }

    fn wait_for<T>(mut f: impl FnMut() -> Option<T>) -> T {
        let t0 = Instant::now();
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(t0.elapsed() < Duration::from_secs(20), "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn reports_focus_with_pid_and_survives_a_compositor_restart() {
        let tmp = crate::actuators::test_dir();
        let got: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let visible: Arc<Mutex<Vec<Apps>>> = Arc::default();
        let sink: EventSink = {
            let got = Arc::clone(&got);
            let visible = Arc::clone(&visible);
            Arc::new(move |e| match e {
                Event::Focus { pid, app_id, source, .. } => {
                    assert_eq!(source, SOURCE);
                    got.lock().unwrap().push((pid, app_id));
                }
                Event::Visible { pids, app_ids, .. } => visible.lock().unwrap().push((pids, app_ids)),
                _ => {}
            })
        };
        let seen = |n: usize| wait_for(|| Some(got.lock().unwrap().clone()).filter(|g| g.len() >= n));

        // The agent is up before the compositor.
        let mut a = HyprlandAdapter::new(HyprlandConfig {
            instance: Some("first".into()),
            runtime_dir: Some(tmp.path().to_path_buf()),
        });
        a.start(sink).unwrap();
        assert!(!a.connected());

        let window = Arc::new(Mutex::new(REPLY.to_owned()));
        let sent: Arc<Mutex<Vec<String>>> = Arc::default();
        let (dir, events, requests) = instance(tmp.path(), "first");
        serve(requests, Arc::clone(&window), Arc::clone(&sent));
        let (mut ev, _) = events.accept().unwrap();
        // The window focused at connect time is reported without any event.
        assert_eq!(seen(1), [(Some(4242), Some("chromium".to_owned()))]);
        assert!(a.connected());
        // So is what is on screen; a workspace switch that changes nothing is not reported again.
        let on_screen = wait_for(|| visible.lock().unwrap().first().cloned());
        assert_eq!(on_screen, (vec![77, 900, 4242], vec!["steam_app_1".to_owned()]));
        ev.write_all(b"workspace>>1\n").unwrap();

        // Focus moves; the reply decides, however many events came in one read.
        *window.lock().unwrap() = r#"{"address":"0x2","pid":77,"class":"Alacritty"}"#.into();
        ev.write_all(b"activewindow>>Alacritty,~\nactivewindowv2>>2\nactivewindowv2>>2\n").unwrap();
        assert_eq!(seen(2)[1], (Some(77), Some("Alacritty".to_owned())));
        // The same window announced again is not a change; an empty workspace in between is.
        ev.write_all(b"activewindowv2>>2\n").unwrap();
        *window.lock().unwrap() = "{}".into();
        ev.write_all(b"activewindowv2>>\n").unwrap();
        wait_for(|| sent.lock().unwrap().iter().any(|r| r == "{}").then_some(()));
        *window.lock().unwrap() = r#"{"address":"0x2","pid":77,"class":"Alacritty"}"#.into();
        ev.write_all(b"activewindowv2>>2\n").unwrap();
        assert_eq!(seen(3)[2], (Some(77), Some("Alacritty".to_owned())));
        assert_eq!(got.lock().unwrap().len(), 3);

        // Hyprland restarts under a new signature; the configured one is stale now.
        drop(ev);
        drop(events);
        std::fs::remove_dir_all(&dir).unwrap();
        let (_dir2, events2, requests2) = instance(tmp.path(), "second");
        *window.lock().unwrap() = r#"{"address":"0x9","pid":900,"class":"firefox"}"#.into();
        serve(requests2, Arc::clone(&window), sent);
        let (_ev2, _) = events2.accept().unwrap();
        assert_eq!(seen(4)[3], (Some(900), Some("firefox".to_owned())));

        a.stop();
        assert!(!a.connected());
    }

    #[test]
    fn config_and_missing_runtime_dir() {
        let mut a = HyprlandAdapter::default();
        let ctx = |config| InitContext {
            api_version: lindesk_plugin_api::API_VERSION,
            host: "test".into(),
            privilege: lindesk_plugin_api::Privilege::User,
            uid: Some(1000),
            caps: Default::default(),
            config,
        };
        assert!(a.init(&ctx(serde_json::json!({"instance": "abc", "runtime_dir": "/run/user/1000"}))).is_ok());
        assert_eq!(a.config.instance.as_deref(), Some("abc"));
        assert!(a.init(&ctx(serde_json::json!({"display": "wayland-1"}))).is_err());
        assert_eq!(a.info().name, HYPRLAND);
    }
}
