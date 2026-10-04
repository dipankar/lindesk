//! Spawn the real `lindesk-mcp` binary over stdio and drive it with rmcp's client.

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, GetPromptRequestParams, ReadResourceRequestParams, ResourceContents};
use rmcp::transport::TokioChildProcess;
use serde_json::Value;

#[tokio::test]
async fn stdio_session_against_this_machine() {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_lindesk-mcp"));
    // Deterministic: never talk to a real daemon from tests (read-only tools use local probing).
    cmd.arg("--no-daemon").arg("--log-level").arg("warn");
    let client = ().serve(TokioChildProcess::new(cmd).expect("spawn lindesk-mcp")).await.expect("initialize");

    let info = client.peer_info().expect("server info after initialize");
    assert_eq!(info.server_info.as_ref().map(|i| i.name.as_str()), Some("lindesk-mcp"));
    assert!(info.instructions.as_deref().unwrap_or("").contains("auto-revert"), "{:?}", info.instructions);

    // tools/list: names and annotations.
    let tools = client.list_all_tools().await.expect("tools/list");
    let get = |n: &str| tools.iter().find(|t| t.name == n).unwrap_or_else(|| panic!("tool {n} missing"));
    for n in ["status", "doctor", "hardware", "latency_map", "swap_analysis", "query", "recommendations"] {
        let a = get(n).annotations.clone().unwrap();
        assert_eq!((a.read_only_hint, a.open_world_hint), (Some(true), Some(false)), "{n}");
    }
    for n in ["apply_change", "confirm_change", "revert_change", "set_profile", "calibrate"] {
        let a = get(n).annotations.clone().unwrap();
        assert_eq!((a.read_only_hint, a.destructive_hint), (Some(false), Some(false)), "{n}");
    }

    // hardware and doctor fall back to local probing.
    for (tool, key) in [("hardware", "hardware"), ("doctor", "doctor")] {
        let r = client.call_tool(CallToolRequestParams::new(tool)).await.expect(tool);
        assert_ne!(r.is_error, Some(true), "{tool}: {r:?}");
        let v = r.structured_content.clone().expect("structured content");
        assert_eq!(v["source"], "local", "{tool}");
        assert!(v[key].is_object(), "{tool}: {v}");
    }

    // Mutations need a daemon.
    let mut args = serde_json::Map::new();
    args.insert("scope".into(), "user".into());
    args.insert("id".into(), 1.into());
    let r = client.call_tool(CallToolRequestParams::new("confirm_change").with_arguments(args)).await.unwrap();
    assert_eq!(r.is_error, Some(true));

    // resources/read lindesk://hardware.
    let res = client.read_resource(ReadResourceRequestParams::new("lindesk://hardware")).await.expect("resources/read");
    let ResourceContents::TextResourceContents { text, mime_type, .. } = &res.contents[0] else { panic!("{res:?}") };
    assert_eq!(mime_type.as_deref(), Some("application/json"));
    let v: Value = serde_json::from_str(text).unwrap();
    assert!(v["hardware"]["cpus"].as_u64().unwrap_or(0) >= 1, "{v}");

    // Docs are embedded.
    let res = client.read_resource(ReadResourceRequestParams::new("lindesk://docs/control-plane")).await.unwrap();
    let ResourceContents::TextResourceContents { text, .. } = &res.contents[0] else { panic!() };
    assert!(text.contains("Versioned, guardrailed changes"));

    // prompts/get assess-responsiveness.
    let p = client.get_prompt(GetPromptRequestParams::new("assess-responsiveness")).await.expect("prompts/get");
    let body = serde_json::to_string(&p.messages).unwrap();
    for step in ["recommendations", "plan_change", "apply_change", "confirm_change", "revert_change"] {
        assert!(body.contains(step), "prompt lacks {step}");
    }
    client.cancel().await.unwrap();
}
