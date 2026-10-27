//! The link to the root broker (`org.lindesk.Broker1` on the system bus).
//!
//! Registers at startup, follows `NameOwnerChanged` for the broker's name (re-registering
//! when a broker appears, going standalone when it vanishes), retries every
//! [`RETRY`] while standalone, feeds `UserState` signals for this uid into the engine and
//! forwards focus/interaction/class events while connected.

use std::time::Duration;

use futures_util::StreamExt;
use lindesk_ipc::{BROKER_NAME, BrokerProxy};
use lindesk_plugin_api::Event;
use tokio::sync::mpsc::UnboundedReceiver;
use zbus::Connection;
use zbus::names::BusName;
use zbus::proxy::CacheProperties;

use crate::common::engine_thread::EngineHandle;
use crate::mode::{self, Input, Mode};
use crate::session::SessionExt;

/// Registration attempt timeout.
pub const REGISTER_TIMEOUT: Duration = Duration::from_secs(5);
/// While standalone with a broker present (or no system bus), retry this often.
pub const RETRY: Duration = Duration::from_secs(30);

/// Something to tell the broker.
#[derive(Clone, Debug, PartialEq)]
pub enum Forward {
    Dependency {
        client_pid: i32,
        server_pid: i32,
        ttl_ms: u32,
        sent: std::time::Instant,
    },
    /// `ReportForeground(pid, app_id)`; `pid = 0` when only the app id is known.
    Focus {
        pid: i32,
        app_id: String,
    },
    Interaction {
        pid: i32,
        end: bool,
    },
    SetClass {
        pid: i32,
        class: String,
    },
    /// `ReportPresence("visible", ..)`: the apps with a window on screen.
    Visible {
        pids: Vec<i32>,
        app_ids: Vec<String>,
    },
    /// `ReportPresence("audible", ..)`: the processes making sound.
    Audible {
        pids: Vec<i32>,
    },
}

impl Forward {
    /// The forward for an adapter event, if any.
    pub fn from_event(e: &Event) -> Option<Forward> {
        match e {
            Event::Focus { pid, app_id, .. } if pid.is_some() || app_id.is_some() => {
                Some(Forward::Focus { pid: pid.unwrap_or(0), app_id: app_id.clone().unwrap_or_default() })
            }
            Event::Interaction { pid: Some(pid), end, .. } => Some(Forward::Interaction { pid: *pid, end: *end }),
            Event::Visible { pids, app_ids, .. } => {
                Some(Forward::Visible { pids: pids.clone(), app_ids: app_ids.clone() })
            }
            Event::Audible { pids, .. } => Some(Forward::Audible { pids: pids.clone() }),
            _ => None,
        }
    }
}

async fn proxy(conn: &Connection) -> zbus::Result<BrokerProxy<'static>> {
    BrokerProxy::builder(conn).cache_properties(CacheProperties::No).build().await
}

/// One registration attempt: the `SessionState` JSON on success.
pub async fn register(conn: &Connection) -> Result<String, String> {
    let p = proxy(conn).await.map_err(|e| e.to_string())?;
    match tokio::time::timeout(REGISTER_TIMEOUT, p.register_session()).await {
        Ok(Ok(json)) => Ok(json),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("timed out".into()),
    }
}

/// Connect to the system bus and register. `(connection, state JSON)`; either may be `None`.
pub async fn connect_and_register() -> (Option<Connection>, Option<String>) {
    let conn = match Connection::system().await {
        Ok(c) => c,
        Err(e) => {
            tracing::info!("no system bus ({e}); running standalone");
            return (None, None);
        }
    };
    match register(&conn).await {
        Ok(json) => {
            tracing::info!("registered with the lindeskd broker");
            (Some(conn), Some(json))
        }
        Err(e) => {
            tracing::info!("no lindeskd broker ({e}); running standalone");
            (Some(conn), None)
        }
    }
}

async fn send(p: &BrokerProxy<'static>, f: Forward) {
    let r = match &f {
        Forward::Dependency { client_pid, server_pid, ttl_ms, sent } => {
            let remaining = ttl_ms.saturating_sub(sent.elapsed().as_millis().min(u32::MAX as u128) as u32);
            p.report_dependency(*client_pid, *server_pid, remaining).await
        }
        Forward::Focus { pid, app_id } => p.report_foreground(*pid, app_id).await,
        Forward::Interaction { pid, end } => p.interaction(*pid, *end).await,
        Forward::SetClass { pid, class } => p.set_class(*pid, class).await,
        Forward::Visible { pids, app_ids } => p.report_presence("visible", pids.clone(), app_ids.clone()).await,
        Forward::Audible { pids } => p.report_presence("audible", pids.clone(), Vec::new()).await,
    };
    if let Err(e) = r {
        tracing::debug!("forwarding {f:?} to the broker failed: {e}");
    }
}

/// Run the link until the engine goes away. `mode` is the mode the agent started in.
pub async fn run(
    conn: Option<Connection>,
    uid: u32,
    forced: bool,
    mut mode: Mode,
    engine: EngineHandle<SessionExt>,
    mut forward: UnboundedReceiver<Forward>,
) {
    if forced {
        // Standalone by request: drop forwards, never touch the system bus.
        while forward.recv().await.is_some() {}
        return;
    }
    let conn = match conn {
        Some(c) => c,
        None => loop {
            // No system bus yet: retry now and then, dropping forwards meanwhile.
            tokio::select! {
                f = forward.recv() => if f.is_none() { return },
                _ = tokio::time::sleep(RETRY) => {
                    if let Ok(c) = Connection::system().await { break c; }
                }
            }
        },
    };
    let (Ok(dbus), Ok(broker)) = (zbus::fdo::DBusProxy::new(&conn).await, proxy(&conn).await) else {
        tracing::warn!("cannot create broker proxies; staying standalone");
        while forward.recv().await.is_some() {}
        return;
    };
    let (Ok(mut owners), Ok(mut states)) =
        (dbus.receive_name_owner_changed_with_args(&[(0, BROKER_NAME)]).await, broker.receive_user_state().await)
    else {
        tracing::warn!("cannot subscribe to broker signals; staying standalone");
        while forward.recv().await.is_some() {}
        return;
    };

    let apply = |mode: &mut Mode, input: Input, state: Option<String>| {
        let t = mode::next(forced, *mode, input);
        if t.mode != *mode || state.is_some() {
            *mode = t.mode;
            let _ = engine.ext(SessionExt::SetMode { mode: t.mode, state });
        }
        t.register
    };
    // If the broker appeared between startup and our subscription, catch up.
    if mode != Mode::Connected
        && dbus.name_has_owner(BusName::try_from(BROKER_NAME).expect("valid")).await.unwrap_or(false)
    {
        match register(&conn).await {
            Ok(json) => {
                apply(&mut mode, Input::Registered, Some(json));
            }
            Err(e) => tracing::debug!("broker present but registration failed: {e}"),
        }
    }

    let mut retry = tokio::time::interval(RETRY);
    retry.tick().await;
    loop {
        tokio::select! {
            sig = owners.next() => {
                let Some(sig) = sig else { break };
                let Ok(args) = sig.args() else { continue };
                let input = if args.new_owner().is_some() { Input::BrokerAppeared } else { Input::BrokerVanished };
                if input == Input::BrokerVanished {
                    tracing::info!("the lindeskd broker went away; running standalone");
                }
                if apply(&mut mode, input, None) {
                    match register(&conn).await {
                        Ok(json) => {
                            tracing::info!("registered with the lindeskd broker");
                            apply(&mut mode, Input::Registered, Some(json));
                        }
                        Err(e) => {
                            tracing::warn!("broker registration failed: {e}");
                            apply(&mut mode, Input::RegisterFailed, None);
                        }
                    }
                }
            }
            sig = states.next() => {
                let Some(sig) = sig else { break };
                if let Ok(args) = sig.args()
                    && args.uid == uid
                    && mode == Mode::Connected
                {
                    let _ = engine.ext(SessionExt::BrokerState(args.json.clone()));
                }
            }
            f = forward.recv() => {
                let Some(f) = f else { break };
                if mode == Mode::Connected {
                    let p = broker.clone();
                    if matches!(f, Forward::Dependency { .. }) {
                        let _ = tokio::time::timeout(REGISTER_TIMEOUT, send(&p, f)).await;
                    } else {
                        tokio::spawn(async move { send(&p, f).await });
                    }
                }
            }
            _ = retry.tick() => {
                if mode != Mode::Connected
                    && dbus.name_has_owner(BusName::try_from(BROKER_NAME).expect("valid")).await.unwrap_or(false)
                    && let Ok(json) = register(&conn).await
                {
                    tracing::info!("registered with the lindeskd broker");
                    apply(&mut mode, Input::Registered, Some(json));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwards_from_adapter_events() {
        let f = Event::Focus { ts: 1, pid: Some(5), app_id: None, target: None, source: "x11".into() };
        assert_eq!(Forward::from_event(&f), Some(Forward::Focus { pid: 5, app_id: String::new() }));
        let f = Event::Focus { ts: 1, pid: None, app_id: Some("org.x".into()), target: None, source: "cosmic".into() };
        assert_eq!(Forward::from_event(&f), Some(Forward::Focus { pid: 0, app_id: "org.x".into() }));
        let f = Event::Focus { ts: 1, pid: None, app_id: None, target: None, source: "x11".into() };
        assert_eq!(Forward::from_event(&f), None);
        let i = Event::Interaction { ts: 1, pid: Some(3), target: None, end: true, source: "x11".into() };
        assert_eq!(Forward::from_event(&i), Some(Forward::Interaction { pid: 3, end: true }));
        let i = Event::Interaction { ts: 1, pid: None, target: None, end: false, source: "x11".into() };
        assert_eq!(Forward::from_event(&i), None);
    }
}
