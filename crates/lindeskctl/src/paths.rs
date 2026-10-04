//! Where lindesk keeps things (docs/ARCHITECTURE.md, "Filesystem layout").

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lindesk_core::config::Config;

pub const SYSTEM_CONFIG: &str = "/etc/lindesk/lindesk.toml";
pub const SYSTEM_PLUGIN_DIR: &str = "/etc/lindesk/plugins";
pub const VENDOR_PLUGIN_DIR: &str = "/usr/lib/lindesk/plugins";

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

fn xdg(var: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).or_else(|| home().map(|h| h.join(fallback)))
}

/// `$XDG_CONFIG_HOME/lindesk/lindesk.toml`.
pub fn user_config() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").unwrap_or_else(|| PathBuf::from(".config")).join("lindesk/lindesk.toml")
}

/// The config file for the selected scope.
pub fn config_path(system: bool) -> PathBuf {
    if system { PathBuf::from(SYSTEM_CONFIG) } else { user_config() }
}

/// `$XDG_DATA_HOME/lindesk/plugins`.
pub fn user_plugin_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").unwrap_or_else(|| PathBuf::from(".local/share")).join("lindesk/plugins")
}

/// Where `plugin install` puts plugins for the scope.
pub fn install_dir(system: bool) -> PathBuf {
    if system { PathBuf::from(SYSTEM_PLUGIN_DIR) } else { user_plugin_dir() }
}

/// Load a config file (missing = defaults) with an actionable error.
pub fn load_config(path: &Path) -> Result<Config> {
    Config::load(path)
        .with_context(|| format!("invalid config; check it with: lindeskctl config check {}", path.display()))
}

/// The lake root: an explicit `--lake`, else `telemetry.dir` from the config, else the
/// standard location for the scope.
pub fn lake_root(explicit: Option<&Path>, system: bool, config: Option<&Config>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Some(dir) = config.and_then(|c| c.telemetry.dir.clone()) {
        return dir;
    }
    lindesk_store::layout::default_root(system)
}

/// Is the effective uid root?
pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// The effective uid.
pub fn uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

/// Find an executable on PATH.
pub fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    if name.contains('/') {
        let p = PathBuf::from(name);
        return p.is_file().then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false))
}

/// Total size of the regular files under `dir` (0 if missing).
pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                total += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// Require root for a --system operation that writes system state.
pub fn require_root(what: &str) -> Result<()> {
    if !is_root() {
        anyhow::bail!(
            "{what} needs root. Run it with sudo: sudo lindeskctl {}",
            std::env::args().skip(1).collect::<Vec<_>>().join(" ")
        );
    }
    Ok(())
}
