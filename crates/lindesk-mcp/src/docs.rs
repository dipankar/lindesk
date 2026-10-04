//! Documentation embedded for `lindesk://docs/{topic}`.

/// One embedded document.
pub struct Doc {
    pub topic: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub text: &'static str,
}

pub const DOCS: &[Doc] = &[
    Doc {
        topic: "control-plane",
        title: "Control plane: profiles, versioned changes, calibration, MCP",
        description: "ChangeOp shapes, guardrails and their ranges, risk classes, the dead-man switch (ttl), versioning and revert semantics.",
        text: include_str!("../../../docs/CONTROL-PLANE.md"),
    },
    Doc {
        topic: "architecture",
        title: "lindesk architecture and shared contracts",
        description: "Components, filesystem layout, D-Bus API and the Status/Explain/Event JSON payloads.",
        text: include_str!("../../../docs/ARCHITECTURE.md"),
    },
    Doc {
        topic: "plugin-concepts",
        title: "Plugin concepts",
        description: "The pipeline (sensors, adapters, classifiers, policies, actuators, sinks), QoS classes, intents and priorities.",
        text: include_str!("../../../docs/src/plugins/concepts.md"),
    },
    Doc {
        topic: "telemetry-schema",
        title: "Telemetry schema",
        description: "Tables and columns of the telemetry lake, for writing `query` SQL.",
        text: include_str!("../../../docs/src/reference/schema.md"),
    },
    Doc {
        topic: "safety",
        title: "Safety invariants",
        description: "What lindesk never does (realtime classes, throttling critical work, below-floor limits) and how changes are restored.",
        text: include_str!("../../../docs/src/reference/safety.md"),
    },
    Doc {
        topic: "mcp",
        title: "lindesk-mcp guide",
        description: "This server: tools, resources, prompts, transports and the safety model.",
        text: include_str!("../../../docs/src/guide/mcp.md"),
    },
];

pub fn get(topic: &str) -> Option<&'static Doc> {
    DOCS.iter().find(|d| d.topic == topic)
}
