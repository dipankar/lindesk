//! Engine setup steps shared by both daemons.

use std::path::Path;

use lindesk_core::config::{Config, merge_json};
use lindesk_host::{Engine, RegistryError, discovery};
use lindesk_plugin_api::Privilege;
use serde_json::json;

/// Plugin name of the Parquet sink.
pub const PARQUET: &str = "parquet-store";

/// Apply command-line overrides and derived plugin config to a freshly loaded config:
/// `--dry-run`, and the Parquet sink's `dir` (`--telemetry-dir`, else `[telemetry] dir`),
/// `flush` (`[telemetry] flush`) and permission `mode`. Explicit
/// `[plugins.parquet-store.config]` values win over the derived ones, except `--telemetry-dir`.
pub fn prepare_config(config: &mut Config, dry_run: bool, telemetry_dir: Option<&Path>, privilege: Privilege) {
    if dry_run {
        config.general.dry_run = true;
    }
    let mut derived = json!({
        "flush": humantime_secs(config.telemetry.flush),
        "mode": if privilege == Privilege::System { "system" } else { "user" },
    });
    if let Some(d) = &config.telemetry.dir {
        derived["dir"] = json!(d);
    }
    let pc = config.plugins.entry(PARQUET.to_string()).or_default();
    merge_json(&mut derived, &pc.config);
    if let Some(d) = telemetry_dir {
        derived["dir"] = json!(d);
    }
    pc.config = derived;
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
        let pc = &c.plugin(PARQUET).config;
        assert_eq!(pc["mode"], "user");
        assert_eq!(pc["flush"], "60s");
        assert!(pc.get("dir").is_none(), "sink default");
        assert!(!c.general.dry_run);

        let mut c = Config::from_toml(
            "[telemetry]\ndir = \"/cfg\"\nflush = \"30s\"\n[plugins.parquet-store.config]\nmode = \"0750\"\n",
        )
        .unwrap();
        prepare_config(&mut c, true, None, Privilege::System);
        let pc = &c.plugin(PARQUET).config;
        assert_eq!(
            (pc["dir"].as_str(), pc["flush"].as_str(), pc["mode"].as_str()),
            (Some("/cfg"), Some("30s"), Some("0750"))
        );
        assert!(c.general.dry_run);

        prepare_config(&mut c, false, Some(Path::new("/cli")), Privilege::System);
        assert_eq!(c.plugin(PARQUET).config["dir"], "/cli");
        assert!(c.general.dry_run, "never switched off by a missing flag");
    }
}
