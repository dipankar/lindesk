//! Pure, I/O-free building blocks shared by every lindesk binary.

pub mod cgroup_path;
pub mod clock;
pub mod config;
pub mod conflicts;
pub mod profile;
pub mod reconcile;
pub mod resolve;
pub mod safety;

pub use lindesk_plugin_api as api;
