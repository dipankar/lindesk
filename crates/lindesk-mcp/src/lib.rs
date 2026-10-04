//! # lindesk-mcp
//!
//! A Model Context Protocol server (built on [rmcp]) that lets a local AI agent assess a
//! Linux desktop's responsiveness and tune lindesk through guardrailed, versioned,
//! revertible changesets (docs/CONTROL-PLANE.md §6).
//!
//! * [`server::LindeskMcp`]: tools, resources (`lindesk://…`) and prompts.
//! * [`backend`]: the [`backend::Daemon`] (D-Bus, [`daemon::DbusDaemon`]) and
//!   [`backend::Local`] ([`local::HostLocal`]) traits. Read-only tools prefer the daemon and
//!   fall back to local probing; mutations only ever go to the daemon. This process holds no
//!   privileges.
//! * [`http`]: streamable HTTP over a 0600 unix socket or loopback TCP with a bearer token.

pub mod backend;
pub mod clientcfg;
pub mod daemon;
pub mod docs;
pub mod http;
pub mod local;
pub mod ops;
pub mod recommend;
pub mod server;
pub mod sql;

#[cfg(test)]
mod tests;

pub use backend::{Backends, Daemon, DaemonError, DaemonRequest, Local, NoDaemon};
pub use server::LindeskMcp;

/// Backends for the real machine. `daemon = false` disables D-Bus (read-only tools then
/// always use local probing, mutating tools always fail).
pub fn host_backends(daemon: bool) -> Backends {
    let d: std::sync::Arc<dyn Daemon> =
        if daemon { std::sync::Arc::new(daemon::DbusDaemon::new()) } else { std::sync::Arc::new(NoDaemon) };
    Backends { daemon: d, local: std::sync::Arc::new(local::HostLocal::new()) }
}
