//! # lindesk plugin API
//!
//! lindesk is a pipeline of plugins:
//!
//! ```text
//! adapters ─┐
//! sensors ──┼─► classifiers ─► policies ─► resolver + safety caps ─► actuators
//!           └──────────────── records ─────────────────────────────► sinks
//! ```
//!
//! Built-in plugins implement the traits in [`traits`] in-process. External plugins can be
//! written in any language: they are executables that speak newline-delimited JSON-RPC 2.0
//! on stdio ([`protocol`]) and are described by a `plugin.toml` ([`manifest`]).
//!
//! Policies never touch the system directly: they return [`model::Intent`]s, which the host
//! resolves, clamps to safety caps and applies through actuators, recording provenance.

pub mod manifest;
pub mod model;
pub mod protocol;
pub mod traits;

pub use manifest::*;
pub use model::*;
pub use traits::*;

/// Version of the plugin API and stdio protocol. Bumped on breaking changes only.
pub const API_VERSION: u32 = 1;

/// JSON Schema (draft 2020-12) for every protocol type, keyed by type name.
/// Published in the docs and printed by `lindeskctl plugin schema`.
pub fn protocol_schema() -> serde_json::Value {
    use protocol::*;
    let mut generator = schemars::generate::SchemaSettings::draft2020_12().into_generator();
    macro_rules! add {
        ($($t:ty),* $(,)?) => { $( generator.subschema_for::<$t>(); )* };
    }
    add!(
        Request,
        Response,
        Notification,
        InitializeParams,
        InitializeResult,
        ClassifyParams,
        EvaluateParams,
        ActuatorReadParams,
        ActuatorApplyParams,
        ActuatorResetParams,
        SinkRecordParams,
        LogParams,
        SensorInput,
        SensorOutput,
        Classification,
        PolicyOutput,
        Event,
        Metric,
        Record,
        Manifest,
    );
    let defs = generator.take_definitions(true);
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "lindesk plugin protocol",
        "api_version": API_VERSION,
        "$defs": defs,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn schema_contains_core_types() {
        let s = super::protocol_schema();
        let defs = s["$defs"].as_object().unwrap();
        for t in ["Snapshot", "Intent", "Setting", "Event", "Manifest", "ProcInfo"] {
            assert!(defs.contains_key(t), "missing {t}");
        }
    }
}
