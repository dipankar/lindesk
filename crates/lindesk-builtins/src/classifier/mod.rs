//! Built-in classifier plugins.
//!
//! * [`rules`]: TOML rule files with glob matchers, read from `rules.d` directories with
//!   systemd-like override semantics.

pub mod rules;

pub use rules::RulesClassifier;
