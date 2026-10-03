//! Built-in policy plugins.
//!
//! * [`threshold`]: the default policy. GREEN/YELLOW/RED levels with hysteresis driven by
//!   PSI, interaction epochs for the foreground, and `io.max` AIMD admission control in RED.

pub mod threshold;

pub use threshold::{ThresholdConfig, ThresholdPolicy};
