//! The MCP server: tools, resources and prompts.

use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, GetPromptResult, Implementation, ListResourceTemplatesResult, ListResourcesResult,
    PaginatedRequestParams, PromptMessage, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
    Resource, ResourceContents, ResourceTemplate, Role, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, prompt, prompt_handler, prompt_router, tool, tool_handler,
    tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use lindesk_ipc::ChangeEnvelope;
use lindesk_plugin_api::Hardware;

use crate::backend::{Backends, DaemonError, DaemonRequest, Local, daemon_name};
use crate::ops::{self, ChangeOp, Scope};
use crate::recommend::{self, Inputs, RedHour, Telemetry, Throttled};
use crate::{docs, sql};

/// Server instructions sent in `initialize`.
pub const INSTRUCTIONS: &str = "\
lindesk keeps a Linux desktop responsive (macOS-style QoS: the foreground app is protected, background work yields). \
Use this server to assess a machine and tune lindesk safely. Safety model: every change is a versioned, guardrailed \
changeset applied by the lindesk daemons (this server holds no privileges); changes made here auto-revert after their \
ttl unless you call confirm_change, and any changeset can be reverted later with revert_change. Always call \
plan_change before apply_change and show the user the plan; apply with a short ttl, observe with status/query, then \
confirm or revert. Prefer scope=user (your own session, no admin rights); scope=system (sysctls, I/O schedulers, \
cpufreq, the system config) may pop up an admin authentication dialog for the user. Start with the \
assess-responsiveness prompt or the recommendations tool. Read-only tools work without a running daemon.";

/// Tool argument structs. Field docs become the JSON Schema descriptions shown to the model.
#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct ScopeArg {
    /// `user` (default): your session's lindesk-session. `system`: the lindeskd broker.
    #[serde(default)]
    pub scope: Scope,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExplainArgs {
    /// What to explain: a pid (`1234` or `pid:1234`), a target (`system:docker.service`,
    /// `user/1000:app-firefox-1234.scope`, `cgroup:/user.slice/...`) or a bare unit name.
    pub target: String,
    /// Which daemon to ask; `user` (default) for your apps, `system` for system services.
    #[serde(default)]
    pub scope: Scope,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueryArgs {
    /// One read-only DuckDB statement: SELECT or WITH … SELECT. Tables: pressure, units, events,
    /// metrics, pressure_1m/_1h, units_1m/_1h, bench_runs, bench_samples (see lindesk://docs/telemetry-schema).
    /// `ts` is TIMESTAMPTZ (UTC), e.g. `WHERE ts > now() - INTERVAL 1 HOUR`.
    pub sql: String,
    /// Maximum rows to return, 1–1000 (default 200). Aggregate in SQL rather than fetching raw rows.
    #[serde(default)]
    pub limit: Option<u32>,
    /// `user` (default): your own lake. `system`: lindeskd's lake (needs membership in the `lindesk` group).
    #[serde(default)]
    pub scope: Scope,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListChangesArgs {
    #[serde(default)]
    pub scope: Scope,
    /// Newest N changesets (default 20, max 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChangeIdArgs {
    /// Scope the changeset belongs to (ids are per scope).
    #[serde(default)]
    pub scope: Scope,
    /// Changeset id (the `id` field returned by apply_change / list_changes).
    pub id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecentEventsArgs {
    /// How many events (default 50, max 500), newest last.
    #[serde(default)]
    pub n: Option<u32>,
    /// Only this kind: focus | interaction | class_set | level | action_applied | action_reverted | plugin_error | notice.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub scope: Scope,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecommendationsArgs {
    /// Include telemetry from the last 24 h (time at RED, most-throttled units). Default true.
    #[serde(default)]
    pub include_telemetry: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PlanArgs {
    /// `user` for config/profile/class ops on your session; `system` for sysctl, io_scheduler, zswap,
    /// cpu_epp, cpu_governor and the system config. All ops must belong to this scope.
    pub scope: Scope,
    /// 1–16 ChangeOps, e.g. `{"op":"sysctl","key":"vm.swappiness","value":"10"}` or
    /// `{"op":"profile_use","scope":"user","name":"latency"}`.
    pub ops: Vec<ChangeOp>,
    /// Why (one sentence, recorded in the change history).
    pub description: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApplyArgs {
    /// Same scope as in plan_change.
    pub scope: Scope,
    /// The ops exactly as planned.
    pub ops: Vec<ChangeOp>,
    /// Why (one sentence, recorded in the change history).
    pub description: String,
    /// Dead-man switch: minutes until the change auto-reverts unless confirmed (1–1440, default 10).
    #[serde(default)]
    pub ttl_minutes: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetProfileArgs {
    #[serde(default)]
    pub scope: Scope,
    /// Profile name from list_profiles (e.g. `latency`, `balanced`, `battery`, `gaming`), or `auto`.
    pub name: String,
    /// Minutes until auto-revert unless confirmed (1–1440, default 10).
    #[serde(default)]
    pub ttl_minutes: Option<u32>,
}

/// Longest pause an agent may ask for: a pause always ends by itself.
pub const MAX_PAUSE_MINUTES: u32 = 120;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PauseArgs {
    /// `user` (default): your session agent (your apps). `system`: lindeskd (needs admin).
    #[serde(default)]
    pub scope: Scope,
    /// Minutes until lindesk resumes by itself (1–120, default 10). There is no indefinite
    /// pause from MCP.
    #[serde(default)]
    pub minutes: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResumeArgs {
    #[serde(default)]
    pub scope: Scope,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CalibrateArgs {
    /// A shorter, less precise run (a few seconds instead of ≈15 s).
    #[serde(default)]
    pub quick: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AssessArgs {
    /// Optional: what the user complained about (e.g. "typing lags while compiling").
    #[serde(default)]
    pub symptom: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SlowdownArgs {
    /// The app that feels slow (name, desktop id or pid), e.g. `firefox`.
    pub app: String,
    /// Optional: when it happened (e.g. "around 14:30", "during the last build").
    #[serde(default)]
    pub when: Option<String>,
}

pub const DEFAULT_TTL_MIN: u32 = 10;
pub const MAX_TTL_MIN: u32 = 1440;

// ---------------------------------------------------------------------------------------
// results

fn compact(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

fn object(v: Value, key: &str) -> Value {
    if v.is_object() { v } else { json!({ key: v }) }
}

/// A successful result: summary text + the JSON (structured and as text).
pub fn success(summary: impl Into<String>, data: Value) -> CallToolResult {
    let data = object(data, "result");
    let mut r = CallToolResult::structured(data.clone());
    r.content = vec![ContentBlock::text(summary), ContentBlock::text(compact(&data))];
    r
}

/// A tool-level failure (`is_error = true`): not a protocol error.
pub fn failure(summary: impl Into<String>, data: Value) -> CallToolResult {
    let data = object(data, "result");
    let mut r = CallToolResult::structured_error(data.clone());
    r.content = vec![ContentBlock::text(summary), ContentBlock::text(compact(&data))];
    r
}

fn daemon_failure(e: &DaemonError, what: &str) -> CallToolResult {
    let remedies = e.remedies();
    let mut summary = format!("{what}: {e}");
    if !remedies.is_empty() {
        summary.push_str("\nWhat to do:");
        for r in &remedies {
            summary.push_str("\n- ");
            summary.push_str(r);
        }
    }
    failure(summary, json!({ "error": e.kind(), "scope": e.scope(), "message": e.to_string(), "remedies": remedies }))
}

fn source(scope: Scope) -> String {
    format!("daemon:{}", daemon_name(scope))
}

fn first_lines(s: &str, n: usize) -> String {
    s.lines().take(n).collect::<Vec<_>>().join("\n")
}

async fn blocking<T: Send + 'static>(
    local: &std::sync::Arc<dyn Local>,
    f: impl FnOnce(&dyn Local) -> T + Send + 'static,
) -> T {
    let local = local.clone();
    match tokio::task::spawn_blocking(move || f(local.as_ref())).await {
        Ok(v) => v,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

// ---------------------------------------------------------------------------------------
// server

/// The lindesk MCP server. Cheap to clone; one per connection.
#[derive(Clone)]
pub struct LindeskMcp {
    backends: Backends,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
}

impl LindeskMcp {
    pub fn new(backends: Backends) -> Self {
        LindeskMcp { backends, tool_router: Self::tool_router(), prompt_router: Self::prompt_router() }
    }

    async fn daemon_json(&self, scope: Scope, req: DaemonRequest) -> Result<Value, DaemonError> {
        let member = req.member();
        let s = self.backends.daemon.call(scope, req).await?;
        if s.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&s)
            .map_err(|e| DaemonError::Failed { scope, message: format!("{member} returned invalid JSON: {e}") })
    }

    /// Ask the session daemon, then the broker; `None` when neither answers.
    async fn daemon_any(&self, req: DaemonRequest) -> (Option<(Scope, Value)>, Vec<String>) {
        let mut errors = Vec::new();
        for scope in [Scope::User, Scope::System] {
            match self.daemon_json(scope, req.clone()).await {
                Ok(v) if !v.is_null() => return (Some((scope, v)), errors),
                Ok(_) => {}
                Err(e) => errors.push(e.to_string()),
            }
        }
        (None, errors)
    }

    async fn hardware_value(&self) -> Result<(String, Value, Vec<String>), String> {
        let (found, errors) = self.daemon_any(DaemonRequest::GetHardware).await;
        if let Some((scope, v)) = found {
            return Ok((source(scope), v, errors));
        }
        let v = blocking(&self.backends.local, |l| l.hardware()).await.map_err(|e| e.to_string())?;
        Ok(("local".into(), v, errors))
    }

    async fn swap_value(&self) -> Result<(String, Value, Vec<String>), String> {
        let (found, errors) = self.daemon_any(DaemonRequest::SwapReport).await;
        if let Some((scope, v)) = found {
            return Ok((source(scope), v, errors));
        }
        let v = blocking(&self.backends.local, |l| l.swap()).await.map_err(|e| e.to_string())?;
        Ok(("local".into(), v, errors))
    }

    async fn status_value(&self, scope: Scope) -> Result<(String, Value, Option<String>), String> {
        match self.daemon_json(scope, DaemonRequest::Status).await {
            Ok(v) => Ok((source(scope), v, None)),
            Err(e) => {
                let v = blocking(&self.backends.local, |l| l.status()).await.map_err(|e| e.to_string())?;
                Ok(("local".into(), v, Some(e.to_string())))
            }
        }
    }

    /// Telemetry evidence for `recommendations`: `None` (and one note) when the optional
    /// lindesk-telemetry package isn't installed.
    async fn telemetry(&self) -> (Option<Telemetry>, Vec<String>) {
        let mut errors = Vec::new();
        for scope in [Scope::User, Scope::System] {
            let red = blocking(&self.backends.local, move |l| {
                l.query(scope, &sql::guard(recommend::RED_BY_HOUR_SQL, 48).expect("valid built-in SQL"))
            })
            .await;
            let thr = blocking(&self.backends.local, move |l| {
                l.query(scope, &sql::guard(recommend::TOP_THROTTLED_SQL, 5).expect("valid built-in SQL"))
            })
            .await;
            match (red, thr) {
                (Ok(red), Ok(thr)) => {
                    let red_by_hour: Vec<RedHour> = serde_json::from_value(red).unwrap_or_default();
                    let throttled: Vec<Throttled> = serde_json::from_value(thr).unwrap_or_default();
                    if red_by_hour.is_empty() && throttled.is_empty() && scope == Scope::User {
                        // Nothing in the user lake (e.g. connected mode writes to the system lake).
                        continue;
                    }
                    return (Some(Telemetry { red_by_hour, throttled }), errors);
                }
                (Err(e), _) | (_, Err(e)) if e == crate::local::NOT_INSTALLED => return (None, vec![e]),
                (Err(e), _) | (_, Err(e)) => errors.push(format!("{} lake: {e}", scope.as_str())),
            }
        }
        (Some(Telemetry::default()), errors)
    }

    fn envelope(ctx: &RequestContext<RoleServer>, ops: &[ChangeOp]) -> String {
        ChangeEnvelope {
            origin: "mcp".into(),
            client: ctx.client_info().map(|i| i.name),
            ops: ops.iter().map(|o| serde_json::to_value(o).unwrap_or(Value::Null)).collect(),
        }
        .to_json()
    }

    async fn apply(
        &self,
        ctx: &RequestContext<RoleServer>,
        scope: Scope,
        ops: &[ChangeOp],
        description: &str,
        ttl_minutes: Option<u32>,
    ) -> CallToolResult {
        let ttl = ttl_minutes.unwrap_or(DEFAULT_TTL_MIN);
        if !(1..=MAX_TTL_MIN).contains(&ttl) {
            return failure(
                format!(
                    "rejected: ttl_minutes={ttl} outside 1–{MAX_TTL_MIN}; MCP changes always auto-revert unless confirmed"
                ),
                json!({"error": "rejected", "rejections": [format!("ttl_minutes={ttl} outside 1–{MAX_TTL_MIN}")]}),
            );
        }
        if description.trim().is_empty() {
            return failure(
                "rejected: description is empty; say why the change is made (it is recorded in the history)",
                json!({"error": "rejected", "rejections": ["description is empty"]}),
            );
        }
        let pre = ops::precheck(scope, ops);
        if !pre.is_empty() {
            return failure(
                format!("Changeset rejected before reaching the daemon:\n- {}", pre.join("\n- ")),
                json!({"error": "rejected", "rejections": pre, "applied": false}),
            );
        }
        let req = DaemonRequest::ApplyChange {
            ops_json: Self::envelope(ctx, ops),
            description: description.to_string(),
            ttl_secs: ttl * 60,
        };
        match self.daemon_json(scope, req).await {
            Ok(cs) => {
                let id = cs.get("id").cloned().unwrap_or(Value::Null);
                let status = cs.get("status").and_then(Value::as_str).unwrap_or("applied").to_string();
                let failed = matches!(status.as_str(), "failed" | "reverted");
                let summary = if failed {
                    format!(
                        "Changeset {id} ended as '{status}' in scope={}: nothing stays changed. See the changeset for the reason.",
                        scope.as_str()
                    )
                } else {
                    format!(
                        "Applied changeset {id} in scope={} (status {status}). It auto-reverts in {ttl} min unless you call \
                         confirm_change {{scope: \"{}\", id: {id}}}. Observe first (status, query), then confirm or revert_change.",
                        scope.as_str(),
                        scope.as_str()
                    )
                };
                let data = json!({ "scope": scope, "ttl_minutes": ttl, "changeset": cs });
                if failed { failure(summary, data) } else { success(summary, data) }
            }
            Err(e) => daemon_failure(&e, "apply_change"),
        }
    }
}

/// A `lindesk_changes::Rejection` (`{index, rule, message}`) or a plain string, as text.
fn rejection_text(r: &Value) -> String {
    if let Some(s) = r.as_str() {
        return s.to_string();
    }
    let msg = r.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| compact(r));
    match r.get("index").and_then(Value::as_u64) {
        Some(i) => format!("op {i}: {msg}"),
        None => msg,
    }
}

fn hardware_summary(v: &Value) -> String {
    match serde_json::from_value::<Hardware>(v.clone()) {
        Ok(hw) => {
            let mut s = lindesk_sys::hardware::summary_line(&hw);
            s.push_str(match &hw.latency {
                Some(l) if l.noisy => "\nlatency map: present (noisy calibration)",
                Some(_) => "\nlatency map: present",
                None => "\nlatency map: none (call calibrate)",
            });
            s
        }
        Err(_) => "hardware profile".into(),
    }
}

fn kind_matches(ev: &Value, kind: &str) -> bool {
    ev.get("kind").and_then(Value::as_str) == Some(kind)
}

#[tool_router]
impl LindeskMcp {
    #[tool(
        description = "Current lindesk status: health level (green/yellow/red), PSI pressure (system and the \
            foreground app; `some`/`full` are stall ratios 0..1, `*_avg10` kernel percentages), which apps are \
            protected or yielding, daemon mode, plugins and requirement warnings. Use it first, and again after \
            apply_change to observe the effect. Falls back to a local probe (no managed state) when the daemon \
            isn't running; `source` says which.",
        annotations(
            title = "Status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn status(&self, Parameters(a): Parameters<ScopeArg>) -> Result<CallToolResult, McpError> {
        match self.status_value(a.scope).await {
            Ok((src, v, err)) => {
                let text = v.get("text").and_then(Value::as_str).map(|t| first_lines(t, 25)).unwrap_or_default();
                let mut summary = format!("source: {src}\n{text}");
                if let Some(e) = &err {
                    summary.push_str(&format!("\n(daemon unavailable: {e})"));
                }
                Ok(success(summary, json!({ "source": src, "daemon_error": err, "status": v })))
            }
            Err(e) => Ok(failure(format!("status failed: {e}"), json!({"error": e}))),
        }
    }

    #[tool(
        description = "What this machine supports, as checks with concrete remedies: cgroup v2 controllers, PSI, \
            uclamp, io.latency/iocost/BFQ, writeback, systemd delegation, desktop session, conflicting daemons and \
            hardware. Severity ok|info|warn|error; overall support level FULL/PARTIAL/MINIMAL. Always local (no \
            daemon needed). Use it when status shows missing features or before recommending kernel/boot changes.",
        annotations(
            title = "Doctor",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn doctor(&self) -> Result<CallToolResult, McpError> {
        match blocking(&self.backends.local, |l| l.doctor()).await {
            Ok(v) => {
                let mut lines =
                    vec![format!("support level: {}", v.get("level").and_then(Value::as_str).unwrap_or("?"))];
                for f in v.get("findings").and_then(Value::as_array).into_iter().flatten() {
                    let sev = f.get("severity").and_then(Value::as_str).unwrap_or("");
                    if sev == "warn" || sev == "error" {
                        lines.push(format!(
                            "[{sev}] {}: {}{}",
                            f.get("check").and_then(Value::as_str).unwrap_or(""),
                            f.get("status").and_then(Value::as_str).unwrap_or(""),
                            f.get("remedy").and_then(Value::as_str).map(|r| format!(" → {r}")).unwrap_or_default()
                        ));
                    }
                }
                if lines.len() == 1 {
                    lines.push("no warnings".into());
                }
                Ok(success(lines.join("\n"), json!({"source": "local", "doctor": v})))
            }
            Err(e) => Ok(failure(format!("doctor failed: {e}"), json!({"error": e.to_string()}))),
        }
    }

    #[tool(
        description = "Hardware profile: CPU arch/vendor/model, cores/threads/SMT, hybrid P/E (or big.LITTLE) CPU \
            lists, cpufreq driver/governor/EPP and their available values (needed for cpu_epp/cpu_governor ops), \
            NUMA, RAM and swap (bytes), compressed swap, managed block devices (kind, I/O scheduler, nominal \
            bandwidth in bytes/s), power source, chassis, virtualization, and the measured latency map if \
            calibrated. Prefers the daemon's copy; falls back to local detection.",
        annotations(
            title = "Hardware",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn hardware(&self) -> Result<CallToolResult, McpError> {
        match self.hardware_value().await {
            Ok((src, v, _)) => {
                Ok(success(format!("source: {src}\n{}", hardware_summary(&v)), json!({"source": src, "hardware": v})))
            }
            Err(e) => Ok(failure(format!("hardware detection failed: {e}"), json!({"error": e}))),
        }
    }

    #[tool(
        description = "The measured latency baseline from the last calibration: per-CPU timer-wakeup p50/p99 (µs) \
            and relative single-thread performance (1.0 = fastest core), memory load latency (ns), copy bandwidth \
            (bytes/s) and page-fault cost (µs), per-device 4 KiB random-read p50/p99 (µs) and sequential read/write \
            (bytes/s), swap page-in estimates (µs). `noisy: true` means the machine was busy during calibration. \
            Returns latency=null when never calibrated: then suggest the calibrate tool.",
        annotations(
            title = "Latency map",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn latency_map(&self) -> Result<CallToolResult, McpError> {
        match self.hardware_value().await {
            Ok((src, v, _)) => {
                let lat = v.get("latency").cloned().unwrap_or(Value::Null);
                let summary = if lat.is_null() {
                    "No latency map yet: the machine was never calibrated. Call `calibrate` (system scope, ≈15 s, on an idle machine).".to_string()
                } else {
                    let storage: Vec<String> = lat
                        .get("storage")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(|s| {
                            format!(
                                "{}: 4k read p50 {} µs / p99 {} µs",
                                s.get("dev").and_then(Value::as_str).unwrap_or("?"),
                                s.get("read_4k_p50_us").and_then(Value::as_f64).unwrap_or(0.0).round(),
                                s.get("read_4k_p99_us").and_then(Value::as_f64).unwrap_or(0.0).round()
                            )
                        })
                        .collect();
                    format!(
                        "source: {src}; noisy: {}; {} CPUs measured; {}",
                        lat.get("noisy").and_then(Value::as_bool).unwrap_or(false),
                        lat.get("cpus").and_then(Value::as_array).map_or(0, Vec::len),
                        if storage.is_empty() { "no storage measured".into() } else { storage.join("; ") }
                    )
                };
                Ok(success(summary, json!({"source": src, "latency": lat})))
            }
            Err(e) => Ok(failure(format!("reading the latency map failed: {e}"), json!({"error": e}))),
        }
    }

    #[tool(
        description = "Swap and memory-reclaim analysis: swap devices (zram, partition, file) with backing storage \
            kind, zswap state, vm.swappiness / page-cluster / watermark_scale_factor / min_free_kbytes, MGLRU, \
            swap-in/out rates (pages/s, sampled over 0.5 s locally), top cgroups by swapped bytes, and findings \
            with proposed ChangeOps (pass them to plan_change, scope=system). Bytes are bytes.",
        annotations(
            title = "Swap analysis",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn swap_analysis(&self) -> Result<CallToolResult, McpError> {
        match self.swap_value().await {
            Ok((src, v, _)) => {
                let mut lines = vec![format!(
                    "source: {src}; swappiness {}; page-cluster {}; {} swap device(s)",
                    v.pointer("/vm/swappiness").map_or("?".into(), Value::to_string),
                    v.pointer("/vm/page_cluster").map_or("?".into(), Value::to_string),
                    v.get("devices").and_then(Value::as_array).map_or(0, Vec::len)
                )];
                for f in v.get("findings").and_then(Value::as_array).into_iter().flatten() {
                    lines.push(format!(
                        "[{}] {} ({} proposed op(s))",
                        f.get("severity").and_then(Value::as_str).unwrap_or(""),
                        f.get("title").and_then(Value::as_str).unwrap_or(""),
                        f.get("proposed_ops").and_then(Value::as_array).map_or(0, Vec::len)
                    ));
                }
                Ok(success(lines.join("\n"), json!({"source": src, "swap": v})))
            }
            Err(e) => Ok(failure(format!("swap analysis failed: {e}"), json!({"error": e}))),
        }
    }

    #[tool(
        description = "Explain how lindesk treats a process or unit: its target, QoS class (critical, interactive, \
            normal, background, batch) and why, whether it is foreground, and every setting lindesk applied \
            (knob, value, original value, which plugin/policy and why, since when). Use it when the user asks why \
            an app is slow or throttled. Without a daemon only basic process info is available.",
        annotations(
            title = "Explain",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn explain(&self, Parameters(a): Parameters<ExplainArgs>) -> Result<CallToolResult, McpError> {
        match self.daemon_json(a.scope, DaemonRequest::Explain(a.target.clone())).await {
            Ok(v) => {
                let text = v.get("text").and_then(Value::as_str).map(|t| first_lines(t, 30)).unwrap_or_default();
                Ok(success(
                    format!("source: {}\n{text}", source(a.scope)),
                    json!({"source": source(a.scope), "explanation": v}),
                ))
            }
            Err(DaemonError::Rejected { message, .. }) => Ok(failure(
                format!("explain: '{}' is not a pid, target or unit name ({message})", a.target),
                json!({"error": "invalid_target", "message": message}),
            )),
            Err(e) => {
                let what = a.target.clone();
                match blocking(&self.backends.local, move |l| l.explain(&what)).await {
                    Ok(v) => Ok(success(
                        format!(
                            "source: local (daemon unavailable: {e}); nothing is managed, so there is nothing applied to explain"
                        ),
                        json!({"source": "local", "daemon_error": e.to_string(), "explanation": v}),
                    )),
                    Err(le) => {
                        Ok(failure(format!("explain failed: {e}; local: {le}"), json!({"error": le.to_string()})))
                    }
                }
            }
        }
    }

    #[tool(
        description = "Run one read-only SQL statement (DuckDB dialect; SELECT or WITH only) over lindesk's \
            telemetry lake. Tables: pressure (PSI samples: scope, resource cpu|io|memory, some_avg10/full_avg10 \
            percent), units (per-unit CPU µs, I/O bytes, memory bytes per interval, class, foreground), events \
            (kind, unit, reason, payload JSON), metrics, and rollups pressure_1m/_1h, units_1m/_1h. ts is UTC \
            TIMESTAMPTZ. Rows are capped (limit ≤ 1000, default 200; `truncated` tells you). Prefer aggregates, e.g. \
            `SELECT date_trunc('minute', ts) m, max(some_avg10) FROM pressure WHERE resource='memory' AND \
            scope='system' AND ts > now() - INTERVAL 1 HOUR GROUP BY 1 ORDER BY 1`. See lindesk://docs/telemetry-schema.",
        annotations(
            title = "Query telemetry",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn query(&self, Parameters(a): Parameters<QueryArgs>) -> Result<CallToolResult, McpError> {
        let limit = a.limit.unwrap_or(sql::DEFAULT_ROWS);
        let wrapped = match sql::guard(&a.sql, limit) {
            Ok(w) => w,
            Err(e) => return Ok(failure(e.clone(), json!({"error": "rejected", "message": e}))),
        };
        let scope = a.scope;
        match blocking(&self.backends.local, move |l| l.query(scope, &wrapped)).await {
            Ok(Value::Array(mut rows)) => {
                let truncated = rows.len() > limit as usize;
                rows.truncate(limit as usize);
                let n = rows.len();
                let summary = format!(
                    "{n} row(s){} from the {} lake",
                    if truncated {
                        format!(" (truncated at limit {limit}; aggregate or raise limit ≤ 1000)")
                    } else {
                        String::new()
                    },
                    scope.as_str()
                );
                Ok(success(summary, json!({"scope": scope, "rows": rows, "row_count": n, "truncated": truncated})))
            }
            Ok(other) => {
                Ok(success("1 result", json!({"scope": scope, "rows": [other], "row_count": 1, "truncated": false})))
            }
            Err(e) if e == crate::local::NOT_INSTALLED => {
                Ok(failure(e.clone(), json!({"error": "telemetry_not_installed", "message": e})))
            }
            Err(e) => Ok(failure(format!("query failed: {e}"), json!({"error": "query_failed", "message": e}))),
        }
    }

    #[tool(
        description = "Plugins: the live pipeline from the daemon (built-in and external sensors, adapters, \
            classifiers, policies, actuators, sinks with health, restarts, last error) plus external plugins \
            installed on disk (manifest, kinds, enabled). Use it when status reports plugin errors.",
        annotations(
            title = "List plugins",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn list_plugins(&self, Parameters(a): Parameters<ScopeArg>) -> Result<CallToolResult, McpError> {
        let live = match self.daemon_json(a.scope, DaemonRequest::Status).await {
            Ok(v) => v.pointer("/status/plugins").cloned(),
            Err(_) => None,
        };
        let installed = blocking(&self.backends.local, |l| l.plugins()).await;
        let installed = match installed {
            Ok(v) => v,
            Err(e) => json!({"error": e.to_string()}),
        };
        let n_live = live.as_ref().and_then(Value::as_array).map(Vec::len);
        let n_ext = installed.get("external").and_then(Value::as_array).map_or(0, Vec::len);
        let summary = match n_live {
            Some(n) => {
                format!("{n} plugin(s) running in {}; {n_ext} external plugin(s) installed", daemon_name(a.scope))
            }
            None => format!(
                "{} not reachable (no live plugin health); {n_ext} external plugin(s) installed",
                daemon_name(a.scope)
            ),
        };
        Ok(success(summary, json!({"live": live, "installed": installed})))
    }

    #[tool(
        description = "Profiles (named overlays on lindesk.toml such as balanced, latency, battery, throughput, \
            gaming, arm-eas, x86-hybrid, hdd): description, whether active, whether its auto-selection rules match \
            this machine now, and why. Switch with set_profile.",
        annotations(
            title = "List profiles",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn list_profiles(&self, Parameters(a): Parameters<ScopeArg>) -> Result<CallToolResult, McpError> {
        let (src, v, err) = match self.daemon_json(a.scope, DaemonRequest::ListProfiles).await {
            Ok(v) => (source(a.scope), v, None),
            Err(e) => match blocking(&self.backends.local, |l| l.profiles()).await {
                Ok(v) => ("local".into(), v, Some(e.to_string())),
                Err(le) => {
                    return Ok(failure(
                        format!("list_profiles failed: {e}; local: {le}"),
                        json!({"error": le.to_string()}),
                    ));
                }
            },
        };
        let list = v.get("profiles").cloned().unwrap_or_else(|| v.clone());
        let lines: Vec<String> = list
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| {
                format!(
                    "{}{}{} — {}",
                    p.get("name").and_then(Value::as_str).unwrap_or("?"),
                    if p.get("active").and_then(Value::as_bool).unwrap_or(false) { " (active)" } else { "" },
                    if p.get("matches_now").and_then(Value::as_bool).unwrap_or(false) { " (matches now)" } else { "" },
                    p.get("description").and_then(Value::as_str).unwrap_or("")
                )
            })
            .collect();
        Ok(success(
            format!("source: {src}\n{}", lines.join("\n")),
            json!({"source": src, "daemon_error": err, "profiles": v}),
        ))
    }

    #[tool(
        description = "Change history (newest first): versioned changesets with id, time, author/origin, \
            description, ops with previous and applied values, status (planned, applying, applied, confirmed, \
            reverted, failed) and ttl_until (auto-revert deadline for unconfirmed changes). Ids are per scope.",
        annotations(
            title = "List changes",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn list_changes(&self, Parameters(a): Parameters<ListChangesArgs>) -> Result<CallToolResult, McpError> {
        let limit = a.limit.unwrap_or(20).clamp(1, 200);
        let scope = a.scope;
        let (src, v) = match self.daemon_json(scope, DaemonRequest::ListChanges(limit)).await {
            Ok(v) => (source(scope), v),
            Err(e) => match blocking(&self.backends.local, move |l| l.changes(scope, limit)).await {
                Ok(v) => (format!("local (daemon unavailable: {e})"), v),
                Err(le) => {
                    return Ok(failure(
                        format!("list_changes failed: {e}; local: {le}"),
                        json!({"error": le.to_string()}),
                    ));
                }
            },
        };
        let lines: Vec<String> = v
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| {
                format!(
                    "#{} [{}] {}",
                    c.get("id").map_or("?".into(), Value::to_string),
                    c.get("status").and_then(Value::as_str).unwrap_or("?"),
                    c.get("description").and_then(Value::as_str).unwrap_or("")
                )
            })
            .collect();
        let summary = if lines.is_empty() {
            format!("source: {src}\nno changesets in scope={}", scope.as_str())
        } else {
            format!("source: {src}\n{}", lines.join("\n"))
        };
        Ok(success(summary, json!({"source": src, "scope": scope, "changes": v})))
    }

    #[tool(
        description = "One changeset by id: ops with previous/applied values, the config diff, status, ttl_until \
            and author. Use it to check whether an applied change was confirmed or auto-reverted.",
        annotations(
            title = "Get change",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn get_change(&self, Parameters(a): Parameters<ChangeIdArgs>) -> Result<CallToolResult, McpError> {
        let scope = a.scope;
        let id = a.id;
        match self.daemon_json(scope, DaemonRequest::GetChange(id)).await {
            Ok(v) => Ok(success(
                format!(
                    "changeset {id} ({}): {}",
                    scope.as_str(),
                    v.get("status").and_then(Value::as_str).unwrap_or("?")
                ),
                json!({"source": source(scope), "change": v}),
            )),
            Err(e @ DaemonError::Rejected { .. }) | Err(e @ DaemonError::Failed { .. }) => {
                Ok(daemon_failure(&e, "get_change"))
            }
            Err(e) => match blocking(&self.backends.local, move |l| l.change(scope, id)).await {
                Ok(Some(v)) => Ok(success(
                    format!(
                        "changeset {id} ({}, read from disk): {}",
                        scope.as_str(),
                        v.get("status").and_then(Value::as_str).unwrap_or("?")
                    ),
                    json!({"source": "local", "change": v}),
                )),
                Ok(None) => Ok(failure(
                    format!("no changeset {id} in scope={} ({e})", scope.as_str()),
                    json!({"error": "not_found"}),
                )),
                Err(le) => {
                    Ok(failure(format!("get_change failed: {e}; local: {le}"), json!({"error": le.to_string()})))
                }
            },
        }
    }

    #[tool(
        description = "Recent lindesk events (oldest first): focus changes, interactions, class changes, level \
            changes (green/yellow/red), actions applied/reverted (knob, value, which policy and why), plugin errors \
            and notices (profile.changed, change.applied, change.reverted, ...). Optional `kind` filter. From the \
            daemon's in-memory ring, else from the telemetry lake.",
        annotations(
            title = "Recent events",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn recent_events(&self, Parameters(a): Parameters<RecentEventsArgs>) -> Result<CallToolResult, McpError> {
        let n = a.n.unwrap_or(50).clamp(1, 500);
        let ask = if a.kind.is_some() { 500 } else { n };
        match self.daemon_json(a.scope, DaemonRequest::RecentEvents(ask)).await {
            Ok(v) => {
                let mut events: Vec<Value> = v.as_array().cloned().unwrap_or_default();
                if let Some(k) = &a.kind {
                    events.retain(|e| kind_matches(e, k));
                }
                let skip = events.len().saturating_sub(n as usize);
                let events: Vec<Value> = events.into_iter().skip(skip).collect();
                Ok(success(
                    format!("{} event(s) from {}", events.len(), daemon_name(a.scope)),
                    json!({"source": source(a.scope), "events": events}),
                ))
            }
            Err(e) => {
                let kind_filter = match &a.kind {
                    Some(k) if k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
                        format!("WHERE kind = '{k}'")
                    }
                    Some(k) => {
                        return Ok(failure(
                            format!("rejected: unknown event kind '{k}'"),
                            json!({"error": "rejected"}),
                        ));
                    }
                    None => String::new(),
                };
                let q = format!(
                    "SELECT * FROM (SELECT ts, kind, unit, uid, app_id, pid, plugin, reason FROM events {kind_filter} ORDER BY ts DESC LIMIT {n}) ORDER BY ts"
                );
                let wrapped = sql::guard(&q, n).expect("valid built-in SQL");
                let scope = a.scope;
                match blocking(&self.backends.local, move |l| l.query(scope, &wrapped)).await {
                    Ok(rows) => Ok(success(
                        format!(
                            "{} event(s) from the {} telemetry lake ({} unavailable: {e})",
                            rows.as_array().map_or(0, Vec::len),
                            scope.as_str(),
                            daemon_name(scope)
                        ),
                        json!({"source": "local:lake", "daemon_error": e.to_string(), "events": rows}),
                    )),
                    Err(qe) => Ok(failure(
                        format!("recent_events: daemon unavailable ({e}) and the lake could not be read ({qe})"),
                        json!({"error": "unavailable", "daemon_error": e.to_string(), "lake_error": qe}),
                    )),
                }
            }
        }
    }

    #[tool(
        description = "Ranked, ready-to-plan recommendations for this machine. Aggregates doctor findings, swap \
            findings, requirement warnings, latency-map anomalies (e.g. a disk much slower than its kind, noisy \
            calibration, slow wakeups on a power-saving EPP) and the last 24 h of telemetry (minutes at RED per \
            hour, most-throttled units). Each item: id, title, why, evidence, proposed_ops (ChangeOp JSON to pass \
            to plan_change unchanged), risk (none|low|medium|high) and scope. Items without ops are advice the user \
            must act on. Explain the top items to the user before planning anything.",
        annotations(
            title = "Recommendations",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn recommendations(
        &self,
        Parameters(a): Parameters<RecommendationsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut sources = serde_json::Map::new();
        let mut errors: Vec<String> = Vec::new();
        let mut inputs = Inputs::default();

        match blocking(&self.backends.local, |l| l.doctor()).await {
            Ok(v) => {
                inputs.doctor =
                    serde_json::from_value(v.get("findings").cloned().unwrap_or_default()).unwrap_or_default();
                sources.insert("doctor".into(), json!("local"));
            }
            Err(e) => errors.push(format!("doctor: {e}")),
        }
        match self.swap_value().await {
            Ok((src, v, _)) => {
                inputs.swap =
                    serde_json::from_value(v.get("findings").cloned().unwrap_or_default()).unwrap_or_default();
                sources.insert("swap".into(), json!(src));
            }
            Err(e) => errors.push(format!("swap: {e}")),
        }
        match self.hardware_value().await {
            Ok((src, v, _)) => {
                inputs.hardware = serde_json::from_value(v).ok();
                sources.insert("hardware".into(), json!(src));
            }
            Err(e) => errors.push(format!("hardware: {e}")),
        }
        let reqs = match self.daemon_json(Scope::User, DaemonRequest::Status).await {
            Ok(v) if v.get("requirements").is_some() => {
                sources.insert("requirements".into(), json!(source(Scope::User)));
                Ok(v["requirements"].clone())
            }
            _ => {
                sources.insert("requirements".into(), json!("local"));
                blocking(&self.backends.local, |l| l.requirements()).await.map_err(|e| e.to_string())
            }
        };
        match reqs {
            Ok(v) => inputs.requirements = serde_json::from_value(v).unwrap_or_default(),
            Err(e) => errors.push(format!("requirements: {e}")),
        }
        match self.daemon_json(Scope::User, DaemonRequest::ListProfiles).await {
            Ok(v) => inputs.profiles = profile_names(&v),
            Err(_) => {
                if let Ok(v) = blocking(&self.backends.local, |l| l.profiles()).await {
                    inputs.profiles = profile_names(&v);
                }
            }
        }
        if a.include_telemetry.unwrap_or(true) {
            let (t, errs) = self.telemetry().await;
            sources.insert("telemetry".into(), json!(if t.is_some() { "lake" } else { "not installed" }));
            inputs.telemetry = t;
            errors.extend(errs);
        }

        let recs = recommend::recommend(&inputs);
        let mut lines = Vec::new();
        for (i, r) in recs.iter().enumerate() {
            lines.push(format!(
                "{}. {} [{}; risk {}; scope {}; {} op(s)]",
                i + 1,
                r.title,
                r.source,
                r.risk.as_str(),
                r.scope.as_str(),
                r.proposed_ops.len()
            ));
        }
        if lines.is_empty() {
            lines.push("Nothing to recommend: no warnings, no swap issues, no latency anomalies, no RED time.".into());
        }
        lines.push(
            "Next: explain the top items to the user, then plan_change the proposed_ops of the one they pick.".into(),
        );
        Ok(success(lines.join("\n"), json!({"recommendations": recs, "sources": sources, "errors": errors})))
    }

    #[tool(
        description = "Dry-run a changeset: the daemon validates every op against its guardrails (allow-listed \
            sysctls and ranges, schedulers/EPP/governors the hardware offers, config keys that exist, [safety] may \
            only be tightened, ≤16 ops, rate limits) and returns the risk (low/medium/high), a diff of what would \
            change (previous → new) and any rejections. No side effects. ALWAYS call this before apply_change and \
            show the user the diff. is_error=true when anything is rejected.",
        annotations(
            title = "Plan change",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn plan_change(
        &self,
        Parameters(a): Parameters<PlanArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let pre = ops::precheck(a.scope, &a.ops);
        if !pre.is_empty() {
            return Ok(failure(
                format!("Plan rejected before reaching the daemon:\n- {}", pre.join("\n- ")),
                json!({"error": "rejected", "rejections": pre}),
            ));
        }
        let req = DaemonRequest::PlanChange { ops_json: Self::envelope(&ctx, &a.ops) };
        match self.daemon_json(a.scope, req).await {
            Ok(plan) => {
                let rejections: Vec<String> = plan
                    .get("rejections")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(rejection_text)
                    .collect();
                let diff = plan.get("diff").and_then(Value::as_str).map(|d| first_lines(d, 40)).unwrap_or_default();
                let risk =
                    plan.get("risk").and_then(Value::as_str).unwrap_or(ops::max_risk(&a.ops).as_str()).to_string();
                let data = json!({"scope": a.scope, "description": a.description, "plan": plan});
                if rejections.is_empty() {
                    Ok(success(
                        format!(
                            "Plan OK in scope={}: {} op(s), risk {risk}. Show the user the diff; to proceed call apply_change \
                             with the same scope/ops/description and a ttl_minutes (default 10).{}",
                            a.scope.as_str(),
                            a.ops.len(),
                            if diff.is_empty() { String::new() } else { format!("\n{diff}") }
                        ),
                        data,
                    ))
                } else {
                    Ok(failure(
                        format!("Plan has {} rejection(s):\n- {}", rejections.len(), rejections.join("\n- ")),
                        data,
                    ))
                }
            }
            Err(e) => Ok(daemon_failure(&e, "plan_change")),
        }
    }

    #[tool(
        description = "Apply a planned changeset through the daemon (versioned, previous values recorded). Always \
            has a dead-man switch: it auto-reverts after ttl_minutes (default 10, 1–1440) unless you call \
            confirm_change. Only apply what the user agreed to after seeing plan_change. Then observe for a few \
            minutes (status, query on pressure/events) and confirm or revert. Guardrail rejections come back as \
            is_error=true with the reason; scope=system may show the user an admin authentication dialog.",
        annotations(
            title = "Apply change",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn apply_change(
        &self,
        Parameters(a): Parameters<ApplyArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        Ok(self.apply(&ctx, a.scope, &a.ops, &a.description, a.ttl_minutes).await)
    }

    #[tool(
        description = "Keep an applied changeset: stops its auto-revert timer. Only after the user agrees the \
            change helped (based on status/query observations).",
        annotations(
            title = "Confirm change",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn confirm_change(&self, Parameters(a): Parameters<ChangeIdArgs>) -> Result<CallToolResult, McpError> {
        match self.backends.daemon.call(a.scope, DaemonRequest::ConfirmChange(a.id)).await {
            Ok(_) => Ok(success(
                format!(
                    "Confirmed changeset {} in scope={}: it stays until reverted (revert_change).",
                    a.id,
                    a.scope.as_str()
                ),
                json!({"scope": a.scope, "id": a.id, "confirmed": true}),
            )),
            Err(e) => Ok(daemon_failure(&e, "confirm_change")),
        }
    }

    #[tool(
        description = "Undo a changeset now: the daemon restores the recorded previous values in a new changeset \
            (history is append-only). Use it when observation shows no improvement or a regression, or when the \
            user asks.",
        annotations(
            title = "Revert change",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn revert_change(&self, Parameters(a): Parameters<ChangeIdArgs>) -> Result<CallToolResult, McpError> {
        match self.daemon_json(a.scope, DaemonRequest::RevertChange(a.id)).await {
            Ok(cs) => Ok(success(
                format!(
                    "Reverted changeset {} in scope={} (new changeset {}).",
                    a.id,
                    a.scope.as_str(),
                    cs.get("id").map_or("?".into(), Value::to_string)
                ),
                json!({"scope": a.scope, "reverted": a.id, "changeset": cs}),
            )),
            Err(e) => Ok(daemon_failure(&e, "revert_change")),
        }
    }

    #[tool(
        description = "Switch the active profile (a profile_use changeset; `auto` restores automatic selection). Like \
            apply_change it auto-reverts after ttl_minutes (default 10) unless confirmed with confirm_change. Check \
            list_profiles first.",
        annotations(
            title = "Set profile",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_profile(
        &self,
        Parameters(a): Parameters<SetProfileArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let ops = [ChangeOp::ProfileUse { scope: a.scope, name: a.name.clone() }];
        Ok(self.apply(&ctx, a.scope, &ops, &format!("switch profile to {}", a.name), a.ttl_minutes).await)
    }

    #[tool(
        description = "Pause lindesk for an A/B comparison: the daemon reverts every setting it applied and only \
            observes until it resumes by itself after `minutes` (1-120, default 10) or resume is called. Nothing is \
            stopped or uninstalled. user scope (default) pauses your session agent; system pauses lindeskd and may \
            prompt for admin authentication. Tell the user, and resume when the comparison is done. (There is \
            deliberately no tool to turn lindesk off: that is the user's decision, `lindeskctl off`.)",
        annotations(
            title = "Pause lindesk",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn pause(&self, Parameters(a): Parameters<PauseArgs>) -> Result<CallToolResult, McpError> {
        let minutes = a.minutes.unwrap_or(10);
        if !(1..=MAX_PAUSE_MINUTES).contains(&minutes) {
            let msg = format!("rejected: minutes must be 1..={MAX_PAUSE_MINUTES} (a pause always ends by itself)");
            return Ok(failure(msg.clone(), json!({"error": "rejected", "message": msg})));
        }
        match self.daemon_json(a.scope, DaemonRequest::SetPaused { paused: true, seconds: minutes * 60 }).await {
            Ok(v) => Ok(success(
                format!(
                    "{} paused for {minutes} min: every setting it applied is reverted, it only observes. Call resume \
                     (or wait) to turn it back on.",
                    daemon_name(a.scope)
                ),
                json!({"scope": a.scope, "pause": v}),
            )),
            Err(e) => Ok(daemon_failure(&e, "pause")),
        }
    }

    #[tool(
        description = "End a pause early: lindesk applies its decisions again (the settings it reverted for the \
            pause come back on its next step). Use the same scope as the pause.",
        annotations(
            title = "Resume lindesk",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn resume(&self, Parameters(a): Parameters<ResumeArgs>) -> Result<CallToolResult, McpError> {
        match self.daemon_json(a.scope, DaemonRequest::SetPaused { paused: false, seconds: 0 }).await {
            Ok(v) => Ok(success(format!("{} resumed", daemon_name(a.scope)), json!({"scope": a.scope, "pause": v}))),
            Err(e) => Ok(daemon_failure(&e, "resume")),
        }
    }

    #[tool(
        description = "Measure the machine's latency baseline (system scope; lindeskd runs it, ≈15 s or a few seconds \
            with quick=true): CPU wakeup latency and relative core speed, memory latency/bandwidth, per-disk 4 KiB \
            read latency and throughput, swap page-in cost. Run it on an idle machine (the result is flagged noisy \
            otherwise). Policies then scale throttles from measured numbers. May prompt for admin authentication.",
        annotations(
            title = "Calibrate",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn calibrate(&self, Parameters(a): Parameters<CalibrateArgs>) -> Result<CallToolResult, McpError> {
        match self.daemon_json(Scope::System, DaemonRequest::Calibrate { quick: a.quick }).await {
            Ok(map) => Ok(success(
                format!(
                    "Calibration done{}: {} CPUs, {} disk(s) measured{}.",
                    if a.quick { " (quick)" } else { "" },
                    map.get("cpus").and_then(Value::as_array).map_or(0, Vec::len),
                    map.get("storage").and_then(Value::as_array).map_or(0, Vec::len),
                    if map.get("noisy").and_then(Value::as_bool).unwrap_or(false) {
                        "; NOISY: the machine was busy, re-run when idle"
                    } else {
                        ""
                    }
                ),
                json!({"latency": map}),
            )),
            Err(e) => Ok(daemon_failure(&e, "calibrate")),
        }
    }
}

fn profile_names(v: &Value) -> Vec<String> {
    let list = v.get("profiles").unwrap_or(v);
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p.get("name").and_then(Value::as_str).map(str::to_string))
        .collect()
}

// ---------------------------------------------------------------------------------------
// prompts

pub const ASSESS_STEPS: &str = "\
Assess this Linux desktop's responsiveness with the lindesk tools and fix what is worth fixing, safely.

1. Gather facts: call `status`, `hardware`, `latency_map` and `swap_analysis` (and `doctor` if status shows \
missing features). Note the health level, pressure (PSI) per resource, storage kinds, RAM/swap, and whether a \
daemon is running (`source`).
2. Call `recommendations`. It ranks doctor/swap/requirement findings, latency anomalies and the last 24 h of \
telemetry, each with proposed ops, risk and scope.
3. Explain the top 3 issues to the user in plain language: what you saw (cite the evidence numbers), why it hurts \
responsiveness, and what the fix would change. Ask which one(s) to try. Advice without ops is for the user to act on.
4. For the chosen fix call `plan_change` with its scope and proposed_ops (one scope per changeset). Show the user \
the diff and risk. Fix or drop rejected ops; never retry a rejection unchanged.
5. With the user's agreement call `apply_change` with the same ops and a short ttl (10 minutes is the default; it \
auto-reverts unless confirmed). Prefer scope=user; scope=system may show an admin authentication dialog.
6. Observe for a few minutes while the user works normally: call `status` again and `query` the lake, e.g. \
`SELECT date_trunc('minute', ts) AS m, resource, max(some_avg10) FROM pressure WHERE scope='system' AND ts > now() \
- INTERVAL 15 MINUTE GROUP BY 1, 2 ORDER BY 1` and `recent_events` with kind=level. Compare with before.
7. If it helped and the user agrees, `confirm_change`; otherwise `revert_change` (or let the ttl expire). Summarise \
what changed and how to undo it later (`list_changes`, `revert_change`).

Rules: never apply anything the user hasn't seen planned; one changeset at a time; if no daemon is running, stop \
after step 3 and tell the user how to start it.";

#[prompt_router]
impl LindeskMcp {
    #[prompt(
        name = "assess-responsiveness",
        description = "Guided workflow: measure the machine, rank recommendations, explain them, plan, apply with a ttl, observe, then confirm or revert."
    )]
    async fn assess_responsiveness(&self, Parameters(a): Parameters<AssessArgs>) -> Result<GetPromptResult, McpError> {
        let mut text = ASSESS_STEPS.to_string();
        if let Some(s) = a.symptom.filter(|s| !s.trim().is_empty()) {
            text.push_str(&format!(
                "\n\nThe user reports: \"{s}\". Keep that symptom in focus when ranking and explaining."
            ));
        }
        let mut r = GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)]);
        r.description = Some("Assess responsiveness with lindesk and apply guardrailed, revertible fixes".into());
        Ok(r)
    }

    #[prompt(
        name = "explain-slowdown",
        description = "Investigate why one app felt slow: how lindesk classified and treated it, what pressure it hit, and what competed with it."
    )]
    async fn explain_slowdown(&self, Parameters(a): Parameters<SlowdownArgs>) -> Result<GetPromptResult, McpError> {
        let when = a.when.filter(|w| !w.trim().is_empty()).map(|w| format!(" ({w})")).unwrap_or_default();
        let app = a.app.replace('\'', "");
        let text = format!(
            "The user says \"{app}\" felt slow{when}. Find out why with the lindesk tools, read-only first.\n\n\
1. `status`: current level, foreground app, protected/yielding units.\n\
2. `explain` with target `{app}` (or its pid / unit name): its QoS class, whether it was foreground, and every \
setting lindesk applied to it (or to its competitors) with the reason.\n\
3. `recent_events` (kind=level and kind=action_applied) and `query` the lake around that time, e.g.\n\
   `SELECT date_trunc('minute', ts) AS m, resource, max(some_avg10) AS stall_pct FROM pressure WHERE scope='system' \
AND ts > now() - INTERVAL 2 HOUR GROUP BY 1, 2 ORDER BY 1`\n\
   `SELECT unit, class, sum(cpu_usage_us)/1e6 AS cpu_s, sum(io_rbytes + io_wbytes)/1e6 AS io_mb, max(mem_current)/1e6 AS \
mem_mb FROM units WHERE ts > now() - INTERVAL 2 HOUR GROUP BY 1, 2 ORDER BY cpu_s DESC LIMIT 10`\n\
   and find the unit matching '{app}' (unit names look like app-…{app}…scope).\n\
4. Explain to the user which resource stalled (CPU, memory/swap, or I/O), what competed with the app, and what \
lindesk did about it.\n\
5. If a fix makes sense (e.g. a class_override for a noisy background unit, or a different profile), call \
`plan_change` and show the plan; only `apply_change` (with a ttl) after the user agrees, then observe and \
confirm or revert."
        );
        let mut r = GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)]);
        r.description = Some(format!("Why {app} felt slow"));
        Ok(r)
    }
}

// ---------------------------------------------------------------------------------------
// resources and server info

const URI_STATUS: &str = "lindesk://status";
const URI_HARDWARE: &str = "lindesk://hardware";
const URI_CONFIG: &str = "lindesk://config";
const URI_CHANGES: &str = "lindesk://changes/";
const URI_DOCS: &str = "lindesk://docs/";

fn json_resource(uri: &str, v: &Value) -> ReadResourceResult {
    ReadResourceResult::new(vec![
        ResourceContents::text(serde_json::to_string_pretty(v).unwrap_or_default(), uri)
            .with_mime_type("application/json"),
    ])
}

impl LindeskMcp {
    async fn read(&self, uri: &str) -> Result<ReadResourceResult, McpError> {
        let internal = |e: String| McpError::internal_error(e, None);
        match uri {
            URI_STATUS => {
                let (src, v, _) = self.status_value(Scope::User).await.map_err(internal)?;
                Ok(json_resource(uri, &json!({"source": src, "status": v})))
            }
            URI_HARDWARE => {
                let (src, v, _) = self.hardware_value().await.map_err(internal)?;
                Ok(json_resource(uri, &json!({"source": src, "hardware": v})))
            }
            URI_CONFIG => {
                let (path, toml) =
                    blocking(&self.backends.local, |l| l.config()).await.map_err(|e| internal(e.to_string()))?;
                let text = format!("# effective user config ({path}; defaults filled in)\n{toml}");
                Ok(ReadResourceResult::new(vec![ResourceContents::text(text, uri).with_mime_type("application/toml")]))
            }
            _ => {
                if let Some(rest) = uri.strip_prefix(URI_CHANGES) {
                    let (scope, id) = match rest.strip_prefix("system/") {
                        Some(id) => (Scope::System, id),
                        None => (Scope::User, rest),
                    };
                    let id: u64 =
                        id.parse().map_err(|_| McpError::invalid_params(format!("bad changeset id in {uri}"), None))?;
                    let v = match self.daemon_json(scope, DaemonRequest::GetChange(id)).await {
                        Ok(v) => v,
                        Err(_) => blocking(&self.backends.local, move |l| l.change(scope, id))
                            .await
                            .map_err(|e| internal(e.to_string()))?
                            .ok_or_else(|| {
                                McpError::resource_not_found(
                                    format!("no changeset {id} in scope={}", scope.as_str()),
                                    None,
                                )
                            })?,
                    };
                    return Ok(json_resource(uri, &v));
                }
                if let Some(topic) = uri.strip_prefix(URI_DOCS) {
                    let doc = docs::get(topic).ok_or_else(|| {
                        let topics: Vec<&str> = docs::DOCS.iter().map(|d| d.topic).collect();
                        McpError::resource_not_found(
                            format!("unknown doc topic '{topic}'; known: {}", topics.join(", ")),
                            None,
                        )
                    })?;
                    return Ok(ReadResourceResult::new(vec![
                        ResourceContents::text(doc.text, uri).with_mime_type("text/markdown"),
                    ]));
                }
                Err(McpError::resource_not_found(format!("unknown resource {uri}"), None))
            }
        }
    }
}

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for LindeskMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_prompts().enable_resources().build())
            .with_server_info(
                Implementation::new("lindesk-mcp", env!("CARGO_PKG_VERSION")).with_title("lindesk").with_description(
                    "Assess Linux desktop responsiveness and apply guardrailed, versioned, revertible lindesk changes",
                ),
            )
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let mut resources = vec![
            Resource::new(URI_STATUS, "status")
                .with_title("lindesk status")
                .with_description("Current level, pressure, protected/yielding units and daemon state (JSON).")
                .with_mime_type("application/json"),
            Resource::new(URI_HARDWARE, "hardware")
                .with_title("Hardware profile")
                .with_description("CPU, memory, storage, power and the latency map (JSON).")
                .with_mime_type("application/json"),
            Resource::new(URI_CONFIG, "config")
                .with_title("Effective user config")
                .with_description("~/.config/lindesk/lindesk.toml with defaults filled in (TOML).")
                .with_mime_type("application/toml"),
        ];
        for d in docs::DOCS {
            resources.push(
                Resource::new(format!("{URI_DOCS}{}", d.topic), format!("docs/{}", d.topic))
                    .with_title(d.title)
                    .with_description(d.description)
                    .with_mime_type("text/markdown"),
            );
        }
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let topics: Vec<&str> = docs::DOCS.iter().map(|d| d.topic).collect();
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new("lindesk://changes/{id}", "change")
                .with_title("User-scope changeset")
                .with_description(
                    "One versioned changeset of your session (lindesk://changes/system/{id} for system scope).",
                )
                .with_mime_type("application/json"),
            ResourceTemplate::new("lindesk://changes/system/{id}", "system-change")
                .with_title("System-scope changeset")
                .with_description("One versioned changeset of lindeskd.")
                .with_mime_type("application/json"),
            ResourceTemplate::new("lindesk://docs/{topic}", "docs")
                .with_title("lindesk documentation")
                .with_description(format!("Embedded docs; topics: {}", topics.join(", ")))
                .with_mime_type("text/markdown"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        self.read(&request.uri).await.map(Into::into)
    }
}
