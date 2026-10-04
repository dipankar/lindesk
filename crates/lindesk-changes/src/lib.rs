//! Versioned, guardrailed, reversible changes (docs/CONTROL-PLANE.md §4).
//!
//! Every mutation that outlives a policy tick (config keys, the active profile, class
//! overrides, VM sysctls, I/O schedulers, zswap, cpufreq EPP/governor) goes through a
//! [`ChangeSet`]:
//!
//! 1. [`ChangeStore::plan`] runs the [`guard`] rules and shows a diff. No side effects.
//! 2. [`ChangeStore::apply`] records the previous values (write-ahead), applies the ops
//!    through an [`Applier`], and rolls back on any failure.
//! 3. [`ChangeStore::confirm`] keeps it; otherwise a ttl'd changeset is auto-reverted by
//!    [`ChangeStore::expire_due`] (the dead-man switch for changes made by agents).
//! 4. [`ChangeStore::revert`] / [`ChangeStore::revert_to`] append new changesets that
//!    restore earlier values; history is append-only.
//!
//! ```no_run
//! use lindesk_changes::*;
//! # fn main() -> Result<()> {
//! let store = ChangeStore::open("/var/lib/lindesk/changes", Scope::System)?;
//! store.recover()?; // at startup
//! let ctx = GuardContext::new(Scope::System, Origin::Mcp, Roots::host(),
//!     ConfigPaths::host(Scope::System), Default::default());
//! let plan = store.plan(&[ChangeOp::Sysctl { key: "vm.swappiness".into(), value: "10".into(), persist: false }], &ctx);
//! let cs = store.apply(&plan, Author::new(1000, Origin::Mcp), "lower swappiness", Some(DEFAULT_MCP_TTL))?;
//! store.confirm(cs.id)?;
//! # Ok(()) }
//! ```

pub mod apply;
pub mod diff;
pub mod guard;
pub mod model;
pub mod roots;
pub mod store;
pub mod toml_doc;

use std::path::PathBuf;

pub use apply::{Applier, RealApplier};
pub use guard::{ConfigValidator, DEFAULT_MCP_TTL, GuardContext, Limits, SafetyChange, Tighter, compare_safety};
pub use model::{
    Author, ChangeOp, ChangeSet, OpRecord, Origin, Plan, PlannedOp, Rejection, Risk, Rule, Scope, Status, keys_overlap,
};
pub use roots::{ConfigPaths, Roots};
pub use store::ChangeStore;

#[derive(Debug, thiserror::Error)]
pub enum ChangeError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("rejected: {}", .0.iter().map(|r| r.to_string()).collect::<Vec<_>>().join("; "))]
    Rejected(Vec<Rejection>),
    #[error("the plan was not produced by ChangeStore::plan (re-plan the ops)")]
    NotPlanned,
    #[error("changeset {0} not found")]
    NotFound(u64),
    #[error("changeset {id} is {status}: {message}")]
    InvalidState { id: u64, status: Status, message: String },
    #[error("changeset {id} was already reverted{}", .by.map(|b| format!(" by {b}")).unwrap_or_default())]
    AlreadyReverted { id: u64, by: Option<u64> },
    #[error(
        "changeset {id} conflicts with later changeset(s) {} on {}: revert those first, use revert_to({}) to undo everything since, or force",
        .later.iter().map(|l| l.to_string()).collect::<Vec<_>>().join(", "),
        .keys.join(", "),
        .id.saturating_sub(1)
    )]
    Conflict { id: u64, later: Vec<u64>, keys: Vec<String> },
    #[error("changeset {} failed and was rolled back: {}", .0.id, .0.error.as_deref().unwrap_or("?"))]
    ApplyFailed(Box<ChangeSet>),
    #[error("{0}")]
    Apply(String),
    #[error("corrupt record {path}: {message}")]
    Corrupt { path: PathBuf, message: String },
}

impl ChangeError {
    /// The changeset involved, when there is one (failed applies).
    pub fn changeset(&self) -> Option<&ChangeSet> {
        match self {
            ChangeError::ApplyFailed(cs) => Some(cs),
            _ => None,
        }
    }
}

pub type Result<T, E = ChangeError> = std::result::Result<T, E>;
