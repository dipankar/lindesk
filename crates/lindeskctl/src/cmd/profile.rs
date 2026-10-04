//! `lindeskctl profile list|show|use|current`: CPU profiles (docs/CONTROL-PLANE.md §2).
//!
//! Listing asks the daemon (`ListProfiles`) and falls back to evaluating the profiles here
//! when none is running; `use` is a versioned change (`SetProfile`), so it needs a daemon.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use lindesk_changes::ChangeSet;
use lindesk_core::config::{Config, PROFILE_AUTO};
use lindesk_core::profile::{ProfileContext, ProfileSet};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::bus::{self, Daemon};
use crate::cli::ProfileCmd;
use crate::cmd::{changes, hw};
use crate::paths;
use crate::ui::{self, Table, Ui, Usage, closest, out};

pub fn run(ui: &Ui, cmd: &ProfileCmd) -> Result<()> {
    match cmd {
        ProfileCmd::List => list(ui),
        ProfileCmd::Show { name } => show(ui, name),
        ProfileCmd::Use { name } => use_profile(ui, name),
        ProfileCmd::Current => current(ui),
    }
}

/// One row of `profile list` (the daemon's `ListProfiles` items, read leniently).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ProfileRow {
    pub name: String,
    pub description: String,
    pub source: String,
    pub builtin: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inherits: Option<String>,
    pub priority: Option<i32>,
    pub auto: bool,
    #[serde(alias = "matches-now", alias = "matches")]
    pub matches_now: bool,
    pub reason: Option<String>,
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What `profile list` shows.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct ProfilesView {
    /// `lindesk-session`, `lindeskd`, or `local` (evaluated here, no daemon).
    pub source: String,
    /// `[general] profile`: `auto` or a pinned name.
    pub configured: Option<String>,
    pub active: Option<String>,
    pub reason: Option<String>,
    /// The machine as auto-selection sees it.
    pub context: Option<String>,
    pub profiles: Vec<ProfileRow>,
    pub errors: Vec<String>,
}

/// The daemon's `daemon.profile` status block.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ProfileState {
    pub active: String,
    /// `[general] profile` (`auto` or a name).
    pub setting: Option<String>,
    pub pinned: bool,
    pub reason: String,
}

impl ProfileState {
    /// From a status reply: `daemon.profile` (or a top-level `profile`).
    pub fn from_status(raw: &Value) -> Option<ProfileState> {
        let p = raw.pointer("/daemon/profile").or_else(|| raw.get("profile")).filter(|p| !p.is_null())?;
        if let Some(s) = p.as_str() {
            return Some(ProfileState { active: s.to_string(), ..Default::default() });
        }
        let mut st: ProfileState = serde_json::from_value(p.clone()).ok()?;
        if st.active.is_empty() {
            st.active = p.get("name").and_then(Value::as_str)?.to_string();
        }
        Some(st)
    }

    /// `gaming`, `auto→battery`.
    pub fn short(&self) -> String {
        match self.setting.as_deref() {
            Some(PROFILE_AUTO) if !self.pinned => format!("auto→{}", self.active),
            _ => self.active.clone(),
        }
    }
}

/// Profile directories for the scope: vendor, `/etc` and (user scope) `$XDG_CONFIG_HOME`.
pub fn dirs(system: bool) -> Vec<PathBuf> {
    let xdg = if system {
        None
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    lindesk_core::profile::default_dirs(xdg.as_deref())
}

/// Every profile, evaluated on this machine without a daemon.
pub fn local_view(system: bool) -> ProfilesView {
    let (set, errors) = ProfileSet::load(&dirs(system));
    let hwv = hw::local_hardware(system);
    let ctx = ProfileContext::from_hardware(&hwv, None);
    let config = paths::load_config(&paths::config_path(system)).unwrap_or_default();
    let configured = config.general.profile.clone();
    let selection = set.select(&ctx);
    let (active, reason) = if configured == PROFILE_AUTO {
        (selection.name.clone(), selection.reason.clone())
    } else {
        (configured.clone(), format!("pinned by [general] profile = \"{configured}\""))
    };
    let profiles = set
        .list(&ctx, Some(&active))
        .into_iter()
        .filter_map(|p| serde_json::to_value(p).ok().and_then(|v| serde_json::from_value(v).ok()))
        .collect();
    ProfilesView {
        source: "local".into(),
        configured: Some(configured),
        active: Some(active),
        reason: Some(reason),
        context: Some(ctx.summary()),
        profiles,
        errors: errors.iter().map(ToString::to_string).collect(),
    }
}

/// Parse a `ListProfiles` reply: an array of rows, or `{profiles, configured, ...}`.
pub fn parse_view(json: &str, source: &str) -> Result<ProfilesView> {
    let v: Value = serde_json::from_str(json).map_err(|e| anyhow!("ListProfiles returned invalid JSON: {e}"))?;
    let (rows, obj) = match v {
        Value::Array(a) => (a, None),
        Value::Object(mut o) => match o.remove("profiles") {
            Some(Value::Array(a)) => (a, Some(o)),
            _ => return Err(anyhow!("ListProfiles returned an object without \"profiles\"")),
        },
        _ => return Err(anyhow!("ListProfiles returned neither a list nor an object")),
    };
    let profiles: Vec<ProfileRow> = rows.into_iter().filter_map(|r| serde_json::from_value(r).ok()).collect();
    let get = |k: &str| obj.as_ref().and_then(|o| o.get(k)).and_then(Value::as_str).map(str::to_string);
    let selected = obj.as_ref().and_then(|o| o.get("would_select").or_else(|| o.get("selection")));
    let active = get("active")
        .or_else(|| profiles.iter().find(|p| p.active).map(|p| p.name.clone()))
        .or_else(|| selected.and_then(|s| s.get("name")).and_then(Value::as_str).map(str::to_string));
    let reason = get("reason")
        .or_else(|| selected.and_then(|s| s.get("reason")).and_then(Value::as_str).map(str::to_string))
        .or_else(|| profiles.iter().find(|p| p.active).and_then(|p| p.reason.clone()));
    let errors = obj
        .as_ref()
        .and_then(|o| o.get("errors"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|e| e.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    Ok(ProfilesView {
        source: source.into(),
        configured: get("configured").or_else(|| get("setting")),
        active,
        reason,
        context: get("context"),
        profiles,
        errors,
    })
}

/// The view from the daemon (with its status' `daemon.profile` filled in), else local.
pub fn view(system: bool) -> (ProfilesView, Option<String>) {
    let d = Daemon::for_scope(system);
    match bus::list_profiles(d).and_then(|j| parse_view(&j, d.name())) {
        Ok(mut v) => {
            if let Some(st) = bus::status_json(d)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .as_ref()
                .and_then(ProfileState::from_status)
            {
                v.active = Some(st.active.clone());
                if !st.reason.is_empty() {
                    v.reason = Some(st.reason.clone());
                }
                if v.configured.is_none() {
                    v.configured = st.setting.clone();
                }
                for p in &mut v.profiles {
                    p.active = p.name == st.active;
                }
            }
            if v.context.is_none() {
                v.context = Some(ProfileContext::from_hardware(&hw::local_hardware(system), None).summary());
            }
            (v, None)
        }
        Err(e) => {
            let note = if bus::is_not_running(&e) {
                format!("{} isn't running: this is what it would pick on this machine", d.name())
            } else {
                format!("{}; evaluated locally", e.to_string().lines().next().unwrap_or_default())
            };
            (local_view(system), Some(note))
        }
    }
}

/// Order: active first, then matching by priority, then the rest by name.
fn sort_rows(rows: &mut [ProfileRow]) {
    rows.sort_by(|a, b| {
        b.active
            .cmp(&a.active)
            .then(b.matches_now.cmp(&a.matches_now))
            .then(b.priority.unwrap_or(i32::MIN).cmp(&a.priority.unwrap_or(i32::MIN)))
            .then(a.name.cmp(&b.name))
    });
}

pub fn render_list(ui: &Ui, v: &ProfilesView) -> String {
    let mut rows = v.profiles.clone();
    sort_rows(&mut rows);
    let mut t = Table::new(&["NAME", "ACTIVE", "MATCHES", "PRIO", "DESCRIPTION", "WHY"]).right(&[3]);
    for p in &rows {
        let name = if p.active { ui.paint("1;32", &p.name) } else { p.name.clone() };
        let active = if p.active { ui.green("●") } else { String::new() };
        let matches = if p.error.is_some() {
            ui.red("broken")
        } else if !p.auto {
            ui.dim("manual")
        } else if p.matches_now {
            ui.green("yes")
        } else {
            ui.dim("no")
        };
        let prio = p.priority.map(|n| n.to_string()).unwrap_or_else(|| ui.dim("-"));
        let mut desc = p.description.clone();
        if let Some(i) = &p.inherits {
            desc.push_str(&ui.dim(format!(" (inherits {i})")));
        }
        let why = match (&p.error, &p.reason) {
            (Some(e), _) => ui.red(e),
            (None, Some(r)) if p.matches_now => ui.dim(r),
            _ => String::new(),
        };
        t.row(vec![name, active, matches, prio, desc, why]);
    }
    let mut s = t.render(ui);
    s.push('\n');
    if let Some(a) = &v.active {
        let mut line = format!("{} {}", ui.bold("active:"), ui.paint("1;32", a));
        if let Some(r) = v.reason.as_ref().filter(|r| !r.is_empty()) {
            line.push_str(&format!(" — {r}"));
        }
        s.push_str(&line);
        s.push('\n');
    }
    let how = match v.configured.as_deref() {
        Some(PROFILE_AUTO) | None => {
            "profile = \"auto\": the best match wins (highest PRIO); pin one: lindeskctl profile use NAME".to_string()
        }
        Some(p) => format!("pinned (profile = \"{p}\"); back to automatic: lindeskctl profile use auto"),
    };
    s.push_str(&ui.dim(how));
    if let Some(c) = &v.context {
        s.push_str(&format!("\n{}", ui.dim(format!("this machine: {c}"))));
    }
    for e in &v.errors {
        s.push_str(&format!("\n{} {e}", ui.yellow("⚠")));
    }
    s
}

fn list(ui: &Ui) -> Result<()> {
    let (v, note) = view(ui.system);
    if ui.json {
        return ui.print_json(&v);
    }
    if let Some(n) = note {
        ui.info(ui.dim(n));
    }
    out(&render_list(ui, &v));
    Ok(())
}

/// The base config for the scope (missing = defaults).
fn base_config(system: bool) -> Config {
    paths::load_config(&paths::config_path(system)).unwrap_or_default()
}

/// JSON → TOML text (`{}` → empty); `null`s (unset options) are dropped.
pub fn to_toml(v: &Value) -> String {
    fn strip(v: &Value) -> Value {
        match v {
            Value::Object(o) => {
                Value::Object(o.iter().filter(|(_, x)| !x.is_null()).map(|(k, x)| (k.clone(), strip(x))).collect())
            }
            Value::Array(a) => Value::Array(a.iter().filter(|x| !x.is_null()).map(strip).collect()),
            other => other.clone(),
        }
    }
    let v = strip(v);
    toml::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
}

fn show(ui: &Ui, name: &str) -> Result<()> {
    use lindesk_builtins::policy::threshold::Source;
    let (set, _) = ProfileSet::load(&dirs(ui.system));
    let Some(p) = set.get(name) else {
        let names = set.names();
        let mut msg = format!("unknown profile '{name}'");
        if let Some(c) = closest(name, names.iter().map(String::as_str)) {
            msg.push_str(&format!(". Did you mean '{c}'?"));
        }
        msg.push_str(&format!("\nProfiles: {}", names.join(", ")));
        return Err(Usage(msg).into());
    };
    let chain = set.chain(name).map_err(|e| anyhow!("{e}"))?;
    let mut overlay = json!({});
    for c in &chain {
        lindesk_core::config::merge_json(&mut overlay, &c.config);
    }
    let base = base_config(ui.system);
    let eff = set.effective_config(&base, name).map_err(|e| anyhow!("{e}"))?;
    let caps = hw::local_caps(ui.system);
    let resolve = |c: &Config| -> Result<lindesk_builtins::policy::threshold::ResolvedThreshold> {
        let cfg = lindesk_builtins::policy::ThresholdConfig::from_json(&c.plugin("threshold").config)
            .map_err(|e| anyhow!("[plugins.threshold.config]: {e}"))?;
        Ok(lindesk_builtins::policy::ThresholdPolicy::new(cfg).effective_config(&caps))
    };
    let resolved = resolve(&eff)?;
    let baseline = resolve(&base)?;
    let ctx = ProfileContext::from_hardware(&caps.hardware, None);
    let matches = p.matches.as_ref().and_then(|m| m.evaluate(&ctx));
    if ui.json {
        return ui.print_json(&json!({
            "profile": p,
            "chain": chain.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            "overlay": overlay,
            "matches_now": matches.is_some(),
            "reason": matches.map(|m| m.join("; ")),
            "threshold": resolved,
        }));
    }
    let mut s = format!("{}  {}\n", ui.bold(&p.name), p.description);
    s.push_str(&format!("  {}  {}\n", ui.dim("source  "), p.source.display()));
    if chain.len() > 1 {
        let names: Vec<&str> = chain.iter().map(|c| c.name.as_str()).collect();
        s.push_str(&format!("  {}  lindesk.toml → {}\n", ui.dim("applies "), names.join(" → ")));
    }
    match (&p.matches, &matches) {
        (None, _) => {
            s.push_str(&format!("  {}  {}\n", ui.dim("selected"), "manually only (lindeskctl profile use NAME)"))
        }
        (Some(m), Some(why)) => s.push_str(&format!(
            "  {}  automatically, priority {} — {}\n",
            ui.dim("selected"),
            m.priority,
            ui.green(format!("matches now: {}", why.join("; ")))
        )),
        (Some(m), None) => s.push_str(&format!(
            "  {}  automatically, priority {} — {}\n",
            ui.dim("selected"),
            m.priority,
            ui.dim("doesn't match this machine now")
        )),
    }
    if let Some(m) = &p.matches {
        let text = to_toml(&json!({ "match": m }));
        s.push('\n');
        s.push_str(&ui.dim(text.trim_end()));
        s.push('\n');
    }
    let text = to_toml(&json!({ "config": overlay }));
    s.push_str(&format!(
        "\n{}\n",
        ui.dim(if chain.len() > 1 { "# effective overlay of the whole chain" } else { "# overlay on lindesk.toml" })
    ));
    s.push_str(if text.trim().is_empty() { "# (empty: the defaults)\n" } else { &text });
    s.push_str(&format!(
        "\n{}  {}\n",
        ui.bold(format!("threshold policy with {name} on this machine")),
        ui.dim(ui::hardware_compact(&caps.hardware))
    ));
    let mut t = Table::new(&["", "KEY", "VALUE", "SOURCE", "WHY"]);
    for w in &resolved.why {
        let changed = baseline.why.iter().find(|b| b.key == w.key).is_none_or(|b| b.value != w.value);
        let src = match w.source {
            Source::Explicit => ui.cyan("explicit"),
            Source::Auto => ui.green("auto"),
            Source::Default => ui.dim("default"),
        };
        t.row(vec![
            if changed { ui.cyan("●") } else { String::new() },
            w.key.clone(),
            if changed { ui.paint("1;36", &w.value) } else { ui.bold(&w.value) },
            src,
            ui.dim(&w.why),
        ]);
    }
    s.push_str(&t.render(ui));
    s.push_str(&ui.dim("● = differs from your config without a profile"));
    out(&s);
    Ok(())
}

fn use_profile(ui: &Ui, name: &str) -> Result<()> {
    if name != PROFILE_AUTO {
        let (set, _) = ProfileSet::load(&dirs(ui.system));
        if !set.contains(name) {
            let names = set.names();
            let mut msg = format!("unknown profile '{name}'");
            if let Some(c) = closest(name, names.iter().map(String::as_str)) {
                msg.push_str(&format!(". Did you mean '{c}'?"));
            }
            msg.push_str(&format!("\nProfiles: {}, or auto", names.join(", ")));
            return Err(Usage(msg).into());
        }
    }
    let d = Daemon::for_scope(ui.system);
    let raw = bus::set_profile(d, name).map_err(|e| {
        if bus::is_not_running(&e) {
            anyhow!(
                "can't switch profiles: {e}\n  Switching is a versioned change made by the daemon, so it has to be running.\n  \
                 (Without it nothing is managed; `[general] profile` in {} is read at start.)",
                crate::cmd::doctor::tilde(&paths::config_path(ui.system))
            )
        } else {
            e
        }
    })?;
    let cs: Option<ChangeSet> = serde_json::from_str(&raw).ok();
    if ui.json {
        let v: Value = serde_json::from_str(&raw).unwrap_or(Value::String(raw));
        return ui.print_json(&v);
    }
    let what = if name == PROFILE_AUTO {
        format!("profile selection is automatic again {}", ui.dim("(profile = \"auto\")"))
    } else {
        format!("profile {} pinned", ui.paint("1;32", name))
    };
    match cs {
        Some(cs) => {
            let mut s = format!("{} {what} {}", ui.ok_mark(), ui.dim(format!("— change #{}", cs.id)));
            if let Some(ms) = changes::ttl_left(&cs, lindesk_core::clock::now_ms()) {
                s.push_str(&format!(
                    "\n  {}",
                    ui.yellow(format!(
                        "auto-reverts in {} unless `lindeskctl changes confirm{} {}`",
                        ui::duration_ms(ms),
                        if ui.system { " --system" } else { "" },
                        cs.id
                    ))
                ));
            }
            s.push_str(&format!(
                "\n  {}",
                ui.dim(format!(
                    "undo: lindeskctl changes revert{} {}{}",
                    if ui.system { " --system" } else { "" },
                    cs.id,
                    if name == PROFILE_AUTO { "" } else { " · or: lindeskctl profile use auto" }
                ))
            ));
            out(&s);
        }
        None => out(&format!("{} {what}", ui.ok_mark())),
    }
    Ok(())
}

fn current(ui: &Ui) -> Result<()> {
    let from_daemon = crate::cmd::status::fetch(ui.system)
        .ok()
        .and_then(|f| ProfileState::from_status(&f.raw).map(|p| (f.source, p)));
    let (source, st) = match from_daemon {
        Some((d, st)) => (d.name().to_string(), st),
        None => {
            let v = local_view(ui.system);
            let setting = v.configured.clone();
            (
                "local".to_string(),
                ProfileState {
                    active: v.active.unwrap_or_default(),
                    pinned: setting.as_deref().is_some_and(|s| s != PROFILE_AUTO),
                    setting,
                    reason: v.reason.unwrap_or_default(),
                },
            )
        }
    };
    if ui.json {
        let mut v = serde_json::to_value(&st)?;
        v["source"] = json!(source);
        return ui.print_json(&v);
    }
    let mode = if st.pinned || st.setting.as_deref().is_some_and(|s| s != PROFILE_AUTO) {
        "pinned".to_string()
    } else {
        "auto-selected".to_string()
    };
    let mut s = format!("{} {}  {}", ui.green("●"), ui.paint("1;32", &st.active), ui.dim(mode));
    if !st.reason.is_empty() {
        s.push_str(&format!("\n  {} {}", ui.dim("why:"), st.reason));
    }
    if source == "local" {
        s.push_str(&format!("\n  {}", ui.dim("no daemon running: this is what lindesk would pick")));
    }
    out(&s);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<ProfileRow> {
        serde_json::from_value(json!([
            {"name": "balanced", "description": "The shipped defaults", "source": "<builtin>/00-balanced.toml", "builtin": true, "auto": false, "matches_now": false, "active": false},
            {"name": "battery", "description": "On battery", "priority": 50, "auto": true, "matches_now": true, "reason": "battery power", "active": true},
            {"name": "gaming", "description": "Games", "inherits": "latency", "priority": 60, "auto": true, "matches-now": false, "active": false},
            {"name": "broken", "description": "x", "auto": true, "matches_now": false, "active": false, "error": "inherits 'nope', which does not exist"}
        ]))
        .unwrap()
    }

    #[test]
    fn renders_profile_list() {
        let v = ProfilesView {
            source: "lindesk-session".into(),
            configured: Some("auto".into()),
            active: Some("battery".into()),
            reason: Some("battery power".into()),
            context: Some("battery power; x86_64 Intel hybrid, 16 CPUs; NVMe".into()),
            profiles: rows(),
            errors: vec![],
        };
        let s = render_list(&Ui::plain(), &v);
        let lines: Vec<&str> = s.lines().collect();
        assert!(lines[0].starts_with("NAME") && lines[0].contains("ACTIVE  MATCHES  PRIO  DESCRIPTION"), "{s}");
        assert!(
            lines[1].starts_with("battery") && lines[1].contains("●") && lines[1].contains("yes"),
            "active first: {s}"
        );
        assert!(lines[1].ends_with("battery power"), "{s}");
        assert!(
            lines.iter().any(|l| l.starts_with("gaming") && l.contains("no") && l.contains("(inherits latency)")),
            "{s}"
        );
        assert!(lines.iter().any(|l| l.starts_with("balanced") && l.contains("manual")), "{s}");
        assert!(lines.iter().any(|l| l.starts_with("broken") && l.contains("does not exist")), "{s}");
        assert!(s.contains("active: battery — battery power"), "{s}");
        assert!(s.contains("pin one: lindeskctl profile use NAME") && s.contains("this machine: battery power"), "{s}");
    }

    #[test]
    fn parses_daemon_replies() {
        let arr = serde_json::to_string(&rows()).unwrap();
        let v = parse_view(&arr, "lindesk-session").unwrap();
        assert_eq!(v.active.as_deref(), Some("battery"));
        assert_eq!(v.profiles.len(), 4);
        let obj = json!({"configured": "auto", "would_select": {"name": "gaming", "reason": "steam focused", "matched": ["gaming"]},
                         "context": "AC", "profiles": [{"name": "gaming", "active": false}], "errors": ["x.toml: bad"]});
        let v = parse_view(&obj.to_string(), "local").unwrap();
        assert_eq!((v.active.as_deref(), v.reason.as_deref()), (Some("gaming"), Some("steam focused")));
        assert_eq!(v.errors, vec!["x.toml: bad"]);
        assert!(parse_view("3", "x").is_err());
        let st = ProfileState::from_status(&json!({"daemon": {"profile": {"active": "battery", "setting": "auto", "pinned": false, "reason": "battery power"}}})).unwrap();
        assert_eq!(st.short(), "auto→battery");
        let st = ProfileState::from_status(
            &json!({"daemon": {"profile": {"active": "gaming", "setting": "gaming", "pinned": true, "reason": ""}}}),
        )
        .unwrap();
        assert_eq!(st.short(), "gaming");
        assert!(ProfileState::from_status(&json!({"daemon": {"profile": null}})).is_none());
    }

    #[test]
    fn local_view_works_out_of_the_box() {
        let v = local_view(false);
        assert_eq!(v.source, "local");
        assert!(v.profiles.iter().any(|p| p.name == "balanced"), "built-ins are embedded");
        assert!(v.profiles.iter().filter(|p| p.active).count() <= 1, "{v:?}");
        assert!(v.active.is_some() && v.context.is_some());
    }
}
