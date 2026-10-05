//! Engine setup steps shared by both daemons.

use std::path::Path;

use lindesk_core::config::{Config, merge_json};
use lindesk_host::{Engine, RegistryError, discovery};
use lindesk_plugin_api::Privilege;
use serde_json::json;

/// Name of the telemetry sink plugin (`lindesk-telemetry-sink`, from the optional
/// lindesk-telemetry package). Discovered like any external plugin; absent = no telemetry.
pub const TELEMETRY: &str = "telemetry";
/// The built-in sink's former name: `[plugins.parquet-store]` still configures `telemetry`.
pub const LEGACY_TELEMETRY: &str = "parquet-store";

/// What the daemons (and `lindeskctl`) say when the plugin isn't there.
pub const TELEMETRY_NOT_INSTALLED: &str = lindesk_sys::telemetry::NOT_INSTALLED;

/// Apply command-line overrides and derived plugin config to a freshly loaded config:
/// `--dry-run`, and the `telemetry` plugin's `dir` (`--telemetry-dir`, else `[telemetry]
/// dir`), `flush` (`[telemetry] flush`) and permission `mode`. `[plugins.parquet-store]` is
/// folded into `[plugins.telemetry]` (the newer section wins key by key). Explicit plugin
/// config values win over the derived ones, except `--telemetry-dir`. `[telemetry] enabled =
/// false` disables the plugin, so it is not even started.
pub fn prepare_config(config: &mut Config, dry_run: bool, telemetry_dir: Option<&Path>, privilege: Privilege) {
    if dry_run {
        config.general.dry_run = true;
    }
    if let Some(legacy) = config.plugins.remove(LEGACY_TELEMETRY) {
        let pc = config.plugins.entry(TELEMETRY.to_string()).or_default();
        let mut merged = legacy.config;
        merge_json(&mut merged, &pc.config);
        pc.config = merged;
        pc.enabled &= legacy.enabled;
        pc.priority = pc.priority.or(legacy.priority);
    }
    let mut derived = json!({
        "flush": humantime_secs(config.telemetry.flush),
        "mode": if privilege == Privilege::System { "system" } else { "user" },
    });
    if let Some(d) = &config.telemetry.dir {
        derived["dir"] = json!(d);
    }
    let enabled = config.telemetry.enabled;
    let pc = config.plugins.entry(TELEMETRY.to_string()).or_default();
    merge_json(&mut derived, &pc.config);
    if let Some(d) = telemetry_dir {
        derived["dir"] = json!(d);
    }
    pc.config = derived;
    if !enabled {
        pc.enabled = false;
    }
}

/// `daemon.telemetry` of the status JSON: whether the `telemetry` plugin runs here.
/// `{"installed": true, "enabled": true, "state": "running", "dir": …}`, `{"installed":
/// false, "enabled": true, "hint": …}` without the plugin, `"enabled": false` when the
/// config turns telemetry off.
pub fn telemetry_status(engine: &Engine, default_dir: &Path) -> serde_json::Value {
    let config = engine.config();
    let enabled = config.telemetry.enabled && config.plugins.get(TELEMETRY).is_none_or(|p| p.enabled);
    let status = engine.status();
    match status.plugins.iter().find(|p| p.name == TELEMETRY) {
        Some(p) => {
            let dir =
                config.plugin(TELEMETRY).config["dir"].as_str().map_or_else(|| default_dir.to_path_buf(), Into::into);
            json!({"installed": true, "enabled": enabled, "state": p.state, "dir": dir})
        }
        None if !enabled => json!({"enabled": false}),
        None => json!({"installed": false, "enabled": true, "hint": TELEMETRY_NOT_INSTALLED}),
    }
}

/// The lake root the plugin uses when nothing configures one (as `lindesk-store` decides).
pub fn default_telemetry_dir(privilege: Privilege) -> std::path::PathBuf {
    if privilege == Privilege::System {
        return "/var/lib/lindesk/telemetry".into();
    }
    std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| "/tmp".into())
        .join("lindesk/telemetry")
}

/// One line for the status text.
pub fn telemetry_text(v: &serde_json::Value) -> String {
    match (v["installed"].as_bool(), v["enabled"].as_bool()) {
        (_, Some(false)) => "telemetry: disabled ([telemetry] enabled = false)".into(),
        (Some(true), _) => {
            format!("telemetry: {} into {}", v["state"].as_str().unwrap_or("?"), v["dir"].as_str().unwrap_or("?"))
        }
        _ => "telemetry: not installed (optional: the lindesk-telemetry package)".into(),
    }
}

fn humantime_secs(d: std::time::Duration) -> String {
    format!("{}s", d.as_secs().max(1))
}

/// Log a built-in registration result.
pub fn registered(what: &str, r: Result<(), RegistryError>) {
    match r {
        Ok(()) => tracing::debug!("registered {what}"),
        Err(e) => tracing::warn!("{what}: {e}"),
    }
}

/// True when the `telemetry` plugin is installed (discovered) and enabled.
pub fn telemetry_loaded(engine: &Engine) -> bool {
    let c = engine.config();
    c.telemetry.enabled
        && c.plugins.get(TELEMETRY).is_none_or(|p| p.enabled)
        && engine.registry().names().iter().any(|n| n == TELEMETRY)
}

/// Discover external plugins for `privilege` (default dirs plus `[general] plugin_dirs`) and
/// load the ones not registered yet. Returns the names loaded.
pub fn load_externals(engine: &mut Engine, privilege: Privilege) -> Vec<String> {
    let config = engine.config().clone();
    let mut dirs = discovery::default_plugin_dirs(privilege);
    dirs.extend(config.general.plugin_dirs.clone());
    let found = discovery::discover(&discovery::DiscoveryOptions {
        host: privilege,
        dirs,
        config: &config,
        ownership: &discovery::RootOwned,
    });
    for r in &found.rejected {
        if r.skipped {
            tracing::debug!("plugin skipped: {r}");
        } else {
            tracing::warn!("plugin rejected: {r}");
        }
    }
    let known = engine.registry().names();
    let mut loaded = Vec::new();
    for p in found.plugins.iter().filter(|p| !known.contains(&p.name)) {
        match engine.registry_mut().load_external(p) {
            Ok(Some(_)) => {
                tracing::info!(plugin = %p.name, "loaded external plugin from {}", p.dir.display());
                loaded.push(p.name.clone());
            }
            Ok(None) => tracing::info!(plugin = %p.name, "external plugin is disabled"),
            Err(e) => tracing::warn!(plugin = %p.name, "{e}"),
        }
    }
    loaded
}

/// Re-read the config file, prepare it like at start-up (`prepare`), hand it to `apply`
/// (normally the profile layer, which reloads the engine), then load newly installed
/// plugins. Plugins that disappeared keep running until restart.
pub fn reload(
    engine: &mut Engine,
    path: &Path,
    privilege: Privilege,
    prepare: &dyn Fn(&mut Config),
    apply: &mut dyn FnMut(&mut Engine, Config) -> Result<(), String>,
) -> Result<String, String> {
    let mut config = Config::load(path).map_err(|e| e.to_string())?;
    prepare(&mut config);
    apply(engine, config)?;
    let loaded = load_externals(engine, privilege);
    Ok(if loaded.is_empty() {
        format!("config {} applied; no new plugins", path.display())
    } else {
        format!("config {} applied; new plugins: {}", path.display(), loaded.join(", "))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_dir_precedence() {
        let mut c = Config::default();
        prepare_config(&mut c, false, None, Privilege::User);
        let pc = &c.plugin(TELEMETRY).config;
        assert_eq!(pc["mode"], "user");
        assert_eq!(pc["flush"], "60s");
        assert!(pc.get("dir").is_none(), "sink default");
        assert!(!c.general.dry_run);

        let mut c = Config::from_toml(
            "[telemetry]\ndir = \"/cfg\"\nflush = \"30s\"\n[plugins.parquet-store.config]\nmode = \"0750\"\n",
        )
        .unwrap();
        prepare_config(&mut c, true, None, Privilege::System);
        assert!(!c.plugins.contains_key(LEGACY_TELEMETRY), "folded into [plugins.telemetry]");
        let pc = &c.plugin(TELEMETRY).config;
        assert_eq!(
            (pc["dir"].as_str(), pc["flush"].as_str(), pc["mode"].as_str()),
            (Some("/cfg"), Some("30s"), Some("0750"))
        );
        assert!(c.general.dry_run);

        prepare_config(&mut c, false, Some(Path::new("/cli")), Privilege::System);
        assert_eq!(c.plugin(TELEMETRY).config["dir"], "/cli");
        assert!(c.general.dry_run, "never switched off by a missing flag");
    }

    #[test]
    fn legacy_section_and_enabled_flag() {
        // Both sections: the new one wins per key, the old one still contributes.
        let mut c = Config::from_toml(
            "[plugins.parquet-store]\npriority = 3\n[plugins.parquet-store.config]\nqueue_rows = 7\nmode = \"0750\"\n\
             [plugins.telemetry.config]\nmode = \"0700\"\n",
        )
        .unwrap();
        prepare_config(&mut c, false, None, Privilege::User);
        let pc = c.plugin(TELEMETRY);
        assert_eq!((pc.config["queue_rows"].as_u64(), pc.config["mode"].as_str()), (Some(7), Some("0700")));
        assert_eq!(pc.priority, Some(3));
        assert!(pc.enabled);

        let mut c = Config::from_toml("[plugins.parquet-store]\nenabled = false\n").unwrap();
        prepare_config(&mut c, false, None, Privilege::User);
        assert!(!c.plugin(TELEMETRY).enabled, "disabling the old name disables the plugin");

        let mut c = Config::from_toml("[telemetry]\nenabled = false\n").unwrap();
        prepare_config(&mut c, false, None, Privilege::User);
        assert!(!c.plugin(TELEMETRY).enabled, "telemetry.enabled = false keeps the plugin from starting");
    }

    #[test]
    fn status_text() {
        assert_eq!(
            telemetry_text(&json!({"installed": false, "enabled": true})),
            "telemetry: not installed (optional: the lindesk-telemetry package)"
        );
        assert_eq!(
            telemetry_text(&json!({"installed": true, "enabled": true, "state": "running", "dir": "/x"})),
            "telemetry: running into /x"
        );
        assert!(telemetry_text(&json!({"enabled": false})).contains("disabled"));
    }
}
