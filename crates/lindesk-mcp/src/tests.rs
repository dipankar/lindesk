//! Server tests over an in-process MCP connection with fake backends.

use std::sync::{Arc, Mutex};

use rmcp::model::{CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

use crate::backend::{Backends, Daemon, DaemonError, DaemonRequest, Local};
use crate::ops::Scope;
use crate::server::LindeskMcp;

type Responder = Box<dyn Fn(Scope, &DaemonRequest) -> Result<String, DaemonError> + Send + Sync>;

/// A daemon that records every call and answers with `respond`.
struct FakeDaemon {
    calls: Mutex<Vec<(Scope, DaemonRequest)>>,
    respond: Responder,
}

impl FakeDaemon {
    fn new(
        respond: impl Fn(Scope, &DaemonRequest) -> Result<String, DaemonError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(FakeDaemon { calls: Mutex::new(Vec::new()), respond: Box::new(respond) })
    }

    fn absent() -> Arc<Self> {
        Self::new(|scope, _| Err(DaemonError::NotRunning { scope, detail: "nobody owns the name".into() }))
    }

    fn calls(&self) -> Vec<(Scope, DaemonRequest)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Daemon for FakeDaemon {
    async fn call(&self, scope: Scope, req: DaemonRequest) -> Result<String, DaemonError> {
        let r = (self.respond)(scope, &req);
        self.calls.lock().unwrap().push((scope, req));
        r
    }
}

/// Fixture machine: an HDD with swap on it, a warning from doctor, a RED hour in the lake.
#[derive(Default)]
struct FakeLocal {
    queries: Mutex<Vec<String>>,
    /// Behave as if the lindesk-telemetry package weren't installed.
    no_telemetry: bool,
}

impl Local for FakeLocal {
    fn status(&self) -> anyhow::Result<Value> {
        Ok(json!({"daemon": null, "text": "local status text", "requirements": []}))
    }
    fn doctor(&self) -> anyhow::Result<Value> {
        Ok(json!({"level": "PARTIAL", "findings": [
            {"severity": "ok", "check": "cgroup v2", "status": "unified"},
            {"severity": "warn", "check": "io.latency", "status": "not built", "remedy": "rebuild the kernel"}
        ]}))
    }
    fn hardware(&self) -> anyhow::Result<Value> {
        Ok(json!({
            "cpus": 4, "cores": 2, "arch": "x86_64", "mem_total": 8589934592u64, "power": "ac",
            "storage": [{"dev": "8:0", "name": "sda", "kind": "hdd", "scheduler": "mq-deadline", "nominal_bps": 150000000}],
            "latency": {"noisy": false, "storage": [{"dev": "8:0", "read_4k_p50_us": 9000.0, "read_4k_p99_us": 150000.0}]}
        }))
    }
    fn swap(&self) -> anyhow::Result<Value> {
        Ok(json!({"vm": {"swappiness": 60, "page_cluster": 3}, "devices": [{"path": "/dev/sda2"}], "findings": [
            {"severity": "warn", "title": "Swap on a hard disk", "detail": "page-in costs ms",
             "proposed_ops": [{"op": "sysctl", "key": "vm.swappiness", "value": "10"}]}
        ]}))
    }
    fn requirements(&self) -> anyhow::Result<Value> {
        Ok(json!([{"id": "kernel", "title": "Kernel", "status": "pass", "detail": "6.8"}]))
    }
    fn query(&self, _scope: Scope, sql: &str) -> Result<Value, String> {
        if self.no_telemetry {
            return Err(crate::local::NOT_INSTALLED.to_string());
        }
        self.queries.lock().unwrap().push(sql.to_string());
        if sql.contains("lvl = 'red'") {
            Ok(json!([{"hour": "2026-10-04T09:00Z", "red_s": 1500.0}]))
        } else if sql.contains("action_applied") {
            Ok(json!([]))
        } else {
            Ok(json!([{"x": 1}, {"x": 2}, {"x": 3}]))
        }
    }
    fn plugins(&self) -> anyhow::Result<Value> {
        Ok(json!({"external": [], "rejected": []}))
    }
    fn profiles(&self) -> anyhow::Result<Value> {
        Ok(json!({"profiles": [{"name": "balanced", "active": true}, {"name": "latency"}]}))
    }
    fn config(&self) -> anyhow::Result<(String, String)> {
        Ok(("/home/u/.config/lindesk/lindesk.toml".into(), "[general]\nprofile = \"auto\"\n".into()))
    }
    fn changes(&self, _scope: Scope, _limit: u32) -> anyhow::Result<Value> {
        Ok(json!([]))
    }
    fn change(&self, _scope: Scope, _id: u64) -> anyhow::Result<Option<Value>> {
        Ok(None)
    }
    fn explain(&self, what: &str) -> anyhow::Result<Value> {
        Ok(json!({"query": what, "known": false}))
    }
}

struct Harness {
    client: RunningService<RoleClient, ClientConfig>,
    daemon: Arc<FakeDaemon>,
    local: Arc<FakeLocal>,
}

async fn harness(daemon: Arc<FakeDaemon>) -> Harness {
    harness_with(daemon, FakeLocal::default()).await
}

async fn harness_with(daemon: Arc<FakeDaemon>, local: FakeLocal) -> Harness {
    let local = Arc::new(local);
    let backends = Backends { daemon: daemon.clone(), local: local.clone() };
    let (server_io, client_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        let svc = LindeskMcp::new(backends).serve(server_io).await.expect("server starts");
        let _ = svc.waiting().await;
    });
    let client = ClientConfig::new(ClientCapabilities::default(), Implementation::new("test-agent", "1.0"))
        .serve(client_io)
        .await
        .expect("client connects");
    Harness { client, daemon, local }
}

impl Harness {
    async fn call(&self, name: &'static str, args: Value) -> CallToolResult {
        let mut p = CallToolRequestParams::new(name);
        if let Value::Object(m) = args {
            p = p.with_arguments(m);
        }
        self.client.call_tool(p).await.expect("tool call is not a protocol error")
    }
}

fn data(r: &CallToolResult) -> &Value {
    r.structured_content.as_ref().expect("structured content")
}

fn text(r: &CallToolResult) -> String {
    r.content.iter().filter_map(|c| c.as_text().map(|t| t.text.clone())).collect::<Vec<_>>().join("\n")
}

fn is_error(r: &CallToolResult) -> bool {
    r.is_error == Some(true)
}

#[tokio::test]
async fn read_only_tools_fall_back_to_local() {
    let h = harness(FakeDaemon::absent()).await;
    let r = h.call("hardware", json!({})).await;
    assert!(!is_error(&r), "{r:?}");
    assert_eq!(data(&r)["source"], "local");
    assert_eq!(data(&r)["hardware"]["arch"], "x86_64");
    let r = h.call("status", json!({})).await;
    assert_eq!(data(&r)["source"], "local");
    assert!(text(&r).contains("local status text"));
    let r = h.call("swap_analysis", json!({})).await;
    assert_eq!(data(&r)["source"], "local");
    assert!(text(&r).contains("Swap on a hard disk"));
    // Both daemons were tried first.
    let scopes: Vec<Scope> =
        h.daemon.calls().iter().filter(|(_, r)| *r == DaemonRequest::GetHardware).map(|(s, _)| *s).collect();
    assert_eq!(scopes, [Scope::User, Scope::System]);
}

#[tokio::test]
async fn read_only_tools_prefer_the_daemon() {
    let d = FakeDaemon::new(|scope, req| match (scope, req) {
        (Scope::User, DaemonRequest::GetHardware) => Ok(json!({"arch": "aarch64", "cpus": 8}).to_string()),
        (Scope::User, DaemonRequest::Status) => {
            Ok(json!({"text": "level green", "status": {"plugins": [{"name": "psi"}]}}).to_string())
        }
        (s, _) => Err(DaemonError::NotRunning { scope: s, detail: "x".into() }),
    });
    let h = harness(d).await;
    let r = h.call("hardware", json!({})).await;
    assert_eq!(data(&r)["source"], "daemon:lindesk-session");
    assert_eq!(data(&r)["hardware"]["arch"], "aarch64");
    let r = h.call("status", json!({"scope": "user"})).await;
    assert_eq!(data(&r)["source"], "daemon:lindesk-session");
    let r = h.call("list_plugins", json!({})).await;
    assert_eq!(data(&r)["live"][0]["name"], "psi");
}

#[tokio::test]
async fn mutations_without_a_daemon_fail_with_remedies() {
    let h = harness(FakeDaemon::absent()).await;
    for (tool, args) in [
        (
            "apply_change",
            json!({"scope": "user", "ops": [{"op": "profile_use", "scope": "user", "name": "latency"}], "description": "try latency"}),
        ),
        (
            "plan_change",
            json!({"scope": "system", "ops": [{"op": "sysctl", "key": "vm.swappiness", "value": "10"}], "description": "swap"}),
        ),
        ("confirm_change", json!({"scope": "user", "id": 3})),
        ("revert_change", json!({"scope": "user", "id": 3})),
        ("set_profile", json!({"name": "latency"})),
        ("calibrate", json!({"quick": true})),
    ] {
        let r = h.call(tool, args).await;
        assert!(is_error(&r), "{tool}: {r:?}");
        assert_eq!(data(&r)["error"], "daemon_not_running", "{tool}");
        let t = text(&r);
        assert!(t.contains("systemctl"), "{tool}: {t}");
    }
    assert!(h.local.queries.lock().unwrap().is_empty(), "mutations never touch the local backend");
}

#[tokio::test]
async fn pause_is_always_timed_and_off_is_not_a_tool() {
    let d = FakeDaemon::new(|_, req| match req {
        DaemonRequest::SetPaused { paused, seconds } => {
            Ok(json!({"paused": paused, "until_ms": (*seconds > 0).then_some(1_000)}).to_string())
        }
        _ => Ok("{}".into()),
    });
    let h = harness(d).await;
    let r = h.call("pause", json!({})).await;
    assert!(!is_error(&r), "{r:?}");
    assert!(text(&r).contains("paused for 10 min"), "{}", text(&r));
    let r = h.call("pause", json!({"minutes": 0})).await;
    assert!(is_error(&r) && text(&r).starts_with("rejected"));
    let r = h.call("pause", json!({"minutes": 121})).await;
    assert!(is_error(&r));
    let r = h.call("resume", json!({"scope": "system"})).await;
    assert!(!is_error(&r), "{r:?}");
    let calls = h.daemon.calls();
    assert!(matches!(calls[0], (Scope::User, DaemonRequest::SetPaused { paused: true, seconds: 600 })));
    assert!(matches!(calls[1], (Scope::System, DaemonRequest::SetPaused { paused: false, seconds: 0 })));
    assert_eq!(calls.len(), 2, "rejected pauses never reach the daemon");
    let tools = h.client.list_all_tools().await.unwrap();
    assert!(!tools.iter().any(|t| t.name == "off" || t.name == "uninstall"), "turning lindesk off is a human decision");
}

#[tokio::test]
async fn apply_sends_envelope_and_default_ttl() {
    let d = FakeDaemon::new(|_, req| match req {
        DaemonRequest::ApplyChange { .. } => Ok(json!({"id": 7, "status": "applied", "ttl_until": 1}).to_string()),
        _ => Ok("{}".into()),
    });
    let h = harness(d).await;
    let r = h
        .call(
            "apply_change",
            json!({"scope": "system", "ops": [{"op": "sysctl", "key": "vm.page-cluster", "value": "0"}], "description": "no swap readahead on zram"}),
        )
        .await;
    assert!(!is_error(&r), "{r:?}");
    assert!(text(&r).contains("auto-reverts in 10 min"), "{}", text(&r));
    let calls = h.daemon.calls();
    let (scope, req) = calls.last().unwrap();
    assert_eq!(*scope, Scope::System);
    let DaemonRequest::ApplyChange { ops_json, description, ttl_secs } = req else { panic!("{req:?}") };
    assert_eq!(*ttl_secs, 600);
    assert_eq!(description, "no swap readahead on zram");
    let env: Value = serde_json::from_str(ops_json).unwrap();
    assert_eq!(env["origin"], "mcp");
    assert_eq!(env["client"], "test-agent");
    assert_eq!(env["ops"], json!([{"op": "sysctl", "key": "vm.page-cluster", "value": "0"}]));

    // Explicit ttl, and out-of-range ttls never reach the daemon.
    let n = h.daemon.calls().len();
    let ok = h
        .call("apply_change", json!({"scope": "user", "ops": [{"op": "profile_use", "scope": "user", "name": "latency"}], "description": "x", "ttl_minutes": 30}))
        .await;
    assert!(!is_error(&ok));
    assert!(matches!(h.daemon.calls().last().unwrap().1, DaemonRequest::ApplyChange { ttl_secs: 1800, .. }));
    for bad in [0, 1441] {
        let r = h
            .call("apply_change", json!({"scope": "user", "ops": [{"op": "profile_use", "scope": "user", "name": "latency"}], "description": "x", "ttl_minutes": bad}))
            .await;
        assert!(is_error(&r) && text(&r).contains("ttl_minutes"), "{r:?}");
    }
    assert_eq!(h.daemon.calls().len(), n + 1);

    // set_profile is a profile_use changeset with the same envelope and dead-man switch.
    h.call("set_profile", json!({"scope": "user", "name": "gaming"})).await;
    let (_, req) = h.daemon.calls().last().unwrap().clone();
    let DaemonRequest::ApplyChange { ops_json, ttl_secs, .. } = req else { panic!() };
    assert_eq!(ttl_secs, 600);
    let env: Value = serde_json::from_str(&ops_json).unwrap();
    assert_eq!(env["ops"][0], json!({"op": "profile_use", "scope": "user", "name": "gaming"}));
}

#[tokio::test]
async fn guardrail_rejections_are_tool_errors() {
    let d = FakeDaemon::new(|scope, req| match req {
        DaemonRequest::ApplyChange { .. } => {
            Err(DaemonError::Rejected { scope, message: "rejected: vm.swappiness=500 outside 0–200".into() })
        }
        DaemonRequest::PlanChange { .. } => {
            // lindesk_changes::Plan shape.
            Ok(json!({"scope": "system", "ops": [], "risk": "medium", "diff": "",
                      "rejections": [{"index": 0, "rule": "out_of_range", "message": "vm.swappiness=500 outside 0–200"}]})
            .to_string())
        }
        _ => Ok("{}".into()),
    });
    let h = harness(d).await;
    let ops = json!([{"op": "sysctl", "key": "vm.swappiness", "value": "500"}]);
    let r = h.call("plan_change", json!({"scope": "system", "ops": ops, "description": "x"})).await;
    assert!(is_error(&r));
    assert!(text(&r).contains("op 0: vm.swappiness=500 outside 0–200"), "{}", text(&r));
    let r = h.call("apply_change", json!({"scope": "system", "ops": ops, "description": "x"})).await;
    assert!(is_error(&r));
    assert_eq!(data(&r)["error"], "rejected");
    assert!(text(&r).starts_with("apply_change: rejected: vm.swappiness=500 outside 0–200"), "{}", text(&r));

    // Scope mistakes are caught before the daemon is asked.
    let n = h.daemon.calls().len();
    let r = h.call("plan_change", json!({"scope": "user", "ops": ops, "description": "x"})).await;
    assert!(is_error(&r) && text(&r).contains("needs scope=system"), "{r:?}");
    assert_eq!(h.daemon.calls().len(), n);
}

#[tokio::test]
async fn plan_ok_is_not_an_error() {
    let d = FakeDaemon::new(|_, _| {
        Ok(json!({"risk": "low", "diff": "-profile = \"auto\"\n+profile = \"latency\"", "rejections": []}).to_string())
    });
    let h = harness(d).await;
    let r = h
        .call("plan_change", json!({"scope": "user", "ops": [{"op": "profile_use", "scope": "user", "name": "latency"}], "description": "x"}))
        .await;
    assert!(!is_error(&r), "{r:?}");
    assert!(text(&r).contains("Plan OK") && text(&r).contains("+profile = \"latency\""), "{}", text(&r));
}

#[tokio::test]
async fn query_rejects_non_select() {
    let h = harness(FakeDaemon::absent()).await;
    for sql in ["DELETE FROM events", "COPY events TO '/tmp/x'", "select 1; select 2"] {
        let r = h.call("query", json!({"sql": sql})).await;
        assert!(is_error(&r), "{sql}");
        assert!(text(&r).starts_with("rejected"), "{sql}: {}", text(&r));
    }
    let r = h.call("query", json!({"sql": "select 1", "limit": 5000})).await;
    assert!(is_error(&r));
    assert!(h.local.queries.lock().unwrap().is_empty());

    let r = h.call("query", json!({"sql": "SELECT x FROM t", "limit": 2})).await;
    assert!(!is_error(&r), "{r:?}");
    assert_eq!(data(&r)["row_count"], 2);
    assert_eq!(data(&r)["truncated"], true);
    assert!(h.local.queries.lock().unwrap()[0].ends_with("LIMIT 3"));
}

#[tokio::test]
async fn recommendations_rank_fixture_inputs() {
    let h = harness(FakeDaemon::absent()).await;
    let r = h.call("recommendations", json!({})).await;
    assert!(!is_error(&r), "{r:?}");
    let recs = data(&r)["recommendations"].as_array().unwrap();
    let ids: Vec<&str> = recs.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["telemetry.red-time", "swap.swap-on-a-hard-disk", "latency.storage.sda", "doctor.io-latency"]);
    assert_eq!(recs[1]["proposed_ops"], json!([{"op": "sysctl", "key": "vm.swappiness", "value": "10"}]));
    assert_eq!(recs[1]["scope"], "system");
    assert_eq!(recs[1]["risk"], "medium");
    assert_eq!(recs[2]["proposed_ops"][0]["scheduler"], "bfq");
    assert_eq!(recs[0]["proposed_ops"][0]["name"], "latency");
    assert!(text(&r).starts_with("1. Under heavy pressure (RED) for 25 min"), "{}", text(&r));
}

#[tokio::test]
async fn without_telemetry_query_fails_clearly_and_recommendations_skip_it() {
    let h = harness_with(FakeDaemon::absent(), FakeLocal { no_telemetry: true, ..Default::default() }).await;
    let r = h.call("query", json!({"sql": "select 1"})).await;
    assert!(is_error(&r));
    assert_eq!(data(&r)["error"], "telemetry_not_installed");
    assert!(text(&r).starts_with("telemetry isn't installed: sudo apt install lindesk-telemetry"), "{}", text(&r));

    let r = h.call("recommendations", json!({})).await;
    assert!(!is_error(&r), "{r:?}");
    assert_eq!(data(&r)["sources"]["telemetry"], "not installed");
    let ids: Vec<&str> =
        data(&r)["recommendations"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["swap.swap-on-a-hard-disk", "latency.storage.sda", "doctor.io-latency"]);
    assert_eq!(data(&r)["errors"][0].as_str(), Some(crate::local::NOT_INSTALLED));
}

#[tokio::test]
async fn tools_have_annotations() {
    let h = harness(FakeDaemon::absent()).await;
    let tools = h.client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    for n in [
        "status",
        "doctor",
        "hardware",
        "latency_map",
        "swap_analysis",
        "explain",
        "query",
        "list_plugins",
        "list_profiles",
        "list_changes",
        "get_change",
        "recent_events",
        "recommendations",
        "plan_change",
        "apply_change",
        "confirm_change",
        "revert_change",
        "set_profile",
        "calibrate",
        "pause",
        "resume",
    ] {
        assert!(names.contains(&n), "missing {n}: {names:?}");
    }
    for t in &tools {
        let a = t.annotations.as_ref().unwrap_or_else(|| panic!("{} has no annotations", t.name));
        assert_eq!(a.open_world_hint, Some(false), "{}", t.name);
        assert!(
            a.read_only_hint.is_some() && a.destructive_hint.is_some() && a.idempotent_hint.is_some(),
            "{}",
            t.name
        );
        let mutating =
            ["apply_change", "confirm_change", "revert_change", "set_profile", "calibrate", "pause", "resume"]
                .contains(&t.name.as_ref());
        assert_eq!(a.read_only_hint, Some(!mutating), "{}", t.name);
        assert!(t.description.as_ref().is_some_and(|d| d.len() > 80), "{} needs a real description", t.name);
    }
    let apply = tools.iter().find(|t| t.name == "apply_change").unwrap();
    let schema = serde_json::to_string(&apply.input_schema).unwrap();
    for needle in ["ttl_minutes", "sysctl", "io_scheduler", "class_override", "cpu_epp"] {
        assert!(schema.contains(needle), "apply_change schema lacks {needle}");
    }
}
