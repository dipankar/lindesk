//! `lindeskctl changes …`: versioned, guardrailed changes (docs/CONTROL-PLANE.md §4).
//!
//! Every mutation goes to a daemon over D-Bus (`PlanChange`, `ApplyChange`, …), which runs
//! the guardrails, records the previous values and versions the changeset. lindeskctl only
//! builds ops, renders plans and history, and asks for confirmation. Reading the history
//! falls back to the change store on disk when no daemon is running.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use lindesk_changes::{ChangeOp, ChangeSet, ChangeStore, Plan, Risk, Scope, Status};
use lindesk_ipc::ChangeEnvelope;
use serde::Serialize;
use serde_json::{Value, json};

use crate::bus::{self, Daemon};
use crate::cli::{ChangesCmd, LocalStore, OnOff, OpsArgs, StoreScope};
use crate::ui::{self, Exit, Table, Ui, Usage, out};

/// Default ttl for high-risk changes made from the CLI without `--ttl`.
pub const DEFAULT_HIGH_RISK_TTL: Duration = Duration::from_secs(600);

pub fn run(ui: &Ui, cmd: &ChangesCmd) -> Result<()> {
    match cmd {
        ChangesCmd::List { limit } => list(ui, *limit),
        ChangesCmd::Show { id } => show(ui, *id),
        ChangesCmd::Diff { a, b } => diff(ui, *a, *b),
        ChangesCmd::Plan(ops) => plan_cmd(ui, ops),
        ChangesCmd::Apply { ops, ttl, message, yes, offline: true, local } => {
            apply_offline(ui, ops, *ttl, message.as_deref(), *yes, local)
        }
        ChangesCmd::Apply { ops, ttl, message, yes, .. } => apply_cmd(ui, ops, *ttl, message.as_deref(), *yes),
        ChangesCmd::Confirm { id } => confirm(ui, *id),
        ChangesCmd::Revert { id } => revert(ui, *id),
        ChangesCmd::RevertTo { to, yes } => revert_to(ui, *to, *yes),
        ChangesCmd::RevertAll { scope, yes, local } => revert_all(ui, *scope, *yes, local),
    }
}

// ------------------------------------------------------------------------ offline store

fn scope_of(s: StoreScope) -> Scope {
    match s {
        StoreScope::System => Scope::System,
        StoreScope::User => Scope::User,
    }
}

/// The change store of `scope`, opened directly (no daemon) with the real applier, or one
/// rooted at `--root` (fixture trees). System scope needs root unless `--store` points
/// elsewhere.
pub fn open_local(scope: Scope, local: &LocalStore, what: &str) -> Result<ChangeStore> {
    let dir = match &local.store {
        Some(d) => d.clone(),
        None => {
            if scope == Scope::System {
                crate::paths::require_root(what)?;
            }
            store_dir(scope).ok_or_else(|| anyhow!("no change store for {scope} scope (HOME unset?)"))?
        }
    };
    let roots = local.root.as_ref().map_or_else(lindesk_changes::Roots::host, lindesk_changes::Roots::under);
    let applier = lindesk_changes::RealApplier::new(roots, lindesk_changes::ConfigPaths::host(scope));
    Ok(ChangeStore::open(&dir, scope)
        .with_context(|| format!("opening the change store {}", dir.display()))?
        .with_applier(std::sync::Arc::new(applier)))
}

fn local_ctx(scope: Scope, local: &LocalStore, origin: lindesk_changes::Origin) -> lindesk_changes::GuardContext {
    let roots = local.root.as_ref().map_or_else(lindesk_changes::Roots::host, lindesk_changes::Roots::under);
    let hw = lindesk_sys::hardware::detect(&lindesk_sys::Roots::host());
    lindesk_changes::GuardContext::new(scope, origin, roots, lindesk_changes::ConfigPaths::host(scope), hw)
}

/// `changes apply --offline`: plan and apply through the store on disk.
fn apply_offline(
    ui: &Ui,
    args: &OpsArgs,
    ttl: Option<Duration>,
    message: Option<&str>,
    yes: bool,
    local: &LocalStore,
) -> Result<()> {
    let ops = build_ops(args, ui.system)?;
    let scope = ops.first().map_or(Scope::User, ChangeOp::scope);
    if ops.iter().any(|o| o.scope() != scope) {
        bail!("--offline applies one scope at a time; split the ops");
    }
    if ttl.is_some_and(|t| !t.is_zero()) {
        ui.warn("--ttl needs the daemon's dead-man switch; ignored with --offline");
    }
    let store = open_local(scope, local, "applying a system change offline")?;
    store.recover()?;
    let plan = store.plan(&ops, &local_ctx(scope, local, lindesk_changes::Origin::Cli));
    if !ui.json {
        out(render_plan(ui, &plan).trim_end());
    }
    if !plan.is_ok() {
        if ui.json {
            ui.print_json(&plan)?;
        }
        return Err(Exit(1).into());
    }
    if !yes {
        if !ui.stdin_tty {
            return Err(Usage("not a terminal: pass --yes to apply without confirmation".into()).into());
        }
        if !ui::confirm("Apply this change?") {
            ui.info("nothing changed");
            return Err(Exit(1).into());
        }
    }
    let description = message.map(str::to_string).unwrap_or_else(|| default_description(&ops));
    let author =
        lindesk_changes::Author::new(crate::paths::uid(), lindesk_changes::Origin::Cli).with_client("lindeskctl");
    let cs = store.apply(&plan, author, &description, None)?;
    if ui.json {
        ui.print_json(&cs)?;
    } else {
        out(&applied_message(ui, &cs, now_ms()));
    }
    if cs.status == Status::Failed { Err(Exit(1).into()) } else { Ok(()) }
}

/// `changes revert-all`: revert every effective changeset of a scope, offline.
fn revert_all(ui: &Ui, scope: StoreScope, yes: bool, local: &LocalStore) -> Result<()> {
    let scope = scope_of(scope);
    let store = open_local(scope, local, "reverting every system change")?;
    let recovered = store.recover()?;
    let history = store.history(usize::MAX)?;
    // Everything up to the newest revert-to-0 that is still in effect is already undone.
    let floor = history.iter().filter(|c| c.revert_to == Some(0) && c.is_effective()).map(|c| c.id).max().unwrap_or(0);
    let effective: Vec<ChangeSet> = history.into_iter().filter(|c| c.id > floor && c.is_effective()).collect();
    if effective.is_empty() {
        if ui.json {
            return ui.print_json(&json!({"scope": scope, "recovered": recovered.len(), "reverted": []}));
        }
        out(&ui.dim(format!("no {scope} changes in effect; nothing to revert")));
        return Ok(());
    }
    if !ui.json {
        let now = now_ms();
        let mut s = format!(
            "{}
",
            ui.bold(format!("revert all {} {scope} change(s) still in effect:", effective.len()))
        );
        for c in &effective {
            s.push_str(&format!(
                "  #{:<4} {}  {}
",
                c.id,
                status_colored(ui, c, now),
                c.description
            ));
        }
        out(s.trim_end());
    }
    if !yes {
        if !ui.stdin_tty {
            return Err(Usage("not a terminal: pass --yes to revert without confirmation".into()).into());
        }
        if !ui::confirm("Revert all of them?") {
            ui.info("nothing changed");
            return Err(Exit(1).into());
        }
    }
    let author =
        lindesk_changes::Author::new(crate::paths::uid(), lindesk_changes::Origin::Cli).with_client("revert-all");
    let cs = store.revert_to(0, author)?;
    let ids: Vec<u64> = effective.iter().map(|c| c.id).collect();
    if ui.json {
        return ui.print_json(
            &json!({"scope": scope, "recovered": recovered.len(), "reverted": ids, "by": cs.as_ref().map(|c| c.id)}),
        );
    }
    match cs {
        Some(c) if c.status == Status::Failed => {
            out(&format!(
                "{} reverting failed and was rolled back: {}",
                ui.fail_mark(),
                c.error.clone().unwrap_or_default()
            ));
            Err(Exit(1).into())
        }
        Some(c) => {
            out(&format!(
                "{} reverted {} change(s) {}",
                ui.ok_mark(),
                ids.len(),
                ui.dim(format!("(as change #{})", c.id))
            ));
            Ok(())
        }
        None => {
            out(&ui.dim("nothing to revert"));
            Ok(())
        }
    }
}

fn now_ms() -> i64 {
    lindesk_core::clock::now_ms()
}

// ----------------------------------------------------------------------------- building ops

/// A `--set` value: TOML syntax when it parses (`true`, `5`, `0.5`, `[1, 2]`, `"x"`), else a
/// plain string (`400ms`, `auto`).
pub fn parse_value(s: &str) -> Value {
    let s = s.trim();
    if let Ok(t) = toml::from_str::<toml::Table>(&format!("v = {s}"))
        && let Some(v) = t.get("v")
        && let Ok(j) = serde_json::to_value(v)
    {
        return j;
    }
    Value::String(s.to_string())
}

fn read_ops_file(path: &str) -> Result<Vec<Value>> {
    let text = if path == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).context("reading ops from stdin")?;
        s
    } else {
        std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?
    };
    let env = ChangeEnvelope::from_json(&text).map_err(|e| {
        anyhow!(
            "{}: not a JSON array of ops or an {{\"ops\": [...]}} envelope ({e})",
            if path == "-" { "stdin" } else { path }
        )
    })?;
    Ok(env.ops)
}

/// The ops given on the command line, validated against the `ChangeOp` schema.
pub fn build_ops(args: &OpsArgs, system: bool) -> Result<Vec<ChangeOp>> {
    let scope = if system { Scope::System } else { Scope::User };
    let mut raw: Vec<Value> = Vec::new();
    if let Some(f) = &args.file {
        raw.extend(read_ops_file(f)?);
    }
    let mut ops: Vec<ChangeOp> = Vec::new();
    for (i, v) in raw.into_iter().enumerate() {
        let op: ChangeOp = serde_json::from_value(v.clone()).map_err(|e| {
            Usage(format!("op {i} ({v}) is not a valid change op: {e}\nSee: lindeskctl changes plan --help"))
        })?;
        ops.push(op);
    }
    for (k, v) in &args.set {
        ops.push(ChangeOp::ConfigSet { scope, key: k.clone(), value: parse_value(v) });
    }
    for k in &args.unset {
        ops.push(ChangeOp::ConfigUnset { scope, key: k.clone() });
    }
    if let Some(p) = &args.profile {
        ops.push(ChangeOp::ProfileUse { scope, name: p.clone() });
    }
    for (unit, class) in &args.class {
        let class = class.parse().map_err(|_| {
            Usage(format!("--class {unit}={class}: unknown class (critical, interactive, normal, background, batch)"))
        })?;
        ops.push(ChangeOp::ClassOverride { scope, unit: unit.clone(), class });
    }
    for (k, v) in &args.sysctl {
        ops.push(ChangeOp::Sysctl { key: k.clone(), value: v.clone(), persist: args.persist });
    }
    for (d, s) in &args.scheduler {
        ops.push(ChangeOp::IoScheduler { device: d.clone(), scheduler: s.clone(), persist: args.persist });
    }
    if args.zswap.is_some() || args.zswap_compressor.is_some() || args.zswap_max_pool.is_some() {
        ops.push(ChangeOp::Zswap {
            enabled: args.zswap.map(|z| z == OnOff::On),
            compressor: args.zswap_compressor.clone(),
            max_pool_percent: args.zswap_max_pool,
        });
    }
    if let Some(p) = &args.epp {
        ops.push(ChangeOp::CpuEpp { preference: p.clone() });
    }
    if let Some(g) = &args.governor {
        ops.push(ChangeOp::CpuGovernor { governor: g.clone() });
    }
    if ops.is_empty() {
        return Err(Usage(
            "nothing to change: give an OPS.json file (or - for stdin) or shortcuts like \
             --set KEY=VALUE, --sysctl KEY=VALUE, --scheduler DEV=SCHED, --profile NAME.\n\
             See: lindeskctl changes plan --help"
                .into(),
        )
        .into());
    }
    Ok(ops)
}

/// Which daemon handles these ops: lindeskd for system scope (or `--system`), else
/// lindesk-session. Mixed scopes are refused with a hint.
pub fn daemon_for(ops: &[ChangeOp], system: bool) -> Result<Daemon> {
    let scopes: BTreeSet<Scope> = ops.iter().map(ChangeOp::scope).collect();
    if system || scopes == BTreeSet::from([Scope::System]) {
        if let Some(op) = ops.iter().find(|o| o.scope() == Scope::User) {
            return Err(Usage(format!(
                "'{op}' is a user-scope op but this changeset goes to lindeskd (system scope); \
                 use \"scope\": \"system\" in the JSON, or drop --system"
            ))
            .into());
        }
        return Ok(Daemon::Broker);
    }
    if scopes.contains(&Scope::System) {
        let user: Vec<String> = ops.iter().filter(|o| o.scope() == Scope::User).map(|o| o.to_string()).collect();
        return Err(Usage(format!(
            "this changeset mixes your config ({}) with system tunables. Split it in two, or pass \
             --system to edit /etc/lindesk/lindesk.toml instead of your own config",
            user.join("; ")
        ))
        .into());
    }
    Ok(Daemon::Session)
}

/// The `ops_json` argument: an envelope that records lindeskctl as the author.
pub fn envelope(ops: &[ChangeOp]) -> String {
    ChangeEnvelope {
        origin: "cli".into(),
        client: Some("lindeskctl".into()),
        ops: ops.iter().filter_map(|o| serde_json::to_value(o).ok()).collect(),
    }
    .to_json()
}

/// A description when `-m` is missing: the ops, shortened.
pub fn default_description(ops: &[ChangeOp]) -> String {
    let s = ops.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ");
    if s.chars().count() > 100 { format!("{}…", s.chars().take(99).collect::<String>()) } else { s }
}

/// Explain that changes need a daemon, around a "not running" error.
fn needs_daemon(what: &str, e: anyhow::Error) -> anyhow::Error {
    if bus::is_not_running(&e) {
        anyhow!(
            "can't {what}: {e}\n  Changes are validated, versioned and applied by the lindesk daemons, so one has to be running."
        )
    } else {
        e
    }
}

// ----------------------------------------------------------------------------- rendering

/// `● LOW` / `● MEDIUM` / `● HIGH`, coloured.
pub fn risk_badge(ui: &Ui, r: Risk) -> String {
    match r {
        Risk::Low => ui.paint("1;32", "● LOW"),
        Risk::Medium => ui.paint("1;33", "● MEDIUM"),
        Risk::High => ui.paint("1;31", "● HIGH"),
    }
}

fn risk_word(r: Risk) -> &'static str {
    match r {
        Risk::Low => "low",
        Risk::Medium => "medium",
        Risk::High => "high",
    }
}

/// The status as shown: `expired` is a revert by the dead-man switch.
pub fn status_word(cs: &ChangeSet) -> &'static str {
    match cs.status {
        Status::Reverted if cs.ttl_until.is_some_and(|t| t <= cs.updated) => "expired",
        Status::Planned => "planned",
        Status::Applying => "applying",
        Status::Applied => "applied",
        Status::Confirmed => "confirmed",
        Status::Reverted => "reverted",
        Status::Failed => "failed",
    }
}

pub fn status_colored(ui: &Ui, cs: &ChangeSet, now: i64) -> String {
    let w = status_word(cs);
    match w {
        "applied" if ttl_left(cs, now).is_some() => ui.yellow("◐ applied"),
        "applied" => ui.green("● applied"),
        "confirmed" => ui.green("✓ confirmed"),
        "reverted" => ui.dim("↺ reverted"),
        "expired" => ui.paint("35", "↺ expired"),
        "failed" => ui.red("✗ failed"),
        _ => ui.cyan(format!("◌ {w}")),
    }
}

/// Milliseconds until the dead-man switch reverts it (applied, unconfirmed, ttl'd).
pub fn ttl_left(cs: &ChangeSet, now: i64) -> Option<i64> {
    (cs.status == Status::Applied).then_some(())?;
    cs.ttl_until.map(|t| (t - now).max(0))
}

/// `cli`, `mcp:claude-code`, `daemon:dead-man`.
pub fn author_label(cs: &ChangeSet) -> String {
    let origin = cs.author.origin.to_string();
    match &cs.author.client {
        Some(c) if !c.is_empty() && c != "lindeskctl" => format!("{origin}:{c}"),
        _ => origin,
    }
}

/// `12s ago`, `5m ago`, `3h ago`, else the date.
pub fn ago(ts: i64, now: i64) -> String {
    let d = now - ts;
    if d < 0 {
        return ui::datetime(ts);
    }
    if d < 48 * 3600 * 1000 {
        let short = ui::duration_ms(d);
        // `1h02m` → `1h`, `3m10s` → `3m`: a list needs only the leading unit.
        let lead: String = short.chars().take_while(|c| c.is_ascii_digit()).collect();
        let unit: String = short.chars().skip(lead.len()).take_while(|c| c.is_ascii_alphabetic()).collect();
        return format!("{lead}{unit} ago");
    }
    ui::datetime(ts).get(..10).unwrap_or_default().to_string()
}

/// A JSON value for a table cell: strings bare, `null` as `(unset)`.
pub fn value_short(v: &Value) -> String {
    match v {
        Value::Null => "(unset)".into(),
        Value::String(s) => s.clone(),
        Value::Object(o) if o.contains_key("runtime") => {
            let r = o.get("runtime").map(value_short).unwrap_or_default();
            if o.get("persisted").is_some_and(|p| !p.is_null()) { format!("{r} (persisted)") } else { r }
        }
        Value::Object(o) => {
            let vals: BTreeSet<String> = o.values().map(value_short).collect();
            if vals.len() == 1 && o.len() > 1 {
                format!("{} (all {})", vals.into_iter().next().unwrap_or_default(), o.len())
            } else {
                o.iter().map(|(k, v)| format!("{k}={}", value_short(v))).collect::<Vec<_>>().join(" ")
            }
        }
        other => other.to_string(),
    }
}

/// `(kind, what)` of an op for tables: `("sysctl", "vm.swappiness")`.
pub fn op_target(op: &ChangeOp) -> (&'static str, String) {
    match op {
        ChangeOp::ConfigSet { key, .. } | ChangeOp::ConfigUnset { key, .. } => ("config", key.clone()),
        ChangeOp::ProfileUse { .. } => ("profile", "general.profile".into()),
        ChangeOp::ClassOverride { unit, .. } => ("class", unit.clone()),
        ChangeOp::Sysctl { key, .. } => ("sysctl", key.clone()),
        ChangeOp::IoScheduler { device, .. } => ("scheduler", device.clone()),
        ChangeOp::Zswap { .. } => ("zswap", "parameters".into()),
        ChangeOp::CpuEpp { .. } => ("cpu_epp", "all CPUs".into()),
        ChangeOp::CpuGovernor { .. } => ("governor", "all CPUs".into()),
        ChangeOp::ServiceState { unit, .. } => ("service", unit.clone()),
    }
}

/// A unified diff with `+`/`-`/`@@` colours.
pub fn colored_diff(ui: &Ui, diff: &str) -> String {
    let mut s = String::new();
    for l in diff.lines() {
        let line = if l.starts_with("+++") || l.starts_with("---") {
            ui.bold(l)
        } else if l.starts_with('+') {
            ui.green(l)
        } else if l.starts_with('-') {
            ui.red(l)
        } else if l.starts_with("@@") {
            ui.cyan(l)
        } else {
            ui.dim(l)
        };
        s.push_str(&line);
        s.push('\n');
    }
    s
}

fn arrow(ui: &Ui, before: &Value, after: &Value) -> String {
    let (b, a) = (value_short(before), value_short(after));
    if b == a { ui.dim(format!("{a} (unchanged)")) } else { format!("{} → {}", ui.red(b), ui.green(a)) }
}

/// `plan` output: badge, the values before → after, the diff, the rejections.
pub fn render_plan(ui: &Ui, plan: &Plan) -> String {
    let mut s = format!(
        "{}  {} risk · {} scope · {} op{}\n",
        ui.bold("plan"),
        risk_badge(ui, plan.risk),
        plan.scope,
        plan.ops.len(),
        if plan.ops.len() == 1 { "" } else { "s" }
    );
    let rejected: BTreeSet<usize> = plan.rejections.iter().filter_map(|r| r.index).collect();
    let mut t = Table::new(&["  #", "OP", "TARGET", "RISK", "CHANGE"]);
    for (i, p) in plan.ops.iter().enumerate() {
        let (kind, what) = op_target(&p.op);
        let change = if rejected.contains(&i) { ui.red("✗ rejected") } else { arrow(ui, &p.current, &p.target) };
        t.row(vec![format!("  {i}"), kind.into(), what, risk_word(p.risk).into(), change]);
    }
    s.push_str(&t.render(ui));
    if !plan.diff.trim().is_empty() {
        s.push('\n');
        s.push_str(&colored_diff(ui, plan.diff.trim_end()));
    }
    if !plan.rejections.is_empty() {
        s.push('\n');
        for r in &plan.rejections {
            let rule =
                serde_json::to_value(r.rule).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
            s.push_str(&format!("{} {}  {}\n", ui.red("✗ rejected:"), r, ui.dim(format!("[{rule}]"))));
        }
    } else if plan.ttl_required {
        s.push_str(&ui.yellow("⚠ this change needs a ttl (--ttl 10m): it auto-reverts unless confirmed\n"));
    }
    s
}

/// The `list` table.
pub fn render_list(ui: &Ui, list: &[ChangeSet], now: i64) -> String {
    let mut t = Table::new(&["ID", "WHEN", "BY", "SCOPE", "RISK", "STATUS", "TTL", "DESCRIPTION"]).right(&[0]);
    for cs in list {
        let ttl = match ttl_left(cs, now) {
            Some(ms) => ui.yellow(ui::duration_ms(ms)),
            None => ui.dim("-"),
        };
        let mut desc = cs.description.clone();
        if let Some(e) = &cs.error {
            desc = format!("{desc} {}", ui.red(format!("({e})")));
        }
        t.row(vec![
            format!("#{}", cs.id),
            ago(cs.ts, now),
            author_label(cs),
            cs.scope.to_string(),
            match cs.risk {
                Risk::Low => ui.green("low"),
                Risk::Medium => ui.yellow("medium"),
                Risk::High => ui.red("high"),
            },
            status_colored(ui, cs, now),
            ttl,
            desc,
        ]);
    }
    t.render(ui)
}

fn sys_flag(scope: Scope) -> &'static str {
    if scope == Scope::System { " --system" } else { "" }
}

/// `show` output.
pub fn render_show(ui: &Ui, cs: &ChangeSet, now: i64) -> String {
    let mut s = format!(
        "{}  {}  {} risk · {} scope\n",
        ui.bold(format!("change #{}", cs.id)),
        status_colored(ui, cs, now),
        risk_badge(ui, cs.risk),
        cs.scope
    );
    if !cs.description.is_empty() {
        s.push_str(&format!("  {}\n", ui.bold(&cs.description)));
    }
    let mut by = format!("{} (uid {})", author_label(cs), cs.author.uid);
    if let Some(b) = &cs.author.bus_name {
        by.push_str(&format!(" via {b}"));
    }
    s.push_str(&format!("  {} {}\n", ui.dim("by     "), by));
    s.push_str(&format!("  {} {} ({})\n", ui.dim("when   "), ui::datetime(cs.ts), ago(cs.ts, now)));
    if let Some(t) = cs.ttl_until {
        let line = match ttl_left(cs, now) {
            Some(ms) => ui.yellow(format!("auto-reverts in {} ({})", ui::duration_ms(ms), ui::datetime(t))),
            None => ui.dim(format!("ttl until {}", ui::datetime(t))),
        };
        s.push_str(&format!("  {} {line}\n", ui.dim("ttl    ")));
    }
    if let Some(r) = cs.reverts {
        s.push_str(&format!("  {} reverts #{r}\n", ui.dim("undoes ")));
    }
    if let Some(v) = cs.revert_to {
        s.push_str(&format!("  {} returns to version {v}\n", ui.dim("undoes ")));
    }
    if let Some(r) = cs.reverted_by {
        s.push_str(&format!("  {} by #{r}\n", ui.dim("undone ")));
    }
    if let Some(e) = &cs.error {
        s.push_str(&format!("  {} {}\n", ui.dim("error  "), ui.red(e)));
    }
    s.push('\n');
    let mut t = Table::new(&["  OP", "TARGET", "BEFORE", "AFTER"]);
    for rec in &cs.ops {
        let (kind, what) = op_target(&rec.op);
        let changed = value_short(&rec.previous) != value_short(&rec.applied);
        t.row(vec![
            format!("  {kind}"),
            what,
            if changed { ui.red(value_short(&rec.previous)) } else { value_short(&rec.previous) },
            if changed { ui.green(value_short(&rec.applied)) } else { value_short(&rec.applied) },
        ]);
    }
    s.push_str(&t.render(ui));
    let flag = sys_flag(cs.scope);
    match status_word(cs) {
        "applied" => {
            s.push_str(&format!(
                "\n{}\n",
                ui.dim(format!(
                    "keep: lindeskctl changes confirm{flag} {id} · undo: lindeskctl changes revert{flag} {id}",
                    id = cs.id
                ))
            ));
        }
        "confirmed" => {
            s.push_str(&format!("\n{}\n", ui.dim(format!("undo: lindeskctl changes revert{flag} {}", cs.id))));
        }
        _ => {}
    }
    s
}

// ------------------------------------------------------------------------------- pending

/// An applied, unconfirmed changeset with a running dead-man ttl.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Pending {
    pub id: u64,
    pub scope: Scope,
    pub ttl_until: i64,
    pub description: String,
}

impl Pending {
    /// `change #7 reverts in 8m` (system ones say so).
    pub fn countdown(&self, now: i64) -> String {
        let ms = (self.ttl_until - now).max(0);
        let left = if ms >= 3_600_000 {
            ui::duration_ms(ms)
        } else if ms >= 60_000 {
            format!("{}m", ms / 60_000)
        } else {
            format!("{}s", ms / 1000)
        };
        let sys = if self.scope == Scope::System { " (system)" } else { "" };
        format!("change #{}{sys} reverts in {left}", self.id)
    }

    pub fn confirm_hint(&self) -> String {
        format!("lindeskctl changes confirm{} {}", sys_flag(self.scope), self.id)
    }
}

pub fn pending_of(list: &[ChangeSet], now: i64) -> Vec<Pending> {
    let mut v: Vec<Pending> = list
        .iter()
        .filter(|c| ttl_left(c, now).is_some())
        .map(|c| Pending {
            id: c.id,
            scope: c.scope,
            ttl_until: c.ttl_until.unwrap_or(now),
            description: c.description.clone(),
        })
        .collect();
    v.sort_by_key(|p| p.ttl_until);
    v
}

/// Pending changes from every running daemon (best effort, never an error).
pub fn pending(daemons: &[Daemon]) -> Vec<Pending> {
    let now = now_ms();
    let mut out = Vec::new();
    for &d in daemons {
        if !matches!(bus::reach(d), bus::Reach::Running) {
            continue;
        }
        if let Ok(list) = bus::list_changes(d, 50).and_then(|j| parse_list(&j)) {
            out.extend(pending_of(&list, now));
        }
    }
    out.sort_by_key(|p| p.ttl_until);
    out
}

// --------------------------------------------------------------------------------- data

pub fn parse_list(json: &str) -> Result<Vec<ChangeSet>> {
    let v: Value = serde_json::from_str(json).context("ListChanges returned invalid JSON")?;
    let arr = match v {
        Value::Array(a) => a,
        Value::Object(mut o) => match o.remove("changes") {
            Some(Value::Array(a)) => a,
            _ => bail!("ListChanges returned an object without \"changes\""),
        },
        _ => bail!("ListChanges returned something other than a list"),
    };
    Ok(arr.into_iter().filter_map(|c| serde_json::from_value(c).ok()).collect())
}

fn parse_changeset(json: &str, member: &str) -> Result<ChangeSet> {
    serde_json::from_str(json).with_context(|| format!("{member} returned a changeset this lindeskctl can't read"))
}

/// The change store directory of a scope.
fn store_dir(scope: Scope) -> Option<PathBuf> {
    ChangeStore::default_dir(scope)
}

/// Changesets read from the store on disk, newest first.
fn local_list(scope: Scope, limit: usize) -> Result<Vec<ChangeSet>> {
    let Some(dir) = store_dir(scope) else { return Ok(Vec::new()) };
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => bail!("reading {}: {e}", dir.display()),
    };
    let mut ids: Vec<u64> =
        rd.flatten().filter_map(|e| e.file_name().to_str()?.strip_suffix(".json")?.parse().ok()).collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    let mut out = Vec::new();
    for id in ids.into_iter().take(limit) {
        let p = dir.join(format!("{id:06}.json"));
        if let Ok(cs) = std::fs::read(&p).map_err(anyhow::Error::from).and_then(|b| Ok(serde_json::from_slice(&b)?)) {
            out.push(cs);
        }
    }
    Ok(out)
}

/// Every changeset of the scope(s) shown by `list`, newest first, and where they came from.
fn fetch_list(ui: &Ui, limit: u32) -> Result<(Vec<ChangeSet>, Vec<String>)> {
    let scopes: &[(Daemon, Scope)] = if ui.system {
        &[(Daemon::Broker, Scope::System)]
    } else {
        &[(Daemon::Session, Scope::User), (Daemon::Broker, Scope::System)]
    };
    let mut all = Vec::new();
    let mut notes = Vec::new();
    for &(d, scope) in scopes {
        match bus::list_changes(d, limit) {
            Ok(j) => all.extend(parse_list(&j)?),
            Err(e) if bus::is_not_running(&e) || bus::is_unsupported(&e) => match local_list(scope, limit as usize) {
                Ok(l) => {
                    if !l.is_empty() {
                        notes.push(format!("{} isn't running; {scope} history read from disk", d.name()));
                    }
                    all.extend(l);
                }
                Err(err) => notes.push(format!("{scope} history: {err}")),
            },
            Err(e) => return Err(e),
        }
    }
    all.sort_by(|a, b| b.ts.cmp(&a.ts).then(b.id.cmp(&a.id)));
    all.truncate(limit as usize);
    Ok((all, notes))
}

fn get_one(ui: &Ui, id: u64) -> Result<ChangeSet> {
    let (d, scope) = if ui.system { (Daemon::Broker, Scope::System) } else { (Daemon::Session, Scope::User) };
    match bus::get_change(d, id) {
        Ok(j) => parse_changeset(&j, "GetChange"),
        Err(e) if bus::is_not_running(&e) || bus::is_unsupported(&e) => {
            let p = store_dir(scope).map(|d| d.join(format!("{id:06}.json")));
            match p.as_ref().map(std::fs::read) {
                Some(Ok(b)) => Ok(serde_json::from_slice(&b)?),
                _ => Err(anyhow!(
                    "change #{id} not found in the {scope} history{}",
                    if ui.system { "" } else { " (system changes: add --system)" }
                )),
            }
        }
        Err(e) => {
            if e.to_string().contains("not found") && !ui.system {
                return Err(anyhow!(
                    "{e}\n  (system-scope changes are numbered separately: lindeskctl changes show --system {id})"
                ));
            }
            Err(e)
        }
    }
}

// ------------------------------------------------------------------------------ commands

fn list(ui: &Ui, limit: u32) -> Result<()> {
    let (list, notes) = fetch_list(ui, limit)?;
    if ui.json {
        return ui.print_json(&list);
    }
    for n in &notes {
        ui.info(ui.dim(n));
    }
    if list.is_empty() {
        out(&format!(
            "{}\n{}",
            ui.dim("no changes yet"),
            ui.dim(
                "make one: lindeskctl changes apply --set KEY=VALUE -m \"why\" (see lindeskctl changes apply --help)"
            )
        ));
        return Ok(());
    }
    let now = now_ms();
    let mut s = render_list(ui, &list, now);
    let pend = pending_of(&list, now);
    if let Some(p) = pend.first() {
        s.push_str(&format!(
            "\n{} {}  {}",
            ui.yellow("↺"),
            ui.yellow(p.countdown(now)),
            ui.dim(format!("keep it: {}", p.confirm_hint()))
        ));
    }
    out(s.trim_end());
    Ok(())
}

fn show(ui: &Ui, id: u64) -> Result<()> {
    let cs = get_one(ui, id)?;
    if ui.json {
        return ui.print_json(&cs);
    }
    out(render_show(ui, &cs, now_ms()).trim_end());
    Ok(())
}

fn fetch_range(ui: &Ui, a: u64, b: u64) -> Result<Vec<ChangeSet>> {
    let mut v = Vec::new();
    for id in (a + 1)..=b {
        match get_one(ui, id) {
            Ok(cs) => v.push(cs),
            Err(e) if e.to_string().contains("not found") => {}
            Err(e) => return Err(e),
        }
    }
    Ok(v)
}

fn diff(ui: &Ui, a: u64, b: Option<u64>) -> Result<()> {
    let scope = if ui.system { Scope::System } else { Scope::User };
    let b = match b {
        Some(b) => b,
        None => fetch_list(ui, 1)?.0.into_iter().filter(|c| c.scope == scope).map(|c| c.id).max().unwrap_or(a),
    };
    let (a, b) = if a <= b { (a, b) } else { (b, a) };
    let between = fetch_range(ui, a, b)?;
    // Config snapshots: readable from the store (yours; the system's needs the right group).
    let config_diff = store_dir(scope)
        .filter(|d| d.join("config").is_dir())
        .map(|dir| ChangeStore::open(&dir, scope).and_then(|s| s.diff(a, b)).map_err(|e| e.to_string()));
    if ui.json {
        let cd = match &config_diff {
            Some(Ok(d)) => json!(d),
            Some(Err(e)) => json!({"error": e}),
            None => Value::Null,
        };
        return ui.print_json(&json!({"scope": scope, "from": a, "to": b, "changes": between, "config_diff": cd}));
    }
    let mut s = format!("{}  {scope} scope\n", ui.bold(format!("version {a} → {b}")));
    if between.is_empty() {
        s.push_str(&format!("  {}\n", ui.dim("no changesets in between")));
    }
    let now = now_ms();
    for cs in &between {
        s.push_str(&format!(
            "\n{} {}  {}  {}\n",
            ui.bold(format!("#{}", cs.id)),
            status_colored(ui, cs, now),
            ui.dim(format!("{} · {}", author_label(cs), ago(cs.ts, now))),
            cs.description
        ));
        for rec in &cs.ops {
            let (kind, what) = op_target(&rec.op);
            s.push_str(&format!("  {kind:<9} {what}  {}\n", arrow(ui, &rec.previous, &rec.applied)));
        }
    }
    match config_diff {
        Some(Ok(d)) if !d.trim().is_empty() => {
            s.push('\n');
            s.push_str(&colored_diff(ui, d.trim_end()));
        }
        Some(Ok(_)) => s.push_str(&format!("\n{}\n", ui.dim("config file: no difference"))),
        Some(Err(e)) => s.push_str(&format!("\n{}\n", ui.dim(format!("config file diff unavailable: {e}")))),
        None => {}
    }
    out(s.trim_end());
    Ok(())
}

fn parse_plan(json: &str) -> Result<Plan> {
    serde_json::from_str(json).context("PlanChange returned a plan this lindeskctl can't read (version mismatch?)")
}

fn plan_cmd(ui: &Ui, args: &OpsArgs) -> Result<()> {
    let ops = build_ops(args, ui.system)?;
    let d = daemon_for(&ops, ui.system)?;
    let raw = bus::plan_change(d, &envelope(&ops)).map_err(|e| needs_daemon("plan this change", e))?;
    let plan = parse_plan(&raw)?;
    if ui.json {
        ui.print_json(&plan)?;
    } else {
        out(render_plan(ui, &plan).trim_end());
        if plan.is_ok() {
            ui.info(ui.dim(format!(
                "nothing changed. Apply it: lindeskctl changes apply {}",
                command_line_tail(&ops, ui.system && d == Daemon::Broker)
            )));
        }
    }
    if plan.is_ok() { Ok(()) } else { Err(Exit(1).into()) }
}

fn apply_cmd(ui: &Ui, args: &OpsArgs, ttl: Option<Duration>, message: Option<&str>, yes: bool) -> Result<()> {
    let ops = build_ops(args, ui.system)?;
    let d = daemon_for(&ops, ui.system)?;
    let env = envelope(&ops);
    let raw = bus::plan_change(d, &env).map_err(|e| needs_daemon("apply this change", e))?;
    let plan = parse_plan(&raw)?;
    if !ui.json {
        out(render_plan(ui, &plan).trim_end());
    }
    if !plan.is_ok() {
        if ui.json {
            ui.print_json(&plan)?;
        }
        return Err(Exit(1).into());
    }
    let ttl = match ttl {
        Some(t) => t,
        None if plan.risk == Risk::High || plan.ttl_required => {
            ui.info(ui.dim(format!(
                "high risk: it auto-reverts in {} unless confirmed (--ttl 0 to keep it right away)",
                ui::duration_ms(DEFAULT_HIGH_RISK_TTL.as_millis() as i64)
            )));
            DEFAULT_HIGH_RISK_TTL
        }
        None => Duration::ZERO,
    };
    if !yes {
        if !ui.stdin_tty {
            return Err(Usage("not a terminal: pass --yes to apply without confirmation".into()).into());
        }
        let q = if ttl.is_zero() {
            "Apply this change?".to_string()
        } else {
            format!("Apply this change (auto-reverts in {} unless confirmed)?", ui::duration_ms(ttl.as_millis() as i64))
        };
        if !ui::confirm(&q) {
            ui.info("nothing changed");
            return Err(Exit(1).into());
        }
    }
    let description = message.map(str::to_string).unwrap_or_else(|| default_description(&ops));
    let ttl_secs = u32::try_from(ttl.as_secs()).unwrap_or(u32::MAX);
    let raw = bus::apply_change(d, &env, &description, ttl_secs).map_err(|e| needs_daemon("apply this change", e))?;
    let cs = parse_changeset(&raw, "ApplyChange")?;
    if ui.json {
        ui.print_json(&cs)?;
    } else {
        out(&applied_message(ui, &cs, now_ms()));
    }
    if cs.status == Status::Failed { Err(Exit(1).into()) } else { Ok(()) }
}

/// What `apply` prints after the daemon answered.
pub fn applied_message(ui: &Ui, cs: &ChangeSet, now: i64) -> String {
    let flag = sys_flag(cs.scope);
    if cs.status == Status::Failed {
        return format!(
            "{} change #{} failed and was rolled back: {}",
            ui.fail_mark(),
            cs.id,
            cs.error.clone().unwrap_or_else(|| "unknown error".into())
        );
    }
    let mut s = format!(
        "{} change #{} applied {}",
        ui.ok_mark(),
        cs.id,
        ui.dim(format!("({} scope, {} risk)", cs.scope, risk_word(cs.risk)))
    );
    if let Some(ms) = ttl_left(cs, now) {
        s.push_str(&format!(
            "\n  {}",
            ui.yellow(format!(
                "auto-reverts in {} unless `lindeskctl changes confirm{flag} {}`",
                ui::duration_ms(ms),
                cs.id
            ))
        ));
    }
    s.push_str(&format!("\n  {}", ui.dim(format!("undo: lindeskctl changes revert{flag} {}", cs.id))));
    s
}

fn confirm(ui: &Ui, id: u64) -> Result<()> {
    let d = Daemon::for_scope(ui.system);
    bus::confirm_change(d, id).map_err(|e| needs_daemon("confirm it", e))?;
    if ui.json {
        return ui.print_json(&json!({"id": id, "confirmed": true}));
    }
    out(&format!(
        "{} change #{id} confirmed: it stays (undo any time: lindeskctl changes revert{} {id})",
        ui.ok_mark(),
        if ui.system { " --system" } else { "" }
    ));
    Ok(())
}

fn revert(ui: &Ui, id: u64) -> Result<()> {
    let d = Daemon::for_scope(ui.system);
    let raw = bus::revert_change(d, id).map_err(|e| needs_daemon("revert it", e))?;
    let cs = parse_changeset(&raw, "RevertChange")?;
    if ui.json {
        return ui.print_json(&cs);
    }
    if cs.status == Status::Failed {
        out(&format!("{} reverting #{id} failed: {}", ui.fail_mark(), cs.error.unwrap_or_default()));
        return Err(Exit(1).into());
    }
    out(&format!("{} change #{id} reverted {}", ui.ok_mark(), ui.dim(format!("(as change #{})", cs.id))));
    Ok(())
}

fn revert_to(ui: &Ui, version: u64, yes: bool) -> Result<()> {
    let d = Daemon::for_scope(ui.system);
    let list = bus::list_changes(d, 10_000).map_err(|e| needs_daemon("revert", e)).and_then(|j| parse_list(&j))?;
    let mut undo: Vec<&ChangeSet> = list.iter().filter(|c| c.id > version && c.is_effective()).collect();
    undo.sort_by_key(|c| std::cmp::Reverse(c.id));
    if undo.is_empty() {
        if ui.json {
            return ui.print_json(&json!({"version": version, "reverted": []}));
        }
        out(&ui.dim(format!("nothing to undo after version {version}")));
        return Ok(());
    }
    if !ui.json {
        let now = now_ms();
        let mut s = format!(
            "{}\n",
            ui.bold(format!("revert to version {version}: undo {} change(s), newest first", undo.len()))
        );
        for c in &undo {
            s.push_str(&format!("  #{:<4} {}  {}\n", c.id, status_colored(ui, c, now), c.description));
        }
        out(s.trim_end());
    }
    if !yes {
        if !ui.stdin_tty {
            return Err(Usage("not a terminal: pass --yes to revert without confirmation".into()).into());
        }
        if !ui::confirm("Revert these?") {
            ui.info("nothing changed");
            return Err(Exit(1).into());
        }
    }
    let mut done = Vec::new();
    for c in undo {
        let raw = bus::revert_change(d, c.id)?;
        let r = parse_changeset(&raw, "RevertChange")?;
        if !ui.json {
            out(&format!("{} #{} reverted {}", ui.ok_mark(), c.id, ui.dim(format!("(as #{})", r.id))));
        }
        done.push(json!({"id": c.id, "by": r.id}));
    }
    if ui.json {
        return ui.print_json(&json!({"version": version, "reverted": done}));
    }
    Ok(())
}

// ------------------------------------------------------------------- suggested commands

fn quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-:/=@,+%".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// The flags that express `ops` (the part after `lindeskctl changes apply`), or a JSON
/// pipe when an op has no shortcut.
pub fn command_line_tail(ops: &[ChangeOp], system: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut zswap_seen = false;
    let mut persist = false;
    for op in ops {
        match op {
            ChangeOp::ConfigSet { key, value, .. } => {
                let v = match value {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                parts.push(format!("--set {}", quote(&format!("{key}={v}"))));
            }
            ChangeOp::ConfigUnset { key, .. } => parts.push(format!("--unset {}", quote(key))),
            ChangeOp::ProfileUse { name, .. } => parts.push(format!("--profile {}", quote(name))),
            ChangeOp::ClassOverride { unit, class, .. } => {
                parts.push(format!("--class {}", quote(&format!("{unit}={class}"))))
            }
            ChangeOp::Sysctl { key, value, persist: p } => {
                persist |= *p;
                parts.push(format!("--sysctl {}", quote(&format!("{key}={value}"))));
            }
            ChangeOp::IoScheduler { device, scheduler, persist: p } => {
                persist |= *p;
                parts.push(format!("--scheduler {}", quote(&format!("{device}={scheduler}"))));
            }
            ChangeOp::Zswap { enabled, compressor, max_pool_percent } if !zswap_seen => {
                zswap_seen = true;
                if let Some(e) = enabled {
                    parts.push(format!("--zswap {}", if *e { "on" } else { "off" }));
                }
                if let Some(c) = compressor {
                    parts.push(format!("--zswap-compressor {}", quote(c)));
                }
                if let Some(p) = max_pool_percent {
                    parts.push(format!("--zswap-max-pool {p}"));
                }
            }
            ChangeOp::Zswap { .. } => {
                // Two zswap ops can't be expressed with flags: fall back to JSON.
                let json = serde_json::to_string(ops).unwrap_or_default();
                return format!("- <<< {}", quote(&json));
            }
            ChangeOp::CpuEpp { preference } => parts.push(format!("--epp {}", quote(preference))),
            ChangeOp::CpuGovernor { governor } => parts.push(format!("--governor {}", quote(governor))),
            ChangeOp::ServiceState { .. } => {
                // No flag for it: `lindeskctl setup conflicts` makes these; fall back to JSON.
                let json = serde_json::to_string(ops).unwrap_or_default();
                return format!("- <<< {}", quote(&json));
            }
        }
    }
    if persist {
        parts.push("--persist".into());
    }
    if system && ops.iter().any(ChangeOp::is_config) {
        parts.insert(0, "--system".into());
    }
    parts.join(" ")
}

/// A ready-to-run `lindeskctl changes apply …` for proposed ops (JSON values).
pub fn suggest_apply(ops: &[Value], description: &str) -> Option<String> {
    let parsed: Vec<ChangeOp> = ops.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect();
    if parsed.is_empty() || parsed.len() != ops.len() {
        return None;
    }
    Some(format!("lindeskctl changes apply {} --ttl 10m -m {}", command_line_tail(&parsed, false), quote(description)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_changes::{Author, OpRecord, Origin, Rejection, Rule};

    fn cs(id: u64, status: Status, ttl_until: Option<i64>, updated: i64) -> ChangeSet {
        ChangeSet {
            id,
            parent: None,
            ts: 1_000_000,
            updated,
            author: Author::new(1000, Origin::Mcp).with_client("claude-code"),
            origin: Origin::Mcp,
            scope: Scope::System,
            description: "less swapping".into(),
            risk: Risk::Medium,
            ops: vec![OpRecord {
                op: ChangeOp::Sysctl { key: "vm.swappiness".into(), value: "10".into(), persist: false },
                previous: json!("60"),
                applied: json!("10"),
                done: true,
            }],
            status,
            ttl_until,
            reverts: None,
            revert_to: None,
            reverted_by: None,
            error: None,
            config_snapshot: None,
        }
    }

    #[test]
    fn values_and_ops() {
        assert_eq!(parse_value("400ms"), json!("400ms"));
        assert_eq!(parse_value("true"), json!(true));
        assert_eq!(parse_value("5"), json!(5));
        assert_eq!(parse_value("0.5"), json!(0.5));
        assert_eq!(parse_value("\"auto\""), json!("auto"));
        assert_eq!(parse_value("[1, 2]"), json!([1, 2]));
        let args =
            OpsArgs { set: vec![("plugins.threshold.config.epoch".into(), "400ms".into())], ..Default::default() };
        let ops = build_ops(&args, false).unwrap();
        assert_eq!(
            serde_json::to_value(&ops[0]).unwrap(),
            json!({"op": "config_set", "scope": "user", "key": "plugins.threshold.config.epoch", "value": "400ms"})
        );
        assert_eq!(daemon_for(&ops, false).unwrap(), Daemon::Session);
        let sys = OpsArgs { sysctl: vec![("vm.swappiness".into(), "10".into())], ..Default::default() };
        let ops = build_ops(&sys, false).unwrap();
        assert_eq!(daemon_for(&ops, false).unwrap(), Daemon::Broker, "system ops go to lindeskd");
        let mixed = OpsArgs { sysctl: sys.sysctl.clone(), set: args.set.clone(), ..Default::default() };
        let e = daemon_for(&build_ops(&mixed, false).unwrap(), false).unwrap_err();
        assert!(e.to_string().contains("mixes your config"), "{e}");
        assert_eq!(daemon_for(&build_ops(&mixed, true).unwrap(), true).unwrap(), Daemon::Broker);
        assert!(build_ops(&OpsArgs::default(), false).unwrap_err().downcast_ref::<Usage>().is_some());
        let env: Value = serde_json::from_str(&envelope(&ops)).unwrap();
        assert_eq!(env["origin"], "cli");
        assert_eq!(env["ops"][0]["op"], "sysctl");
    }

    #[test]
    fn suggested_command_lines() {
        let ops = vec![
            json!({"op": "sysctl", "key": "vm.swappiness", "value": "10"}),
            json!({"op": "zswap", "enabled": true, "compressor": "zstd", "max_pool_percent": 20}),
        ];
        assert_eq!(
            suggest_apply(&ops, "Swap on a hard disk").unwrap(),
            "lindeskctl changes apply --sysctl vm.swappiness=10 --zswap on --zswap-compressor zstd --zswap-max-pool 20 --ttl 10m -m 'Swap on a hard disk'"
        );
        let ops = vec![json!({"op": "io_scheduler", "device": "8:0", "scheduler": "bfq", "persist": true})];
        assert_eq!(
            suggest_apply(&ops, "x").unwrap(),
            "lindeskctl changes apply --scheduler 8:0=bfq --persist --ttl 10m -m x"
        );
        let ops =
            vec![json!({"op": "class_override", "scope": "user", "unit": "app-*steam*.scope", "class": "interactive"})];
        assert!(suggest_apply(&ops, "x").unwrap().contains("--class 'app-*steam*.scope=interactive'"));
        assert!(suggest_apply(&[json!({"op": "nope"})], "x").is_none());
    }

    #[test]
    fn statuses_and_pending() {
        let now = 1_300_000;
        let a = cs(7, Status::Applied, Some(now + 8 * 60_000 + 5_000), 1_000_000);
        assert_eq!(ttl_left(&a, now), Some(485_000));
        assert_eq!(status_word(&a), "applied");
        let p = pending_of(std::slice::from_ref(&a), now);
        assert_eq!(p[0].countdown(now), "change #7 (system) reverts in 8m");
        assert_eq!(p[0].confirm_hint(), "lindeskctl changes confirm --system 7");
        let expired = cs(3, Status::Reverted, Some(1_100_000), 1_100_500);
        assert_eq!(status_word(&expired), "expired");
        let reverted = cs(4, Status::Reverted, Some(2_000_000), 1_100_500);
        assert_eq!(status_word(&reverted), "reverted");
        assert!(pending_of(&[expired, reverted, cs(5, Status::Confirmed, Some(now + 1), 0)], now).is_empty());
        assert_eq!(author_label(&a), "mcp:claude-code");
        assert_eq!(ago(now - 125_000, now), "2m ago");
        assert_eq!(ago(now - 5_000, now), "5s ago");
    }

    #[test]
    fn renders_list_show_plan() {
        let ui = Ui::plain();
        let now = 1_300_000;
        let list = vec![
            cs(7, Status::Applied, Some(now + 600_000), 1_000_000),
            cs(6, Status::Confirmed, None, 1_000_000),
            ChangeSet { error: Some("write failed".into()), ..cs(5, Status::Failed, None, 1_000_000) },
        ];
        let s = render_list(&ui, &list, now);
        let lines: Vec<&str> = s.lines().collect();
        assert!(
            lines[0].starts_with("ID  WHEN") && lines[0].contains("STATUS") && lines[0].contains("DESCRIPTION"),
            "{s}"
        );
        assert!(
            lines[1].contains("#7") && lines[1].contains("mcp:claude-code") && lines[1].contains("◐ applied"),
            "{s}"
        );
        assert!(lines[1].contains("10m00s") && lines[1].contains("less swapping"), "{s}");
        assert!(
            lines[2].contains("✓ confirmed") && lines[3].contains("✗ failed") && lines[3].contains("(write failed)"),
            "{s}"
        );
        let show = render_show(&ui, &list[0], now);
        assert!(show.starts_with("change #7  ◐ applied  ● MEDIUM risk · system scope"), "{show}");
        assert!(show.contains("sysctl  vm.swappiness  60      10"), "{show}");
        assert!(show.contains("auto-reverts in 10m00s"), "{show}");
        assert!(show.contains("keep: lindeskctl changes confirm --system 7"), "{show}");
        let mut plan: Plan = serde_json::from_value(json!({
            "scope": "system",
            "ops": [{"op": "sysctl", "key": "vm.swappiness", "value": "500", "risk": "medium", "current": "60", "target": "500"}],
            "risk": "medium",
            "diff": "--- a/proc/sys/vm/swappiness\n+++ b/proc/sys/vm/swappiness\n-60\n+500\n",
            "rejections": [],
            "ttl_required": false
        }))
        .unwrap();
        plan.rejections.push(Rejection::new(Some(0), Rule::OutOfRange, "vm.swappiness=500 outside 0-200"));
        let s = render_plan(&ui, &plan);
        assert!(s.starts_with("plan  ● MEDIUM risk · system scope · 1 op"), "{s}");
        assert!(s.contains("sysctl  vm.swappiness  medium  ✗ rejected"), "{s}");
        assert!(
            s.contains("+500") && s.contains("✗ rejected: op 0: vm.swappiness=500 outside 0-200  [out_of_range]"),
            "{s}"
        );
        plan.rejections.clear();
        assert!(render_plan(&ui, &plan).contains("60 → 500"));
        let msg = applied_message(&ui, &cs(7, Status::Applied, Some(now + 600_000), 0), now);
        assert!(msg.contains("auto-reverts in 10m00s unless `lindeskctl changes confirm --system 7`"), "{msg}");
    }

    #[test]
    fn parses_daemon_lists() {
        let one = serde_json::to_value(cs(1, Status::Applied, None, 0)).unwrap();
        assert_eq!(parse_list(&json!([one]).to_string()).unwrap().len(), 1);
        assert_eq!(parse_list(&json!({"changes": [one]}).to_string()).unwrap().len(), 1);
        assert!(parse_list("3").is_err());
        assert_eq!(value_short(&json!({"runtime": "10", "persisted": "10"})), "10 (persisted)");
        assert_eq!(value_short(&json!({"policy0": "performance", "policy1": "performance"})), "performance (all 2)");
        assert_eq!(value_short(&Value::Null), "(unset)");
    }
}
