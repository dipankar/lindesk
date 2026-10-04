//! `lindeskctl plugin …`: scaffold, test, install and inspect plugins.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use lindesk_host::conformance::{ConformanceOptions, ConformanceReport, run_conformance_with};
use lindesk_host::discovery::{self, DiscoveryOptions, RootOwned};
use lindesk_host::{Lang, PluginHealth};
use lindesk_plugin_api::{PluginKind, Privilege};
use serde::Serialize;
use serde_json::json;

use crate::bus::{self, Daemon};
use crate::cli::{KindArg, LangArg, PluginCmd};
use crate::cmd::config::set_plugin_enabled_file;
use crate::cmd::doctor::tilde;
use crate::cmd::status::{self, origin_of_path, state_colored};
use crate::paths;
use crate::ui::{Exit, Table, Ui, closest, micros, out};

/// Built-in plugin names and kinds (shown when the daemon can't be asked).
pub const BUILTINS: &[(&str, &str)] = &[
    ("psi", "sensor"),
    ("cgroup-stat", "sensor"),
    ("rules", "classifier"),
    ("threshold", "policy"),
    ("systemd", "actuator"),
    ("cgroup-v2-raw", "actuator"),
    ("cgroup-v1", "actuator"),
    ("proc", "actuator"),
    ("x11", "adapter"),
    ("cosmic", "adapter"),
    ("parquet-store", "sink"),
];

pub fn run(ui: &Ui, cmd: &PluginCmd) -> Result<()> {
    match cmd {
        PluginCmd::New { kind, lang, name, dir } => new(ui, *kind, *lang, name, dir),
        PluginCmd::Test { dir, timeout_scale } => test(ui, dir, *timeout_scale),
        PluginCmd::List { all } => list(ui, *all),
        PluginCmd::Info { name } => info(ui, name),
        PluginCmd::Enable { name } => enable(ui, name, true),
        PluginCmd::Disable { name } => enable(ui, name, false),
        PluginCmd::Install { dir, force, timeout_scale } => install(ui, dir, *force, *timeout_scale),
        PluginCmd::Uninstall { name } => uninstall(ui, name),
        PluginCmd::Logs { name, follow, lines } => logs(ui, name, *follow, *lines),
        PluginCmd::Reload => reload(ui),
        PluginCmd::Schema => {
            crate::ui::out_raw(&serde_json::to_string_pretty(&lindesk_plugin_api::protocol_schema())?);
            Ok(())
        }
    }
}

// --------------------------------------------------------------------------------- new

fn new(ui: &Ui, kind: KindArg, lang: LangArg, name: &str, dir: &Path) -> Result<()> {
    let kind: PluginKind = kind.into();
    let lang: Lang = lang.into();
    let dest = dir.join(name);
    let files = lindesk_host::scaffold(kind, lang, name, &dest).map_err(|e| anyhow::anyhow!("{e}"))?;
    if ui.json {
        return ui.print_json(&json!({"dir": dest, "kind": kind, "lang": lang, "files": files}));
    }
    let shown = if dir == Path::new(".") { PathBuf::from(name) } else { dest.clone() };
    let mut s = format!("{} created {} ({kind} plugin, {lang})\n", ui.ok_mark(), ui.bold(shown.display()));
    for f in &files {
        let rel = f.strip_prefix(&dest).unwrap_or(f);
        s.push_str(&format!("    {}\n", ui.dim(rel.display())));
    }
    let module = name.replace('-', "_");
    let system = kind == PluginKind::Actuator;
    s.push_str("\nNext steps:\n");
    let mut steps: Vec<(String, &str)> = vec![(format!("cd {}", shown.display()), "")];
    match lang {
        Lang::Python => steps.push((format!("python3 test_{module}.py"), "unit tests")),
        Lang::Shell => steps.push(("./test.sh".into(), "unit tests")),
        Lang::Rust => {
            steps.push(("cargo test".into(), "unit tests"));
            steps.push(("cargo build --release".into(), "plugin.toml's exec points at target/release"));
        }
    }
    steps.push(("lindeskctl plugin test .".into(), "protocol conformance"));
    if kind == PluginKind::Policy {
        steps.push(("lindeskctl simulate --policy . --from -1h".into(), "replay your recent telemetry through it"));
    }
    if system {
        steps.push(("sudo lindeskctl plugin install --system .".into(), "actuators run in lindeskd"));
    } else {
        steps.push(("lindeskctl plugin install .".into(), "install for your session and reload"));
    }
    let w = steps.iter().map(|(c, _)| c.chars().count()).max().unwrap_or(0) + 3;
    for (c, why) in steps {
        if why.is_empty() {
            s.push_str(&format!("  {}\n", ui.cyan(c)));
        } else {
            s.push_str(&format!("  {}{}\n", ui.cyan(crate::ui::pad(&c, w)), ui.dim(format!("# {why}"))));
        }
    }
    out(&s);
    Ok(())
}

// -------------------------------------------------------------------------------- test

/// The conformance report, coloured.
pub fn render_report(ui: &Ui, r: &ConformanceReport) -> String {
    let kinds: Vec<&str> = r.kinds.iter().map(|k| k.as_str()).collect();
    let mut s = format!(
        "plugin conformance: {} {} ({})\n",
        ui.bold(r.plugin.as_deref().unwrap_or("?")),
        r.version.as_deref().unwrap_or(""),
        kinds.join(", ")
    );
    for c in &r.checks {
        let mark = if c.ok { ui.ok_mark() } else { ui.fail_mark() };
        s.push_str(&format!("  {mark} {} {}\n", c.name, ui.dim(format!("({:.1}ms)", c.duration_ms))));
        if let Some(d) = &c.detail {
            for l in d.lines() {
                s.push_str(&format!("      {}\n", if c.ok { l.to_string() } else { ui.red(l) }));
            }
        }
    }
    let failed = r.failures().count();
    let summary = format!("{} passed, {failed} failed", r.checks.len() - failed);
    s.push_str(&if failed == 0 && !r.checks.is_empty() { ui.green(summary) } else { ui.red(summary) });
    s.push('\n');
    if failed > 0 && !r.logs.is_empty() {
        s.push_str("plugin output (most recent last):\n");
        let n = r.logs.len();
        for l in &r.logs[n.saturating_sub(15)..] {
            s.push_str(&format!("  {}\n", ui.dim(format!("[{:?}/{:?}] {}", l.source, l.level, l.message))));
        }
    }
    s
}

fn conformance(dir: &Path, timeout_scale: f64) -> ConformanceReport {
    // The plugin runs in its own directory: hand the host an absolute path.
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    run_conformance_with(&dir, &ConformanceOptions { timeout_scale, config: None })
}

fn rust_hint(dir: &Path, r: &ConformanceReport) -> Option<String> {
    let manifest_failed = r.checks.first().is_some_and(|c| !c.ok);
    (manifest_failed && dir.join("Cargo.toml").is_file())
        .then(|| "this is a Rust plugin: build it first (cargo build --release) or use `lindeskctl plugin install`, which builds it".into())
}

fn test(ui: &Ui, dir: &Path, timeout_scale: f64) -> Result<()> {
    if !dir.exists() {
        bail!(
            "{} doesn't exist. Create a plugin with: lindeskctl plugin new --kind <kind> --lang python <name>",
            dir.display()
        );
    }
    let r = conformance(dir, timeout_scale);
    if ui.json {
        ui.print_json(&r)?;
    } else {
        out(&render_report(ui, &r));
        if let Some(h) = rust_hint(dir, &r) {
            ui.info(ui.yellow(format!("hint: {h}")));
        }
    }
    if r.passed() { Ok(()) } else { Err(Exit(1).into()) }
}

// -------------------------------------------------------------------------- discovery

fn host(system: bool) -> Privilege {
    if system { Privilege::System } else { Privilege::User }
}

fn discover(system: bool) -> Result<(discovery::Discovery, lindesk_core::config::Config)> {
    let config = paths::load_config(&paths::config_path(system))?;
    let host = host(system);
    let mut dirs = discovery::default_plugin_dirs(host);
    dirs.extend(config.general.plugin_dirs.clone());
    let found = discovery::discover(&DiscoveryOptions { host, dirs, config: &config, ownership: &RootOwned });
    Ok((found, config))
}

/// One row of `plugin list`.
#[derive(Serialize, Debug, Clone)]
pub struct Row {
    pub name: String,
    pub kinds: Vec<String>,
    pub origin: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<lindesk_host::PluginStats>,
}

/// Merge discovery with live health. `live = None` means the daemon isn't reachable.
pub fn merge(found: &discovery::Discovery, live: Option<&[PluginHealth]>, all: bool) -> Vec<Row> {
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    for h in live.unwrap_or_default() {
        let (origin, path) = match &h.origin {
            lindesk_host::Origin::Builtin => ("builtin".to_string(), None),
            lindesk_host::Origin::External { manifest } => {
                (origin_of_path(manifest).to_string(), manifest.parent().map(Path::to_path_buf))
            }
        };
        rows.insert(
            h.name.clone(),
            Row {
                name: h.name.clone(),
                kinds: h.kinds.iter().map(|k| k.as_str().to_string()).collect(),
                origin,
                state: h.state.clone(),
                detail: h.detail.clone(),
                path,
                stats: Some(h.stats.clone()),
            },
        );
    }
    for p in &found.plugins {
        if rows.contains_key(&p.name) {
            if let Some(r) = rows.get_mut(&p.name) {
                r.path.get_or_insert_with(|| p.dir.clone());
            }
            continue;
        }
        let (state, detail) = if !p.enabled {
            ("disabled".to_string(), p.disabled_reason.clone())
        } else if live.is_some() {
            ("not loaded".to_string(), Some("installed after the daemon started: lindeskctl plugin reload".into()))
        } else {
            ("stopped".to_string(), Some("daemon not running".into()))
        };
        rows.insert(
            p.name.clone(),
            Row {
                name: p.name.clone(),
                kinds: p.manifest.plugin.kinds.iter().map(|k| k.as_str().to_string()).collect(),
                origin: origin_of_path(&p.dir).to_string(),
                state,
                detail,
                path: Some(p.dir.clone()),
                stats: None,
            },
        );
    }
    let mut out: Vec<Row> = rows.into_values().collect();
    // Built-ins first, then by name.
    out.sort_by_key(|r| (r.origin != "builtin", r.name.clone()));
    for r in &found.rejected {
        if r.skipped && !all {
            continue;
        }
        out.push(Row {
            name: r.name.clone().unwrap_or_else(|| r.path.file_name().unwrap_or_default().to_string_lossy().into()),
            kinds: Vec::new(),
            origin: origin_of_path(&r.path).to_string(),
            state: if r.skipped { "skipped".into() } else { "rejected".into() },
            detail: Some(r.reason.clone()),
            path: Some(r.path.clone()),
            stats: None,
        });
    }
    out
}

fn live_plugins(system: bool) -> Option<(Daemon, Vec<PluginHealth>)> {
    let f = status::fetch(system).ok()?;
    if !system && f.source != Daemon::Session {
        // The broker's plugins are not the session's; don't mix them into a user listing.
        return None;
    }
    Some((f.source, f.status?.plugins))
}

fn list(ui: &Ui, all: bool) -> Result<()> {
    let (found, _) = discover(ui.system)?;
    let live = live_plugins(ui.system);
    let rows = merge(&found, live.as_ref().map(|(_, p)| p.as_slice()), all);
    if ui.json {
        return ui.print_json(&rows);
    }
    let mut t = Table::new(&["NAME", "KINDS", "ORIGIN", "STATE", "CALLS", "ERRORS", "P99", "DETAIL"]).right(&[4, 5, 6]);
    for r in &rows {
        let (calls, errors, p99) = match &r.stats {
            Some(s) => (
                s.calls.to_string(),
                if s.errors > 0 { ui.yellow(s.errors) } else { "0".into() },
                if s.calls > 0 { micros(s.latency_p99_us) } else { "-".into() },
            ),
            None => ("-".into(), "-".into(), "-".into()),
        };
        let detail = r.detail.clone().or_else(|| r.path.as_ref().map(|p| tilde(p))).unwrap_or_default();
        t.row(vec![
            r.name.clone(),
            r.kinds.join(","),
            r.origin.clone(),
            state_colored(ui, &r.state),
            calls,
            errors,
            p99,
            detail,
        ]);
    }
    if t.is_empty() {
        out("no plugins found");
    } else {
        out(&t.render(ui));
    }
    if live.is_none() && !ui.quiet {
        let d = Daemon::for_scope(ui.system);
        ui.info(ui.dim(format!(
            "{} isn't running, so built-in plugins and live health aren't shown. Start it: {}",
            d.name(),
            d.start_hint()
        )));
    }
    Ok(())
}

// -------------------------------------------------------------------------------- info

fn info(ui: &Ui, name: &str) -> Result<()> {
    let (found, config) = discover(ui.system)?;
    let live = live_plugins(ui.system);
    let health = live.as_ref().and_then(|(_, ps)| ps.iter().find(|p| p.name == name).cloned());
    let disc = found.plugins.iter().find(|p| p.name == name);
    let rejected = found.rejected.iter().find(|r| r.name.as_deref() == Some(name));
    let builtin = BUILTINS.iter().find(|(n, _)| *n == name);
    if disc.is_none() && health.is_none() && rejected.is_none() && builtin.is_none() {
        let mut names: Vec<String> = found.plugins.iter().map(|p| p.name.clone()).collect();
        names.extend(BUILTINS.iter().map(|(n, _)| n.to_string()));
        if let Some((_, ps)) = &live {
            names.extend(ps.iter().map(|p| p.name.clone()));
        }
        let hint = closest(name, names.iter().map(String::as_str))
            .map(|c| format!(" Did you mean '{c}'?"))
            .unwrap_or_default();
        bail!("no plugin named '{name}'.{hint} See: lindeskctl plugin list");
    }
    let cfg = config.plugins.get(name).cloned();
    if ui.json {
        return ui.print_json(&json!({
            "name": name,
            "builtin": builtin.is_some() && disc.is_none(),
            "discovered": disc,
            "rejected": rejected,
            "config": cfg,
            "health": health,
        }));
    }
    let mut s = String::new();
    if let Some(p) = disc {
        let m = &p.manifest.plugin;
        let kinds: Vec<&str> = m.kinds.iter().map(|k| k.as_str()).collect();
        s.push_str(&format!("{} {}  ({})\n", ui.bold(&m.name), m.version, kinds.join(", ")));
        if !m.description.is_empty() {
            s.push_str(&format!("  {}\n", m.description));
        }
        s.push_str(&format!("  {:<11}{}\n", "manifest", tilde(&p.manifest_path)));
        s.push_str(&format!(
            "  {:<11}{}{}\n",
            "exec",
            tilde(&p.exec),
            if m.args.is_empty() { String::new() } else { format!(" {}", m.args.join(" ")) }
        ));
        s.push_str(&format!("  {:<11}{:?}\n", "privilege", m.privilege).to_lowercase());
        s.push_str(&format!(
            "  {:<11}{}\n",
            "enabled",
            if p.enabled {
                "yes".to_string()
            } else {
                format!("no ({})", p.disabled_reason.clone().unwrap_or_default())
            }
        ));
        for k in &m.kinds {
            if matches!(k, PluginKind::Classifier | PluginKind::Policy) {
                let src = if p.priority_override.is_some() { "config override" } else { "manifest" };
                s.push_str(&format!("  {:<11}{} ({k}, from {src})\n", "priority", p.priority(*k)));
            }
        }
        if let Some(c) = &p.manifest.classifier
            && !c.prefilter.is_empty()
        {
            s.push_str(&format!("  {:<11}{}\n", "prefilter", serde_json::to_string(&c.prefilter)?));
        }
        if !p.config.is_null() {
            s.push_str(&format!("  {:<11}{}\n", "config", serde_json::to_string(&p.config)?));
        }
    } else if let Some(r) = rejected {
        s.push_str(&format!("{}  {}\n", ui.bold(name), ui.red(if r.skipped { "skipped" } else { "rejected" })));
        s.push_str(&format!("  {:<11}{}\n  {:<11}{}\n", "path", tilde(&r.path), "reason", r.reason));
    } else {
        let kinds = health.as_ref().map(|h| h.kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", "));
        s.push_str(&format!(
            "{}  (built-in{})\n",
            ui.bold(name),
            kinds.or(builtin.map(|b| b.1.to_string())).map(|k| format!(" {k}")).unwrap_or_default()
        ));
        if let Some(c) = &cfg
            && !c.config.is_null()
        {
            s.push_str(&format!("  {:<11}{}\n", "config", serde_json::to_string(&c.config)?));
        }
    }
    match &health {
        Some(h) => {
            s.push_str(&format!("  {:<11}{}", "health", state_colored(ui, &h.state)));
            if let Some(d) = &h.detail {
                s.push_str(&format!(" ({d})"));
            }
            if let Some(pid) = h.pid {
                s.push_str(&format!(" pid {pid}"));
            }
            s.push('\n');
            let st = &h.stats;
            s.push_str(&format!(
                "  {:<11}calls {}  errors {}  timeouts {}  failures {}  restarts {}  p99 {}  avg {}\n",
                "stats",
                st.calls,
                st.errors,
                st.timeouts,
                st.failures,
                st.restarts,
                micros(st.latency_p99_us),
                micros(st.latency_ewma_us as u64)
            ));
            if let Some(e) = &st.last_error {
                let at = st.last_error_at.map(|t| format!(" at {}", crate::ui::datetime(t))).unwrap_or_default();
                s.push_str(&format!("  {:<11}{}{}\n", "last error", ui.red(e), ui.dim(at)));
            }
        }
        None => {
            let d = Daemon::for_scope(ui.system);
            let why = if live.is_some() {
                "not loaded by the daemon".to_string()
            } else {
                format!("{} not running", d.name())
            };
            s.push_str(&format!("  {:<11}{}\n", "health", ui.dim(why)));
        }
    }
    s.push_str(&ui.dim(format!("  logs: lindeskctl plugin logs {name}{}", if ui.system { " --system" } else { "" })));
    out(&s);
    Ok(())
}

// ----------------------------------------------------------------------------- reload

/// Ask the daemon for the scope to reload. Returns a human description of what happened.
pub fn request_reload(system: bool) -> Result<String> {
    if system {
        return match bus::reach(Daemon::Broker) {
            bus::Reach::Running => {
                bus::broker_reload()?;
                Ok("lindeskd reloaded its config and plugins".into())
            }
            _ => Ok("lindeskd isn't running; the change applies when it starts".into()),
        };
    }
    // lindesk-session re-reads its config and plugin dirs on SIGHUP (or a restart).
    let unit = "lindesk-session.service";
    let active = Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", unit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if active {
        let o = Command::new("systemctl").args(["--user", "kill", "-s", "HUP", unit]).output()?;
        if o.status.success() {
            return Ok("sent SIGHUP to lindesk-session; it re-reads its config and plugins".into());
        }
        bail!("systemctl --user kill -s HUP {unit} failed: {}", String::from_utf8_lossy(&o.stderr).trim());
    }
    // Running outside systemd? Find it by its bus name.
    if let Ok(conn) = bus::running(Daemon::Session)
        && let Ok(dbus) = zbus::blocking::fdo::DBusProxy::new(&conn)
        && let Ok(name) = zbus::names::BusName::try_from(Daemon::Session.bus_name())
        && let Ok(pid) = dbus.get_connection_unix_process_id(name)
    {
        // SAFETY: plain kill(2).
        if unsafe { libc::kill(pid as i32, libc::SIGHUP) } == 0 {
            return Ok(format!("sent SIGHUP to lindesk-session (pid {pid}); it re-reads its config and plugins"));
        }
    }
    Ok("lindesk-session isn't running; the change applies when it starts".into())
}

fn reload(ui: &Ui) -> Result<()> {
    let msg = request_reload(ui.system)?;
    if ui.json {
        return ui.print_json(&json!({"result": msg}));
    }
    ui.info(format!("{} {msg}", ui.ok_mark()));
    Ok(())
}

// ----------------------------------------------------------------------- enable/disable

fn enable(ui: &Ui, name: &str, on: bool) -> Result<()> {
    if ui.system {
        paths::require_root(if on { "enabling a system plugin" } else { "disabling a system plugin" })?;
    }
    let (found, _) = discover(ui.system).unwrap_or_default();
    let known = found.plugins.iter().any(|p| p.name == name)
        || found.rejected.iter().any(|r| r.name.as_deref() == Some(name))
        || BUILTINS.iter().any(|(n, _)| *n == name);
    if !known {
        let names: Vec<&str> =
            found.plugins.iter().map(|p| p.name.as_str()).chain(BUILTINS.iter().map(|(n, _)| *n)).collect();
        let hint = closest(name, names).map(|c| format!(" (did you mean '{c}'?)")).unwrap_or_default();
        ui.warn(format!("no plugin named '{name}' is installed or built in{hint}; writing the setting anyway"));
    }
    let path = paths::config_path(ui.system);
    let changed = set_plugin_enabled_file(&path, name, on)?;
    let reload = if changed { Some(request_reload(ui.system)?) } else { None };
    if ui.json {
        return ui
            .print_json(&json!({"name": name, "enabled": on, "config": path, "changed": changed, "reload": reload}));
    }
    let word = if on { "enabled" } else { "disabled" };
    if changed {
        ui.info(format!("{} {name} {word} in {}", ui.ok_mark(), tilde(&path)));
    } else {
        ui.info(format!("{name} was already {word} in {}", tilde(&path)));
    }
    if let Some(r) = reload {
        ui.info(ui.dim(r));
    }
    Ok(())
}

// ----------------------------------------------------------------------------- install

const SKIP_DIRS: &[&str] = &["target", ".git", "__pycache__", "node_modules", ".venv", ".mypy_cache", ".pytest_cache"];

/// Copy a directory tree, following symlinks, skipping build/VCS directories.
pub fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    for e in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let e = e?;
        let name = e.file_name();
        let from = e.path();
        let to = dst.join(&name);
        let meta = std::fs::metadata(&from).with_context(|| format!("reading {}", from.display()))?;
        if meta.is_dir() {
            if SKIP_DIRS.iter().any(|s| name == *s) {
                continue;
            }
            copy_tree(&from, &to)?;
        } else if meta.is_file() {
            std::fs::copy(&from, &to).with_context(|| format!("copying {}", from.display()))?;
        }
    }
    Ok(())
}

/// Clear group/world write bits (system plugins must not be writable by others).
fn harden(dir: &Path) -> Result<()> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755))?;
        for e in std::fs::read_dir(&d)? {
            let p = e?.path();
            let m = std::fs::symlink_metadata(&p)?;
            if m.is_dir() {
                stack.push(p);
            } else {
                let exec = m.permissions().mode() & 0o111 != 0;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }))?;
            }
        }
    }
    Ok(())
}

fn rewrite_exec(manifest: &Path, exec: &str) -> Result<()> {
    let text = std::fs::read_to_string(manifest)?;
    let mut doc: toml_edit::DocumentMut = text.parse().with_context(|| format!("parsing {}", manifest.display()))?;
    doc["plugin"]["exec"] = toml_edit::value(exec);
    std::fs::write(manifest, doc.to_string())?;
    Ok(())
}

fn install(ui: &Ui, dir: &Path, force: bool, timeout_scale: f64) -> Result<()> {
    if ui.system {
        paths::require_root("installing a system plugin")?;
    }
    let dir = std::fs::canonicalize(dir).with_context(|| format!("{} doesn't exist", dir.display()))?;
    let manifest_path = dir.join(discovery::MANIFEST);
    let manifest = discovery::load_manifest(&manifest_path).map_err(|e| anyhow::anyhow!("{e}"))?;
    let name = manifest.plugin.name.clone();
    if manifest.plugin.privilege == Privilege::System && !ui.system {
        bail!(
            "{name} is a privilege = \"system\" plugin; it runs in lindeskd. Install it with: sudo lindeskctl plugin install --system {}",
            dir.display()
        );
    }
    let rust = dir.join("Cargo.toml").is_file();
    if rust {
        ui.info(format!("building {name} (cargo build --release)…"));
        let cargo = paths::which("cargo").context("this is a Rust plugin but cargo isn't on PATH")?;
        let status = Command::new(cargo)
            .args(["build", "--release"])
            .current_dir(&dir)
            .stdout(if ui.json { std::process::Stdio::null() } else { std::process::Stdio::inherit() })
            .status()
            .context("running cargo")?;
        if !status.success() {
            bail!("cargo build --release failed in {}", dir.display());
        }
    }
    ui.info(format!("testing {name} (protocol conformance)…"));
    let report = conformance(&dir, timeout_scale);
    if !report.passed() {
        if ui.json {
            ui.print_json(&report)?;
        } else {
            out(&render_report(ui, &report));
        }
        bail!("{name} failed conformance; not installed. Fix it, then run: lindeskctl plugin test {}", dir.display());
    }
    let root = paths::install_dir(ui.system);
    let dest = root.join(&name);
    if dest.exists() && !force {
        bail!(
            "{name} is already installed at {}. Replace it with: lindeskctl plugin install --force {}",
            tilde(&dest),
            dir.display()
        );
    }
    std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
    let staging = root.join(format!(".{name}.install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let result = (|| -> Result<()> {
        if rust {
            std::fs::create_dir_all(&staging)?;
            let exec = dir.join(&manifest.plugin.exec);
            let bin = exec.file_name().context("exec has no file name")?;
            std::fs::copy(&exec, staging.join(bin)).with_context(|| format!("copying {}", exec.display()))?;
            std::fs::copy(&manifest_path, staging.join(discovery::MANIFEST))?;
            for extra in ["README.md", "LICENSE", "config.toml"] {
                if dir.join(extra).is_file() {
                    std::fs::copy(dir.join(extra), staging.join(extra))?;
                }
            }
            rewrite_exec(&staging.join(discovery::MANIFEST), &bin.to_string_lossy())?;
        } else {
            copy_tree(&dir, &staging)?;
        }
        if ui.system {
            harden(&staging)?;
        }
        discovery::inspect_dev(&staging).map_err(|r| anyhow::anyhow!("installed copy is invalid: {}", r.reason))?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    if dest.exists() {
        let old = root.join(format!(".{name}.old-{}", std::process::id()));
        std::fs::rename(&dest, &old)?;
        std::fs::rename(&staging, &dest)?;
        let _ = std::fs::remove_dir_all(&old);
    } else {
        std::fs::rename(&staging, &dest)?;
    }
    let reload = request_reload(ui.system)?;
    if ui.json {
        return ui.print_json(&json!({"name": name, "installed": dest, "conformance": report, "reload": reload}));
    }
    ui.info(format!("{} installed {name} into {}", ui.ok_mark(), tilde(&dest)));
    ui.info(ui.dim(reload));
    ui.info(ui.dim(format!("check it: lindeskctl plugin info {name}{}", if ui.system { " --system" } else { "" })));
    Ok(())
}

fn uninstall(ui: &Ui, name: &str) -> Result<()> {
    if ui.system {
        paths::require_root("removing a system plugin")?;
    }
    let root = paths::install_dir(ui.system);
    let dest = root.join(name);
    if !dest.join(discovery::MANIFEST).is_file() {
        let vendor = Path::new(paths::VENDOR_PLUGIN_DIR).join(name);
        if vendor.is_dir() {
            bail!(
                "{name} lives in {} and belongs to your package manager. Disable it instead: lindeskctl plugin disable{} {name}",
                vendor.display(),
                if ui.system { " --system" } else { "" }
            );
        }
        if !ui.system && Path::new(paths::SYSTEM_PLUGIN_DIR).join(name).is_dir() {
            bail!(
                "{name} is a system plugin in {}. Remove it with: sudo lindeskctl plugin uninstall --system {name}",
                paths::SYSTEM_PLUGIN_DIR
            );
        }
        bail!("{name} isn't installed in {}. See: lindeskctl plugin list", tilde(&root));
    }
    std::fs::remove_dir_all(&dest).with_context(|| format!("removing {}", dest.display()))?;
    let reload = request_reload(ui.system)?;
    if ui.json {
        return ui.print_json(&json!({"name": name, "removed": dest, "reload": reload}));
    }
    ui.info(format!("{} removed {name} from {}", ui.ok_mark(), tilde(&dest)));
    ui.info(ui.dim(reload));
    Ok(())
}

// -------------------------------------------------------------------------------- logs

/// journalctl arguments: the daemon's unit AND the plugin's journal field. tracing-journald
/// writes the `plugin` span/event field as `F_PLUGIN` (its default field prefix) or `PLUGIN`
/// (without a prefix); both are matched (`+` is OR in journalctl).
pub fn journal_args(system: bool, name: &str, follow: bool, lines: usize) -> Vec<String> {
    let (unit_field, unit) =
        if system { ("_SYSTEMD_UNIT", "lindeskd.service") } else { ("_SYSTEMD_USER_UNIT", "lindesk-session.service") };
    let mut a: Vec<String> = Vec::new();
    if !system {
        a.push("--user".into());
    }
    a.extend(["--no-pager".into(), "-o".into(), "short-precise".into(), "-n".into(), lines.to_string()]);
    if follow {
        a.push("-f".into());
    }
    a.push(format!("{unit_field}={unit}"));
    a.push(format!("F_PLUGIN={name}"));
    a.push("+".into());
    a.push(format!("{unit_field}={unit}"));
    a.push(format!("PLUGIN={name}"));
    a
}

/// Fallback for daemons logging formatted text: grep the unit's messages.
fn journal_grep_args(system: bool, name: &str, follow: bool, lines: usize) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    if system {
        a.extend(["-u".into(), "lindeskd.service".into()]);
    } else {
        a.extend(["--user".into(), "-u".into(), "lindesk-session.service".into()]);
    }
    a.extend(["--no-pager".into(), "-o".into(), "short-precise".into(), "-n".into(), lines.to_string()]);
    if follow {
        a.push("-f".into());
    }
    let escaped: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_string() } else { format!("\\{c}") })
        .collect();
    a.extend(["--grep".into(), format!("plugin=\"?{escaped}\"?\\b")]);
    a
}

fn logs(ui: &Ui, name: &str, follow: bool, lines: usize) -> Result<()> {
    let jc = paths::which("journalctl").context("journalctl isn't available: plugin logs come from the systemd journal. Without systemd, read the daemon's stderr.")?;
    if !follow {
        let o = Command::new(&jc)
            .args(journal_args(ui.system, name, false, lines))
            .output()
            .context("running journalctl")?;
        let text = String::from_utf8_lossy(&o.stdout);
        let has_lines = text.lines().any(|l| !l.starts_with("-- "));
        if o.status.success() && has_lines {
            out(&text);
            return Ok(());
        }
        let o2 = Command::new(&jc).args(journal_grep_args(ui.system, name, false, lines)).output()?;
        let text2 = String::from_utf8_lossy(&o2.stdout);
        if text2.lines().any(|l| !l.starts_with("-- ")) {
            out(&text2);
            return Ok(());
        }
        let d = Daemon::for_scope(ui.system);
        let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
        ui.info(format!(
            "no journal lines for plugin '{name}' from {}{}.\n  All of the daemon's logs: journalctl {}-u {}",
            d.name(),
            if err.is_empty() { String::new() } else { format!(" ({err})") },
            if ui.system { "" } else { "--user " },
            d.name()
        ));
        return Ok(());
    }
    let err = Command::new(&jc).args(journal_args(ui.system, name, true, lines)).exec_replace();
    Err(anyhow::anyhow!("running journalctl: {err}"))
}

trait ExecReplace {
    fn exec_replace(&mut self) -> std::io::Error;
}

impl ExecReplace for Command {
    fn exec_replace(&mut self) -> std::io::Error {
        use std::os::unix::process::CommandExt;
        self.exec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_host::discovery::{Discovery, Rejected};

    #[test]
    fn journal_filter_matches_both_field_spellings() {
        let a = journal_args(false, "steam-games", true, 20);
        let joined = a.join(" ");
        assert!(joined.starts_with("--user --no-pager -o short-precise -n 20 -f"), "{joined}");
        assert!(joined.ends_with(
            "_SYSTEMD_USER_UNIT=lindesk-session.service F_PLUGIN=steam-games + _SYSTEMD_USER_UNIT=lindesk-session.service PLUGIN=steam-games"
        ));
        let s = journal_args(true, "x", false, 5).join(" ");
        assert!(!s.contains("--user") && s.contains("_SYSTEMD_UNIT=lindeskd.service F_PLUGIN=x"), "{s}");
        let g = journal_grep_args(false, "a.b", false, 5).join(" ");
        assert!(g.contains(r#"plugin="?a\.b"?\b"#), "{g}");
    }

    #[test]
    fn copy_tree_follows_links_and_skips_build_dirs() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        std::fs::create_dir_all(src.join("target/release")).unwrap();
        std::fs::create_dir_all(src.join("__pycache__")).unwrap();
        std::fs::write(src.join("plugin.toml"), "x").unwrap();
        std::fs::write(d.path().join("sdk.py"), "sdk").unwrap();
        std::os::unix::fs::symlink(d.path().join("sdk.py"), src.join("lindesk_plugin.py")).unwrap();
        copy_tree(&src, &d.path().join("dst")).unwrap();
        let dst = d.path().join("dst");
        assert!(dst.join("plugin.toml").is_file());
        let link = std::fs::symlink_metadata(dst.join("lindesk_plugin.py")).unwrap();
        assert!(link.is_file(), "symlinks are copied as files");
        assert!(!dst.join("target").exists() && !dst.join("__pycache__").exists());
    }

    #[test]
    fn merge_marks_states() {
        let found = Discovery {
            plugins: vec![],
            rejected: vec![Rejected {
                path: "/x/bad".into(),
                name: Some("bad".into()),
                reason: "api v2".into(),
                skipped: false,
            }],
        };
        let live = vec![PluginHealth {
            name: "threshold".into(),
            kinds: vec![PluginKind::Policy],
            origin: lindesk_host::Origin::Builtin,
            state: "running".into(),
            detail: None,
            pid: None,
            stats: Default::default(),
        }];
        let rows = merge(&found, Some(&live), false);
        assert_eq!(rows[0].name, "threshold");
        assert_eq!(rows[0].origin, "builtin");
        assert_eq!((rows[1].name.as_str(), rows[1].state.as_str()), ("bad", "rejected"));
        let rows = merge(&found, None, false);
        assert_eq!(rows.len(), 1);
    }
}
