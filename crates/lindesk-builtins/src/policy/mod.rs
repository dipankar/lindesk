//! Built-in policy plugins.
//!
//! * [`threshold`]: the default policy. GREEN/YELLOW/RED levels with hysteresis driven by
//!   PSI, interaction epochs for the foreground, and `io.max` AIMD admission control in RED.
//! * [`relay`] (`broker`): the session agent's relay of the root broker's decisions.

pub mod relay;
pub mod threshold;

pub use relay::{RelayFeed, RelayPolicy, RelayedSetting};
pub use threshold::{ThresholdConfig, ThresholdPolicy};
