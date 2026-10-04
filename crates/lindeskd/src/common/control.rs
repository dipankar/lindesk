//! The control plane shared by both daemons (docs/CONTROL-PLANE.md §4–5): versioned,
//! guardrailed changes through one [`ChangeStore`] per daemon, profiles, hardware and swap
//! reports. The D-Bus interfaces (`org.lindesk.QoS1`, `org.lindesk.Broker1`) authorize the
//! caller and then call into [`Control`].
//!
//! Threads: store operations do file I/O with `fsync` (and runtime writes for system ops),
//! so they run on tokio's blocking pool, never on the reactor and never on the engine
//! thread. The engine is only asked for cheap snapshots (hardware, profile names) and told
//! to reload after a change to the config file.
//!
//! Every changeset transition reaches the world twice: as a `ChangeEvent` signal (the
//! changeset JSON, from [`Control::open`]'s channel) and as an `Event::Notice`
//! (`change.applied`, `change.reverted`, ...) in the engine's events.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lindesk_changes::{
    Author, ChangeError, ChangeOp, ChangeSet, ChangeStore, ConfigPaths, GuardContext, Origin, RealApplier, Scope,
};
use lindesk_core::clock::now_ms;
use lindesk_core::config::Config;
use lindesk_ipc::ChangeEnvelope;
use lindesk_plugin_api::Event;
use serde::Serialize;
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;
use zbus::message::Header;
use zbus::names::ErrorName;
use zbus::{Connection, fdo};

use super::engine_thread::{EngineGone, EngineHandle};

/// D-Bus error name of a guardrail rejection (lindesk-mcp matches the `.Rejected` suffix).
pub const ERROR_REJECTED: &str = "org.lindesk.Error.Rejected";
/// No such changeset.
pub const ERROR_NOT_FOUND: &str = "org.lindesk.Error.NotFound";
/// Anything else that went wrong while applying.
pub const ERROR_FAILED: &str = "org.lindesk.Error.Failed";

/// Longest sleep of the dead-man timer between two checks (guards against clock jumps).
const EXPIRY_MAX_WAIT: Duration = Duration::from_secs(60);

/// Errors of the control-plane D-Bus members.
#[derive(Debug)]
pub enum CtlError {
    /// A standard error (`AccessDenied`, `InvalidArgs`, ...).
    Fdo(fdo::Error),
    /// A guardrail refused the change ([`ERROR_REJECTED`]); the message says why.
    Rejected(String),
    /// [`ERROR_NOT_FOUND`].
    NotFound(String),
    /// [`ERROR_FAILED`].
    Failed(String),
}

impl CtlError {
    fn message(&self) -> &str {
        match self {
            CtlError::Fdo(e) => zbus::DBusError::description(e).unwrap_or("error"),
            CtlError::Rejected(m) | CtlError::NotFound(m) | CtlError::Failed(m) => m,
        }
    }
}

impl std::fmt::Display for CtlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", zbus::DBusError::name(self), self.message())
    }
}

impl std::error::Error for CtlError {}

impl zbus::DBusError for CtlError {
    fn create_reply(&self, call: &Header<'_>) -> zbus::Result<zbus::message::Message> {
        match self {
            CtlError::Fdo(e) => e.create_reply(call),
            _ => zbus::message::Message::error(call, self.name())?.build(&(self.message(),)),
        }
    }

    fn name(&self) -> ErrorName<'_> {
        match self {
            CtlError::Fdo(e) => e.name(),
            CtlError::Rejected(_) => ErrorName::from_static_str_unchecked(ERROR_REJECTED),
            CtlError::NotFound(_) => ErrorName::from_static_str_unchecked(ERROR_NOT_FOUND),
            CtlError::Failed(_) => ErrorName::from_static_str_unchecked(ERROR_FAILED),
        }
    }

    fn description(&self) -> Option<&str> {
        Some(self.message())
    }
}

impl From<fdo::Error> for CtlError {
    fn from(e: fdo::Error) -> Self {
        CtlError::Fdo(e)
    }
}

impl From<zbus::Error> for CtlError {
    fn from(e: zbus::Error) -> Self {
        CtlError::Fdo(e.into())
    }
}

impl From<EngineGone> for CtlError {
    fn from(e: EngineGone) -> Self {
        CtlError::Failed(e.to_string())
    }
}

impl From<ChangeError> for CtlError {
    fn from(e: ChangeError) -> Self {
        match e {
            ChangeError::Rejected(_)
            | ChangeError::NotPlanned
            | ChangeError::InvalidState { .. }
            | ChangeError::AlreadyReverted { .. }
            | ChangeError::Conflict { .. } => CtlError::Rejected(e.to_string()),
            ChangeError::NotFound(_) => CtlError::NotFound(e.to_string()),
            e => CtlError::Failed(e.to_string()),
        }
    }
}

/// Who called: uid and unique bus name (recorded as the changeset's author).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    pub uid: u32,
    pub bus_name: String,
}

/// Identify the sender of a method call.
pub async fn caller(conn: &Connection, hdr: &Header<'_>) -> Result<Caller, CtlError> {
    let sender = hdr.sender().ok_or_else(|| fdo::Error::AccessDenied("message without a sender".into()))?;
    let name = zbus::names::BusName::from(sender.clone());
    let c = lindesk_ipc::auth::caller(conn, &name).await.map_err(|e| fdo::Error::AccessDenied(e.to_string()))?;
    Ok(Caller { uid: c.uid, bus_name: sender.to_string() })
}

/// The `origin` hint of a [`ChangeEnvelope`] as an [`Origin`].
pub fn origin_of(hint: &str) -> Origin {
    match hint {
        "mcp" => Origin::Mcp,
        "cli" | "lindeskctl" => Origin::Cli,
        _ => Origin::Dbus,
    }
}

/// Parse `ops_json` (a [`ChangeEnvelope`] or a bare array of ops).
pub fn parse_ops(ops_json: &str) -> Result<(ChangeEnvelope, Vec<ChangeOp>), CtlError> {
    let env = ChangeEnvelope::from_json(ops_json)
        .map_err(|e| fdo::Error::InvalidArgs(format!("ops_json is not a change envelope or an array of ops: {e}")))?;
    let ops = env
        .ops
        .iter()
        .enumerate()
        .map(|(i, v)| {
            serde_json::from_value::<ChangeOp>(v.clone()).map_err(|e| fdo::Error::InvalidArgs(format!("op {i}: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((env, ops))
}

/// The config validator the guardrails run on the config a change would leave:
/// `[[override]]` globs and every built-in plugin's config (`[plugins.threshold.config]`
/// keys and ranges, `psi`, `rules`, ...), so a key the plugin would refuse at `init` is
/// rejected before it is written.
pub fn validate_config(cfg: &Config) -> Result<(), String> {
    lindesk_builtins::validate::config(cfg)
}

fn to_json<T: Serialize>(v: &T) -> Result<String, CtlError> {
    serde_json::to_string(v).map_err(|e| CtlError::Failed(e.to_string()))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, CtlError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| CtlError::Failed(format!("internal error: {e}")))
}

fn touches_config(cs: &ChangeSet) -> bool {
    cs.ops.iter().any(|o| o.op.is_config())
}

/// `ListProfiles`: the engine's profile rows.
pub async fn profiles_json<X: Send + 'static>(engine: &EngineHandle<X>) -> Result<String, CtlError> {
    Ok(engine.profiles().await?.list.to_string())
}

/// `GetHardware`: `Hardware` JSON including the latency map.
pub async fn hardware_json<X: Send + 'static>(engine: &EngineHandle<X>) -> Result<String, CtlError> {
    to_json(&engine.hardware().await?)
}

/// `SwapReport`, computed on the blocking pool (it reads `/proc` and `/sys`).
pub async fn swap_json<X: Send + 'static>(engine: &EngineHandle<X>) -> Result<String, CtlError> {
    let hw = engine.hardware().await?;
    let report = blocking(move || lindesk_sys::swap::analyze(&lindesk_sys::Roots::host(), &hw)).await?;
    to_json(&report)
}

/// One daemon's control plane. See the [module docs](self).
pub struct Control<X> {
    store: Arc<ChangeStore>,
    engine: EngineHandle<X>,
    scope: Scope,
    roots: lindesk_changes::Roots,
    config_paths: ConfigPaths,
    /// Re-arms the dead-man timer.
    expiry: Notify,
}

impl<X: Send + 'static> Control<X> {
    /// Open (creating) the store in `dir` for `scope`; config ops edit `config_path`. Every
    /// changeset notice goes into the engine's events and, as changeset JSON, to `events`
    /// (the `ChangeEvent` signal).
    pub fn open(
        dir: &Path,
        scope: Scope,
        config_path: PathBuf,
        engine: EngineHandle<X>,
        events: UnboundedSender<String>,
    ) -> Result<Self, ChangeError> {
        let roots = lindesk_changes::Roots::host();
        let config_paths = match scope {
            Scope::User => ConfigPaths { user: Some(config_path), system: None },
            Scope::System => ConfigPaths { user: None, system: Some(config_path) },
        };
        let applier = RealApplier::new(roots.clone(), config_paths.clone());
        let h = engine.clone();
        let store = ChangeStore::open(dir, scope)?.with_applier(Arc::new(applier)).on_event(move |e: &Event| {
            if let Event::Notice { data, .. } = e {
                let _ = events.send(data.to_string());
            }
            let _ = h.push_event(e.clone());
        });
        Ok(Control { store: Arc::new(store), engine, scope, roots, config_paths, expiry: Notify::new() })
    }

    pub fn scope(&self) -> Scope {
        self.scope
    }

    pub fn store(&self) -> &Arc<ChangeStore> {
        &self.store
    }

    /// Roll back changesets a crash left half-applied (call before serving D-Bus). Returns
    /// true when one of them touched the config file (the caller then reloads).
    pub fn recover(&self) -> bool {
        match self.store.recover() {
            Ok(v) => {
                for cs in &v {
                    tracing::warn!(
                        "changeset {} was interrupted and has been rolled back ({})",
                        cs.id,
                        cs.error.as_deref().unwrap_or("")
                    );
                }
                v.iter().any(touches_config)
            }
            Err(e) => {
                tracing::error!("recovering changes in {}: {e}", self.store.dir().display());
                false
            }
        }
    }

    async fn guard(&self, origin: Origin) -> Result<GuardContext, CtlError> {
        let hardware = self.engine.hardware().await?;
        let profiles = self.engine.profiles().await?;
        Ok(GuardContext::new(self.scope, origin, self.roots.clone(), self.config_paths.clone(), hardware)
            .with_profiles(profiles.names)
            .with_config_validator(validate_config))
    }

    /// After a changeset changed the config file: reload config and profiles.
    async fn after_change(&self, changed: &[ChangeSet]) {
        if !changed.iter().any(touches_config) {
            return;
        }
        match self.engine.reload().await {
            Ok(Ok(s)) => tracing::info!("config changed by changeset(s): {s}"),
            Ok(Err(e)) => tracing::warn!("config changed but the reload failed: {e}"),
            Err(e) => tracing::warn!("config changed: {e}"),
        }
    }

    /// `PlanChange`: Plan JSON (risk, diff, rejections). No side effects.
    pub async fn plan(&self, ops_json: &str) -> Result<String, CtlError> {
        let (env, ops) = parse_ops(ops_json)?;
        let ctx = self.guard(origin_of(&env.origin)).await?;
        let store = self.store.clone();
        let plan = blocking(move || store.plan(&ops, &ctx)).await?;
        to_json(&plan)
    }

    /// `ApplyChange`: validate, apply and version; ChangeSet JSON. A guardrail refusal is
    /// [`CtlError::Rejected`]; a write that failed (and was rolled back) is returned as the
    /// changeset with status `failed`.
    pub async fn apply(
        &self,
        ops_json: &str,
        description: &str,
        ttl_secs: u32,
        who: &Caller,
    ) -> Result<String, CtlError> {
        let (env, ops) = parse_ops(ops_json)?;
        let origin = origin_of(&env.origin);
        let mut author = Author::new(who.uid, origin).with_bus_name(who.bus_name.clone());
        if let Some(c) = env.client.as_deref().filter(|c| !c.is_empty()) {
            author = author.with_client(c);
        }
        let ttl = (ttl_secs > 0).then(|| Duration::from_secs(u64::from(ttl_secs)));
        self.apply_ops(ops, origin, author, description.to_string(), ttl).await
    }

    async fn apply_ops(
        &self,
        ops: Vec<ChangeOp>,
        origin: Origin,
        author: Author,
        description: String,
        ttl: Option<Duration>,
    ) -> Result<String, CtlError> {
        let ctx = self.guard(origin).await?;
        let store = self.store.clone();
        let r = blocking(move || {
            let plan = store.plan(&ops, &ctx);
            store.apply(&plan, author, &description, ttl)
        })
        .await?;
        self.expiry.notify_one();
        match r {
            Ok(cs) => {
                tracing::info!(
                    "changeset {} applied by uid {} ({}): {}",
                    cs.id,
                    cs.author.uid,
                    cs.origin,
                    cs.description
                );
                self.after_change(std::slice::from_ref(&cs)).await;
                to_json(&cs)
            }
            Err(ChangeError::ApplyFailed(cs)) => {
                tracing::warn!(
                    "changeset {} failed and was rolled back: {}",
                    cs.id,
                    cs.error.as_deref().unwrap_or("?")
                );
                to_json(&*cs)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// `ConfirmChange`.
    pub async fn confirm(&self, id: u64) -> Result<(), CtlError> {
        let store = self.store.clone();
        blocking(move || store.confirm(id)).await??;
        self.expiry.notify_one();
        Ok(())
    }

    /// `RevertChange`: the new (reverting) ChangeSet JSON.
    pub async fn revert(&self, id: u64, who: &Caller) -> Result<String, CtlError> {
        let store = self.store.clone();
        let author = Author::new(who.uid, Origin::Dbus).with_bus_name(who.bus_name.clone());
        let cs = blocking(move || store.revert(id, author, false)).await??;
        tracing::info!("changeset {id} reverted by uid {} (changeset {})", who.uid, cs.id);
        self.expiry.notify_one();
        self.after_change(std::slice::from_ref(&cs)).await;
        to_json(&cs)
    }

    /// `ListChanges`: the newest `limit` changesets (newest first).
    pub async fn list(&self, limit: u32) -> Result<String, CtlError> {
        let store = self.store.clone();
        let v = blocking(move || store.history(limit.max(1) as usize)).await??;
        to_json(&v)
    }

    /// `GetChange`.
    pub async fn get(&self, id: u64) -> Result<String, CtlError> {
        let store = self.store.clone();
        let cs = blocking(move || store.get(id)).await??;
        to_json(&cs)
    }

    /// `SetProfile`: a `profile_use` changeset (no ttl).
    pub async fn set_profile(&self, name: &str, who: &Caller) -> Result<String, CtlError> {
        let ops = vec![ChangeOp::ProfileUse { scope: self.scope, name: name.to_string() }];
        let author = Author::new(who.uid, Origin::Dbus).with_bus_name(who.bus_name.clone());
        self.apply_ops(ops, Origin::Dbus, author, format!("switch profile to {name}"), None).await
    }

    /// The dead-man switch: sleep until the earliest `ttl_until` (re-armed after every
    /// apply/confirm/revert), then revert what expired. Runs until the runtime stops.
    pub async fn run_expiry(self: Arc<Self>) {
        loop {
            let store = self.store.clone();
            let next = match blocking(move || store.next_expiry()).await {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => {
                    tracing::warn!("changes: {e}");
                    None
                }
                Err(_) => None,
            };
            let wait = next
                .map(|t| Duration::from_millis(t.saturating_sub(now_ms()).max(0) as u64))
                .unwrap_or(EXPIRY_MAX_WAIT)
                .min(EXPIRY_MAX_WAIT);
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.expiry.notified() => {}
            }
            let store = self.store.clone();
            match blocking(move || store.expire_due(now_ms())).await {
                Ok(Ok(expired)) if !expired.is_empty() => {
                    for cs in &expired {
                        tracing::info!(
                            "changeset {} was not confirmed in time: reverted by changeset {}",
                            cs.id,
                            cs.reverted_by.map_or("?".to_string(), |r| r.to_string())
                        );
                    }
                    self.after_change(&expired).await;
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!("changes: expiring: {e}"),
                Err(e) => tracing::warn!("changes: expiring: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use zbus::DBusError;

    use super::*;

    #[test]
    fn error_names() {
        assert_eq!(CtlError::Rejected("x".into()).name().as_str(), ERROR_REJECTED);
        assert_eq!(CtlError::from(ChangeError::NotFound(3)).name().as_str(), ERROR_NOT_FOUND);
        let denied = CtlError::from(fdo::Error::AccessDenied("no".into()));
        assert_eq!(denied.name().as_str(), "org.freedesktop.DBus.Error.AccessDenied");
        assert_eq!(denied.description(), Some("no"));
        let r = CtlError::from(ChangeError::Rejected(vec![lindesk_changes::Rejection::new(
            Some(0),
            lindesk_changes::Rule::ConfigKey,
            "unknown field",
        )]));
        assert_eq!(r.name().as_str(), ERROR_REJECTED);
        assert!(r.description().unwrap().contains("op 0: unknown field"), "{r}");
    }

    #[test]
    fn envelopes_and_origins() {
        let (env, ops) = parse_ops(
            r#"{"origin":"mcp","client":"claude-code","ops":[{"op":"config_set","scope":"user","key":"plugins.threshold.config.epoch","value":"400ms"}]}"#,
        )
        .unwrap();
        assert_eq!((origin_of(&env.origin), env.client.as_deref(), ops.len()), (Origin::Mcp, Some("claude-code"), 1));
        let (env, ops) = parse_ops(r#"[{"op":"profile_use","scope":"user","name":"battery"}]"#).unwrap();
        assert_eq!((origin_of(&env.origin), ops.len()), (Origin::Dbus, 1));
        assert_eq!(origin_of("lindeskctl"), Origin::Cli);
        let e = parse_ops(r#"[{"op":"nope"}]"#).unwrap_err();
        assert_eq!(e.name().as_str(), "org.freedesktop.DBus.Error.InvalidArgs");
        assert!(parse_ops("{").is_err());
    }

    #[test]
    fn validator_rejects_bad_plugin_keys() {
        assert!(validate_config(&Config::default()).is_ok());
        let bad = Config::from_toml("[plugins.threshold.config]\nnot_a_key = 1\n").unwrap();
        assert!(validate_config(&bad).unwrap_err().contains("not_a_key"));
    }
}
