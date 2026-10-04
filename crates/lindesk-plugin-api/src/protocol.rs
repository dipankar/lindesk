//! The out-of-process plugin protocol: JSON-RPC 2.0, one JSON object per line, over the
//! plugin's stdin (host → plugin) and stdout (plugin → host). stderr is captured as logs.
//!
//! Lifecycle:
//! 1. host → `initialize` ([`InitializeParams`]) → [`InitializeResult`]
//! 2. host → kind-specific requests (`classifier.classify`, `policy.evaluate`, ...)
//! 3. plugin → notifications at any time (`event`, `log`, `metric`, `samples`)
//! 4. host → `shutdown` notification, then closes stdin.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::*;
use crate::traits::*;

pub const JSONRPC: &str = "2.0";

/// Method names.
pub mod methods {
    pub const INITIALIZE: &str = "initialize";
    pub const SHUTDOWN: &str = "shutdown";
    pub const SENSOR_POLL: &str = "sensor.poll";
    pub const CLASSIFY: &str = "classifier.classify";
    pub const EVALUATE: &str = "policy.evaluate";
    pub const ACTUATOR_CAPS: &str = "actuator.caps";
    pub const ACTUATOR_READ: &str = "actuator.read";
    pub const ACTUATOR_APPLY: &str = "actuator.apply";
    pub const ACTUATOR_RESET: &str = "actuator.reset";
    pub const SINK_RECORD: &str = "sink.record";
    pub const SINK_FLUSH: &str = "sink.flush";
    pub const ADAPTER_START: &str = "adapter.start";

    // plugin → host notifications
    pub const EVENT: &str = "event";
    pub const LOG: &str = "log";
    pub const METRIC: &str = "metric";
    pub const SAMPLES: &str = "samples";
}

/// Standard and lindesk-specific error codes.
pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;
    pub const FAILED: i64 = -32000;
    pub const UNSUPPORTED: i64 = -32001;
    pub const GONE: i64 = -32002;
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Request {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Response {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// Any line on the wire.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum Message {
    Request(Request),
    Response(Response),
    Notification(Notification),
}

impl Message {
    pub fn parse(line: &str) -> Result<Message, serde_json::Error> {
        // Untagged matching would accept a response as a notification; decide by shape.
        let v: Value = serde_json::from_str(line)?;
        let has_id = v.get("id").is_some_and(|i| !i.is_null());
        let has_method = v.get("method").is_some();
        if has_method && has_id {
            serde_json::from_value(v).map(Message::Request)
        } else if has_method {
            serde_json::from_value(v).map(Message::Notification)
        } else {
            serde_json::from_value(v).map(Message::Response)
        }
    }
}

impl Request {
    pub fn new(id: u64, method: &str, params: impl Serialize) -> Self {
        Request {
            jsonrpc: JSONRPC.into(),
            id,
            method: method.into(),
            params: serde_json::to_value(params).unwrap_or(Value::Null),
        }
    }
}

impl Notification {
    pub fn new(method: &str, params: impl Serialize) -> Self {
        Notification {
            jsonrpc: JSONRPC.into(),
            method: method.into(),
            params: serde_json::to_value(params).unwrap_or(Value::Null),
        }
    }
}

impl Response {
    pub fn ok(id: u64, result: impl Serialize) -> Self {
        Response {
            jsonrpc: JSONRPC.into(),
            id,
            result: Some(serde_json::to_value(result).unwrap_or(Value::Null)),
            error: None,
        }
    }

    pub fn err(id: u64, code: i64, message: impl Into<String>) -> Self {
        Response {
            jsonrpc: JSONRPC.into(),
            id,
            result: None,
            error: Some(RpcError { code, message: message.into(), data: None }),
        }
    }

    pub fn from_plugin_error(id: u64, e: &PluginError) -> Self {
        let code = match e {
            PluginError::Unsupported(_) => codes::UNSUPPORTED,
            PluginError::Gone(_) => codes::GONE,
            PluginError::Protocol(_) => codes::INVALID_PARAMS,
            _ => codes::FAILED,
        };
        Response::err(id, code, e.to_string())
    }

    /// Convert into the typed result, mapping RPC errors to [`PluginError`].
    pub fn into_result<T: serde::de::DeserializeOwned>(self) -> PluginResult<T> {
        if let Some(e) = self.error {
            return Err(match e.code {
                codes::UNSUPPORTED => PluginError::Unsupported(e.message),
                codes::METHOD_NOT_FOUND => PluginError::Unsupported(e.message),
                codes::FAILED => PluginError::Failed(e.message),
                codes::GONE => PluginError::Gone(e.message),
                _ => PluginError::Protocol(format!("{} ({})", e.message, e.code)),
            });
        }
        serde_json::from_value(self.result.unwrap_or(Value::Null))
            .map_err(|e| PluginError::Protocol(format!("bad result: {e}")))
    }
}

pub type InitializeParams = InitContext;

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct InitializeResult {
    pub name: String,
    pub version: String,
    pub api_version: u32,
    pub kinds: Vec<PluginKind>,
    /// For sinks: the tables to receive. Empty = all.
    #[serde(default)]
    pub tables: Vec<String>,
    /// For actuators: their capabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actuator: Option<ActuatorCaps>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct ClassifyParams {
    pub proc: ProcInfo,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct EvaluateParams {
    pub snapshot: Snapshot,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct ActuatorReadParams {
    pub target: Target,
    pub knob: Knob,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct ActuatorApplyParams {
    pub target: Target,
    pub setting: Setting,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct ActuatorResetParams {
    pub target: Target,
    pub knob: Knob,
    #[serde(default)]
    pub original: Option<Setting>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct SinkRecordParams {
    pub records: Vec<Record>,
}

/// Params of the `samples` notification (plugin → host). The `event` and `metric`
/// notifications carry a bare [`Event`] and [`Metric`] respectively.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default, PartialEq)]
pub struct SamplesParams {
    pub samples: Vec<Sample>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct LogParams {
    pub level: LogLevel,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_shapes_are_distinguished() {
        let r = Message::parse(r#"{"jsonrpc":"2.0","id":1,"method":"policy.evaluate","params":{}}"#).unwrap();
        assert!(matches!(r, Message::Request(_)));
        let n = Message::parse(r#"{"jsonrpc":"2.0","method":"log","params":{"level":"info","message":"hi"}}"#).unwrap();
        assert!(matches!(n, Message::Notification(_)));
        let s = Message::parse(r#"{"jsonrpc":"2.0","id":1,"result":null}"#).unwrap();
        assert!(matches!(s, Message::Response(_)));
    }

    #[test]
    fn error_codes_map_to_plugin_errors() {
        let r = Response::err(3, codes::UNSUPPORTED, "nope");
        assert_eq!(r.into_result::<()>(), Err(PluginError::Unsupported("nope".into())));
    }

    #[test]
    fn settings_roundtrip() {
        let s = Setting::IoMax { device: "259:0".into(), limit: IoLimit { wbps: Some(1 << 20), ..Default::default() } };
        let j = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Setting>(&j).unwrap(), s);
        assert_eq!(s.knob(), Knob::IoMax("259:0".into()));
    }
}
