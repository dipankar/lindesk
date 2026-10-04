//! `lindeskctl config check|show|path`, and comment-preserving edits of lindesk.toml.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use lindesk_core::config::Config;
use lindesk_host::{Engine, EngineOptions, MemoryEnvironment};
use lindesk_plugin_api::Privilege;
use serde_json::json;
use toml_edit::{DocumentMut, Item, Table, value};

use crate::cli::ConfigCmd;
use crate::cmd::doctor::tilde;
use crate::cmd::plugin::BUILTINS;
use crate::paths;
use crate::ui::{Exit, Ui, out};

pub fn run(ui: &Ui, cmd: &ConfigCmd) -> Result<()> {
    match cmd {
        ConfigCmd::Check { path } => check(ui, path.clone().unwrap_or_else(|| paths::config_path(ui.system))),
        ConfigCmd::Show { path, resolved: false } => {
            show(ui, path.clone().unwrap_or_else(|| paths::config_path(ui.system)))
        }
        ConfigCmd::Show { path, resolved: true } => {
            resolved(ui, path.clone().unwrap_or_else(|| paths::config_path(ui.system)))
        }
        ConfigCmd::Init => init(ui),
        ConfigCmd::Path => {
            let p = paths::config_path(ui.system);
            if ui.json {
                return ui.print_json(&json!({"path": p, "exists": p.exists()}));
            }
            out(&p.display().to_string());
            Ok(())
        }
    }
}

/// A problem found by `config check`.
#[derive(serde::Serialize, Debug, Clone, PartialEq)]
pub struct Problem {
    pub error: bool,
    pub message: String,
}

/// Validate config text beyond parsing: overrides, built-in plugin configs, plugin names.
pub fn validate(text: &str, known_plugins: &[String]) -> std::result::Result<(Config, Vec<Problem>), String> {
    let config = Config::from_toml(text).map_err(|e| e.to_string())?;
    let mut problems = Vec::new();
    // [[override]] globs and settings are compiled by the engine.
    if let Err(e) = Engine::new(
        config.clone(),
        Box::new(MemoryEnvironment::new()),
        EngineOptions::new("config-check", Privilege::User),
    ) {
        problems.push(Problem { error: true, message: e.to_string() });
    }
    if let Some(pc) = config.plugins.get("threshold")
        && !pc.config.is_null()
        && let Err(e) = lindesk_builtins::policy::ThresholdConfig::from_json(&pc.config)
    {
        problems.push(Problem { error: true, message: format!("[plugins.threshold.config]: {e}") });
    }
    for name in config.plugins.keys() {
        if !BUILTINS.iter().any(|(n, _)| n == name) && !known_plugins.iter().any(|k| k == name) {
            problems.push(Problem {
                error: false,
                message: format!(
                    "[plugins.{name}]: no built-in or installed plugin has this name (typo, or not installed yet?)"
                ),
            });
        }
    }
    for d in &config.general.plugin_dirs {
        if !d.is_dir() {
            problems.push(Problem {
                error: false,
                message: format!("[general] plugin_dirs: {} doesn't exist", d.display()),
            });
        }
    }
    Ok((config, problems))
}

fn installed_names(system: bool) -> Vec<String> {
    let host = if system { Privilege::System } else { Privilege::User };
    let mut dirs = lindesk_host::discovery::default_plugin_dirs(host);
    dirs.push(paths::user_plugin_dir());
    let mut names = Vec::new();
    for d in dirs {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                if let Ok(m) = lindesk_host::discovery::load_manifest(&e.path().join("plugin.toml")) {
                    names.push(m.plugin.name);
                }
            }
        }
    }
    names
}

fn check(ui: &Ui, path: PathBuf) -> Result<()> {
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if ui.json {
                return ui.print_json(&json!({"path": path, "exists": false, "ok": true, "problems": []}));
            }
            ui.info(format!("{} doesn't exist; every setting has its default (lindeskctl config show).", tilde(&path)));
            return Ok(());
        }
        Err(e) => return Err(anyhow!("reading {}: {e}", path.display())),
    };
    match validate(&text, &installed_names(ui.system)) {
        Ok((_, problems)) => {
            let ok = !problems.iter().any(|p| p.error);
            if ui.json {
                ui.print_json(&json!({"path": path, "exists": true, "ok": ok, "problems": problems}))?;
            } else {
                for p in &problems {
                    let tag = if p.error { ui.red("error:") } else { ui.yellow("warning:") };
                    out(&format!("{tag} {}", p.message));
                }
                if ok {
                    ui.info(format!("{} {} is valid", ui.ok_mark(), tilde(&path)));
                }
            }
            if ok { Ok(()) } else { Err(Exit(1).into()) }
        }
        Err(e) => {
            if ui.json {
                ui.print_json(
                    &json!({"path": path, "exists": true, "ok": false, "problems": [{"error": true, "message": e}]}),
                )?;
            } else {
                out(&format!("{} {}:\n{e}", ui.red("error:"), path.display()));
            }
            Err(Exit(1).into())
        }
    }
}

/// The shipped, commented default configuration (every key at its default).
pub const DEFAULT_CONFIG: &str = include_str!("../../../../packaging/config/lindesk.toml");

/// Copy `src`'s values onto `dst`, keeping `dst`'s comments and layout.
fn overlay(dst: &mut dyn toml_edit::TableLike, src: &dyn toml_edit::TableLike) {
    for (k, v) in src.iter() {
        let both_tables = dst.get(k).is_some_and(|d| d.is_table_like()) && v.is_table_like();
        if both_tables {
            if let (Some(d), Some(s)) = (dst.get_mut(k).and_then(Item::as_table_like_mut), v.as_table_like()) {
                overlay(d, s);
            }
            continue;
        }
        match (dst.get_mut(k), v) {
            (Some(Item::Value(d)), Item::Value(sv)) => {
                let decor = d.decor().clone();
                *d = sv.clone();
                *d.decor_mut() = decor;
            }
            (Some(d), item) => *d = item.clone(),
            (None, item) => {
                dst.insert(k, item.clone());
            }
        }
    }
}

/// The shipped commented defaults with the user's values laid over them.
pub fn commented_effective(user_text: &str) -> Result<String> {
    let mut doc: DocumentMut = DEFAULT_CONFIG.parse().context("parsing the built-in default config")?;
    let user: DocumentMut = user_text.parse().context("parsing your config")?;
    overlay(doc.as_table_mut(), user.as_table());
    Ok(doc.to_string())
}

fn show(ui: &Ui, path: PathBuf) -> Result<()> {
    let config = paths::load_config(&path)?;
    if ui.json {
        return ui.print_json(&config);
    }
    let user_text = std::fs::read_to_string(&path).unwrap_or_default();
    let header = if path.exists() {
        format!("# effective lindesk configuration: built-in defaults + {}\n", tilde(&path))
    } else {
        format!(
            "# effective lindesk configuration: built-in defaults ({} doesn't exist; `lindeskctl config init` creates it)\n",
            tilde(&path)
        )
    };
    // Prefer the commented form; fall back to plain TOML if the overlay doesn't round-trip.
    let text = match commented_effective(&user_text) {
        Ok(t) if Config::from_toml(&t).ok().as_ref() == Some(&config) => t,
        _ => config.to_toml(),
    };
    crate::ui::out_raw(&format!("{header}{text}"));
    Ok(())
}

/// `config show --resolved`: threshold's tunables resolved against this machine.
fn resolved(ui: &Ui, path: PathBuf) -> Result<()> {
    use lindesk_builtins::policy::threshold::Source;
    let config = paths::load_config(&path)?;
    let cfg = lindesk_builtins::policy::ThresholdConfig::from_json(&config.plugin("threshold").config)
        .map_err(|e| anyhow!("[plugins.threshold.config]: {e}"))?;
    let probe = lindesk_sys::probe(&lindesk_sys::Roots::host(), &lindesk_sys::Env::from_process());
    let mut caps = probe.host_caps();
    // Use a stored disk calibration when there is one (lindesk calibrate).
    for system in [ui.system, !ui.system] {
        if let Ok(map) = lindesk_sys::calibrate::load(&lindesk_sys::calibrate::default_path(system)) {
            lindesk_sys::calibrate::attach(&mut caps.hardware, map);
            break;
        }
    }
    let r = lindesk_builtins::policy::ThresholdPolicy::new(cfg).effective_config(&caps);
    if ui.json {
        return ui.print_json(&json!({"hardware": caps.hardware, "threshold": r}));
    }
    let mut t = crate::ui::Table::new(&["KEY", "VALUE", "SOURCE", "WHY"]);
    for w in &r.why {
        let src = match w.source {
            Source::Explicit => ui.cyan("explicit"),
            Source::Auto => ui.green("auto"),
            Source::Default => ui.dim("default"),
        };
        t.row(vec![w.key.clone(), ui.bold(&w.value), src, ui.dim(&w.why)]);
    }
    let mut s = format!(
        "{}  {}\n\n",
        ui.bold("threshold, resolved for this machine"),
        ui.dim(crate::ui::hardware_compact(&caps.hardware))
    );
    s.push_str(&t.render(ui));
    s.push_str(&ui.dim("\nauto = scaled to the hardware; set a key in [plugins.threshold.config] to pin it."));
    out(&s);
    Ok(())
}

fn init(ui: &Ui) -> Result<()> {
    let path = paths::config_path(ui.system);
    if ui.system {
        paths::require_root("writing /etc/lindesk/lindesk.toml")?;
    }
    if path.exists() {
        bail!(
            "{} already exists. See it with: lindeskctl config show{}",
            tilde(&path),
            if ui.system { " --system" } else { "" }
        );
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    std::fs::write(&path, DEFAULT_CONFIG).with_context(|| format!("writing {}", path.display()))?;
    if ui.json {
        return ui.print_json(&json!({"path": path, "created": true}));
    }
    ui.info(format!("{} wrote {} (every key at its default, with comments)", ui.ok_mark(), tilde(&path)));
    ui.info(ui.dim(format!(
        "edit it, then: lindeskctl config check{} && lindeskctl plugin reload{}",
        if ui.system { " --system" } else { "" },
        if ui.system { " --system" } else { "" }
    )));
    Ok(())
}

// ----------------------------------------------------------------------------- editing

/// Set `[plugins.<name>] enabled = <on>` in config text, keeping comments and layout.
/// Returns the new text (validated) and whether anything changed.
pub fn set_plugin_enabled(text: &str, name: &str, on: bool) -> Result<(String, bool)> {
    let mut doc: DocumentMut =
        text.parse().context("the config isn't valid TOML; fix it first (lindeskctl config check)")?;
    let before = doc.to_string();
    if !doc.contains_key("plugins") {
        let mut t = Table::new();
        t.set_implicit(true);
        doc.insert("plugins", Item::Table(t));
    }
    let plugins =
        doc["plugins"].as_table_like_mut().ok_or_else(|| anyhow!("`plugins` in the config is not a table"))?;
    if plugins.get(name).is_none() {
        plugins.insert(name, Item::Table(Table::new()));
    }
    let entry = plugins
        .get_mut(name)
        .and_then(|i| i.as_table_like_mut())
        .ok_or_else(|| anyhow!("[plugins.{name}] is not a table"))?;
    let current = entry.get("enabled").and_then(|i| i.as_bool());
    if current == Some(on) {
        return Ok((before, false));
    }
    match entry.get_mut("enabled") {
        // Keep the key's position and its decor (comments around it).
        Some(item) if item.is_value() => {
            let decor = item.as_value().map(|v| v.decor().clone());
            *item = value(on);
            if let (Some(d), Some(v)) = (decor, item.as_value_mut()) {
                *v.decor_mut() = d;
            }
        }
        _ => {
            entry.insert("enabled", value(on));
        }
    }
    let after = doc.to_string();
    Config::from_toml(&after).map_err(|e| anyhow!("the edited config would be invalid: {e}"))?;
    Ok((after, true))
}

/// [`set_plugin_enabled`] on a file (created if missing), written atomically.
pub fn set_plugin_enabled_file(path: &Path, name: &str, on: bool) -> Result<bool> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => bail!("reading {}: {e}", path.display()),
    };
    let (new, changed) = set_plugin_enabled(&text, name, on)?;
    if !changed {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("toml.lindeskctl-tmp");
    std::fs::write(&tmp, new).with_context(|| format!("writing {}", tmp.display()))?;
    if let Ok(m) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, m.permissions());
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONF: &str = r#"# my lindesk config
[general]
tick = "250ms"   # fast

# The threshold policy.
[plugins.threshold]
# keep it on
enabled = true   # trailing comment
priority = 5

[plugins.threshold.config]
epoch = "500ms"
"#;

    #[test]
    fn disable_existing_keeps_comments() {
        let (out, changed) = set_plugin_enabled(CONF, "threshold", false).unwrap();
        assert!(changed);
        assert!(out.contains("# my lindesk config"));
        assert!(out.contains("tick = \"250ms\"   # fast"));
        assert!(out.contains("# keep it on\nenabled = false   # trailing comment\npriority = 5"), "{out}");
        assert!(out.contains("[plugins.threshold.config]\nepoch = \"500ms\""));
        assert!(!Config::from_toml(&out).unwrap().plugin("threshold").enabled);
        // Round trip back.
        let (back, changed) = set_plugin_enabled(&out, "threshold", true).unwrap();
        assert!(changed);
        assert_eq!(back, CONF);
    }

    #[test]
    fn adds_new_section_and_is_idempotent() {
        let (out, changed) = set_plugin_enabled(CONF, "steam-games", false).unwrap();
        assert!(changed);
        assert!(out.starts_with(CONF.trim_end_matches('\n')), "{out}");
        assert!(out.contains("[plugins.steam-games]\nenabled = false\n"), "{out}");
        let c = Config::from_toml(&out).unwrap();
        assert!(!c.plugin("steam-games").enabled);
        assert_eq!(c.plugin("threshold").priority, Some(5));
        let (again, changed) = set_plugin_enabled(&out, "steam-games", false).unwrap();
        assert!(!changed);
        assert_eq!(again, out);
    }

    #[test]
    fn empty_file_and_dotted_names() {
        let (out, _) = set_plugin_enabled("", "a.b", true).unwrap();
        assert_eq!(out.trim(), "[plugins.\"a.b\"]\nenabled = true");
        assert!(Config::from_toml(&out).unwrap().plugin("a.b").enabled);
    }

    #[test]
    fn inline_tables_and_bad_input() {
        let (out, _) = set_plugin_enabled("plugins = { rules = { priority = 3 } }\n", "rules", false).unwrap();
        let c = Config::from_toml(&out).unwrap();
        assert!(!c.plugin("rules").enabled);
        assert_eq!(c.plugin("rules").priority, Some(3));
        assert!(set_plugin_enabled("[plugins\n", "x", true).is_err());
        assert!(set_plugin_enabled("plugins = 3\n", "x", true).is_err());
    }

    #[test]
    fn file_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/lindesk.toml");
        assert!(set_plugin_enabled_file(&p, "rules", false).unwrap());
        assert!(!set_plugin_enabled_file(&p, "rules", false).unwrap());
        assert!(!Config::load(&p).unwrap().plugin("rules").enabled);
    }

    #[test]
    fn defaults_are_valid_and_overlay_keeps_comments() {
        let shipped = Config::from_toml(DEFAULT_CONFIG).unwrap();
        assert_eq!(
            (shipped.general, shipped.telemetry, shipped.safety),
            (Config::default().general, Config::default().telemetry, Config::default().safety)
        );
        let t = commented_effective("[general]\ntick = \"100ms\"\n[plugins.steam-games]\nenabled = false\n").unwrap();
        assert!(t.contains("# Pipeline tick."), "comments survive");
        assert!(t.contains("tick = \"100ms\""), "{t}");
        let c = Config::from_toml(&t).unwrap();
        assert_eq!(c.general.tick, std::time::Duration::from_millis(100));
        assert!(!c.plugin("steam-games").enabled);
        assert_eq!(c.telemetry, Config::default().telemetry);
        let t = commented_effective("[plugins.threshold.config.enter]\nio = 0.5\n").unwrap();
        assert_eq!(Config::from_toml(&t).unwrap().plugin("threshold").config["enter"]["io"], 0.5);
    }

    #[test]
    fn validation() {
        let (_, p) = validate("[plugins.nope]\nenabled = false\n", &[]).unwrap();
        assert_eq!(p.len(), 1);
        assert!(!p[0].error && p[0].message.contains("[plugins.nope]"));
        let (_, p) = validate("[plugins.threshold.config]\nexit_ratio = 7.0\n", &[]).unwrap();
        assert!(p.iter().any(|p| p.error), "{p:?}");
        assert!(validate("[generl]\n", &[]).is_err());
        let (_, p) = validate("[[override]]\nunit = \"[\"\nclass = \"batch\"\n", &[]).unwrap();
        assert!(p.iter().any(|p| p.error && p.message.contains("override")), "{p:?}");
    }
}
