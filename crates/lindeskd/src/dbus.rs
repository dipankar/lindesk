//! `org.lindesk.Broker1` on the system bus.
//!
//! Authorization (callers are identified by their bus credentials):
//!
//! | Member | Rule |
//! |---|---|
//! | `RegisterSession` | anyone; the caller's uid is recorded |
//! | `ReportForeground`, `Interaction` | pid owned by the caller, else polkit `set-class-any`; root always. `pid <= 0` with an app id: only that user's units match |
//! | `SetClass` | own pid: polkit `set-class-own`; other pids: `set-class-any`; root always |
//! | `GetStatus`, `Explain`, `RecentEvents` | anyone |
//! | `Reload` | polkit `manage` |
//!
//! polkit checks are non-interactive unless the message carries
//! `ALLOW_INTERACTIVE_AUTHORIZATION`. `LINDESK_INSECURE_NO_POLKIT=1` skips polkit (test
//! buses have none); it is honoured only when the broker does not run as root.
//!
//! `UserState` is sent *unicast* to each registered agent of that user (other users never
//! see it). `Events` is broadcast.

use std::collections::BTreeMap;
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
use crate::common::engine_thread::{EngineGone, EngineHandle};
use crate::common::proc_uid;

/// Env var that disables polkit checks for unprivileged test runs.
pub const INSECURE_ENV: &str = "LINDESK_INSECURE_NO_POLKIT";

/// True when polkit may be skipped: the env var is `1` and we are not root.
pub fn insecure_no_polkit(env: Option<&str>, euid_is_root: bool) -> bool {
    env == Some("1") && !euid_is_root
}

/// Registered session agents: unique bus name → uid.
pub type Sessions = Arc<Mutex<BTreeMap<OwnedUniqueName, u32>>>;

pub struct Broker {
    pub engine: EngineHandle<BrokerExt>,
    pub sessions: Sessions,
    pub insecure: bool,
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

    /// Status JSON (see ARCHITECTURE.md).
    async fn get_status(&self) -> fdo::Result<String> {
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

    /// Re-read the config and rediscover plugins.
    async fn reload(&self, #[zbus(header)] hdr: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        let caller = self.caller(conn, &hdr).await?;
        self.polkit(conn, &hdr, &caller, actions::MANAGE).await?;
        self.engine.reload().await.map_err(gone)?.map(|_| ()).map_err(fdo::Error::Failed)
    }

    /// Desired settings for one user's units (unicast to that user's agents).
    #[zbus(signal)]
    pub async fn user_state(emitter: &SignalEmitter<'_>, uid: u32, json: &str) -> zbus::Result<()>;

    /// A JSON array of engine events (see ARCHITECTURE.md).
    #[zbus(signal)]
    pub async fn events(emitter: &SignalEmitter<'_>, json: &str) -> zbus::Result<()>;
}

/// Send `UserState` to every registered agent of `uid`.
pub async fn send_user_state(conn: &Connection, sessions: &Sessions, uid: u32, json: &str) {
    let names: Vec<OwnedUniqueName> = {
        let s = sessions.lock().unwrap_or_else(|e| e.into_inner());
        s.iter().filter(|(_, u)| **u == uid).map(|(n, _)| n.clone()).collect()
    };
    for name in names {
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

/// Broadcast `Events`.
pub async fn send_events(conn: &Connection, json: &str) {
    match SignalEmitter::new(conn, BROKER_PATH) {
        Ok(e) => {
            if let Err(e) = Broker::events(&e, json).await {
                tracing::debug!("Events not sent: {e}");
            }
        }
        Err(e) => tracing::warn!("Events: {e}"),
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
