//! Which apps are making sound (plugin `"pipewire"`).
//!
//! An app that is playing or recording must not be treated as put away, however long its
//! window has been off screen: music, a call in another workspace. PipeWire knows every
//! stream and the process behind it. The adapter runs `pw-dump --monitor`, which prints the
//! graph as JSON and then each change, keeps the audio streams that are *running*, and
//! emits `Event::Audible { pids }` whenever that set changes.
//!
//! The process of a stream is taken from its client's `pipewire.sec.pid` (what the kernel
//! says is on the other end of the socket; correct for sandboxed apps too), falling back to
//! the `application.process.id` the app declares.
//!
//! Without `pw-dump` (no PipeWire, or its tools aren't installed) [`PipeWireAdapter::start`]
//! returns [`PluginError::Unsupported`]: nothing is known to be audible, and the engine
//! treats that as "nobody is".
//!
//! Config (`InitContext::config`):
//! ```toml
//! command = "pw-dump"     # looked up in $PATH
//! ```

use std::collections::BTreeMap;
use std::io::Read;
use std::os::fd::AsFd;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use lindesk_core::clock::now_ms;
use lindesk_plugin_api::{
    Adapter, Event, EventSink, InitContext, Plugin, PluginError, PluginInfo, PluginKind, PluginResult,
};
use serde::Deserialize;
use serde_json::Value;

use super::worker::{Backoff, Control, Wake, Worker};
use super::{Debouncer, PIPEWIRE};

const SOURCE: &str = "pipewire";
/// The list is sent again this often even when nothing changed.
const RESEND: Duration = Duration::from_millis(lindesk_host::PRESENCE_RESEND_MS);
/// A dump that grows past this without completing a JSON value is not one.
const MAX_PENDING: usize = 64 << 20;

// ---------------------------------------------------------------------------------------------
// Pure state: pw-dump objects in, the set of sounding processes out.
// ---------------------------------------------------------------------------------------------

/// What is known about the PipeWire graph, as far as sound goes.
#[derive(Clone, Debug, Default)]
pub struct Graph {
    /// Node id → (the node is a running audio stream, its client's id, the pid it declares).
    streams: BTreeMap<u64, Stream>,
    /// Client id → the pid on the other end of its socket.
    clients: BTreeMap<u64, i32>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Stream {
    running: bool,
    client: Option<u64>,
    declared_pid: Option<i32>,
}

/// A property that tools write as a number or as a string.
fn number(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

impl Graph {
    /// Applies one object from a dump (the initial list and every later change have the same
    /// shape; a removed object comes with `"info": null`).
    pub fn apply(&mut self, object: &Value) {
        let Some(id) = object.get("id").and_then(Value::as_u64) else { return };
        let info = object.get("info").filter(|i| !i.is_null());
        let Some(info) = info else {
            self.streams.remove(&id);
            self.clients.remove(&id);
            return;
        };
        let kind = object.get("type").and_then(Value::as_str).unwrap_or_default();
        let props = info.get("props");
        let prop = |name: &str| props.and_then(|p| p.get(name));
        if kind.ends_with(":Client") {
            match number(prop("pipewire.sec.pid")).and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 0) {
                Some(pid) => self.clients.insert(id, pid),
                None => self.clients.remove(&id),
            };
        } else if kind.ends_with(":Node") {
            // A change may carry only what changed: without properties, a node is whatever
            // it was already known to be.
            if props.is_some() {
                let class = prop("media.class").and_then(Value::as_str).unwrap_or_default();
                // Playback and capture streams of apps; not devices, not video.
                if !(class.starts_with("Stream/") && class.ends_with("/Audio")) {
                    self.streams.remove(&id);
                    return;
                }
            } else if !self.streams.contains_key(&id) {
                return;
            }
            let known = self.streams.entry(id).or_default();
            if let Some(state) = info.get("state").and_then(Value::as_str) {
                known.running = state == "running";
            }
            if props.is_some() {
                known.client = number(prop("client.id")).and_then(|c| u64::try_from(c).ok());
                known.declared_pid =
                    number(prop("application.process.id")).and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 0);
            }
        }
    }

    /// The processes with a running audio stream, sorted.
    pub fn audible(&self) -> Vec<i32> {
        let mut pids: Vec<i32> = self
            .streams
            .values()
            .filter(|s| s.running)
            .filter_map(|s| s.client.and_then(|c| self.clients.get(&c).copied()).or(s.declared_pid))
            .collect();
        pids.sort_unstable();
        pids.dedup();
        pids
    }
}

/// Takes the complete JSON values off the front of `buf` (`pw-dump --monitor` prints one
/// array after another with nothing in between), leaving an unfinished one in place.
fn take_values(buf: &mut Vec<u8>) -> Vec<Value> {
    let mut out = Vec::new();
    let mut stream = serde_json::Deserializer::from_slice(buf).into_iter::<Value>();
    let mut consumed = 0;
    loop {
        match stream.next() {
            Some(Ok(v)) => {
                out.push(v);
                consumed = stream.byte_offset();
            }
            // An unfinished value at the end: wait for the rest.
            Some(Err(e)) if e.is_eof() => break,
            // Not JSON: drop what is there rather than choke on it forever.
            Some(Err(_)) => {
                consumed = buf.len();
                break;
            }
            None => {
                consumed = buf.len();
                break;
            }
        }
    }
    buf.drain(..consumed);
    if buf.len() > MAX_PENDING {
        buf.clear();
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PipeWireConfig {
    /// The `pw-dump` binary.
    pub command: String,
}

impl Default for PipeWireConfig {
    fn default() -> Self {
        PipeWireConfig { command: "pw-dump".into() }
    }
}

/// See the [module docs](self).
#[derive(Default)]
pub struct PipeWireAdapter {
    config: PipeWireConfig,
    worker: Option<Worker>,
}

impl PipeWireAdapter {
    pub fn new(config: PipeWireConfig) -> Self {
        PipeWireAdapter { config, worker: None }
    }
}

impl Plugin for PipeWireAdapter {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: PIPEWIRE.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Adapter],
            description: "Which apps are playing or recording sound, from PipeWire (pw-dump)".into(),
        }
    }

    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
        if !ctx.config.is_null() {
            self.config = PipeWireConfig::deserialize(&ctx.config)
                .map_err(|e| PluginError::Failed(format!("invalid pipewire adapter config: {e}")))?;
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.stop();
    }
}

fn spawn(command: &str) -> std::io::Result<Child> {
    Command::new(command)
        .args(["--monitor", "--no-colors"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

impl Adapter for PipeWireAdapter {
    fn start(&mut self, sink: EventSink) -> PluginResult<()> {
        if self.worker.as_ref().is_some_and(Worker::is_running) {
            return Ok(());
        }
        let command = self.config.command.clone();
        let child = spawn(&command)
            .map_err(|e| PluginError::Unsupported(format!("cannot run {command} (is PipeWire installed?): {e}")))?;
        let worker = Worker::spawn("lindesk-pipewire", true, move |ctl| run(ctl, child, &command, &sink))?;
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

fn run(ctl: &Control, first: Child, command: &str, sink: &EventSink) {
    let mut backoff = Backoff::default();
    let mut child = Some(first);
    loop {
        let mut c = match child.take() {
            Some(c) => c,
            None => match spawn(command) {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!(error = %e, "pipewire: cannot start {command}");
                    if ctl.sleep(backoff.next_delay()) {
                        return;
                    }
                    continue;
                }
            },
        };
        ctl.set_connected(true);
        let stopped = follow(ctl, &mut c, sink, &mut backoff);
        ctl.set_connected(false);
        let _ = c.kill();
        let _ = c.wait();
        if stopped {
            return;
        }
        // pw-dump exits when PipeWire restarts. Until it is back nothing is known to sound.
        sink(Event::Audible { ts: now_ms(), pids: Vec::new(), source: SOURCE.into() });
        if ctl.sleep(backoff.next_delay()) {
            return;
        }
    }
}

/// Reads the dump until the child exits (false) or the adapter is stopped (true).
fn follow(ctl: &Control, child: &mut Child, sink: &EventSink, backoff: &mut Backoff) -> bool {
    let Some(mut out) = child.stdout.take() else { return false };
    let mut graph = Graph::default();
    let mut sent: Debouncer<Vec<i32>> = Debouncer::new();
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match ctl.wait(Some(out.as_fd()), Some(RESEND)) {
            Ok(Wake::Stopped) | Err(_) => return true,
            Ok(Wake::Timeout) => {
                sink(Event::Audible { ts: now_ms(), pids: graph.audible(), source: SOURCE.into() });
                continue;
            }
            Ok(Wake::Ready) => {}
        }
        let n = match out.read(&mut chunk) {
            Ok(0) | Err(_) => return false,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        let values = take_values(&mut buf);
        if values.is_empty() {
            continue;
        }
        // It talks: the next restart starts from a short delay again.
        backoff.reset();
        for v in &values {
            for object in v.as_array().into_iter().flatten() {
                graph.apply(object);
            }
        }
        let pids = graph.audible();
        if sent.accept(pids.clone()) {
            tracing::trace!(?pids, "pipewire: audible processes");
            sink(Event::Audible { ts: now_ms(), pids, source: SOURCE.into() });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    fn node(id: u64, class: &str, state: &str, client: u64, pid: Value) -> Value {
        json!({"id": id, "type": "PipeWire:Interface:Node", "info": {
            "state": state,
            "props": {"media.class": class, "client.id": client, "application.process.id": pid, "node.name": "x"}
        }})
    }

    fn client(id: u64, pid: i32) -> Value {
        json!({"id": id, "type": "PipeWire:Interface:Client", "info": {"props": {"pipewire.sec.pid": pid}}})
    }

    #[test]
    fn running_audio_streams_and_whose_they_are() {
        let mut g = Graph::default();
        for o in [
            client(40, 4242),
            client(41, 500),
            // Playing, declared pid is a sandbox's view: the client's real pid wins.
            node(60, "Stream/Output/Audio", "running", 40, json!(2)),
            // A paused player, a microphone in use by a client without a socket pid, a
            // camera, a sound card: only the microphone counts.
            node(61, "Stream/Output/Audio", "idle", 41, json!(500)),
            node(62, "Stream/Input/Audio", "running", 99, json!("777")),
            node(63, "Stream/Output/Video", "running", 41, json!(500)),
            node(64, "Audio/Sink", "running", 41, json!(1)),
        ] {
            g.apply(&o);
        }
        assert_eq!(g.audible(), [777, 4242]);

        // The paused one starts playing: a change carrying only the state.
        g.apply(&json!({"id": 61, "type": "PipeWire:Interface:Node", "info": {"state": "running"}}));
        assert_eq!(g.audible(), [500, 777, 4242]);
        // The first stream goes away; then its client.
        g.apply(&json!({"id": 60, "info": null}));
        g.apply(&json!({"id": 40, "info": null}));
        assert_eq!(g.audible(), [500, 777]);
        g.apply(&json!({"no": "id"}));
        g.apply(&node(62, "Stream/Input/Audio", "suspended", 99, json!("777")));
        assert_eq!(g.audible(), [500]);
    }

    #[test]
    fn values_are_taken_as_they_complete() {
        let mut buf = br#"[{"id": 1}]
[{"id": 2}, {"id""#
            .to_vec();
        let v = take_values(&mut buf);
        assert_eq!(v, [json!([{"id": 1}])]);
        buf.extend_from_slice(br#": 3}]"#);
        assert_eq!(take_values(&mut buf), [json!([{"id": 2}, {"id": 3}])]);
        assert!(buf.iter().all(u8::is_ascii_whitespace));
        let mut junk = b"pw-dump: can't connect\n".to_vec();
        assert!(take_values(&mut junk).is_empty() && junk.is_empty(), "not JSON: dropped");
    }

    /// End to end against a stand-in for `pw-dump --monitor`: a script that prints a graph,
    /// then a change, then stays alive.
    #[test]
    fn follows_a_dump_and_stops_cleanly() {
        let dir = crate::actuators::test_dir();
        let script = dir.path().join("pw-dump");
        let first = json!([client(40, 4242), node(60, "Stream/Output/Audio", "running", 40, json!(4242))]);
        let second = json!([{"id": 60, "type": "PipeWire:Interface:Node", "info": {"state": "idle"}}]);
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' '{first}'\nsleep 0.3\nprintf '%s\\n' '{second}'\nexec sleep 600\n"),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let got: Arc<Mutex<Vec<Vec<i32>>>> = Arc::default();
        let sink: EventSink = {
            let got = Arc::clone(&got);
            Arc::new(move |e| {
                if let Event::Audible { pids, source, .. } = e {
                    assert_eq!(source, SOURCE);
                    got.lock().unwrap().push(pids);
                }
            })
        };
        let mut a = PipeWireAdapter::new(PipeWireConfig { command: script.to_string_lossy().into_owned() });
        a.start(sink).unwrap();
        let t0 = Instant::now();
        while got.lock().unwrap().len() < 2 {
            assert!(t0.elapsed() < Duration::from_secs(30), "timed out: {:?}", got.lock().unwrap());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(*got.lock().unwrap(), [vec![4242], vec![]]);
        assert!(a.connected());
        a.stop();
        assert!(!a.connected());

        // No such program: the host skips the adapter.
        let mut missing = PipeWireAdapter::new(PipeWireConfig { command: "/nonexistent/pw-dump".into() });
        assert!(matches!(missing.start(Arc::new(|_| {})), Err(PluginError::Unsupported(_))));
    }
}
