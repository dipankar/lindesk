//! The stdio JSON-RPC loop.

use std::io::{self, BufRead};
use std::sync::Arc;

use lindesk_plugin_api::protocol::{
    ActuatorApplyParams, ActuatorReadParams, ActuatorResetParams, ClassifyParams, EvaluateParams, InitializeParams,
    InitializeResult, Message, Request, Response, SinkRecordParams, codes, methods,
};
use lindesk_plugin_api::{
    API_VERSION, Actuator, ActuatorCaps, Adapter, Classifier, Event, EventSink, Plugin, PluginError, PluginKind,
    PluginResult, Policy, Sensor, SensorInput, Sink,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::notifier::Notifier;

/// Routes one method for one kind. `None` = "not my method".
type Dispatch<P> = fn(&mut P, &str, Value, &Notifier) -> Option<PluginResult<Value>>;

/// Hands the plugin its notifier.
type NotifierHook<P> = Box<dyn FnMut(&mut P, Notifier)>;

/// Serves a plugin value over the stdio protocol. Register each kind the plugin implements:
///
/// ```no_run
/// # use lindesk_plugin::prelude::*;
/// # #[derive(Default)] struct MyPlugin;
/// # impl Plugin for MyPlugin { fn info(&self) -> PluginInfo { unimplemented!() } }
/// # impl Classifier for MyPlugin { fn classify(&mut self, _: &ProcInfo) -> PluginResult<Option<Classification>> { Ok(None) } }
/// # impl Policy for MyPlugin { fn evaluate(&mut self, _: &Snapshot) -> PluginResult<PolicyOutput> { Ok(Default::default()) } }
/// fn main() {
///     Server::new(MyPlugin::default()).classifier().policy().run_or_exit();
/// }
/// ```
///
/// The server answers `initialize` (checking the API version, then calling
/// [`Plugin::init`]), routes kind methods, maps [`PluginError`]s to RPC errors, replies
/// `METHOD_NOT_FOUND` to anything else, and calls [`Plugin::shutdown`] on `shutdown` or EOF.
pub struct Server<P> {
    plugin: P,
    kinds: Vec<PluginKind>,
    handlers: Vec<Dispatch<P>>,
    sink_tables: Option<fn(&P) -> Vec<String>>,
    actuator_caps: Option<fn(&P) -> ActuatorCaps>,
    on_notifier: Option<NotifierHook<P>>,
    notifier: Notifier,
    shut_down: bool,
}

impl<P: Plugin> Server<P> {
    /// Wrap a plugin. Then call one registration method per kind it implements.
    pub fn new(plugin: P) -> Self {
        Server {
            plugin,
            kinds: Vec::new(),
            handlers: Vec::new(),
            sink_tables: None,
            actuator_caps: None,
            on_notifier: None,
            notifier: Notifier::stdout(),
            shut_down: false,
        }
    }

    fn add(mut self, kind: PluginKind, d: Dispatch<P>) -> Self {
        if !self.kinds.contains(&kind) {
            self.kinds.push(kind);
            self.handlers.push(d);
        }
        self
    }

    /// Serve `classifier.classify`.
    pub fn classifier(self) -> Self
    where
        P: Classifier,
    {
        self.add(PluginKind::Classifier, dispatch_classifier::<P>)
    }

    /// Serve `policy.evaluate`.
    pub fn policy(self) -> Self
    where
        P: Policy,
    {
        self.add(PluginKind::Policy, dispatch_policy::<P>)
    }

    /// Serve `sensor.poll`.
    pub fn sensor(self) -> Self
    where
        P: Sensor,
    {
        self.add(PluginKind::Sensor, dispatch_sensor::<P>)
    }

    /// Serve `sink.record` (a notification) and `sink.flush`; advertises [`Sink::tables`].
    pub fn sink(mut self) -> Self
    where
        P: Sink,
    {
        self.sink_tables = Some(|p: &P| p.tables());
        self.add(PluginKind::Sink, dispatch_sink::<P>)
    }

    /// Serve `adapter.start`; events passed to the sink become `event` notifications.
    pub fn adapter(self) -> Self
    where
        P: Adapter,
    {
        self.add(PluginKind::Adapter, dispatch_adapter::<P>)
    }

    /// Serve `actuator.caps|read|apply|reset`; advertises [`Actuator::caps`].
    pub fn actuator(mut self) -> Self
    where
        P: Actuator,
    {
        self.actuator_caps = Some(|p: &P| p.caps());
        self.add(PluginKind::Actuator, dispatch_actuator::<P>)
    }

    /// Hand the plugin a [`Notifier`] before `initialize` (e.g. to store it in a field).
    /// Under [`crate::testing::Harness`] the notifier records into the harness instead.
    pub fn with_notifier(mut self, f: impl FnMut(&mut P, Notifier) + 'static) -> Self {
        self.on_notifier = Some(Box::new(f));
        self
    }

    /// The kinds registered so far.
    pub fn kinds(&self) -> &[PluginKind] {
        &self.kinds
    }

    /// The wrapped plugin.
    pub fn plugin(&self) -> &P {
        &self.plugin
    }

    /// The wrapped plugin, mutably.
    pub fn plugin_mut(&mut self) -> &mut P {
        &mut self.plugin
    }

    /// Unwrap the plugin.
    pub fn into_plugin(self) -> P {
        self.plugin
    }

    pub(crate) fn attach(&mut self, notifier: Notifier) {
        self.notifier = notifier.clone();
        if let Some(f) = self.on_notifier.as_mut() {
            f(&mut self.plugin, notifier);
        }
    }

    pub(crate) fn notifier(&self) -> &Notifier {
        &self.notifier
    }

    /// Run until `shutdown` or EOF on stdin. Returns I/O errors on stdin/stdout.
    pub fn run(mut self) -> io::Result<()> {
        self.attach(Notifier::stdout());
        let stdin = io::stdin().lock();
        for line in stdin.lines() {
            let line = line?;
            let (reply, exit) = self.handle_line(&line);
            if let Some(reply) = reply {
                self.notifier.write_line(&reply)?;
            }
            if exit {
                break;
            }
        }
        self.finish();
        Ok(())
    }

    /// [`Server::run`], then exit the process (status 1 on I/O errors).
    pub fn run_or_exit(self) -> ! {
        match self.run() {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("lindesk plugin: {e}");
                std::process::exit(1)
            }
        }
    }

    pub(crate) fn finish(&mut self) {
        if !self.shut_down {
            self.shut_down = true;
            self.plugin.shutdown();
        }
    }

    /// Handle one line from the host. Returns the reply line (for requests) and whether
    /// the loop should stop.
    pub(crate) fn handle_line(&mut self, line: &str) -> (Option<String>, bool) {
        let line = line.trim();
        if line.is_empty() {
            return (None, false);
        }
        let msg = match Message::parse(line) {
            Ok(m) => m,
            Err(e) => {
                self.notifier.error(format!("unparseable line from host: {e}"));
                return (None, false);
            }
        };
        match msg {
            Message::Request(req) => (Some(self.handle_request(req)), false),
            Message::Notification(n) => {
                if n.method == methods::SHUTDOWN {
                    self.finish();
                    return (None, true);
                }
                if let Some(Err(e)) = self.dispatch(&n.method, n.params) {
                    self.notifier.error(format!("{}: {e}", n.method));
                }
                (None, false)
            }
            Message::Response(_) => (None, false),
        }
    }

    fn handle_request(&mut self, req: Request) -> String {
        let resp = if req.method == methods::INITIALIZE {
            match self.initialize(req.params) {
                Ok(r) => Response::ok(req.id, r),
                Err(e) => Response::from_plugin_error(req.id, &e),
            }
        } else {
            match self.dispatch(&req.method, req.params) {
                Some(Ok(v)) => Response::ok(req.id, v),
                Some(Err(e)) => Response::from_plugin_error(req.id, &e),
                None => Response::err(req.id, codes::METHOD_NOT_FOUND, format!("method not found: {}", req.method)),
            }
        };
        serde_json::to_string(&resp).unwrap_or_else(|e| {
            serde_json::to_string(&Response::err(req.id, codes::INTERNAL, format!("serialising reply: {e}")))
                .unwrap_or_default()
        })
    }

    fn dispatch(&mut self, method: &str, params: Value) -> Option<PluginResult<Value>> {
        // Each dispatcher only consumes params for its own methods, so cloning is cheap
        // in the common single-kind case and correct in the multi-kind one.
        let (last, rest) = self.handlers.split_last()?;
        for h in rest {
            if let Some(r) = h(&mut self.plugin, method, params.clone(), &self.notifier) {
                return Some(r);
            }
        }
        last(&mut self.plugin, method, params, &self.notifier)
    }

    fn initialize(&mut self, params: Value) -> PluginResult<InitializeResult> {
        let ctx: InitializeParams = parse(params)?;
        if ctx.api_version != API_VERSION {
            return Err(PluginError::Failed(format!(
                "host speaks plugin API v{}, this plugin was built for v{API_VERSION}",
                ctx.api_version
            )));
        }
        self.plugin.init(&ctx)?;
        let info = self.plugin.info();
        Ok(InitializeResult {
            name: info.name,
            version: info.version,
            api_version: API_VERSION,
            kinds: self.kinds.clone(),
            tables: self.sink_tables.map(|f| f(&self.plugin)).unwrap_or_default(),
            actuator: self.actuator_caps.map(|f| f(&self.plugin)),
        })
    }
}

fn parse<T: DeserializeOwned>(params: Value) -> PluginResult<T> {
    serde_json::from_value(params).map_err(|e| PluginError::Protocol(format!("invalid params: {e}")))
}

fn to_value(v: impl Serialize) -> PluginResult<Value> {
    serde_json::to_value(v).map_err(|e| PluginError::Failed(format!("serialising result: {e}")))
}

fn dispatch_classifier<P: Classifier>(p: &mut P, m: &str, params: Value, _: &Notifier) -> Option<PluginResult<Value>> {
    (m == methods::CLASSIFY)
        .then(|| parse::<ClassifyParams>(params).and_then(|c| p.classify(&c.proc)).and_then(to_value))
}

fn dispatch_policy<P: Policy>(p: &mut P, m: &str, params: Value, _: &Notifier) -> Option<PluginResult<Value>> {
    (m == methods::EVALUATE)
        .then(|| parse::<EvaluateParams>(params).and_then(|e| p.evaluate(&e.snapshot)).and_then(to_value))
}

fn dispatch_sensor<P: Sensor>(p: &mut P, m: &str, params: Value, _: &Notifier) -> Option<PluginResult<Value>> {
    (m == methods::SENSOR_POLL).then(|| parse::<SensorInput>(params).and_then(|i| p.poll(&i)).and_then(to_value))
}

fn dispatch_sink<P: Sink>(p: &mut P, m: &str, params: Value, _: &Notifier) -> Option<PluginResult<Value>> {
    match m {
        methods::SINK_RECORD => {
            Some(parse::<SinkRecordParams>(params).and_then(|r| p.record(&r.records)).map(|()| Value::Null))
        }
        methods::SINK_FLUSH => Some(p.flush().map(|()| Value::Null)),
        _ => None,
    }
}

fn dispatch_adapter<P: Adapter>(p: &mut P, m: &str, _: Value, n: &Notifier) -> Option<PluginResult<Value>> {
    (m == methods::ADAPTER_START).then(|| {
        let n = n.clone();
        let sink: EventSink = Arc::new(move |e: Event| n.event(e));
        p.start(sink).map(|()| Value::Null)
    })
}

fn dispatch_actuator<P: Actuator>(p: &mut P, m: &str, params: Value, _: &Notifier) -> Option<PluginResult<Value>> {
    match m {
        methods::ACTUATOR_CAPS => Some(to_value(p.caps())),
        methods::ACTUATOR_READ => {
            Some(parse::<ActuatorReadParams>(params).and_then(|r| p.read(&r.target, &r.knob)).and_then(to_value))
        }
        methods::ACTUATOR_APPLY => Some(
            parse::<ActuatorApplyParams>(params).and_then(|a| p.apply(&a.target, &a.setting)).map(|()| Value::Null),
        ),
        methods::ACTUATOR_RESET => Some(
            parse::<ActuatorResetParams>(params)
                .and_then(|r| p.reset(&r.target, &r.knob, r.original.as_ref()))
                .map(|()| Value::Null),
        ),
        _ => None,
    }
}
