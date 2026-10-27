//! `org.lindesk.Broker1` on the system bus.
//!
//! Authorization (callers are identified by their bus credentials):
//!
//! | Member | Rule |
//! |---|---|
//! | `RegisterSession` | anyone; the caller's uid is recorded |
//! | `ReportForeground`, `Interaction` | pid owned by the caller, else polkit `set-class-any`; root always. `pid <= 0` with an app id: only that user's units match |
//! | `SetClass` | own pid: polkit `set-class-own`; other pids: `set-class-any`; root always |
//! | `GetStatus`, `Explain` | anyone |
//! | `RecentEvents` | anyone; filtered: root sees every event, other users system-scope events plus their own |
//! | `PlanChange`, `ListChanges`, `GetChange`, `ListProfiles`, `GetHardware`, `SwapReport` | anyone |
//! | `Reload`, `ApplyChange`, `ConfirmChange`, `RevertChange`, `SetProfile`, `Calibrate`, `CalibrateWith` | polkit `manage` |
//!
//! polkit checks are non-interactive unless the message carries
//! `ALLOW_INTERACTIVE_AUTHORIZATION`. `LINDESK_INSECURE_NO_POLKIT=1` skips polkit (test
//! buses have none); it is honoured only when the broker does not run as root.
//!
//! `UserState` is sent *unicast* to each registered agent of that user (other users never
//! see it). `Events` carrying system-scope events is broadcast; events about one user's
//! units or processes go unicast to that user's registered agents only. `ChangeEvent`
//! (system-scope changesets) is broadcast. Guardrail refusals are the D-Bus error
//! `org.lindesk.Error.Rejected` with the human-readable reasons.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use lindesk_core::clock::now_ms;
use lindesk_ipc::{BROKER_PATH, actions, auth};
use lindesk_plugin_api::{Event, QosClass};
use zbus::message::{Flags, Header};
use zbus::names::{BusName, OwnedUniqueName, UniqueName};
use zbus::object_server::SignalEmitter;
use zbus::{Connection, fdo, interface};

use crate::broker::BrokerExt;
use crate::common::calibration;
use crate::common::control::{self, Control, CtlError};
use crate::common::engine_thread::{Audience, Command, EngineGone, EngineHandle};
use crate::common::proc_uid;

/// Env var that disables polkit checks for unprivileged test runs.
pub const INSECURE_ENV: &str = "LINDESK_INSECURE_NO_POLKIT";

/// True when polkit may be skipped: the env var is `1` and we are not root.
pub fn insecure_no_polkit(env: Option<&str>, euid_is_root: bool) -> bool {
    env == Some("1") && !euid_is_root
}

/// Registered session agents: unique bus name → uid.
pub type Sessions = Arc<Mutex<BTreeMap<OwnedUniqueName, u32>>>;

/// Where and how on-demand calibrations run.
#[derive(Clone, Debug)]
pub struct CalibrationSetup {
    pub roots: lindesk_sys::Roots,
    /// Where the result is saved (`/var/lib/lindesk/calibration.json` as root).
    pub path: PathBuf,
    /// Shared with the first-run calibrator: one calibration at a time.
    pub busy: Arc<AtomicBool>,
}

pub struct Broker {
    pub engine: EngineHandle<BrokerExt>,
    pub sessions: Sessions,
    pub insecure: bool,
    /// System-scope versioned changes; `None` when the store couldn't be opened.
    pub control: Option<Arc<Control<BrokerExt>>>,
    pub calibration: CalibrationSetup,
}

fn gone(e: EngineGone) -> fdo::Error {
    fdo::Error::Failed(e.to_string())
}

fn sender<'a>(hdr: &'a Header<'_>) -> fdo::Result<&'a UniqueName<'a>> {
    hdr.sender().ok_or_else(|| fdo::Error::AccessDenied("message without a sender".into()))
}

impl Broker {
    async fn caller(&self, conn: &Connection, hdr: &Header<'_>) -> fdo::Result<auth::Caller> {
        let name = BusName::from(sender(hdr)?.clone());
        auth::caller(conn, &name).await.map_err(|e| fdo::Error::AccessDenied(e.to_string()))
    }

    /// polkit `action` for the sender (root and the insecure test mode pass).
    async fn polkit(
        &self,
        conn: &Connection,
        hdr: &Header<'_>,
        caller: &auth::Caller,
        action: &str,
    ) -> fdo::Result<()> {
        if caller.uid == 0 || self.insecure {
            return Ok(());
        }
        let name = BusName::from(sender(hdr)?.clone());
        let interactive = hdr.primary().flags().contains(Flags::AllowInteractiveAuth);
        auth::authorize(conn, &name, action, interactive).await.map_err(|e| match e {
            auth::AuthError::Denied(a) => fdo::Error::AccessDenied(format!("not authorized for {a}")),
            e => fdo::Error::AccessDenied(e.to_string()),
        })
    }

    /// The caller may report focus/interaction for `pid`.
    async fn check_pid(&self, conn: &Connection, hdr: &Header<'_>, pid: i32) -> fdo::Result<auth::Caller> {
        let caller = self.caller(conn, hdr).await?;
        let owner =
            proc_uid("/proc".as_ref(), pid).ok_or_else(|| fdo::Error::InvalidArgs(format!("no process {pid}")))?;
        if owner != caller.uid {
            self.polkit(conn, hdr, &caller, actions::SET_CLASS_ANY).await?;
        }
        Ok(caller)
    }

    fn control(&self) -> Result<&Control<BrokerExt>, CtlError> {
        self.control
            .as_deref()
            .ok_or_else(|| CtlError::Failed("versioned changes are unavailable (see the lindeskd log)".into()))
    }

    /// The caller, after polkit `org.lindesk.manage`.
    async fn manager(&self, conn: &Connection, hdr: &Header<'_>) -> Result<control::Caller, CtlError> {
        let who = control::caller(conn, hdr).await?;
        let caller = auth::Caller { uid: who.uid, pid: None };
        self.polkit(conn, hdr, &caller, actions::MANAGE).await?;
        Ok(who)
    }

    /// Run a calibration (`CalibrateWith` options: `quick`, plus the broker extensions
    /// `storage` (default true) and `force` (default true: a busy machine yields a map
    /// flagged `noisy` instead of an error)).
    async fn run_calibration(&self, options: &serde_json::Value) -> Result<String, CtlError> {
        let flag = |k: &str, d: bool| options.get(k).and_then(serde_json::Value::as_bool).unwrap_or(d);
        let opts = lindesk_sys::calibrate::CalibrateOptions {
            quick: flag("quick", false),
            force: flag("force", true),
            storage: flag("storage", true),
            ..Default::default()
        };
        let c = &self.calibration;
        if c.busy.swap(true, Ordering::SeqCst) {
            return Err(CtlError::Failed("a calibration is already running; try again in a few seconds".into()));
        }
        tracing::info!("calibrating on request ({} mode)", if opts.quick { "quick" } else { "full" });
        let rx = calibration::spawn_run(c.roots.clone(), opts, c.path.clone(), c.busy.clone())
            .map_err(|e| CtlError::Failed(format!("cannot start the calibration: {e}")))?;
        let map = rx
            .await
            .map_err(|_| CtlError::Failed("the calibration thread died".into()))?
            .map_err(|e| CtlError::Failed(format!("calibration: {e}")))?;
        let json = serde_json::to_string(&map).map_err(|e| CtlError::Failed(e.to_string()))?;
        self.engine.send(Command::Calibrated { map: Box::new(map), path: c.path.clone() })?;
        Ok(json)
    }

    /// Track a session agent until its bus name vanishes.
    fn watch_session(&self, conn: Connection, name: OwnedUniqueName, uid: u32) {
        let sessions = self.sessions.clone();
        let engine = self.engine.clone();
        tokio::spawn(async move {
            let Ok(dbus) = fdo::DBusProxy::new(&conn).await else { return };
            let stream = dbus.receive_name_owner_changed_with_args(&[(0, name.as_str())]).await;
            let mut stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("cannot watch {name}: {e}");
                    return;
                }
            };
            // The name may have vanished before the match rule was in place.
            let alive = dbus.name_has_owner(BusName::from(name.as_ref())).await.unwrap_or(false);
            if alive {
                while let Some(sig) = stream.next().await {
                    if sig.args().is_ok_and(|a| a.new_owner().is_none()) {
                        break;
                    }
                }
            }
            let last = {
                let mut s = sessions.lock().unwrap_or_else(|e| e.into_inner());
                s.remove(&name);
                !s.values().any(|u| *u == uid)
            };
            tracing::info!(uid, %name, "session agent left the bus");
            if last {
                let _ = engine.ext(BrokerExt::UnregisterUser { uid });
            }
        });
    }
}

#[interface(name = "org.lindesk.Broker1")]
impl Broker {
    /// Register the caller's session agent; returns the current `SessionState` JSON.
    async fn register_session(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<String> {
        let caller = self.caller(conn, &hdr).await?;
        let name = OwnedUniqueName::from(sender(&hdr)?.to_owned());
        let new = {
            let mut s = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            s.insert(name.clone(), caller.uid).is_none()
        };
        if new {
            self.watch_session(conn.clone(), name, caller.uid);
        }
        let uid = caller.uid;
        self.engine
            .call(|reply| crate::common::engine_thread::Command::Ext(BrokerExt::RegisterUser { uid, reply }))
            .await
            .map_err(gone)
    }

    /// Focus moved to `pid` (or, with `pid <= 0`, to the app `app_id`).
    async fn report_foreground(
        &self,
        pid: i32,
        app_id: String,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<()> {
        let app_id = Some(app_id).filter(|a| !a.is_empty());
        if pid <= 0 {
            let Some(app_id) = app_id else {
                return Err(fdo::Error::InvalidArgs("need a pid or an app id".into()));
            };
            let caller = self.caller(conn, &hdr).await?;
            let uid = (caller.uid != 0).then_some(caller.uid);
            return self.engine.ext(BrokerExt::ForegroundApp { app_id, uid }).map_err(gone);
        }
        self.check_pid(conn, &hdr, pid).await?;
        self.engine
            .push_event(Event::Focus { ts: now_ms(), pid: Some(pid), app_id, target: None, source: "dbus".into() })
            .map_err(gone)
    }

    /// The caller's apps that have a window on screen (`kind = "visible"`) or are making
    /// sound (`"audible"`). The complete list each time; processes of other users are
    /// dropped from it.
    async fn report_presence(
        &self,
        kind: String,
        pids: Vec<i32>,
        app_ids: Vec<String>,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<()> {
        let caller = self.caller(conn, &hdr).await?;
        let pids: Vec<i32> = pids
            .into_iter()
            .filter(|p| *p > 0 && (caller.uid == 0 || proc_uid("/proc".as_ref(), *p) == Some(caller.uid)))
            .take(1024)
            .collect();
        let source = format!("uid:{}", caller.uid);
        let ts = now_ms();
        let event = match kind.as_str() {
            "visible" => Event::Visible { ts, pids, app_ids: app_ids.into_iter().take(256).collect(), source },
            "audible" => Event::Audible { ts, pids, source },
            other => return Err(fdo::Error::InvalidArgs(format!("unknown kind '{other}'"))),
        };
        self.engine.push_event(event).map_err(gone)
    }

    /// Start (`end = false`) or end an interaction epoch for `pid`.
    async fn interaction(
        &self,
        pid: i32,
        end: bool,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<()> {
        self.check_pid(conn, &hdr, pid).await?;
        self.engine
            .push_event(Event::Interaction { ts: now_ms(), pid: Some(pid), target: None, end, source: "dbus".into() })
            .map_err(gone)
    }

    /// Pin a QoS class on the unit owning `pid`.
    async fn set_class(
        &self,
        pid: i32,
        class: String,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<()> {
        let class: QosClass = class.parse().map_err(fdo::Error::InvalidArgs)?;
        let caller = self.caller(conn, &hdr).await?;
        let owner =
            proc_uid("/proc".as_ref(), pid).ok_or_else(|| fdo::Error::InvalidArgs(format!("no process {pid}")))?;
        let action = if owner == caller.uid { actions::SET_CLASS_OWN } else { actions::SET_CLASS_ANY };
        self.polkit(conn, &hdr, &caller, action).await?;
        let source = format!("dbus (uid {})", caller.uid);
        self.engine.set_class(pid, class, source).await.map_err(gone)?.map(|_| ()).map_err(fdo::Error::Failed)
    }

    async fn report_dependency(
        &self,
        client_pid: i32,
        server_pid: i32,
        ttl_ms: u32,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<()> {
        let who = self.caller(conn, &hdr).await?;
        let lease = crate::common::responsiveness::Lease::capture(client_pid, server_pid, who.uid, ttl_ms)
            .map_err(fdo::Error::InvalidArgs)?;
        self.engine
            .call(|reply| crate::common::engine_thread::Command::Dependency { lease, ttl_ms, reply })
            .await
            .map_err(gone)?
            .map_err(fdo::Error::LimitsExceeded)
    }

    async fn latency_report(&self) -> fdo::Result<String> {
        self.engine.call(|reply| crate::common::engine_thread::Command::Latency { reply }).await.map_err(gone)
    }

    /// Status JSON (see ARCHITECTURE.md).
    async fn get_status(&self) -> fdo::Result<String> {
        self.engine.status().await.map_err(gone)
    }

    /// Explanation JSON for a pid or target string.
    async fn explain(&self, what: String) -> fdo::Result<String> {
        self.engine.explain(what).await.map_err(gone)?.map_err(fdo::Error::InvalidArgs)
    }

    /// The last `n` events the caller may see (oldest first) as a JSON array: root gets
    /// everything, other users system-scope events plus their own.
    async fn recent_events(
        &self,
        n: u32,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<String> {
        let caller = self.caller(conn, &hdr).await?;
        Ok(self.engine.recent_events_for(n as usize, Some(caller.uid)))
    }

    /// Re-read the config and rediscover plugins.
    async fn reload(&self, #[zbus(header)] hdr: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        let caller = self.caller(conn, &hdr).await?;
        self.polkit(conn, &hdr, &caller, actions::MANAGE).await?;
        self.engine.reload().await.map_err(gone)?.map(|_| ()).map_err(fdo::Error::Failed)
    }

    /// Pause (observe-only: every applied setting reverted, sensing continues) or resume.
    /// `seconds > 0` auto-resumes after that long. Returns the pause JSON.
    async fn set_paused(
        &self,
        paused: bool,
        seconds: u32,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<String> {
        let caller = self.caller(conn, &hdr).await?;
        self.polkit(conn, &hdr, &caller, actions::MANAGE).await?;
        self.engine.set_paused(paused, seconds, format!("uid {}", caller.uid)).await.map_err(gone)
    }

    /// The pause JSON: `{"paused", "since_ms", "until_ms", "by"}`.
    async fn get_paused(&self) -> fdo::Result<String> {
        self.engine.get_paused().await.map_err(gone)
    }

    // ---- control plane (docs/CONTROL-PLANE.md §5), system scope ----------------------------

    /// Plan a changeset (no side effects): Plan JSON.
    async fn plan_change(&self, ops_json: String) -> Result<String, CtlError> {
        self.control()?.plan(&ops_json).await
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
        let ctl = self.control()?;
        let who = self.manager(conn, &hdr).await?;
        ctl.apply(&ops_json, &description, ttl_secs, &who).await
    }

    /// Keep an applied changeset (stops its dead-man timer).
    async fn confirm_change(
        &self,
        id: u64,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<(), CtlError> {
        let ctl = self.control()?;
        self.manager(conn, &hdr).await?;
        ctl.confirm(id).await
    }

    /// Revert a changeset: the new ChangeSet JSON.
    async fn revert_change(
        &self,
        id: u64,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<String, CtlError> {
        let ctl = self.control()?;
        let who = self.manager(conn, &hdr).await?;
        ctl.revert(id, &who).await
    }

    /// The newest `limit` changesets (JSON array, newest first).
    async fn list_changes(&self, limit: u32) -> Result<String, CtlError> {
        self.control()?.list(limit).await
    }

    /// One changeset.
    async fn get_change(&self, id: u64) -> Result<String, CtlError> {
        self.control()?.get(id).await
    }

    /// Profiles: name, description, active, matches_now, reason.
    async fn list_profiles(&self) -> Result<String, CtlError> {
        control::profiles_json(&self.engine).await
    }

    /// A `profile_use` changeset (`"auto"` allowed): ChangeSet JSON.
    async fn set_profile(
        &self,
        name: String,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<String, CtlError> {
        let ctl = self.control()?;
        let who = self.manager(conn, &hdr).await?;
        ctl.set_profile(&name, &who).await
    }

    /// `Hardware` JSON including the latency map.
    async fn get_hardware(&self) -> Result<String, CtlError> {
        control::hardware_json(&self.engine).await
    }

    /// Swap analysis JSON.
    async fn swap_report(&self) -> Result<String, CtlError> {
        control::swap_json(&self.engine).await
    }

    /// Run the calibration (≈15 s) and return the `LatencyMap` JSON.
    async fn calibrate(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<String, CtlError> {
        self.manager(conn, &hdr).await?;
        self.run_calibration(&serde_json::Value::Null).await
    }

    /// [`Self::calibrate`] with options (`{"quick": true}`; see `CalibrateRequest`).
    async fn calibrate_with(
        &self,
        options_json: String,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> Result<String, CtlError> {
        let options: serde_json::Value = if options_json.trim().is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&options_json).map_err(|e| fdo::Error::InvalidArgs(format!("options_json: {e}")))?
        };
        self.manager(conn, &hdr).await?;
        self.run_calibration(&options).await
    }

    /// A system-scope changeset transition (ChangeSet JSON).
    #[zbus(signal)]
    pub async fn change_event(emitter: &SignalEmitter<'_>, json: &str) -> zbus::Result<()>;

    /// Desired settings for one user's units (unicast to that user's agents).
    #[zbus(signal)]
    pub async fn user_state(emitter: &SignalEmitter<'_>, uid: u32, json: &str) -> zbus::Result<()>;

    /// A JSON array of engine events (see ARCHITECTURE.md).
    #[zbus(signal)]
    pub async fn events(emitter: &SignalEmitter<'_>, json: &str) -> zbus::Result<()>;
}

fn agents_of(sessions: &Sessions, uid: u32) -> Vec<OwnedUniqueName> {
    let s = sessions.lock().unwrap_or_else(|e| e.into_inner());
    s.iter().filter(|(_, u)| **u == uid).map(|(n, _)| n.clone()).collect()
}

/// Send `UserState` to every registered agent of `uid`.
pub async fn send_user_state(conn: &Connection, sessions: &Sessions, uid: u32, json: &str) {
    for name in agents_of(sessions, uid) {
        let emitter = match SignalEmitter::new(conn, BROKER_PATH) {
            Ok(e) => e.set_destination(BusName::from(name.as_ref())),
            Err(e) => {
                tracing::warn!("UserState: {e}");
                return;
            }
        };
        if let Err(e) = Broker::user_state(&emitter, uid, json).await {
            tracing::debug!(%name, "UserState not delivered: {e}");
        }
    }
}

/// Send `Events`: broadcast for system-scope events, unicast to the registered agents of
/// the user they are about otherwise (nobody else, not even other processes of that user).
pub async fn send_events(conn: &Connection, sessions: &Sessions, json: &str, audience: Audience) {
    let emitter = match SignalEmitter::new(conn, BROKER_PATH) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("Events: {e}");
            return;
        }
    };
    let Some(uid) = audience else {
        if let Err(e) = Broker::events(&emitter, json).await {
            tracing::debug!("Events not sent: {e}");
        }
        return;
    };
    for name in agents_of(sessions, uid) {
        let to = emitter.clone().set_destination(BusName::from(name.as_ref()));
        if let Err(e) = Broker::events(&to, json).await {
            tracing::debug!(%name, "Events not delivered: {e}");
        }
    }
}

/// Broadcast `ChangeEvent`.
pub async fn send_change_event(conn: &Connection, json: &str) {
    match SignalEmitter::new(conn, BROKER_PATH) {
        Ok(e) => {
            if let Err(e) = Broker::change_event(&e, json).await {
                tracing::debug!("ChangeEvent not sent: {e}");
            }
        }
        Err(e) => tracing::warn!("ChangeEvent: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insecure_only_unprivileged() {
        assert!(insecure_no_polkit(Some("1"), false));
        assert!(!insecure_no_polkit(Some("1"), true), "never as root");
        assert!(!insecure_no_polkit(Some("yes"), false));
        assert!(!insecure_no_polkit(None, false));
    }
}
