//! Live data shared by `status`, `watch` and `events`: a persistent D-Bus client, the event
//! stream (the `Event` signal with `RecentEvents` backfill, or status diffs from older
//! daemons), system PSI with history, and the foreground cgroup's own stall.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use lindesk_host::Status;
use lindesk_plugin_api::{Event, Level, Manager, Resource, Target};
use lindesk_sys::psi::{PsiReading, system_path};
use serde_json::Value;
use zbus::blocking::Connection;

use crate::bus::{self, Daemon};

// ------------------------------------------------------------------------------- events

/// One line of an event feed.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct FeedItem {
    pub ts: i64,
    /// `level`, `action_applied`, `focus`, … (the event's `kind`).
    pub kind: String,
    pub text: String,
    /// 0 = info, 1 = notable (level up, throttle), 2 = error.
    pub severity: u8,
    /// The raw event JSON.
    #[serde(skip)]
    pub raw: Value,
}

fn target_label(t: &Target) -> String {
    match t {
        Target::Unit { name, .. } => name.clone(),
        other => other.to_string(),
    }
}

/// Describe an event for humans.
pub fn describe(raw: &Value) -> FeedItem {
    let ts = raw.get("ts").and_then(Value::as_i64).unwrap_or(0);
    let kind = raw.get("kind").and_then(Value::as_str).unwrap_or("event").to_string();
    let (text, severity) = match serde_json::from_value::<Event>(raw.clone()) {
        Ok(e) => match e {
            Event::LevelChanged { from, to, policy, .. } => (
                format!("level {} → {}  ({policy})", from.as_str().to_uppercase(), to.as_str().to_uppercase()),
                u8::from(to > from),
            ),
            Event::ActionApplied { target, setting, provenance, .. } => {
                let mut t = format!("apply  {} on {}", crate::ui::setting_short(&setting), target_label(&target));
                if !provenance.plugin.is_empty() {
                    t.push_str(&format!("  [{}", provenance.plugin));
                    if !provenance.reason.is_empty() {
                        t.push_str(&format!(": {}", provenance.reason));
                    }
                    t.push(']');
                }
                (t, 1)
            }
            Event::ActionReverted { target, knob, reason, .. } => {
                let mut t = format!("revert {knob} on {}", target_label(&target));
                if !reason.is_empty() {
                    t.push_str(&format!("  ({reason})"));
                }
                (t, 0)
            }
            Event::Focus { pid, app_id, target, .. } => {
                let what = app_id
                    .or_else(|| target.as_ref().map(target_label))
                    .or_else(|| pid.map(|p| format!("pid {p}")))
                    .unwrap_or_else(|| "nothing".into());
                (format!("focus → {what}"), 0)
            }
            Event::Interaction { pid, target, end, .. } => {
                let what =
                    target.as_ref().map(target_label).or_else(|| pid.map(|p| format!("pid {p}"))).unwrap_or_default();
                (format!("{} {what}", if end { "input ended on" } else { "input on" }), 0)
            }
            Event::ClassSet { target, class, source, .. } => {
                (format!("class {} → {class}  ({source})", target_label(&target)), 0)
            }
            Event::PluginError { plugin, message, .. } => (format!("plugin {plugin}: {message}"), 2),
            Event::Notice { name, source, message, data, .. } => describe_notice(&name, &source, &message, &data),
            _ => (raw.to_string(), 0),
        },
        Err(_) => (raw.to_string(), 0),
    };
    FeedItem { ts, kind, text, severity, raw: raw.clone() }
}

fn field<'a>(data: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| data.get(*k)).filter(|v| !v.is_null())
}

fn text_of(v: &Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
}

/// `⚙ profile → gaming (steam focused)`, `↺ change #12 reverted`, …
pub fn describe_notice(name: &str, source: &str, message: &str, data: &Value) -> (String, u8) {
    let why =
        field(data, &["reason", "why"]).map(text_of).or_else(|| (!message.is_empty()).then(|| message.to_string()));
    let paren = |s: Option<String>| s.map(|s| format!(" ({s})")).unwrap_or_default();
    let id = field(data, &["id", "change", "change_id", "version"]).map(text_of);
    match name {
        "profile.changed" => match field(data, &["to", "profile", "name"]).map(text_of) {
            Some(to) => (format!("⚙ profile → {to}{}", paren(why.filter(|w| !w.contains(&to)))), 1),
            None => (format!("⚙ {message}"), 1),
        },
        "change.applied" | "change.confirmed" | "change.reverted" | "change.expired" | "change.failed" => {
            describe_change(name, message, data, id)
        }
        "calibration.done" => (format!("◎ calibrated: {message}"), 0),
        "hardware.power" => {
            let on_battery =
                field(data, &["power", "source", "to"]).map(text_of).is_some_and(|p| p.contains("battery"))
                    || message.contains("battery");
            (format!("{} {message}", if on_battery { "🔋" } else { "🔌" }), 0)
        }
        _ => {
            let src = if source.is_empty() { String::new() } else { format!(" [{source}]") };
            (format!("• {name}: {message}{src}"), 0)
        }
    }
}

/// `change.*` notices; `data` is the changeset (for `change.reverted`, the new reverting one).
fn describe_change(name: &str, message: &str, data: &Value, id: Option<String>) -> (String, u8) {
    let desc = data
        .get("description")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty() && !d.starts_with("revert "))
        .map(|d| format!(": {d}"))
        .unwrap_or_default();
    let Some(id) = id else {
        let mark = match name {
            "change.failed" => "✗",
            "change.confirmed" => "✓",
            "change.applied" => "✎",
            _ => "↺",
        };
        let sev = match name {
            "change.failed" => 2,
            "change.confirmed" => 0,
            _ => 1,
        };
        return (format!("{mark} {message}"), sev);
    };
    match name {
        "change.applied" => {
            let ttl = if data.get("ttl_until").is_some_and(|t| !t.is_null()) {
                " (auto-reverts unless confirmed)"
            } else {
                ""
            };
            (format!("✎ change #{id} applied{desc}{ttl}"), 1)
        }
        "change.confirmed" => (format!("✓ change #{id} confirmed{desc}"), 0),
        "change.expired" => (format!("↺ change #{id} expired: auto-reverted, it wasn't confirmed{desc}"), 1),
        "change.failed" => {
            let err = data.get("error").and_then(Value::as_str).map(|e| format!(" ({e})")).unwrap_or_default();
            (format!("✗ change #{id} failed{desc}{err}"), 2)
        }
        _ => {
            if let Some(v) = data.get("revert_to").filter(|v| !v.is_null()) {
                return (format!("↺ back to version {v} (change #{id})"), 1);
            }
            match data.get("reverts").filter(|v| !v.is_null()) {
                Some(t) => (format!("↺ change #{t} reverted (as #{id})"), 1),
                None => (format!("↺ change #{id} reverted{desc}"), 1),
            }
        }
    }
}

/// Where events come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventMode {
    /// The daemon emits `Event(s)` and answers `RecentEvents(u)`.
    Signals,
    /// Older daemon: events are inferred from consecutive statuses.
    StatusDiff,
}

/// `RecentEvents(n)` → parsed events, oldest first. `Ok(None)` if the method is missing.
pub fn recent_events(conn: &Connection, d: Daemon, n: u32) -> anyhow::Result<Option<Vec<Value>>> {
    let (path, iface) = paths(d);
    match conn.call_method(Some(d.bus_name()), path, Some(iface), "RecentEvents", &(n,)) {
        Ok(msg) => {
            let json: String = msg.body().deserialize()?;
            let v: Value = serde_json::from_str(&json)?;
            let mut items: Vec<Value> = match v {
                Value::Array(a) => a,
                _ => anyhow::bail!("{} RecentEvents returned something other than a JSON array", d.name()),
            };
            items.sort_by_key(|e| e.get("ts").and_then(Value::as_i64).unwrap_or(0));
            Ok(Some(items))
        }
        Err(zbus::Error::MethodError(name, _, _))
            if matches!(
                name.as_str(),
                "org.freedesktop.DBus.Error.UnknownMethod" | "org.freedesktop.DBus.Error.UnknownInterface"
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(bus::call_error(d, "RecentEvents", e)),
    }
}

fn paths(d: Daemon) -> (&'static str, &'static str) {
    match d {
        Daemon::Session => (lindesk_ipc::QOS_PATH, lindesk_ipc::QOS_INTERFACE),
        Daemon::Broker => (lindesk_ipc::BROKER_PATH, lindesk_ipc::BROKER_INTERFACE),
    }
}

/// Subscribe to the daemon's `Events(s json)` signal (a JSON array of one step's events) on a
/// background thread; each event is delivered separately.
pub fn subscribe(conn: &Connection, d: Daemon) -> anyhow::Result<Receiver<Value>> {
    let (path, iface) = paths(d);
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(d.bus_name())?
        .path(path)?
        .interface(iface)?
        .member("Events")?
        .build();
    let iter = zbus::blocking::MessageIterator::for_match_rule(rule, conn, Some(1024))?;
    let (tx, rx): (Sender<Value>, Receiver<Value>) = channel();
    std::thread::spawn(move || {
        for msg in iter {
            let Ok(msg) = msg else { break };
            let Ok(json) = msg.body().deserialize::<String>() else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&json) else { continue };
            let items = match v {
                Value::Array(a) => a,
                one => vec![one],
            };
            for e in items {
                if tx.send(e).is_err() {
                    return;
                }
            }
        }
    });
    Ok(rx)
}

/// Synthesize events from two statuses (for daemons without the `Event` signal).
pub fn diff_events(prev: &Status, cur: &Status) -> Vec<Value> {
    crate::cmd::status::diff(prev, cur)
        .into_iter()
        .map(|c| serde_json::json!({"ts": c.ts, "kind": c.kind, "synthetic": true, "text": c.text}))
        .collect()
}

/// Describe a synthesized or real event.
pub fn describe_any(v: &Value) -> FeedItem {
    if v.get("synthetic").and_then(Value::as_bool) == Some(true) {
        let text = v.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
        let kind = v.get("kind").and_then(Value::as_str).unwrap_or("event").to_string();
        let severity = u8::from(kind == "action_applied" || text.contains("→ YELLOW") || text.contains("→ RED"));
        return FeedItem { ts: v.get("ts").and_then(Value::as_i64).unwrap_or(0), kind, text, severity, raw: v.clone() };
    }
    describe(v)
}

// ----------------------------------------------------------------------------- envelope

/// The daemon's `Status()` reply: `{"status": Status, "daemon": …, "pressure": …, "epoch": …,
/// "hardware": …, "requirements": […], "text": …}` (docs/ARCHITECTURE.md). A bare `Status`
/// (older daemons) is accepted too.
#[derive(Clone, Debug, Default)]
pub struct Envelope {
    pub status: Option<Status>,
    /// `broker`, `connected`, `standalone`, `starting`.
    pub mode: Option<String>,
    pub uptime: Option<Duration>,
    /// Interaction epoch flag (`None` = unknown).
    pub epoch: Option<bool>,
    /// The foreground cgroup's own stall (`some_avg10` per resource).
    pub fg_stall: Vec<(Resource, f64)>,
    pub hardware: Option<lindesk_plugin_api::Hardware>,
    /// Requirement warnings, one line each.
    pub requirements: Vec<String>,
    /// The active profile (`daemon.profile`).
    pub profile: Option<crate::cmd::profile::ProfileState>,
    /// Changes that auto-revert unless confirmed (filled by the caller from `ListChanges`).
    pub pending: Vec<crate::cmd::changes::Pending>,
}

fn requirement_line(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(o) => {
            let pick = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
            let mut s = pick("check");
            for k in ["detail", "status", "message"] {
                let x = pick(k);
                if !x.is_empty() && !s.contains(&x) {
                    if !s.is_empty() {
                        s.push_str(": ");
                    }
                    s.push_str(&x);
                    break;
                }
            }
            let r = pick("remedy");
            if !r.is_empty() {
                s.push_str(&format!(" → {r}"));
            }
            if s.is_empty() { v.to_string() } else { s }
        }
        other => other.to_string(),
    }
}

pub fn parse_envelope(raw: &Value) -> Envelope {
    let inner = raw.get("status").filter(|v| v.is_object()).unwrap_or(raw);
    let mut e = Envelope { status: serde_json::from_value(inner.clone()).ok(), ..Default::default() };
    if let Some(d) = raw.get("daemon") {
        e.mode = d.get("mode").and_then(Value::as_str).map(str::to_string);
        e.uptime = d.get("uptime_s").and_then(Value::as_f64).map(|s| Duration::from_secs_f64(s.max(0.0)));
    }
    e.epoch = raw.get("epoch").and_then(|x| x.get("active")).and_then(Value::as_bool);
    if let Some(fg) = raw.get("pressure").and_then(|p| p.get("foreground")).filter(|v| v.is_object()) {
        for r in Resource::ALL {
            if let Some(v) = fg.get(r.as_str()).and_then(|p| p.get("some_avg10")).and_then(Value::as_f64) {
                e.fg_stall.push((r, v));
            }
        }
    }
    e.hardware = raw.get("hardware").and_then(|h| serde_json::from_value(h.clone()).ok());
    if let Some(Value::Array(a)) = raw.get("requirements") {
        e.requirements = a.iter().map(requirement_line).collect();
    }
    e.profile = crate::cmd::profile::ProfileState::from_status(raw);
    e
}

/// Hardware detected on this machine (when no daemon reports it).
pub fn local_hardware() -> lindesk_plugin_api::Hardware {
    lindesk_sys::hardware::detect(&lindesk_sys::Roots::host())
}

// ------------------------------------------------------------------------------- client

/// A long-lived connection to one daemon, reconnecting with backoff.
pub struct Client {
    pub preferred: Vec<Daemon>,
    conn: Option<(Daemon, Connection)>,
    next_try: Instant,
    pub mode: EventMode,
    events: Option<Receiver<Value>>,
    prev: Option<Status>,
    pub uptime_since: Option<Instant>,
    pub uptime_at_connect: Option<Duration>,
    pub broker_present: bool,
    pub last_error: Option<String>,
    last_seq: Option<u64>,
}

/// One poll's worth of data.
pub struct Snapshot {
    pub source: Option<Daemon>,
    pub raw: Option<Value>,
    pub status: Option<Status>,
    pub envelope: Envelope,
    /// New events since the previous poll, oldest first.
    pub events: Vec<Value>,
}

impl Client {
    pub fn new(system: bool) -> Client {
        Client {
            preferred: if system { vec![Daemon::Broker] } else { vec![Daemon::Session, Daemon::Broker] },
            conn: None,
            next_try: Instant::now(),
            mode: EventMode::StatusDiff,
            events: None,
            prev: None,
            uptime_since: None,
            uptime_at_connect: None,
            broker_present: false,
            last_error: None,
            last_seq: None,
        }
    }

    /// Drop events already seen (by their `seq`, when the daemon numbers them).
    fn fresh(&mut self, v: &Value) -> bool {
        match v.get("seq").and_then(Value::as_u64) {
            Some(seq) if self.last_seq.is_some_and(|l| seq <= l) => false,
            Some(seq) => {
                self.last_seq = Some(seq);
                true
            }
            None => true,
        }
    }

    pub fn source(&self) -> Option<Daemon> {
        self.conn.as_ref().map(|(d, _)| *d)
    }

    /// Connect, backfill `backfill` recent events, and subscribe.
    fn connect(&mut self, backfill: u32) -> Vec<Value> {
        if self.conn.is_some() || Instant::now() < self.next_try {
            return Vec::new();
        }
        self.next_try = Instant::now() + Duration::from_secs(2);
        for &d in &self.preferred {
            match bus::running(d) {
                Ok(conn) => {
                    let mut out = Vec::new();
                    let rx = subscribe(&conn, d).ok();
                    match (recent_events(&conn, d, backfill), rx) {
                        (Ok(Some(ev)), Some(rx)) => {
                            self.last_seq = None;
                            for v in ev {
                                if self.fresh(&v) {
                                    out.push(v);
                                }
                            }
                            self.events = Some(rx);
                            self.mode = EventMode::Signals;
                        }
                        _ => self.mode = EventMode::StatusDiff,
                    }
                    self.uptime_at_connect = daemon_uptime(&conn, d);
                    self.uptime_since = Some(Instant::now());
                    self.broker_present =
                        d == Daemon::Broker || matches!(bus::reach(Daemon::Broker), bus::Reach::Running);
                    self.conn = Some((d, conn));
                    self.last_error = None;
                    return out;
                }
                Err(e) => self.last_error = Some(e.to_string().lines().next().unwrap_or_default().to_string()),
            }
        }
        Vec::new()
    }

    fn disconnect(&mut self, why: String) {
        self.conn = None;
        self.events = None;
        self.prev = None;
        self.last_error = Some(why);
        self.next_try = Instant::now() + Duration::from_secs(1);
    }

    /// Poll the status and drain new events.
    pub fn poll(&mut self, backfill: u32) -> Snapshot {
        let mut events = self.connect(backfill);
        let Some((d, conn)) = &self.conn else {
            return Snapshot { source: None, raw: None, status: None, envelope: Envelope::default(), events };
        };
        let d = *d;
        let json = match d {
            Daemon::Session => {
                bus::session_proxy(conn).and_then(|p| p.status().map_err(|e| bus::call_error(d, "Status", e)))
            }
            Daemon::Broker => {
                bus::broker_proxy(conn).and_then(|p| p.get_status().map_err(|e| bus::call_error(d, "GetStatus", e)))
            }
        };
        let raw = match json.map(|s| serde_json::from_str::<Value>(&s)) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                self.disconnect(format!("{} returned invalid JSON: {e}", d.name()));
                return Snapshot { source: None, raw: None, status: None, envelope: Envelope::default(), events };
            }
            Err(e) => {
                self.disconnect(e.to_string().lines().next().unwrap_or_default().to_string());
                return Snapshot { source: None, raw: None, status: None, envelope: Envelope::default(), events };
            }
        };
        let envelope = parse_envelope(&raw);
        let status = envelope.status.clone();
        if let Some(rx) = &self.events {
            let incoming: Vec<Value> = rx.try_iter().collect();
            for v in incoming {
                if self.fresh(&v) {
                    events.push(v);
                }
            }
        } else if let (Some(p), Some(c)) = (&self.prev, &status) {
            events.extend(diff_events(p, c));
        }
        self.prev = status.clone();
        Snapshot { source: Some(d), raw: Some(raw), status, envelope, events }
    }

    /// The daemon's uptime now, when known.
    pub fn uptime(&self) -> Option<Duration> {
        Some(self.uptime_at_connect? + self.uptime_since?.elapsed())
    }

    /// Run `Explain(what)` on the current connection.
    pub fn explain(&self, what: &str) -> anyhow::Result<String> {
        let (d, conn) = self.conn.as_ref().ok_or_else(|| anyhow::anyhow!("not connected"))?;
        match d {
            Daemon::Session => bus::session_proxy(conn)?.explain(what).map_err(|e| bus::call_error(*d, "Explain", e)),
            Daemon::Broker => bus::broker_proxy(conn)?.explain(what).map_err(|e| bus::call_error(*d, "Explain", e)),
        }
    }
}

fn boot_time() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/stat").ok()?;
    s.lines().find_map(|l| l.strip_prefix("btime "))?.trim().parse().ok()
}

/// How long the bus-name owner has been running.
fn daemon_uptime(conn: &Connection, d: Daemon) -> Option<Duration> {
    let dbus = zbus::blocking::fdo::DBusProxy::new(conn).ok()?;
    let pid = dbus.get_connection_unix_process_id(zbus::names::BusName::try_from(d.bus_name()).ok()?).ok()?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    let start_ticks: u64 = rest.split_whitespace().nth(19)?.parse().ok()?;
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    let started = boot_time()? + start_ticks / hz;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(now.saturating_sub(started)))
}

// ---------------------------------------------------------------------------------- PSI

/// System PSI per resource.
pub fn read_psi(proc_root: &Path) -> Vec<(Resource, Option<PsiReading>)> {
    Resource::ALL.iter().map(|r| (*r, PsiReading::read(&system_path(proc_root, *r)).ok())).collect()
}

/// Rolling PSI history: `some`/`full` avg10 per resource, newest last.
#[derive(Default, Clone, Debug)]
pub struct PsiHistory {
    pub cap: usize,
    pub some: [VecDeque<f64>; 3],
    pub full: [VecDeque<f64>; 3],
    pub latest: [Option<PsiReading>; 3],
}

impl PsiHistory {
    pub fn new(cap: usize) -> PsiHistory {
        PsiHistory { cap, ..Default::default() }
    }

    pub fn push(&mut self, readings: &[(Resource, Option<PsiReading>)]) {
        for (i, (_, r)) in readings.iter().enumerate().take(3) {
            self.latest[i] = *r;
            let (s, f) = r.map_or((0.0, 0.0), |r| (r.some.avg10 as f64, r.full.map_or(0.0, |f| f.avg10 as f64)));
            self.some[i].push_back(s);
            self.full[i].push_back(f);
            while self.some[i].len() > self.cap {
                self.some[i].pop_front();
            }
            while self.full[i].len() > self.cap {
                self.full[i].pop_front();
            }
        }
    }
}

/// The cgroup directory of a unit target, searched under its manager (cached by callers).
pub fn find_unit_cgroup(cgroup_root: &Path, target: &Target) -> Option<PathBuf> {
    let (base, name) = match target {
        Target::Unit { manager: Manager::User(uid), name } => {
            (cgroup_root.join(format!("user.slice/user-{uid}.slice/user@{uid}.service")), name.as_str())
        }
        Target::Unit { manager: Manager::System, name } => (cgroup_root.to_path_buf(), name.as_str()),
        Target::Cgroup { path } => return Some(cgroup_root.join(path.trim_start_matches('/'))),
        Target::Process { .. } => return None,
    };
    let mut stack = vec![(base, 0)];
    while let Some((dir, depth)) = stack.pop() {
        let direct = dir.join(name);
        if direct.is_dir() {
            return Some(direct);
        }
        if depth >= 4 {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let n = e.file_name();
            let n = n.to_string_lossy();
            if n.ends_with(".slice") || n.ends_with(".service") && n.starts_with("user@") {
                stack.push((e.path(), depth + 1));
            }
        }
    }
    None
}

/// The `some avg10` stall of each resource inside a cgroup directory.
pub fn cgroup_stall(dir: &Path) -> Vec<(Resource, f64)> {
    Resource::ALL
        .iter()
        .filter_map(|r| {
            let p = dir.join(format!("{}.pressure", r.as_str()));
            PsiReading::read(&p).ok().map(|x| (*r, x.some.avg10 as f64))
        })
        .collect()
}

/// A short "what is happening" summary: `protecting firefox; 3 workloads yielding`.
pub fn summary(st: &Status) -> String {
    let fg = st.foreground.as_ref().map(|f| f.label());
    let n = st.yielding.len();
    let yielding = match n {
        0 => None,
        1 => Some("1 workload yielding".to_string()),
        n => Some(format!("{n} workloads yielding")),
    };
    match (st.level, fg, yielding) {
        (Level::Green, Some(f), None) => format!("all quiet; {f} in front"),
        (Level::Green, None, None) => "all quiet".into(),
        (_, Some(f), Some(y)) => format!("protecting {f}; {y}"),
        (_, Some(f), None) => format!("protecting {f}"),
        (_, None, Some(y)) => y,
        (_, None, None) => "watching pressure".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn describes_events() {
        let e = describe(&json!({"kind": "level", "ts": 5, "from": "green", "to": "yellow", "policy": "threshold"}));
        assert_eq!(
            (e.ts, e.kind.as_str(), e.text.as_str(), e.severity),
            (5, "level", "level GREEN → YELLOW  (threshold)", 1)
        );
        let e = describe(&json!({"kind": "action_applied", "ts": 6,
            "target": {"type": "unit", "manager": {"kind": "user", "uid": 1000}, "name": "app-cargo-1.scope"},
            "setting": {"knob": "cpu_weight", "value": 20},
            "provenance": {"plugin": "threshold", "reason": "YELLOW: batch yields", "priority": 0}}));
        assert_eq!(e.text, "apply  cpu.weight 20 on app-cargo-1.scope  [threshold: YELLOW: batch yields]");
        let e = describe(&json!({"kind": "plugin_error", "ts": 7, "plugin": "x", "message": "timed out"}));
        assert_eq!((e.text.as_str(), e.severity), ("plugin x: timed out", 2));
        let e = describe(&json!({"kind": "focus", "ts": 8, "app_id": "org.mozilla.firefox", "source": "gnome"}));
        assert_eq!(e.text, "focus → org.mozilla.firefox");
        let e = describe(
            &json!({"kind": "notice", "ts": 10, "name": "profile.changed", "source": "profiles", "message": "steam focused", "data": {"to": "gaming"}}),
        );
        assert_eq!((e.kind.as_str(), e.text.as_str()), ("notice", "⚙ profile → gaming (steam focused)"));
        let e = describe(
            &json!({"kind": "notice", "ts": 11, "name": "change.reverted", "message": "", "data": {"id": 12}}),
        );
        assert_eq!(e.text, "↺ change #12 reverted");
        let cs = json!({"id": 7, "description": "shorter epochs", "ttl_until": 5});
        let e = describe(
            &json!({"kind": "notice", "ts": 13, "name": "change.applied", "message": "changeset 7 applied: shorter epochs", "source": "changes", "data": cs}),
        );
        assert_eq!(
            (e.text.as_str(), e.severity),
            ("✎ change #7 applied: shorter epochs (auto-reverts unless confirmed)", 1)
        );
        let e = describe(&json!({"kind": "notice", "ts": 14, "name": "change.expired", "message": "", "data": cs}));
        assert_eq!(e.text, "↺ change #7 expired: auto-reverted, it wasn't confirmed: shorter epochs");
        let e = describe(
            &json!({"kind": "notice", "ts": 15, "name": "change.reverted", "message": "", "data": {"id": 8, "reverts": 7, "description": "revert 7: shorter epochs"}}),
        );
        assert_eq!(e.text, "↺ change #7 reverted (as #8)");
        let e = describe(
            &json!({"kind": "notice", "ts": 16, "name": "change.failed", "message": "", "data": {"id": 9, "description": "x", "error": "EPERM"}}),
        );
        assert_eq!((e.text.as_str(), e.severity), ("✗ change #9 failed: x (EPERM)", 2));
        let e = describe(
            &json!({"kind": "notice", "ts": 17, "name": "change.confirmed", "message": "", "data": {"id": 7}}),
        );
        assert_eq!((e.text.as_str(), e.severity), ("✓ change #7 confirmed", 0));
        let e = describe(&json!({"kind": "notice", "ts": 12, "name": "x.y", "message": "hello", "source": "s"}));
        assert_eq!(e.text, "• x.y: hello [s]");
        let e = describe(&json!({"kind": "mystery", "ts": 9}));
        assert_eq!(e.kind, "mystery");
    }

    #[test]
    fn parses_status_envelopes() {
        let st = crate::cmd::status::tests::status(Level::Yellow, vec![]);
        let bare = serde_json::to_value(&st).unwrap();
        let e = parse_envelope(&bare);
        assert_eq!(e.status.as_ref().unwrap().level, Level::Yellow);
        assert!(e.mode.is_none() && e.hardware.is_none());
        let wrapped = json!({
            "status": bare,
            "daemon": {"name": "lindesk-session", "mode": "connected", "uptime_s": 125.5},
            "pressure": {"system": {}, "foreground": {"target": "user/1000:x.scope", "cpu": {"some_avg10": 1.5}, "io": {"some_avg10": 9.0}}},
            "epoch": {"active": true, "source": "threshold"},
            "hardware": {"cpus": 8, "mem_total": 17179869184u64, "power": "battery"},
            "requirements": [{"check": "PSI", "status": "warn", "detail": "per-cgroup PSI disabled", "remedy": "boot with psi=1"}, "plain string"]
        });
        let e = parse_envelope(&wrapped);
        assert_eq!(e.status.unwrap().level, Level::Yellow);
        assert_eq!(e.mode.as_deref(), Some("connected"));
        assert_eq!(e.uptime.unwrap().as_secs(), 125);
        assert_eq!(e.epoch, Some(true));
        assert_eq!(e.fg_stall, vec![(Resource::Cpu, 1.5), (Resource::Io, 9.0)]);
        assert_eq!(e.hardware.unwrap().cpus, 8);
        assert_eq!(e.requirements, vec!["PSI: per-cgroup PSI disabled → boot with psi=1", "plain string"]);
    }

    #[test]
    fn psi_history_is_bounded() {
        let mut h = PsiHistory::new(3);
        for _ in 0..5 {
            h.push(&[(Resource::Cpu, None), (Resource::Io, None), (Resource::Memory, None)]);
        }
        assert_eq!(h.some[0].len(), 3);
    }

    #[test]
    fn finds_unit_cgroups() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("user.slice/user-1000.slice/user@1000.service/app.slice/app-x.scope");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            base.join("io.pressure"),
            "some avg10=4.50 avg60=0 avg300=0 total=1\nfull avg10=1.00 avg60=0 avg300=0 total=1\n",
        )
        .unwrap();
        let t = Target::user_unit(1000, "app-x.scope");
        let found = find_unit_cgroup(d.path(), &t).unwrap();
        assert_eq!(found, base);
        assert_eq!(cgroup_stall(&found), vec![(Resource::Io, 4.5)]);
        assert!(find_unit_cgroup(d.path(), &Target::user_unit(1000, "nope.scope")).is_none());
    }
}
