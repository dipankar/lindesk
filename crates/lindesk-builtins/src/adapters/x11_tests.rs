//! Integration tests for the X11 adapter against a real X server (`Xvfb`). There is no window
//! manager: the test plays the WM by setting `_NET_ACTIVE_WINDOW` on the root window itself.
//! Skipped (with a note on stderr) when `Xvfb` is not on `PATH`.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lindesk_plugin_api::{Adapter, Event, EventSink};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, PropMode, Window, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::x11::{X11Adapter, X11Config};

struct Xvfb {
    child: Child,
    display: String,
}

impl Xvfb {
    /// Starts Xvfb on `display` (or a free one it picks) and waits until it accepts clients.
    fn start(display: Option<&str>) -> Option<Xvfb> {
        let found =
            std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|dir| dir.join("Xvfb").is_file()));
        if !found {
            eprintln!("Xvfb not on PATH; skipping X11 integration test");
            return None;
        }
        let mut cmd = Command::new("Xvfb");
        if let Some(d) = display {
            cmd.arg(d);
        }
        // -displayfd: Xvfb picks a free display (if none given) and writes its number to fd 1
        // once it is ready to accept connections.
        cmd.args(["-displayfd", "1", "-nolisten", "tcp", "-screen", "0", "320x240x24"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().expect("spawn Xvfb");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).expect("read Xvfb display");
        let n = line.trim();
        assert!(!n.is_empty(), "Xvfb exited before becoming ready");
        Some(Xvfb { child, display: format!(":{n}") })
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The test's own X client, acting as applications plus the window manager.
struct Wm {
    conn: RustConnection,
    root: Window,
}

impl Wm {
    fn connect(display: &str) -> Wm {
        let (conn, screen) = x11rb::connect(Some(display)).expect("connect to Xvfb");
        let root = conn.setup().roots[screen].root;
        Wm { conn, root }
    }

    fn atom(&self, name: &str) -> u32 {
        self.conn.intern_atom(false, name.as_bytes()).unwrap().reply().unwrap().atom
    }

    fn window(&self, parent: Window, pid: Option<u32>, class: Option<&str>) -> Window {
        let w = self.conn.generate_id().unwrap();
        self.conn
            .create_window(0, w, parent, 0, 0, 10, 10, 0, WindowClass::INPUT_OUTPUT, 0, &CreateWindowAux::new())
            .unwrap();
        if let Some(pid) = pid {
            let net_wm_pid = self.atom("_NET_WM_PID");
            self.conn.change_property32(PropMode::REPLACE, w, net_wm_pid, AtomEnum::CARDINAL, &[pid]).unwrap();
        }
        if let Some(class) = class {
            let value = format!("{}\0{class}\0", class.to_lowercase());
            self.conn
                .change_property8(PropMode::REPLACE, w, AtomEnum::WM_CLASS, AtomEnum::STRING, value.as_bytes())
                .unwrap();
        }
        self.conn.flush().unwrap();
        w
    }

    fn set_leader(&self, w: Window, leader: Window) {
        let atom = self.atom("WM_CLIENT_LEADER");
        self.conn.change_property32(PropMode::REPLACE, w, atom, AtomEnum::WINDOW, &[leader]).unwrap();
        self.conn.flush().unwrap();
    }

    fn activate(&self, w: Window) {
        let atom = self.atom("_NET_ACTIVE_WINDOW");
        self.conn.change_property32(PropMode::REPLACE, self.root, atom, AtomEnum::WINDOW, &[w]).unwrap();
        self.conn.sync().unwrap();
    }

    fn press_key(&self) {
        // keycode 38 is 'a' on the default Xvfb keymap; any valid keycode works.
        self.conn.xtest_fake_input(KEY_PRESS_EVENT, 38, 0, self.root, 0, 0, 0).unwrap();
        self.conn.xtest_fake_input(KEY_RELEASE_EVENT, 38, 0, self.root, 0, 0, 0).unwrap();
        self.conn.sync().unwrap();
    }
}

fn channel_sink() -> (EventSink, Receiver<Event>) {
    let (tx, rx) = channel();
    let tx = Mutex::new(tx);
    (Arc::new(move |e| drop(tx.lock().unwrap().send(e))), rx)
}

const WAIT: Duration = Duration::from_secs(5);

/// Next Focus event as (pid, app_id), ignoring other events.
fn next_focus(rx: &Receiver<Event>) -> Option<(Option<i32>, Option<String>)> {
    let deadline = Instant::now() + WAIT;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(Event::Focus { pid, app_id, source, .. }) => {
                assert_eq!(source, "x11");
                return Some((pid, app_id));
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

fn wait_until(what: &str, f: impl Fn() -> bool, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn focus(pid: i32, app: &str) -> Option<(Option<i32>, Option<String>)> {
    Some((Some(pid), Some(app.to_owned())))
}

#[test]
fn xvfb_focus_and_input() {
    let Some(xvfb) = Xvfb::start(None) else { return };
    let wm = Wm::connect(&xvfb.display);
    let one = wm.window(wm.root, Some(4242), Some("One"));
    let two = wm.window(wm.root, Some(4343), Some("Two"));
    // No pid of its own; its client leader is `one`.
    let three = wm.window(wm.root, None, Some("Three"));
    wm.set_leader(three, one);
    // A WM frame: no properties; the client inside carries pid and class.
    let frame = wm.window(wm.root, None, None);
    let _client = wm.window(frame, Some(4545), Some("Five"));

    let (sink, rx) = channel_sink();
    let mut adapter = X11Adapter::new(X11Config { display: Some(xvfb.display.clone()), input: Some(true) });
    adapter.start(sink).expect("start x11 adapter");
    assert!(adapter.connected(), "connected as soon as start() returns");

    wm.activate(one);
    assert_eq!(next_focus(&rx), focus(4242, "One"));
    wm.activate(two);
    assert_eq!(next_focus(&rx), focus(4343, "Two"));
    // Re-announcing the same window is debounced: the next event is for `three`.
    wm.activate(two);
    wm.activate(three);
    assert_eq!(next_focus(&rx), focus(4242, "Three"), "pid via WM_CLIENT_LEADER");
    wm.activate(frame);
    assert_eq!(next_focus(&rx), focus(4545, "Five"), "pid and class via child window");
    // Focus to nothing, then back to the same window: reported again.
    wm.activate(0);
    wm.activate(frame);
    assert_eq!(next_focus(&rx), focus(4545, "Five"));

    // XInput2 raw events → throttled Interaction for the focused pid.
    while rx.try_recv().is_ok() {}
    let t0 = Instant::now();
    for _ in 0..20 {
        wm.press_key();
    }
    let burst = t0.elapsed();
    std::thread::sleep(Duration::from_millis(100));
    let interactions: Vec<_> = rx
        .try_iter()
        .filter_map(|e| match e {
            Event::Interaction { pid, end, source, .. } => Some((pid, end, source)),
            _ => None,
        })
        .collect();
    assert!(!interactions.is_empty(), "no Interaction from XTEST key presses");
    assert!(interactions.iter().all(|i| *i == (Some(4545), false, "x11".to_owned())), "{interactions:?}");
    let max = (burst.as_millis() / 250 + 1) as usize;
    assert!(interactions.len() <= max, "{} interactions in {burst:?}: not throttled", interactions.len());

    let t0 = Instant::now();
    adapter.stop();
    assert!(t0.elapsed() < Duration::from_secs(1), "stop took {:?}", t0.elapsed());
    assert!(!adapter.connected());
    // Nothing is delivered after stop.
    wm.activate(one);
    std::thread::sleep(Duration::from_millis(100));
    assert!(rx.try_recv().is_err());
}

#[test]
fn xvfb_reconnects_after_server_restart() {
    let Some(mut xvfb) = Xvfb::start(None) else { return };
    let display = xvfb.display.clone();
    let (sink, rx) = channel_sink();
    let mut adapter = X11Adapter::new(X11Config { display: Some(display.clone()), input: Some(false) });
    adapter.start(sink).expect("start x11 adapter");
    wait_until("connected", || adapter.connected(), WAIT);

    xvfb.kill();
    wait_until("disconnect noticed", || !adapter.connected(), WAIT);

    let xvfb2 = Xvfb::start(Some(&display)).expect("restart Xvfb on the same display");
    assert_eq!(xvfb2.display, display);
    // Backoff is 0.5 s, 1 s, 2 s, ...
    wait_until("reconnected", || adapter.connected(), Duration::from_secs(10));

    let wm = Wm::connect(&display);
    let w = wm.window(wm.root, Some(777), Some("Again"));
    wm.activate(w);
    assert_eq!(next_focus(&rx), focus(777, "Again"));
    adapter.stop();
}
