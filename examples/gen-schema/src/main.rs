//! Print the JSON Schema (draft 2020-12) of every lindesk plugin protocol type: the same
//! document `lindeskctl plugin schema` prints, generated from the Rust types with schemars.
//!
//! ```sh
//! cargo run -q --manifest-path examples/gen-schema/Cargo.toml > docs/src/reference/protocol.schema.json
//! cargo run -q --manifest-path examples/gen-schema/Cargo.toml -- --check docs/src/reference/protocol.schema.json
//! ```

use std::process::ExitCode;

fn main() -> ExitCode {
    let schema =
        serde_json::to_string_pretty(&lindesk_plugin_api::protocol_schema()).expect("schema serialises") + "\n";
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => {
            print!("{schema}");
            ExitCode::SUCCESS
        }
        [flag, path] if flag == "--check" => match std::fs::read_to_string(path) {
            Ok(current) if current == schema => ExitCode::SUCCESS,
            Ok(_) => {
                eprintln!("{path} is out of date; regenerate it with gen-schema > {path}");
                ExitCode::FAILURE
            }
            Err(e) => {
                eprintln!("{path}: {e}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("usage: gen-schema [--check <protocol.schema.json>]");
            ExitCode::from(2)
        }
    }
}
