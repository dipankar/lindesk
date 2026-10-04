//! Built-in lindesk plugins. They implement the same traits as third-party plugins
//! (see `lindesk-plugin-api`), so everything here is also a reference implementation.

pub mod actuators;
pub mod adapters;
pub mod classifier;
pub mod env;
pub mod policy;
pub mod sensors;
pub mod sinks;
pub mod validate;
