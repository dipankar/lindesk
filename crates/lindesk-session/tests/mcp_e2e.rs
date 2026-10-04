//! End to end: the real `lindesk-mcp` binary over stdio (raw JSON-RPC, as an MCP client
//! sends it) against `lindesk-session --standalone --dry-run` on a private session bus:
//! `status` → `recommendations` → `plan_change` → `apply_change` (user-scope `config_set`,
//! ttl 1 min) → `list_changes` → `revert_change`, then the user config file must be back
//! byte for byte.
//!
//! Needs `lindesk-mcp` next to `lindesk-session`: run with
//! `cargo test -p lindesk-session -p lindesk-mcp --test mcp_e2e`.

mod support;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use lindesk_ipc::QOS_NAME;
use serde_json::{Value, json};
use support::*;

const ORIGINAL: &str = "# written by hand\n[general]\ntick = \"250ms\"\n\n[plugins.threshold]\nenabled = true\n";

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl Mcp {
    fn send(&mut self, v: &Value) {
        println!(">>> {v}");
        writeln!(self.stdin, "{v}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// A request; returns its `result` (panics on a JSON-RPC error).
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let line = self.lines.recv_timeout(Duration::from_secs(60)).expect("lindesk-mcp answered within 60 s");
            let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"));
            if v["id"] == id {
                assert!(v.get("error").is_none(), "JSON-RPC error: {v}");
                return v["result"].clone();
            }
        }
    }

    /// `tools/call`; prints the summary line(s) and returns (is_error, structuredContent).
    fn tool(&mut self, name: &str, args: Value) -> (bool, Value) {
        let r = self.request("tools/call", json!({ "name": name, "arguments": args }));
        let summary = r["content"][0]["text"].as_str().unwrap_or("").to_string();
        let err = r["isError"].as_bool().unwrap_or(false);
        println!(
            "<<< {name}{}: {}",
            if err { " (is_error)" } else { "" },
            summary.lines().take(6).collect::<Vec<_>>().join("\n    ")
        );
        (err, r["structuredContent"].clone())
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_plans_applies_and_reverts_through_the_session_agent() {
    let Some(mcp_bin) = session_bin().parent().map(|d| d.join("lindesk-mcp")).filter(|p| p.exists()) else {
        eprintln!(
            "skipping: lindesk-mcp is not built next to lindesk-session (cargo test -p lindesk-session -p lindesk-mcp)"
        );
        return;
    };
    let tmp = fast_tempdir();
    let bus = Bus::session();
    let dir = |d: &str| tmp.path().join(d).display().to_string();
    let (xdg_config, xdg_state, run, tel) = (dir("config"), dir("state"), dir("run"), dir("state/lindesk/telemetry"));
    let cfg = tmp.path().join("config/lindesk/lindesk.toml");
    std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    std::fs::write(&cfg, ORIGINAL).unwrap();
    let env = [
        ("DBUS_SESSION_BUS_ADDRESS", bus.address.as_str()),
        ("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent/lindesk-test"),
        ("XDG_CONFIG_HOME", xdg_config.as_str()),
        ("XDG_DATA_HOME", xdg_config.as_str()),
        ("XDG_STATE_HOME", xdg_state.as_str()),
    ];
    let mut agent = Daemon::spawn(
        "lindesk-session",
        &session_bin(),
        &["--standalone", "--dry-run", "--no-adapters", "--state-dir", &run, "--telemetry-dir", &tel],
        &env,
    );
    let conn = connect(&bus.address).await;
    wait_for_name(&conn, QOS_NAME, Duration::from_secs(60)).await;

    let mut cmd = Command::new(&mcp_bin);
    cmd.arg("--stdio").arg("--log-level").arg("warn").stdin(Stdio::piped()).stdout(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn lindesk-mcp");
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut mcp = Mcp { child, stdin, lines, next_id: 0 };

    let init = mcp.request(
        "initialize",
        json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e-test", "version": "0" } }),
    );
    assert_eq!(init["serverInfo"]["name"], "lindesk-mcp");
    mcp.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    let (err, st) = mcp.tool("status", json!({ "scope": "user" }));
    assert!(!err);
    assert_eq!(st["source"], "daemon:lindesk-session", "{st}");

    let (err, rec) = mcp.tool("recommendations", json!({ "include_telemetry": false }));
    assert!(!err, "{rec}");

    let ops =
        json!([{ "op": "config_set", "scope": "user", "key": "plugins.threshold.config.epoch", "value": "400ms" }]);
    let description = "shorter interaction epoch to test the MCP path";
    let (err, plan) = mcp.tool("plan_change", json!({ "scope": "user", "ops": ops, "description": description }));
    assert!(!err, "{plan}");
    assert_eq!(plan["plan"]["rejections"], json!([]));
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), ORIGINAL, "planning writes nothing");

    let (err, applied) =
        mcp.tool("apply_change", json!({ "scope": "user", "ops": ops, "description": description, "ttl_minutes": 1 }));
    assert!(!err, "{applied}");
    let cs = &applied["changeset"];
    let id = cs["id"].as_u64().unwrap();
    assert_eq!(cs["status"], "applied");
    assert_eq!((cs["author"]["origin"].as_str(), cs["author"]["client"].as_str()), (Some("mcp"), Some("e2e-test")));
    assert!(cs["ttl_until"].is_i64(), "MCP changes always have a dead-man ttl");
    assert!(std::fs::read_to_string(&cfg).unwrap().contains("epoch = \"400ms\""));

    let (err, list) = mcp.tool("list_changes", json!({ "scope": "user" }));
    assert!(!err);
    assert_eq!(list["changes"][0]["id"].as_u64(), Some(id), "{list}");

    // A guardrail rejection comes back as a tool error with the reason.
    let bad =
        json!([{ "op": "config_set", "scope": "user", "key": "plugins.threshold.config.no_such_key", "value": 1 }]);
    let (err, rej) = mcp.tool("apply_change", json!({ "scope": "user", "ops": bad, "description": "bad key" }));
    assert!(err, "{rej}");
    assert_eq!(rej["error"], "rejected", "{rej}");

    let (err, reverted) = mcp.tool("revert_change", json!({ "scope": "user", "id": id }));
    assert!(!err, "{reverted}");
    assert_eq!(reverted["changeset"]["reverts"].as_u64(), Some(id));
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), ORIGINAL, "the user config is back byte for byte");

    drop(mcp);
    assert!(agent.terminate().success());
}
