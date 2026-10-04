//! What the MCP server talks to: the daemons over D-Bus ([`Daemon`]) and the local,
//! unprivileged view of the machine ([`Local`]). Both are traits so tests can fake them.
//!
//! Read-only tools ask the daemon first and fall back to [`Local`]; mutating tools only
//! ever go through the daemon (lindesk-mcp holds no privileges and never writes system
//! state itself).

use std::fmt;
use std::sync::Arc;

use serde_json::Value;

use crate::ops::Scope;

/// One D-Bus call (docs/ARCHITECTURE.md and docs/CONTROL-PLANE.md §5).
#[derive(Clone, Debug, PartialEq)]
pub enum DaemonRequest {
    Status,
    Explain(String),
    RecentEvents(u32),
    PlanChange {
        ops_json: String,
    },
    ApplyChange {
        ops_json: String,
        description: String,
        ttl_secs: u32,
    },
    ConfirmChange(u64),
    RevertChange(u64),
    ListChanges(u32),
    GetChange(u64),
    ListProfiles,
    SetProfile(String),
    GetHardware,
    SwapReport,
    /// Broker only.
    Calibrate {
        quick: bool,
    },
}

impl DaemonRequest {
    /// The D-Bus member name, for messages.
    pub fn member(&self) -> &'static str {
        match self {
            DaemonRequest::Status => "Status",
            DaemonRequest::Explain(_) => "Explain",
            DaemonRequest::RecentEvents(_) => "RecentEvents",
            DaemonRequest::PlanChange { .. } => "PlanChange",
            DaemonRequest::ApplyChange { .. } => "ApplyChange",
            DaemonRequest::ConfirmChange(_) => "ConfirmChange",
            DaemonRequest::RevertChange(_) => "RevertChange",
            DaemonRequest::ListChanges(_) => "ListChanges",
            DaemonRequest::GetChange(_) => "GetChange",
            DaemonRequest::ListProfiles => "ListProfiles",
            DaemonRequest::SetProfile(_) => "SetProfile",
            DaemonRequest::GetHardware => "GetHardware",
            DaemonRequest::SwapReport => "SwapReport",
            DaemonRequest::Calibrate { .. } => "Calibrate",
        }
    }
}

/// The daemon that serves a scope.
pub fn daemon_name(scope: Scope) -> &'static str {
    match scope {
        Scope::User => "lindesk-session",
        Scope::System => "lindeskd",
    }
}

/// How to start the daemon of a scope.
pub fn start_hint(scope: Scope) -> &'static str {
    match scope {
        Scope::User => "systemctl --user enable --now lindesk-session",
        Scope::System => "sudo systemctl enable --now lindeskd",
    }
}

/// Why a daemon call failed, classified so tools can react (fall back, explain, or report a
/// guardrail rejection).
#[derive(Clone, Debug, PartialEq)]
pub enum DaemonError {
    /// The daemon isn't running or its bus is unreachable.
    NotRunning { scope: Scope, detail: String },
    /// The daemon doesn't implement this member (older lindesk).
    Unsupported { scope: Scope, member: String },
    /// polkit or the daemon refused the caller.
    Denied { scope: Scope, message: String },
    /// A guardrail rejected the request (invalid op, out-of-range value, rate limit, ...).
    Rejected { scope: Scope, message: String },
    /// No answer in time.
    Timeout { scope: Scope, member: String },
    /// Anything else the daemon reported.
    Failed { scope: Scope, message: String },
}

impl DaemonError {
    pub fn scope(&self) -> Scope {
        match self {
            DaemonError::NotRunning { scope, .. }
            | DaemonError::Unsupported { scope, .. }
            | DaemonError::Denied { scope, .. }
            | DaemonError::Rejected { scope, .. }
            | DaemonError::Timeout { scope, .. }
            | DaemonError::Failed { scope, .. } => *scope,
        }
    }

    /// Short machine-readable kind.
    pub fn kind(&self) -> &'static str {
        match self {
            DaemonError::NotRunning { .. } => "daemon_not_running",
            DaemonError::Unsupported { .. } => "daemon_unsupported",
            DaemonError::Denied { .. } => "denied",
            DaemonError::Rejected { .. } => "rejected",
            DaemonError::Timeout { .. } => "timeout",
            DaemonError::Failed { .. } => "failed",
        }
    }

    /// What the user (or agent) can do about it.
    pub fn remedies(&self) -> Vec<String> {
        let scope = self.scope();
        let d = daemon_name(scope);
        match self {
            DaemonError::NotRunning { .. } => vec![
                format!("Start {d}: {}", start_hint(scope)),
                "Diagnose the machine: call the `doctor` tool (or run `lindeskctl doctor`).".into(),
                "Read-only tools still work without a daemon (they probe the machine locally).".into(),
            ],
            DaemonError::Unsupported { .. } => vec![format!(
                "{d} is older than lindesk-mcp {} and lacks this call; upgrade lindesk and restart {d}.",
                env!("CARGO_PKG_VERSION")
            )],
            DaemonError::Denied { .. } if scope == Scope::System => vec![
                "System-scope changes need polkit authorization (org.lindesk.manage); approve the authentication dialog or run from an active local desktop session.".into(),
                "Or make the change in scope=user if it only concerns your own session.".into(),
            ],
            DaemonError::Denied { .. } => vec!["The session daemon refused the request; check `journalctl --user -u lindesk-session`.".into()],
            DaemonError::Rejected { .. } => vec![
                "Fix the ops (see the message) and call plan_change again; plan_change shows every rejection without side effects.".into(),
            ],
            DaemonError::Timeout { .. } => vec![format!(
                "{d} did not answer in time; check `journalctl {}-u {d}`.",
                if scope == Scope::User { "--user " } else { "" }
            )],
            DaemonError::Failed { .. } => vec![format!(
                "Check the daemon log: journalctl {}-u {d}",
                if scope == Scope::User { "--user " } else { "" }
            )],
        }
    }

    /// Whether a read-only tool should fall back to local probing.
    pub fn allows_fallback(&self) -> bool {
        !matches!(self, DaemonError::Rejected { .. })
    }
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DaemonError::NotRunning { scope, detail } => {
                write!(f, "{} ({} scope) is not running: {detail}", daemon_name(*scope), scope.as_str())
            }
            DaemonError::Unsupported { scope, member } => {
                write!(f, "{} does not implement {member} (version mismatch?)", daemon_name(*scope))
            }
            DaemonError::Denied { scope, message } => {
                write!(f, "{} refused: not authorized ({message})", daemon_name(*scope))
            }
            DaemonError::Rejected { message, .. } => {
                if message.starts_with("rejected") {
                    write!(f, "{message}")
                } else {
                    write!(f, "rejected: {message}")
                }
            }
            DaemonError::Timeout { scope, member } => {
                write!(f, "{} did not answer {member} in time", daemon_name(*scope))
            }
            DaemonError::Failed { scope, message } => write!(f, "{} failed: {message}", daemon_name(*scope)),
        }
    }
}

impl std::error::Error for DaemonError {}

/// The daemons, over D-Bus. Every call returns the daemon's JSON reply as a string
/// (`ConfirmChange` returns an empty string).
#[async_trait::async_trait]
pub trait Daemon: Send + Sync + 'static {
    async fn call(&self, scope: Scope, req: DaemonRequest) -> Result<String, DaemonError>;
}

/// The unprivileged local view of the machine. Methods block (filesystem, DuckDB); the
/// server runs them on the blocking pool.
pub trait Local: Send + Sync + 'static {
    /// A status object shaped like the daemons' `Status` (pressure, hardware,
    /// requirements, text) with `"daemon": null`.
    fn status(&self) -> anyhow::Result<Value>;
    /// `lindeskctl doctor --json`: support level, reasons, findings.
    fn doctor(&self) -> anyhow::Result<Value>;
    /// `Hardware` JSON, with the saved latency map attached when one exists.
    fn hardware(&self) -> anyhow::Result<Value>;
    /// `SwapReport` JSON (with a short rate sample).
    fn swap(&self) -> anyhow::Result<Value>;
    /// Start-up requirement checks for lindesk-session (JSON array of items).
    fn requirements(&self) -> anyhow::Result<Value>;
    /// Read-only SQL over the telemetry lake of `scope`; returns the rows as a JSON array.
    /// The statement has already passed [`crate::sql::guard`].
    fn query(&self, scope: Scope, sql: &str) -> Result<Value, String>;
    /// Installed plugins (discovery only; health needs the daemon).
    fn plugins(&self) -> anyhow::Result<Value>;
    /// Known profiles with whether each matches this machine now.
    fn profiles(&self) -> anyhow::Result<Value>;
    /// `(path, toml)` of the effective user config.
    fn config(&self) -> anyhow::Result<(String, String)>;
    /// Changesets recorded on disk (newest first), when readable.
    fn changes(&self, scope: Scope, limit: u32) -> anyhow::Result<Value>;
    /// One changeset from disk.
    fn change(&self, scope: Scope, id: u64) -> anyhow::Result<Option<Value>>;
    /// Best-effort explanation of a pid or unit without a daemon.
    fn explain(&self, what: &str) -> anyhow::Result<Value>;
}

/// Everything a server instance needs.
#[derive(Clone)]
pub struct Backends {
    pub daemon: Arc<dyn Daemon>,
    pub local: Arc<dyn Local>,
}

/// A [`Daemon`] that is never there (`--no-daemon`, tests).
pub struct NoDaemon;

#[async_trait::async_trait]
impl Daemon for NoDaemon {
    async fn call(&self, scope: Scope, _req: DaemonRequest) -> Result<String, DaemonError> {
        Err(DaemonError::NotRunning { scope, detail: "daemon access disabled (--no-daemon)".into() })
    }
}
