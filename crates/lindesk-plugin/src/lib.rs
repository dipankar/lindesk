//! # lindesk-plugin: write lindesk plugins in Rust
//!
//! An external plugin is an executable that speaks newline-delimited JSON-RPC 2.0 on
//! stdio. This crate is the whole runtime: implement the [`lindesk_plugin_api`] traits for
//! a struct (the same traits the built-in plugins use), then serve it.
//!
//! ```no_run
//! use lindesk_plugin::prelude::*;
//!
//! #[derive(Default)]
//! struct Compilers;
//!
//! impl Plugin for Compilers {
//!     fn info(&self) -> PluginInfo {
//!         PluginInfo {
//!             name: "compilers".into(),
//!             version: env!("CARGO_PKG_VERSION").into(),
//!             kinds: vec![PluginKind::Classifier],
//!             description: "compilers are batch work".into(),
//!         }
//!     }
//! }
//!
//! impl Classifier for Compilers {
//!     fn classify(&mut self, p: &ProcInfo) -> PluginResult<Option<Classification>> {
//!         let batch = matches!(p.comm.as_str(), "rustc" | "cc1plus" | "ld");
//!         Ok(batch.then(|| Classification { class: QosClass::Batch, confidence: 1.0, reason: "compiler".into() }))
//!     }
//! }
//!
//! lindesk_plugin::main!(Compilers, classifier);
//! ```
//!
//! A struct can serve several kinds: `main!(MyPlugin, classifier, policy)`, or with the
//! builder, `Server::new(MyPlugin::new()).classifier().policy().run_or_exit()`.
//!
//! * Notifications (`log`, `metric`, `event`, `samples`) go through a [`Notifier`]: call
//!   [`notifier()`] from anywhere, or receive one with [`Server::with_notifier`].
//! * Test the wire behaviour in-process with [`testing::Harness`].
//! * Check the finished binary with `lindeskctl plugin test <dir>`.

mod notifier;
mod server;
pub mod testing;

pub use lindesk_plugin_api as api;
pub use notifier::{Notifier, now_ms};
/// Re-exported so plugins can use `serde_json` (e.g. for `[config]`) without another dependency.
pub use serde_json;
pub use server::Server;

/// Everything a plugin usually needs: the API types and traits plus [`Server`]/[`Notifier`].
pub mod prelude {
    pub use crate::config_as;
    pub use crate::{Notifier, Server, notifier};
    pub use lindesk_plugin_api::protocol::LogLevel;
    pub use lindesk_plugin_api::*;
    pub use serde::{Deserialize, Serialize};
    pub use serde_json::{self, Value, json};
}

/// Deserialize the plugin's `[config]` (from [`InitContext::config`]) into `T`, treating a
/// missing/null config as `{}` so `#[serde(default)]` structs just work.
pub fn config_as<T: serde::de::DeserializeOwned>(
    ctx: &lindesk_plugin_api::InitContext,
) -> lindesk_plugin_api::PluginResult<T> {
    let v = if ctx.config.is_null() { serde_json::json!({}) } else { ctx.config.clone() };
    serde_json::from_value(v).map_err(|e| lindesk_plugin_api::PluginError::Failed(format!("invalid config: {e}")))
}

/// A [`Notifier`] writing to this process's stdout. Usable from any thread, at any time.
///
/// In unit tests prefer the notifier handed out by [`Server::with_notifier`], which the
/// [`testing::Harness`] captures.
pub fn notifier() -> Notifier {
    Notifier::stdout()
}

/// Serve `server` on stdio and exit the process when the host shuts it down.
pub fn run<P: lindesk_plugin_api::Plugin>(server: Server<P>) -> ! {
    server.run_or_exit()
}

/// Generate `fn main` serving a `Default`-constructible plugin type for the listed kinds
/// (`classifier`, `policy`, `sensor`, `sink`, `adapter`, `actuator`).
///
/// ```ignore
/// lindesk_plugin::main!(MyPlugin, classifier, policy);
/// ```
#[macro_export]
macro_rules! main {
    ($plugin:ty $(, $kind:ident)+ $(,)?) => {
        fn main() {
            $crate::run($crate::Server::new(<$plugin as ::core::default::Default>::default())$(.$kind())+)
        }
    };
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::prelude::*;
    use super::testing::{Harness, assert_unsupported, proc_info, snapshot};
    use lindesk_plugin_api::protocol::{Response, codes, methods};

    #[derive(Default)]
    struct Multi {
        notifier: Option<Notifier>,
        records: Vec<Record>,
        inits: u32,
        shut: bool,
        threshold: u64,
    }

    impl Plugin for Multi {
        fn info(&self) -> PluginInfo {
            PluginInfo {
                name: "multi".into(),
                version: "0.2.0".into(),
                kinds: vec![PluginKind::Classifier, PluginKind::Policy, PluginKind::Sink],
                description: String::new(),
            }
        }

        fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
            self.inits += 1;
            self.threshold = ctx.config.get("threshold").and_then(|v| v.as_u64()).unwrap_or(0);
            if ctx.config.get("refuse").is_some() {
                return Err(PluginError::Failed("refusing to start".into()));
            }
            Ok(())
        }

        fn shutdown(&mut self) {
            self.shut = true;
        }
    }

    impl Classifier for Multi {
        fn classify(&mut self, p: &ProcInfo) -> PluginResult<Option<Classification>> {
            match p.comm.as_str() {
                "rustc" => Ok(Some(Classification { class: QosClass::Batch, confidence: 0.8, reason: "cc".into() })),
                "boom" => Err(PluginError::Failed("boom".into())),
                "nope" => Err(PluginError::Unsupported("nope".into())),
                _ => Ok(None),
            }
        }
    }

    impl Policy for Multi {
        fn evaluate(&mut self, s: &Snapshot) -> PluginResult<PolicyOutput> {
            if let Some(n) = &self.notifier {
                n.metric("units", s.units.len() as f64, [("k", "v")]);
                n.info("evaluated");
            }
            Ok(PolicyOutput {
                intents: s
                    .units
                    .iter()
                    .map(|u| Intent {
                        target: u.target.clone(),
                        setting: Setting::CpuWeight { value: self.threshold },
                        priority: 0,
                        reason: "test".into(),
                    })
                    .collect(),
                level: Some(Level::Yellow),
                metrics: vec![],
            })
        }
    }

    impl Sink for Multi {
        fn tables(&self) -> Vec<String> {
            vec!["events".into()]
        }
        fn record(&mut self, records: &[Record]) -> PluginResult<()> {
            self.records.extend_from_slice(records);
            Ok(())
        }
    }

    fn harness() -> Harness<Multi> {
        Harness::new(
            Server::new(Multi::default()).classifier().policy().sink().with_notifier(|p, n| p.notifier = Some(n)),
        )
    }

    #[test]
    fn handshake_reports_kinds_and_tables() {
        let mut h = harness();
        let r = h.initialize_with(super::testing::test_context(serde_json::json!({"threshold": 42}))).unwrap();
        assert_eq!(r.name, "multi");
        assert_eq!(r.version, "0.2.0");
        assert_eq!(r.api_version, API_VERSION);
        assert_eq!(r.kinds, vec![PluginKind::Classifier, PluginKind::Policy, PluginKind::Sink]);
        assert_eq!(r.tables, vec!["events".to_string()]);
        assert!(r.actuator.is_none());
        assert_eq!(h.plugin().threshold, 42);
    }

    #[test]
    fn wrong_api_version_and_init_failure_are_errors() {
        let mut h = harness();
        let mut ctx = super::testing::test_context(serde_json::Value::Null);
        ctx.api_version = 99;
        assert!(matches!(h.initialize_with(ctx), Err(PluginError::Failed(m)) if m.contains("v99")));
        let ctx = super::testing::test_context(serde_json::json!({"refuse": true}));
        assert_eq!(h.initialize_with(ctx), Err(PluginError::Failed("refusing to start".into())));
    }

    #[test]
    fn routes_and_maps_errors() {
        let mut h = harness();
        h.initialize().unwrap();
        assert_eq!(h.classify(&proc_info("rustc")).unwrap().unwrap().class, QosClass::Batch);
        assert_eq!(h.classify(&proc_info("firefox")).unwrap(), None);
        assert_eq!(h.classify(&proc_info("boom")), Err(PluginError::Failed("boom".into())));
        let r = h.request_raw(methods::CLASSIFY, ClassifyArgs { proc: proc_info("nope") });
        assert_eq!(r.error.unwrap().code, codes::UNSUPPORTED);
        let r = h.request_raw(methods::CLASSIFY, serde_json::json!({"bogus": 1}));
        assert_eq!(r.error.unwrap().code, codes::INVALID_PARAMS);
        let r = h.request_raw("no.such.method", serde_json::Value::Null);
        assert_eq!(r.error.unwrap().code, codes::METHOD_NOT_FOUND);
        assert_unsupported(h.poll(&SensorInput {
            now: 0,
            targets: vec![],
            foreground: None,
            foreground_family: Vec::new(),
        }));
    }

    #[derive(serde::Serialize)]
    struct ClassifyArgs {
        proc: ProcInfo,
    }

    #[test]
    fn policy_and_notifications() {
        let mut h = harness();
        h.initialize_with(super::testing::test_context(serde_json::json!({"threshold": 30}))).unwrap();
        let mut s = snapshot(5);
        s.units.push(UnitState {
            target: Target::user_unit(1000, "a.scope"),
            class: QosClass::Batch,
            foreground: false,
            pressure: vec![],
            usage: None,
            applied: vec![],
            top_comm: None,
            visible: None,
            audible: None,
        });
        let out = h.evaluate(&s).unwrap();
        assert_eq!(out.level, Some(Level::Yellow));
        assert_eq!(out.intents[0].setting, Setting::CpuWeight { value: 30 });
        let notes = h.notifications();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].method, "metric");
        let m: Metric = serde_json::from_value(notes[0].params.clone()).unwrap();
        assert_eq!((m.name.as_str(), m.value, m.labels["k"].as_str()), ("units", 1.0, "v"));
        assert_eq!(notes[1].method, "log");
    }

    #[test]
    fn sink_record_is_a_notification_and_shutdown_runs_once() {
        let mut h = harness();
        h.initialize().unwrap();
        let rec = Record::Event(Event::PluginError { ts: 1, plugin: "x".into(), message: "y".into() });
        h.record(std::slice::from_ref(&rec));
        h.flush().unwrap();
        // Garbage and responses are tolerated.
        assert!(h.send_line("not json").is_none());
        assert!(h.send_line(&serde_json::to_string(&Response::ok(9, 1)).unwrap()).is_none());
        assert_eq!(h.logs().len(), 1, "garbage is reported as a log line");
        let p = h.shutdown();
        assert_eq!(p.records, vec![rec]);
        assert!(p.shut);
    }

    #[derive(Default)]
    struct Focuser {
        started: Arc<Mutex<bool>>,
    }

    impl Plugin for Focuser {
        fn info(&self) -> PluginInfo {
            PluginInfo {
                name: "focuser".into(),
                version: "0".into(),
                kinds: vec![PluginKind::Adapter],
                description: String::new(),
            }
        }
    }

    impl Adapter for Focuser {
        fn start(&mut self, sink: EventSink) -> PluginResult<()> {
            *self.started.lock().unwrap() = true;
            sink(Event::Focus { ts: 1, pid: Some(7), app_id: None, target: None, source: "t".into() });
            Ok(())
        }
    }

    #[test]
    fn adapter_events_become_notifications() {
        let mut h = Harness::new(Server::new(Focuser::default()).adapter());
        h.initialize().unwrap();
        h.start_adapter().unwrap();
        let n = h.notifications();
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].method, "event");
        let e: Event = serde_json::from_value(n[0].params.clone()).unwrap();
        assert!(matches!(e, Event::Focus { pid: Some(7), .. }));
        assert!(*h.plugin().started.lock().unwrap());
    }

    struct Act;
    impl Plugin for Act {
        fn info(&self) -> PluginInfo {
            PluginInfo {
                name: "act".into(),
                version: "0".into(),
                kinds: vec![PluginKind::Actuator],
                description: String::new(),
            }
        }
    }
    impl Actuator for Act {
        fn caps(&self) -> ActuatorCaps {
            ActuatorCaps { knobs: vec![Knob::CpuWeight], targets: vec!["unit".into()] }
        }
        fn read(&mut self, _: &Target, _: &Knob) -> PluginResult<Option<Setting>> {
            Ok(Some(Setting::CpuWeight { value: 100 }))
        }
        fn apply(&mut self, _: &Target, _: &Setting) -> PluginResult<()> {
            Ok(())
        }
        fn reset(&mut self, _: &Target, _: &Knob, _: Option<&Setting>) -> PluginResult<()> {
            Ok(())
        }
    }

    #[test]
    fn actuator_caps_advertised() {
        let mut h = Harness::new(Server::new(Act).actuator());
        let r = h.initialize().unwrap();
        assert_eq!(r.actuator.unwrap().knobs, vec![Knob::CpuWeight]);
        let v: Option<Setting> = h
            .request(
                methods::ACTUATOR_READ,
                serde_json::json!({"target": Target::system_unit("a.service"), "knob": Knob::CpuWeight}),
            )
            .unwrap();
        assert_eq!(v, Some(Setting::CpuWeight { value: 100 }));
    }
}
