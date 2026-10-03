//! D-Bus contract between lindesk components (see docs/ARCHITECTURE.md).
//!
//! * `org.lindesk.QoS1` on the session bus, served by `lindesk-session`; used by desktop
//!   shells, applications and `lindeskctl`.
//! * `org.lindesk.Broker1` on the system bus, served by `lindeskd`; used by session agents
//!   and `lindeskctl`.
//!
//! This crate holds the names, client proxies, payload types and the polkit/credential
//! helpers. The servers live in the daemons because they need the engine.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use lindesk_plugin_api::{Level, Provenance, Setting, Target};

pub mod auth;

pub const QOS_NAME: &str = "org.lindesk.QoS1";
pub const QOS_PATH: &str = "/org/lindesk/QoS1";
pub const QOS_INTERFACE: &str = "org.lindesk.QoS1";

pub const BROKER_NAME: &str = "org.lindesk.Broker1";
pub const BROKER_PATH: &str = "/org/lindesk/Broker1";
pub const BROKER_INTERFACE: &str = "org.lindesk.Broker1";

/// polkit action ids (packaging/polkit/org.lindesk.policy).
pub mod actions {
    pub const SET_CLASS_OWN: &str = "org.lindesk.set-class-own";
    pub const SET_CLASS_ANY: &str = "org.lindesk.set-class-any";
    pub const MANAGE: &str = "org.lindesk.manage";
}

/// Session-bus API used by desktop shells and applications.
#[zbus::proxy(interface = "org.lindesk.QoS1", default_service = "org.lindesk.QoS1", default_path = "/org/lindesk/QoS1")]
pub trait QoS {
    /// Focus moved to a window owned by `pid`.
    fn set_foreground_process(&self, pid: i32) -> zbus::Result<()>;
    /// Focus moved to an app known only by its id.
    fn set_foreground_app(&self, app_id: &str) -> zbus::Result<()>;
    /// Start or extend an interaction epoch.
    fn begin_interaction(&self, pid: i32) -> zbus::Result<()>;
    /// End an interaction epoch early.
    fn end_interaction(&self, pid: i32) -> zbus::Result<()>;
    /// Set the QoS class of one of the caller's processes.
    fn set_class(&self, pid: i32, class: &str) -> zbus::Result<()>;
    /// Record a free-form metric.
    fn report_metric(&self, name: &str, value: f64, labels: HashMap<String, String>) -> zbus::Result<()>;
    /// JSON status.
    fn status(&self) -> zbus::Result<String>;
    /// JSON explanation for a pid or target string.
    fn explain(&self, what: &str) -> zbus::Result<String>;
    #[zbus(signal)]
    fn level_changed(&self, level: String) -> zbus::Result<()>;
}

/// System-bus API of the privileged broker.
#[zbus::proxy(
    interface = "org.lindesk.Broker1",
    default_service = "org.lindesk.Broker1",
    default_path = "/org/lindesk/Broker1"
)]
pub trait Broker {
    /// Register the calling user's session agent; returns the current [`SessionState`] JSON.
    fn register_session(&self) -> zbus::Result<String>;
    fn report_foreground(&self, pid: i32, app_id: &str) -> zbus::Result<()>;
    fn interaction(&self, pid: i32, end: bool) -> zbus::Result<()>;
    fn set_class(&self, pid: i32, class: &str) -> zbus::Result<()>;
    fn get_status(&self) -> zbus::Result<String>;
    fn explain(&self, what: &str) -> zbus::Result<String>;
    fn reload(&self) -> zbus::Result<()>;
    /// Desired settings for one user's units, applied by that user's session agent.
    #[zbus(signal)]
    fn user_state(&self, uid: u32, json: String) -> zbus::Result<()>;
}

/// One setting the broker wants a session agent to apply on a user unit.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DesiredSetting {
    pub target: Target,
    pub setting: Setting,
    pub provenance: Provenance,
}

/// Payload of the `UserState` signal (named `SessionState` to avoid clashing with the generated signal type) and `RegisterSession` reply.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct SessionState {
    pub uid: u32,
    pub level: Level,
    /// Complete desired set for this user's units; anything absent must be reverted.
    pub desired: Vec<DesiredSetting>,
}

impl SessionState {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// Map a D-Bus error into an fdo error for method replies.
pub fn failed(e: impl std::fmt::Display) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_state_roundtrip() {
        let s = SessionState {
            uid: 1000,
            level: Level::Red,
            desired: vec![DesiredSetting {
                target: Target::user_unit(1000, "app-x.scope"),
                setting: Setting::CpuWeight { value: 20 },
                provenance: Provenance { plugin: "threshold".into(), ..Default::default() },
            }],
        };
        assert_eq!(SessionState::from_json(&s.to_json()).unwrap(), s);
    }
}
