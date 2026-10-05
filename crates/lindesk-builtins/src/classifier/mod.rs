//! Built-in classifier plugins.
//!
//! * [`rules`]: TOML rule files with glob matchers, read from `rules.d` directories with
//!   systemd-like override semantics.

//! * [`workload`]: marks units that are running a build (compilers, linkers) as background.

pub mod rules;
pub mod workload;

pub use rules::RulesClassifier;
pub use workload::WorkloadClassifier;
