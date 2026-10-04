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
    /// JSON array of the last `n` events (oldest first).
    fn recent_events(&self, n: u32) -> zbus::Result<String>;

    // ---- control plane (docs/CONTROL-PLANE.md §5), user scope --------------------------

    /// Plan a changeset without side effects. `ops_json` is a [`ChangeEnvelope`] (or a bare
    /// array of ops); returns the Plan JSON (`risk`, `diff`, `rejections`).
    fn plan_change(&self, ops_json: &str) -> zbus::Result<String>;
    /// Validate, apply and version a changeset; returns the ChangeSet JSON. `ttl_secs = 0`
    /// means no auto-revert (refused for High risk from MCP).
    fn apply_change(&self, ops_json: &str, description: &str, ttl_secs: u32) -> zbus::Result<String>;
    /// Confirm an applied changeset so its dead-man switch no longer reverts it.
    fn confirm_change(&self, id: u64) -> zbus::Result<()>;
    /// Revert a changeset; returns the new (reverting) ChangeSet JSON.
    fn revert_change(&self, id: u64) -> zbus::Result<String>;
    /// JSON array of the newest `limit` changesets.
    fn list_changes(&self, limit: u32) -> zbus::Result<String>;
    /// One changeset as JSON.
    fn get_change(&self, id: u64) -> zbus::Result<String>;
    /// JSON array of profiles: name, description, active, matches-now, reason.
    fn list_profiles(&self) -> zbus::Result<String>;
    /// Shortcut for a `ProfileUse` changeset (`"auto"` allowed); returns the ChangeSet JSON.
    fn set_profile(&self, name: &str) -> zbus::Result<String>;
    /// `Hardware` JSON including the latency map.
    fn get_hardware(&self) -> zbus::Result<String>;
    /// `SwapReport` JSON.
    fn swap_report(&self) -> zbus::Result<String>;

    #[zbus(signal)]
    fn level_changed(&self, level: String) -> zbus::Result<()>;
    /// JSON array of the events of one engine step.
    #[zbus(signal)]
    fn events(&self, json: String) -> zbus::Result<()>;
    /// A changeset transition (applied, confirmed, reverted, failed); ChangeSet JSON.
    #[zbus(signal)]
    fn change_event(&self, json: String) -> zbus::Result<()>;
}

/// The broker's proxy lives in its own module because zbus names the generated signal types
/// after the D-Bus member (`EventsArgs`, `ChangeEventStream`, ...), which `QoS1` shares.
/// Everything is re-exported at the crate root as before; the broker's signal types are
/// under [`broker`].
pub mod broker {
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
        /// JSON array of the last `n` events (oldest first).
        fn recent_events(&self, n: u32) -> zbus::Result<String>;

        // ---- control plane (docs/CONTROL-PLANE.md §5), system scope ------------------------
        // Mutating members need polkit `org.lindesk.manage` and may show an authentication
        // dialog (`ALLOW_INTERACTIVE_AUTHORIZATION`).

        /// Plan a changeset without side effects; see [`QoSProxy::plan_change`](crate::QoSProxy::plan_change).
        fn plan_change(&self, ops_json: &str) -> zbus::Result<String>;
        /// Validate, apply and version a changeset; see [`QoSProxy::apply_change`](crate::QoSProxy::apply_change).
        #[zbus(allow_interactive_auth)]
        fn apply_change(&self, ops_json: &str, description: &str, ttl_secs: u32) -> zbus::Result<String>;
        #[zbus(allow_interactive_auth)]
        fn confirm_change(&self, id: u64) -> zbus::Result<()>;
        #[zbus(allow_interactive_auth)]
        fn revert_change(&self, id: u64) -> zbus::Result<String>;
        fn list_changes(&self, limit: u32) -> zbus::Result<String>;
        fn get_change(&self, id: u64) -> zbus::Result<String>;
        fn list_profiles(&self) -> zbus::Result<String>;
        #[zbus(allow_interactive_auth)]
        fn set_profile(&self, name: &str) -> zbus::Result<String>;
        fn get_hardware(&self) -> zbus::Result<String>;
        fn swap_report(&self) -> zbus::Result<String>;
        /// Run calibration (≈15 s) and return the `LatencyMap` JSON. Broker only.
        #[zbus(allow_interactive_auth)]
        fn calibrate(&self) -> zbus::Result<String>;
        /// Optional extension of [`BrokerProxy::calibrate`]: `options_json` is
        /// [`CalibrateRequest`](crate::CalibrateRequest) (e.g. `{"quick": true}`). Clients fall back to `Calibrate()` when
        /// the broker doesn't implement it.
        #[zbus(allow_interactive_auth)]
        fn calibrate_with(&self, options_json: &str) -> zbus::Result<String>;

        /// Desired settings for one user's units, applied by that user's session agent.
        #[zbus(signal)]
        fn user_state(&self, uid: u32, json: String) -> zbus::Result<()>;
        /// JSON array of the events of one broker step.
        #[zbus(signal)]
        fn events(&self, json: String) -> zbus::Result<()>;
        /// A system-scope changeset transition; ChangeSet JSON.
        #[zbus(signal)]
        fn change_event(&self, json: String) -> zbus::Result<()>;
    }
}

pub use broker::{BrokerProxy, BrokerProxyBlocking, UserState, UserStateArgs, UserStateIterator, UserStateStream};

/// The `ops_json` argument of `PlanChange`/`ApplyChange`: the ops plus who sent them.
/// The daemons record `origin`/`client` (with the caller's uid and bus name) as the
/// changeset's author. A bare JSON array of ops is also accepted.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ChangeEnvelope {
    /// `"mcp"`, `"lindeskctl"`, ...
    pub origin: String,
    /// The MCP client name from `initialize` (e.g. `"claude-code"`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// `ChangeOp` values (`{"op": "sysctl", ...}`), kept as JSON here so this crate
    /// doesn't depend on `lindesk-changes`.
    pub ops: Vec<serde_json::Value>,
}

impl ChangeEnvelope {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Parse an envelope, or a bare array of ops (origin `"unknown"`).
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        match serde_json::from_str::<serde_json::Value>(s)? {
            serde_json::Value::Array(ops) => Ok(ChangeEnvelope { origin: "unknown".into(), client: None, ops }),
            v => serde_json::from_value(v),
        }
    }
}

/// The argument of `CalibrateWith`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct CalibrateRequest {
    /// A shorter, less precise run (a few seconds instead of ≈15 s).
    pub quick: bool,
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

    #[test]
    fn change_envelope_accepts_bare_arrays() {
        let e = ChangeEnvelope {
            origin: "mcp".into(),
            client: Some("claude-code".into()),
            ops: vec![serde_json::json!({"op": "sysctl", "key": "vm.swappiness", "value": "10"})],
        };
        assert_eq!(ChangeEnvelope::from_json(&e.to_json()).unwrap(), e);
        let bare = ChangeEnvelope::from_json(r#"[{"op":"cpu_epp","preference":"balance_performance"}]"#).unwrap();
        assert_eq!(bare.origin, "unknown");
        assert_eq!(bare.ops.len(), 1);
    }
}
