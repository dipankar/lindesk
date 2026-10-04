//! `org.lindesk.QoS1` on the session bus.
//!
//! Every focus/interaction/class call goes into the local engine (status, explain, and the
//! whole pipeline in standalone mode) and, through the broker link, to `lindeskd` when
//! connected. `BeginInteraction` is coalesced to at most one event per pid every
//! [`INTERACTION_COALESCE`] (GNOME may call it several times a second); `EndInteraction`
//! always passes and resets the window.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lindesk_core::clock::now_ms;
use lindesk_ipc::QOS_PATH;
use lindesk_plugin_api::{Event, Metric, QosClass};
use tokio::sync::mpsc::UnboundedSender;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, fdo, interface};

use crate::common::engine_thread::{EngineGone, EngineHandle};
use crate::common::proc_uid;
use crate::link::Forward;
use crate::session::SessionExt;

/// Minimum spacing of forwarded `BeginInteraction`s per pid.
pub const INTERACTION_COALESCE: Duration = Duration::from_millis(100);

/// Per-pid rate limiter for interactions.
#[derive(Debug, Default)]
pub struct Coalescer {
    last: HashMap<i32, Instant>,
}

impl Coalescer {
    /// True if an interaction for `pid` at `now` should pass.
    pub fn begin(&mut self, pid: i32, now: Instant) -> bool {
        if self.last.len() > 1024 {
            self.last.retain(|_, t| now.saturating_duration_since(*t) < Duration::from_secs(10));
        }
        match self.last.get(&pid) {
            Some(t) if now.saturating_duration_since(*t) < INTERACTION_COALESCE => false,
            _ => {
                self.last.insert(pid, now);
                true
            }
        }
    }

    /// An explicit end: always passes; the next begin starts a new window.
    pub fn end(&mut self, pid: i32) {
        self.last.remove(&pid);
    }
}

pub struct QoS {
    pub engine: EngineHandle<SessionExt>,
    pub uid: u32,
    pub forward: UnboundedSender<Forward>,
    pub coalesce: Mutex<Coalescer>,
}

fn gone(e: EngineGone) -> fdo::Error {
    fdo::Error::Failed(e.to_string())
}

impl QoS {
    fn push(&self, e: Event, fwd: Option<Forward>) -> fdo::Result<()> {
        self.engine.push_event(e).map_err(gone)?;
        if let Some(f) = fwd {
            let _ = self.forward.send(f);
        }
        Ok(())
    }
}

#[interface(name = "org.lindesk.QoS1")]
impl QoS {
    /// Focus moved to a window owned by `pid`.
    async fn set_foreground_process(&self, pid: i32) -> fdo::Result<()> {
        if pid <= 0 {
            return Err(fdo::Error::InvalidArgs(format!("invalid pid {pid}")));
        }
        self.push(
            Event::Focus { ts: now_ms(), pid: Some(pid), app_id: None, target: None, source: "dbus".into() },
            Some(Forward::Focus { pid, app_id: String::new() }),
        )
    }

    /// Focus moved to an app known only by its id.
    async fn set_foreground_app(&self, app_id: String) -> fdo::Result<()> {
        if app_id.trim().is_empty() {
            return Err(fdo::Error::InvalidArgs("empty app id".into()));
        }
        self.push(
            Event::Focus { ts: now_ms(), pid: None, app_id: Some(app_id.clone()), target: None, source: "dbus".into() },
            Some(Forward::Focus { pid: 0, app_id }),
        )
    }

    /// Start or extend an interaction epoch (coalesced per pid).
    async fn begin_interaction(&self, pid: i32) -> fdo::Result<()> {
        let pass = self.coalesce.lock().unwrap_or_else(|e| e.into_inner()).begin(pid, Instant::now());
        if !pass {
            return Ok(());
        }
        self.push(
            Event::Interaction { ts: now_ms(), pid: Some(pid), target: None, end: false, source: "dbus".into() },
            Some(Forward::Interaction { pid, end: false }),
        )
    }

    /// End an interaction epoch early.
    async fn end_interaction(&self, pid: i32) -> fdo::Result<()> {
        self.coalesce.lock().unwrap_or_else(|e| e.into_inner()).end(pid);
        self.push(
            Event::Interaction { ts: now_ms(), pid: Some(pid), target: None, end: true, source: "dbus".into() },
            Some(Forward::Interaction { pid, end: true }),
        )
    }

    /// Set the QoS class of one of this user's processes.
    async fn set_class(&self, pid: i32, class: String) -> fdo::Result<()> {
        let parsed: QosClass = class.parse().map_err(fdo::Error::InvalidArgs)?;
        match proc_uid("/proc".as_ref(), pid) {
            None => return Err(fdo::Error::InvalidArgs(format!("no process {pid}"))),
            Some(owner) if owner != self.uid => {
                return Err(fdo::Error::AccessDenied(format!("pid {pid} belongs to uid {owner}, not {}", self.uid)));
            }
            Some(_) => {}
        }
        self.engine.set_class(pid, parsed, "dbus".into()).await.map_err(gone)?.map_err(fdo::Error::Failed)?;
        let _ = self.forward.send(Forward::SetClass { pid, class });
        Ok(())
    }

    /// Record a free-form metric (lands in the `metrics` table).
    async fn report_metric(&self, name: String, value: f64, labels: HashMap<String, String>) -> fdo::Result<()> {
        if name.trim().is_empty() {
            return Err(fdo::Error::InvalidArgs("empty metric name".into()));
        }
        let m = Metric { ts: now_ms(), name, value, labels: labels.into_iter().collect(), plugin: "dbus".into() };
        self.engine.send(crate::common::engine_thread::Command::Metric(m)).map_err(gone)
    }

    /// Status JSON (see ARCHITECTURE.md).
    async fn status(&self) -> fdo::Result<String> {
        self.engine.status().await.map_err(gone)
    }

    /// Explanation JSON for a pid or target string.
    async fn explain(&self, what: String) -> fdo::Result<String> {
        self.engine.explain(what).await.map_err(gone)?.map_err(fdo::Error::InvalidArgs)
    }

    /// The last `n` events (oldest first) as a JSON array.
    async fn recent_events(&self, n: u32) -> String {
        self.engine.recent_events(n as usize)
    }

    /// Health level changed: `green`, `yellow` or `red`.
    #[zbus(signal)]
    pub async fn level_changed(emitter: &SignalEmitter<'_>, level: &str) -> zbus::Result<()>;

    /// A JSON array of engine events (see ARCHITECTURE.md).
    #[zbus(signal)]
    pub async fn events(emitter: &SignalEmitter<'_>, json: &str) -> zbus::Result<()>;
}

/// Emit a signal on our object, logging failures.
pub async fn emit_level(conn: &Connection, level: &str) {
    if let Ok(e) = SignalEmitter::new(conn, QOS_PATH)
        && let Err(err) = QoS::level_changed(&e, level).await
    {
        tracing::debug!("LevelChanged not sent: {err}");
    }
}

pub async fn emit_events(conn: &Connection, json: &str) {
    if let Ok(e) = SignalEmitter::new(conn, QOS_PATH)
        && let Err(err) = QoS::events(&e, json).await
    {
        tracing::debug!("Events not sent: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_per_pid() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut c = Coalescer::default();
        assert!(c.begin(1, t0));
        assert!(!c.begin(1, t0 + ms(50)));
        assert!(c.begin(2, t0 + ms(50)), "other pids are independent");
        assert!(!c.begin(1, t0 + ms(99)));
        assert!(c.begin(1, t0 + ms(100)));
        c.end(1);
        assert!(c.begin(1, t0 + ms(101)), "end resets the window");
        // 4 calls per second for a second → 4 events (every call passes at 250 ms spacing),
        // 20 calls per second → 10.
        let mut c = Coalescer::default();
        assert_eq!((0..20).filter(|i| c.begin(9, t0 + ms(i * 50))).count(), 10);
    }
}
