//! Built-in lindesk plugins. They implement the same traits as third-party plugins
//! (see `lindesk-plugin-api`), so everything here is also a reference implementation.
//!
//! There is no built-in sink: the telemetry lake writer is the external `telemetry` plugin
//! (`lindesk-telemetry-sink`, in the optional lindesk-telemetry package), so the daemons
//! link neither Vortex nor Arrow.

pub mod actuators;
pub mod adapters;
pub mod classifier;
pub mod env;
pub mod policy;
pub mod sensors;
pub mod validate;
