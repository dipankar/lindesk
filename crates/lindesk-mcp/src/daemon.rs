//! [`Daemon`] over D-Bus: `org.lindesk.QoS1` on the session bus (user scope) and
//! `org.lindesk.Broker1` on the system bus (system scope).

use std::time::Duration;

use lindesk_ipc::{BROKER_NAME, BrokerProxy, CalibrateRequest, QOS_NAME, QoSProxy};
use tokio::sync::Mutex;
use zbus::Connection;

use crate::backend::{Daemon, DaemonError, DaemonRequest};
use crate::ops::Scope;

/// Ordinary calls.
const CALL_TIMEOUT: Duration = Duration::from_secs(20);
/// Calibration takes ≈15 s plus polkit interaction.
const CALIBRATE_TIMEOUT: Duration = Duration::from_secs(240);

/// Connects lazily and reconnects after a failure.
#[derive(Default)]
pub struct DbusDaemon {
    session: Mutex<Option<Connection>>,
    system: Mutex<Option<Connection>>,
}

impl DbusDaemon {
    pub fn new() -> Self {
        Self::default()
    }

    async fn connection(&self, scope: Scope) -> Result<Connection, DaemonError> {
        let slot = match scope {
            Scope::User => &self.session,
            Scope::System => &self.system,
        };
        let mut guard = slot.lock().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let builder = match scope {
            Scope::User => zbus::connection::Builder::session(),
            Scope::System => zbus::connection::Builder::system(),
        };
        // The per-call deadline is enforced with tokio timeouts below; this only has to
        // outlast the longest call.
        let no_bus = |e: zbus::Error| DaemonError::NotRunning {
            scope,
            detail: format!(
                "cannot connect to the D-Bus {} bus ({e})",
                if scope == Scope::User { "session" } else { "system" }
            ),
        };
        let conn = builder
            .map_err(no_bus)?
            .method_timeout(CALIBRATE_TIMEOUT + Duration::from_secs(10))
            .build()
            .await
            .map_err(no_bus)?;
        *guard = Some(conn.clone());
        Ok(conn)
    }

    async fn forget(&self, scope: Scope) {
        match scope {
            Scope::User => *self.session.lock().await = None,
            Scope::System => *self.system.lock().await = None,
        }
    }

    async fn owned(conn: &Connection, scope: Scope) -> Result<bool, DaemonError> {
        let name = match scope {
            Scope::User => QOS_NAME,
            Scope::System => BROKER_NAME,
        };
        let dbus = zbus::fdo::DBusProxy::new(conn).await.map_err(|e| classify(scope, "NameHasOwner", e))?;
        let bus_name = zbus::names::BusName::try_from(name).map_err(|e| classify(scope, "NameHasOwner", e.into()))?;
        dbus.name_has_owner(bus_name).await.map_err(|e| classify(scope, "NameHasOwner", e.into()))
    }

    async fn dispatch(conn: &Connection, scope: Scope, req: DaemonRequest) -> zbus::Result<String> {
        match scope {
            Scope::User => {
                let p = QoSProxy::new(conn).await?;
                match req {
                    DaemonRequest::Status => p.status().await,
                    DaemonRequest::Explain(w) => p.explain(&w).await,
                    DaemonRequest::RecentEvents(n) => p.recent_events(n).await,
                    DaemonRequest::PlanChange { ops_json } => p.plan_change(&ops_json).await,
                    DaemonRequest::ApplyChange { ops_json, description, ttl_secs } => {
                        p.apply_change(&ops_json, &description, ttl_secs).await
                    }
                    DaemonRequest::ConfirmChange(id) => p.confirm_change(id).await.map(|()| String::new()),
                    DaemonRequest::RevertChange(id) => p.revert_change(id).await,
                    DaemonRequest::ListChanges(n) => p.list_changes(n).await,
                    DaemonRequest::GetChange(id) => p.get_change(id).await,
                    DaemonRequest::ListProfiles => p.list_profiles().await,
                    DaemonRequest::SetProfile(n) => p.set_profile(&n).await,
                    DaemonRequest::GetHardware => p.get_hardware().await,
                    DaemonRequest::SwapReport => p.swap_report().await,
                    // Calibration is a broker (system scope) call.
                    DaemonRequest::Calibrate { .. } => Err(zbus::Error::Unsupported),
                    DaemonRequest::SetPaused { paused, seconds } => p.set_paused(paused, seconds).await,
                }
            }
            Scope::System => {
                let p = BrokerProxy::new(conn).await?;
                match req {
                    DaemonRequest::Status => p.get_status().await,
                    DaemonRequest::Explain(w) => p.explain(&w).await,
                    DaemonRequest::RecentEvents(n) => p.recent_events(n).await,
                    DaemonRequest::PlanChange { ops_json } => p.plan_change(&ops_json).await,
                    DaemonRequest::ApplyChange { ops_json, description, ttl_secs } => {
                        p.apply_change(&ops_json, &description, ttl_secs).await
                    }
                    DaemonRequest::ConfirmChange(id) => p.confirm_change(id).await.map(|()| String::new()),
                    DaemonRequest::RevertChange(id) => p.revert_change(id).await,
                    DaemonRequest::ListChanges(n) => p.list_changes(n).await,
                    DaemonRequest::GetChange(id) => p.get_change(id).await,
                    DaemonRequest::ListProfiles => p.list_profiles().await,
                    DaemonRequest::SetProfile(n) => p.set_profile(&n).await,
                    DaemonRequest::GetHardware => p.get_hardware().await,
                    DaemonRequest::SwapReport => p.swap_report().await,
                    DaemonRequest::SetPaused { paused, seconds } => p.set_paused(paused, seconds).await,
                    DaemonRequest::Calibrate { quick } => {
                        let opts = serde_json::to_string(&CalibrateRequest { quick }).unwrap_or_default();
                        match p.calibrate_with(&opts).await {
                            Err(e) if is_unknown_method(&e) => p.calibrate().await,
                            other => other,
                        }
                    }
                }
            }
        }
    }
}

fn is_unknown_method(e: &zbus::Error) -> bool {
    matches!(e, zbus::Error::MethodError(name, _, _)
        if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod")
}

/// Map a zbus error onto [`DaemonError`].
pub fn classify(scope: Scope, member: &str, e: zbus::Error) -> DaemonError {
    match &e {
        zbus::Error::MethodError(name, msg, _) => {
            let message = msg.clone().unwrap_or_else(|| name.to_string());
            let n = name.as_str();
            match n {
                "org.freedesktop.DBus.Error.ServiceUnknown" | "org.freedesktop.DBus.Error.NameHasNoOwner" => {
                    DaemonError::NotRunning { scope, detail: message }
                }
                "org.freedesktop.DBus.Error.UnknownMethod"
                | "org.freedesktop.DBus.Error.UnknownInterface"
                | "org.freedesktop.DBus.Error.UnknownObject" => {
                    DaemonError::Unsupported { scope, member: member.to_string() }
                }
                "org.freedesktop.DBus.Error.AccessDenied"
                | "org.freedesktop.DBus.Error.AuthFailed"
                | "org.freedesktop.DBus.Error.InteractiveAuthorizationRequired" => {
                    DaemonError::Denied { scope, message }
                }
                "org.freedesktop.DBus.Error.NoReply" | "org.freedesktop.DBus.Error.Timeout" => {
                    DaemonError::Timeout { scope, member: member.to_string() }
                }
                "org.freedesktop.DBus.Error.InvalidArgs" => DaemonError::Rejected { scope, message },
                _ if n.ends_with(".Rejected") || n.ends_with(".LimitsExceeded") || message.starts_with("rejected") => {
                    DaemonError::Rejected { scope, message }
                }
                _ => DaemonError::Failed { scope, message },
            }
        }
        zbus::Error::Unsupported => DaemonError::Unsupported { scope, member: member.to_string() },
        zbus::Error::InputOutput(_) | zbus::Error::Address(_) | zbus::Error::Handshake(_) => {
            DaemonError::NotRunning { scope, detail: e.to_string() }
        }
        _ => DaemonError::Failed { scope, message: e.to_string() },
    }
}

#[async_trait::async_trait]
impl Daemon for DbusDaemon {
    async fn call(&self, scope: Scope, req: DaemonRequest) -> Result<String, DaemonError> {
        let member = req.member();
        let conn = self.connection(scope).await?;
        match Self::owned(&conn, scope).await {
            Ok(true) => {}
            Ok(false) => {
                return Err(DaemonError::NotRunning {
                    scope,
                    detail: format!(
                        "nobody owns {} on the {} bus",
                        if scope == Scope::User { QOS_NAME } else { BROKER_NAME },
                        if scope == Scope::User { "session" } else { "system" }
                    ),
                });
            }
            Err(e) => {
                self.forget(scope).await;
                return Err(e);
            }
        }
        let limit = if matches!(req, DaemonRequest::Calibrate { .. }) { CALIBRATE_TIMEOUT } else { CALL_TIMEOUT };
        match tokio::time::timeout(limit, Self::dispatch(&conn, scope, req)).await {
            Err(_) => Err(DaemonError::Timeout { scope, member: member.to_string() }),
            Ok(Ok(s)) => Ok(s),
            Ok(Err(e)) => {
                let err = classify(scope, member, e);
                if matches!(err, DaemonError::NotRunning { .. }) {
                    self.forget(scope).await;
                }
                Err(err)
            }
        }
    }
}
