//! # lindesk-host: the plugin host and pipeline engine
//!
//! Everything in lindesk is a plugin: sensors, desktop adapters, classifiers, policies,
//! actuators and sinks. Built-ins implement the [`lindesk_plugin_api`] traits in-process;
//! third-party plugins are executables speaking JSON-RPC on stdio, wrapped by
//! [`ExternalPlugin`] so they implement the *same* traits. The [`Engine`] can't tell the
//! two apart.
//!
//! | Module | What it does |
//! |---|---|
//! | [`discovery`] | find `<dir>/<name>/plugin.toml`, validate, enforce privilege, apply config |
//! | [`external`] | spawn and talk to plugin processes; route notifications |
//! | [`supervisor`] | health state machine, restart backoff, auto-disable, call stats |
//! | [`registry`] | `Box<dyn Trait>` per kind with metadata (priority, prefilter, interval) |
//! | [`engine`] | the deterministic pipeline: `step(now) -> StepReport`, `status`, `explain` |
//! | [`env`](mod@env) | the [`Environment`] trait and an in-memory implementation |
//! | [`conformance`] | protocol conformance checks behind `lindeskctl plugin test` |
//! | [`scaffold`](mod@scaffold) | `lindeskctl plugin new`: working plugin templates (Python, Rust, shell) |
//!
//! A daemon wires it up like this:
//!
//! ```no_run
//! use lindesk_core::{clock::{Clock, SystemClock}, config::Config};
//! use lindesk_host::{discovery, Engine, EngineOptions, MemoryEnvironment};
//! use lindesk_plugin_api::Privilege;
//!
//! let config = Config::load("/etc/lindesk/lindesk.toml".as_ref()).unwrap();
//! let mut opts = EngineOptions::new("lindeskd", Privilege::System);
//! opts.state_path = Some("/run/lindesk/state.json".into());
//! let env = MemoryEnvironment::new(); // a daemon passes its /proc + systemd environment
//! let mut engine = Engine::new(config.clone(), Box::new(env), opts).unwrap();
//! // engine.registry_mut().register_policy(Box::new(Threshold::default()), 0)?;  (built-ins)
//! let mut dirs = discovery::default_plugin_dirs(Privilege::System);
//! dirs.extend(config.general.plugin_dirs.clone());
//! let found = discovery::discover(&discovery::DiscoveryOptions {
//!     host: Privilege::System, dirs, config: &config, ownership: &discovery::RootOwned,
//! });
//! for p in &found.plugins { let _ = engine.registry_mut().load_external(p); }
//! engine.load_state("/run/lindesk/state.json".as_ref()).unwrap(); // crash recovery
//! engine.start();
//! loop {
//!     let report = engine.step(SystemClock.now());
//!     # break;
//! }
//! engine.shutdown(); // restores everything
//! ```

pub mod conformance;
pub mod discovery;
pub mod engine;
pub mod env;
pub mod external;
pub mod registry;
pub mod report;
pub mod scaffold;
pub mod supervisor;

pub use conformance::{ConformanceReport, run_conformance};
pub use discovery::{DiscoveredPlugin, Discovery, DiscoveryOptions, discover};
pub use engine::{Engine, EngineError, EngineOptions, StateError};
pub use env::{Environment, MemoryEnvironment};
pub use external::{ExternalPlugin, HostReceiver, HostSender, host_queues};
pub use registry::{Origin, PluginHealth, Registry, RegistryError};
pub use report::{
    ActionOutcome, ActionReport, ClassDecision, ClassSource, ExplainNote, ExplainedSetting, Explanation, Status,
    StepReport, StepTimings, TargetSummary, YieldingTarget,
};
pub use scaffold::{Lang, scaffold};
pub use supervisor::{Health, PluginStats, SupervisorConfig};
