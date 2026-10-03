//! {{name}}: a lindesk {{kind}} plugin.
//!
//! * `cargo build --release` (plugin.toml runs target/release/{{name}})
//! * `cargo test` drives the plugin through the JSON protocol in-process
//! * `lindeskctl plugin test .` runs the protocol conformance checks on the binary

use lindesk_plugin::prelude::*;

{{body}}
