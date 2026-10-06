//! `org.lindesk.QoS1` on the session bus.
//!
//! Every focus/interaction/class call goes into the local engine (status, explain, and the
//! whole pipeline in standalone mode) and, through the broker link, to `lindeskd` when
//! connected. `BeginInteraction` is coalesced to at most one event per pid every
//! [`INTERACTION_COALESCE`] (GNOME may call it several times a second); `EndInteraction`
//! always passes and resets the window.
//!
//! The control-plane members (docs/CONTROL-PLANE.md §5, user scope) only serve the agent's
//! own user: every caller's uid is checked against it (`AccessDenied` otherwise; the
//! session bus normally admits nobody else anyway). Guardrail refusals are the D-Bus error
//! `org.lindesk.Error.Rejected` with the human-readable reasons.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lindesk_core::clock::now_ms;
use lindesk_ipc::QOS_PATH;
use lindesk_plugin_api::{Event, Metric, QosClass};
use tokio::sync::mpsc::UnboundedSender;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, fdo, interface};

use crate::common::control::{self, Control, CtlError};
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
    /// User-scope versioned changes; `None` when the store couldn't be opened.
    pub control: Option<Arc<Control<SessionExt>>>,
}

fn gone(e: EngineGone) -> fdo::Error {
    fdo::Error::Failed(e.to_string())
}

impl QoS {
    /// The control plane, for a caller of this agent's own uid.
    async fn control_for(
        &self,
        conn: &Connection,
        hdr: &Header<'_>,
    ) -> Result<(&Control<SessionExt>, control::Caller), CtlError> {
        let who = control::caller(conn, hdr).await?;
        if who.uid != self.uid {
            return Err(fdo::Error::AccessDenied(format!(
                "uid {} may not use the control plane of uid {}'s agent",
                who.uid, self.uid
            ))
            .into());
        }
        let ctl = self.control.as_deref().ok_or_else(|| {
            CtlError::Failed("versioned changes are unavailable (see the lindesk-session log)".into())
        })?;
        Ok((ctl, who))
    }

    fn push(&self, e: Event, fwd: Option<Forward>) -> fdo::Result<()> {
        self.engine.push_event(e).map_err(gone)?;
        if let Some(f) = fwd {
            let _ = self.forward.send(f);
        }
        Ok(())
    }
}

/// Most entries accepted in one list of visible processes.
const MAX_PRESENCE: usize = 1024;

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

    /// Every process with a window on screen right now (not minimized, on a visible
    /// workspace). The complete list; shell adapters send it whenever it changes.
    async fn set_visible_processes(&self, pids: Vec<i32>) -> fdo::Result<()> {
        let pids: Vec<i32> = pids.into_iter().filter(|p| *p > 0).take(MAX_PRESENCE).collect();
        self.push(
            Event::Visible { ts: now_ms(), pids: pids.clone(), app_ids: Vec::new(), source: "dbus".into() },
            Some(Forward::Visible { pids, app_ids: Vec::new() }),
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

    /// Pause (observe-only: every applied setting reverted, sensing continues) or resume.
    /// `seconds > 0` auto-resumes after that long. Returns the pause JSON. Own uid only.
    async fn set_paused(
        &self,
        paused: bool,
        seconds: u32,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<String> {
        let who = control::caller(conn, &hdr).await.map_err(|e| fdo::Error::Failed(e.to_string()))?;
        if who.uid != self.uid {
            return Err(fdo::Error::AccessDenied(format!("uid {} may not pause uid {}'s agent", who.uid, self.uid)));
        }
        self.engine.set_paused(paused, seconds, format!("uid {}", who.uid)).await.map_err(gone)
    }

    /// The pause JSON: `{"paused", "since_ms", "until_ms", "by"}`.
    async fn get_paused(&self) -> fdo::Result<String> {
        self.engine.get_paused().await.map_err(gone)
    }

    // ---- control plane (docs/CONTROL-PLANE.md §5), user scope ------------------------------

    /// Plan a changeset (no side effects): Plan JSON.
    async fn plan_change(
        &self,
        ops_json: String,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<String, CtlError> {
        self.control_for(conn, &hdr).await?.0.plan(&ops_json).await
    }

    /// Validate, apply and version a changeset: ChangeSet JSON.
    async fn apply_change(
        &self,
        ops_json: String,
        description: String,
        ttl_secs: u32,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<String, CtlError> {
        let (ctl, who) = self.control_for(conn, &hdr).await?;
        ctl.apply(&ops_json, &description, ttl_secs, &who).await
    }

    /// Keep an applied changeset (stops its dead-man timer).
    async fn confirm_change(
        &self,
        id: u64,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<(), CtlError> {
        self.control_for(conn, &hdr).await?.0.confirm(id).await
    }

    /// Revert a changeset: the new ChangeSet JSON.
    async fn revert_change(
        &self,
        id: u64,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<String, CtlError> {
        let (ctl, who) = self.control_for(conn, &hdr).await?;
        ctl.revert(id, &who).await
    }

    /// The newest `limit` changesets (JSON array, newest first).
    async fn list_changes(
        &self,
        limit: u32,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<String, CtlError> {
        self.control_for(conn, &hdr).await?.0.list(limit).await
    }

    /// One changeset.
    async fn get_change(
        &self,
        id: u64,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<String, CtlError> {
        self.control_for(conn, &hdr).await?.0.get(id).await
    }

    /// Profiles: name, description, active, matches_now, reason. While connected, the
    /// broker's profile is the active one.
    async fn list_profiles(&self) -> Result<String, CtlError> {
        control::profiles_json(&self.engine).await
    }

    /// A `profile_use` changeset on the user's config (`"auto"` allowed): ChangeSet JSON.
    async fn set_profile(
        &self,
        name: String,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<String, CtlError> {
        let (ctl, who) = self.control_for(conn, &hdr).await?;
        ctl.set_profile(&name, &who).await
    }

    /// `Hardware` JSON including the latency map.
    async fn get_hardware(&self) -> Result<String, CtlError> {
        control::hardware_json(&self.engine).await
    }

    /// Swap analysis JSON (computed on the blocking pool).
    async fn swap_report(&self) -> Result<String, CtlError> {
        control::swap_json(&self.engine).await
    }

    /// A user-scope changeset transition (ChangeSet JSON).
    #[zbus(signal)]
    pub async fn change_event(emitter: &SignalEmitter<'_>, json: &str) -> zbus::Result<()>;

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

pub async fn emit_change_event(conn: &Connection, json: &str) {
    if let Ok(e) = SignalEmitter::new(conn, QOS_PATH)
        && let Err(err) = QoS::change_event(&e, json).await
    {
        tracing::debug!("ChangeEvent not sent: {err}");
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
