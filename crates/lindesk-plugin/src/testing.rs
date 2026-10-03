//! Unit-test a plugin's *wire* behaviour in-process.
//!
//! [`Harness`] pushes every call through the real protocol path (serialise the request to
//! a JSON line → [`Server`] dispatch → serialise the reply → parse it back), so a test
//! catches serde mistakes, wrong error codes and missing kind registrations, not just
//! logic bugs. Notifications sent through the [`crate::Notifier`] handed out by
//! [`Server::with_notifier`] are captured for inspection.
//!
//! ```
//! use lindesk_plugin::prelude::*;
//! use lindesk_plugin::testing::{Harness, proc_info};
//!
//! #[derive(Default)]
//! struct Rustc;
//! impl Plugin for Rustc {
//!     fn info(&self) -> PluginInfo {
//!         PluginInfo { name: "rustc".into(), version: "0.1.0".into(), kinds: vec![PluginKind::Classifier], description: String::new() }
//!     }
//! }
//! impl Classifier for Rustc {
//!     fn classify(&mut self, p: &ProcInfo) -> PluginResult<Option<Classification>> {
//!         Ok((p.comm == "rustc").then(|| Classification { class: QosClass::Batch, confidence: 1.0, reason: "compiler".into() }))
//!     }
//! }
//!
//! let mut h = Harness::new(Server::new(Rustc).classifier());
//! assert_eq!(h.initialize().unwrap().kinds, vec![PluginKind::Classifier]);
//! assert_eq!(h.classify(&proc_info("rustc")).unwrap().unwrap().class, QosClass::Batch);
//! assert!(h.evaluate(&lindesk_plugin::testing::snapshot(0)).is_err()); // not a policy
//! ```

use std::sync::{Arc, Mutex};

use lindesk_plugin_api::protocol::{
    ClassifyParams, EvaluateParams, InitializeResult, LogParams, Notification, Request, Response, SinkRecordParams,
    methods,
};
use lindesk_plugin_api::{
    API_VERSION, Classification, HostCaps, InitContext, Plugin, PluginError, PluginResult, PolicyOutput, Privilege,
    ProcInfo, Record, SensorInput, SensorOutput, Snapshot, TimestampMs,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::notifier::Notifier;
use crate::server::Server;

/// Drives a [`Server`] in-process through the JSON protocol.
pub struct Harness<P: Plugin> {
    server: Server<P>,
    captured: Arc<Mutex<Vec<String>>>,
    next_id: u64,
    exited: bool,
}

impl<P: Plugin> Harness<P> {
    /// Wrap a configured server; its notifier is redirected into the harness.
    pub fn new(mut server: Server<P>) -> Self {
        let (notifier, captured) = Notifier::capture();
        server.attach(notifier);
        Harness { server, captured, next_id: 1, exited: false }
    }

    /// `initialize` with [`test_context`].
    pub fn initialize(&mut self) -> PluginResult<InitializeResult> {
        self.initialize_with(test_context(serde_json::Value::Null))
    }

    /// `initialize` with a custom context (e.g. a `[config]` table).
    pub fn initialize_with(&mut self, ctx: InitContext) -> PluginResult<InitializeResult> {
        self.request(methods::INITIALIZE, ctx)
    }

    /// Send a raw line, as the host would. Returns the raw reply line, if any.
    pub fn send_line(&mut self, line: &str) -> Option<String> {
        if self.exited {
            return None;
        }
        let (reply, exit) = self.server.handle_line(line);
        self.exited |= exit;
        reply
    }

    /// Send a request and return the parsed [`Response`].
    pub fn request_raw(&mut self, method: &str, params: impl Serialize) -> Response {
        let id = self.next_id;
        self.next_id += 1;
        let line = serde_json::to_string(&Request::new(id, method, params)).expect("request serialises");
        let reply = self.send_line(&line).expect("requests always get a reply");
        let resp: Response = serde_json::from_str(&reply).unwrap_or_else(|e| panic!("bad reply {reply}: {e}"));
        assert_eq!(resp.id, id, "reply id must match the request id");
        resp
    }

    /// Send a request and decode the typed result (RPC errors become [`PluginError`]s).
    pub fn request<T: DeserializeOwned>(&mut self, method: &str, params: impl Serialize) -> PluginResult<T> {
        self.request_raw(method, params).into_result()
    }

    /// Send a notification (no reply expected).
    pub fn notify(&mut self, method: &str, params: impl Serialize) {
        let line = serde_json::to_string(&Notification::new(method, params)).expect("notification serialises");
        assert!(self.send_line(&line).is_none(), "notifications must not get replies");
    }

    /// `classifier.classify`.
    pub fn classify(&mut self, proc_info: &ProcInfo) -> PluginResult<Option<Classification>> {
        self.request(methods::CLASSIFY, ClassifyParams { proc: proc_info.clone() })
    }

    /// `policy.evaluate`.
    pub fn evaluate(&mut self, snapshot: &Snapshot) -> PluginResult<PolicyOutput> {
        self.request(methods::EVALUATE, EvaluateParams { snapshot: snapshot.clone() })
    }

    /// `sensor.poll`.
    pub fn poll(&mut self, input: &SensorInput) -> PluginResult<SensorOutput> {
        self.request(methods::SENSOR_POLL, input)
    }

    /// `sink.record` (a notification, as the host sends it).
    pub fn record(&mut self, records: &[Record]) {
        self.notify(methods::SINK_RECORD, SinkRecordParams { records: records.to_vec() });
    }

    /// `sink.flush`.
    pub fn flush(&mut self) -> PluginResult<()> {
        self.request(methods::SINK_FLUSH, serde_json::Value::Null)
    }

    /// `adapter.start`; emitted events show up in [`Harness::notifications`].
    pub fn start_adapter(&mut self) -> PluginResult<()> {
        self.request(methods::ADAPTER_START, serde_json::Value::Null)
    }

    /// Drain the notifications the plugin sent so far.
    pub fn notifications(&mut self) -> Vec<Notification> {
        let lines = std::mem::take(&mut *self.captured.lock().unwrap_or_else(|e| e.into_inner()));
        lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("plugin wrote invalid notification {l}: {e}")))
            .collect()
    }

    /// Drain notifications and keep only `log` lines.
    pub fn logs(&mut self) -> Vec<LogParams> {
        self.notifications()
            .into_iter()
            .filter(|n| n.method == methods::LOG)
            .filter_map(|n| serde_json::from_value(n.params).ok())
            .collect()
    }

    /// Send `shutdown`, returning the plugin for final assertions.
    pub fn shutdown(mut self) -> P {
        self.notify(methods::SHUTDOWN, serde_json::Value::Null);
        self.server.finish();
        self.server.into_plugin()
    }

    /// True once the plugin has processed `shutdown`.
    pub fn exited(&self) -> bool {
        self.exited
    }

    /// The plugin under test.
    pub fn plugin(&self) -> &P {
        self.server.plugin()
    }

    /// The plugin under test, mutably.
    pub fn plugin_mut(&mut self) -> &mut P {
        self.server.plugin_mut()
    }

    /// The notifier the harness installed (useful to emit from test code).
    pub fn notifier(&self) -> Notifier {
        self.server.notifier().clone()
    }
}

/// The `initialize` params the harness sends by default (`host = "test"`, user privilege).
pub fn test_context(config: serde_json::Value) -> InitContext {
    InitContext {
        api_version: API_VERSION,
        host: "test".into(),
        privilege: Privilege::User,
        uid: Some(1000),
        caps: HostCaps { cgroup_v2: true, systemd: true, psi: true, io_max: true, ..Default::default() },
        config,
    }
}

/// A process with the given `comm` in a user app scope.
pub fn proc_info(comm: &str) -> ProcInfo {
    ProcInfo {
        pid: 4242,
        uid: 1000,
        comm: comm.into(),
        exe: Some(format!("/usr/bin/{comm}")),
        cgroup: format!("/user.slice/user-1000.slice/user@1000.service/app.slice/app-{comm}-4242.scope"),
        unit: Some(format!("app-{comm}-4242.scope")),
        slice: Some("app.slice".into()),
        app_id: None,
    }
}

/// An empty snapshot at `now`.
pub fn snapshot(now: TimestampMs) -> Snapshot {
    Snapshot {
        now,
        pressure: Vec::new(),
        units: Vec::new(),
        events: Vec::new(),
        foreground: None,
        caps: test_context(serde_json::Value::Null).caps,
    }
}

impl<P: Plugin> std::fmt::Debug for Harness<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Harness").field("kinds", &self.server.kinds()).field("exited", &self.exited).finish()
    }
}

/// Assert that a result is an RPC "method not found"/unsupported error.
pub fn assert_unsupported<T: std::fmt::Debug>(r: PluginResult<T>) {
    match r {
        Err(PluginError::Unsupported(_)) => {}
        other => panic!("expected an unsupported/method-not-found error, got {other:?}"),
    }
}
