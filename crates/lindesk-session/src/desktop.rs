//! First-run desktop integration: switch on the shell-side adapter once.
//!
//! * GNOME: when the `lindesk@lindesk.org` extension is installed but disabled,
//!   `gnome-extensions enable lindesk@lindesk.org`.
//! * KDE Plasma: when the `lindesk` KWin script is installed,
//!   `kwriteconfig6 --file kwinrc --group Plugins --key lindeskEnabled true` and a KWin
//!   `reconfigure` (`qdbus6`, else `dbus-send`).
//!
//! It happens at most once per user: the outcome is recorded in
//! `$XDG_STATE_HOME/lindesk/first-run.json`, so a user who disables the integration later
//! stays disabled. Nothing is recorded while the integration isn't installed (it is enabled
//! on the first start after it is). Gated by `[plugins.desktop-integration] enabled` (default
//! true); skipped in dry-run. Missing tools are never an error.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

/// Config section / plugin name.
pub const NAME: &str = "desktop-integration";
pub const GNOME_UUID: &str = "lindesk@lindesk.org";
pub const KWIN_SCRIPT: &str = "lindesk";

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct FirstRun {
    /// Set once the desktop integration has been handled for good.
    #[serde(default)]
    pub desktop_integration: Option<Record>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Record {
    pub ts: i64,
    pub desktop: String,
    pub action: String,
}

/// What to do for this desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Desktop {
    Gnome,
    Kde,
    Other,
}

pub fn desktop_of(current_desktop: Option<&str>) -> Desktop {
    let parts: Vec<String> =
        current_desktop.unwrap_or_default().split(':').map(|p| p.trim().to_ascii_lowercase()).collect();
    if parts.iter().any(|p| p == "gnome") {
        Desktop::Gnome
    } else if parts.iter().any(|p| p == "kde") {
        Desktop::Kde
    } else {
        Desktop::Other
    }
}

/// State of the GNOME extension from `gnome-extensions info` output: `Some(enabled)` when
/// installed.
pub fn gnome_state(info: &str) -> Option<bool> {
    for line in info.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("Enabled:") {
            return Some(v.trim().eq_ignore_ascii_case("yes"));
        }
        if let Some(v) = l.strip_prefix("State:") {
            return Some(matches!(v.trim().to_ascii_uppercase().as_str(), "ENABLED" | "ACTIVE"));
        }
    }
    None
}

pub fn first_run_path() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|d| d.join("lindesk/first-run.json"))
}

fn load(path: &Path) -> FirstRun {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save(path: &Path, fr: &FirstRun) {
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Err(e) = std::fs::write(path, serde_json::to_string_pretty(fr).unwrap_or_default()) {
        tracing::warn!("cannot record first-run state in {}: {e}", path.display());
    }
}

fn run(cmd: &str, args: &[&str]) -> Option<std::process::Output> {
    Command::new(cmd).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()
}

fn kwin_script_installed() -> bool {
    let mut dirs: Vec<PathBuf> = ["/usr/share/kwin/scripts", "/usr/local/share/kwin/scripts"].map(PathBuf::from).into();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/kwin/scripts"));
    }
    dirs.iter().any(|d| d.join(KWIN_SCRIPT).is_dir())
}

/// Do the integration if due. Returns a description of what was done (for a `Notice`).
pub fn first_run(current_desktop: Option<&str>, state: &Path) -> Option<String> {
    let mut fr = load(state);
    if fr.desktop_integration.is_some() {
        return None;
    }
    let desktop = desktop_of(current_desktop);
    let action = match desktop {
        Desktop::Gnome => {
            let out = run("gnome-extensions", &["info", GNOME_UUID])?;
            match gnome_state(&String::from_utf8_lossy(&out.stdout)).filter(|_| out.status.success()) {
                None => return None, // not installed (yet)
                Some(true) => "GNOME extension already enabled".to_string(),
                Some(false) => match run("gnome-extensions", &["enable", GNOME_UUID]) {
                    Some(o) if o.status.success() => format!("enabled the GNOME extension {GNOME_UUID}"),
                    _ => {
                        tracing::warn!("could not enable the GNOME extension {GNOME_UUID}");
                        return None;
                    }
                },
            }
        }
        Desktop::Kde => {
            if !kwin_script_installed() {
                return None;
            }
            let key = format!("{KWIN_SCRIPT}Enabled");
            let ok = run("kwriteconfig6", &["--file", "kwinrc", "--group", "Plugins", "--key", &key, "true"])
                .is_some_and(|o| o.status.success());
            if !ok {
                tracing::warn!("could not enable the KWin script (kwriteconfig6 missing or failed)");
                return None;
            }
            let reconfigured = run("qdbus6", &["org.kde.KWin", "/KWin", "reconfigure"])
                .is_some_and(|o| o.status.success())
                || run("dbus-send", &["--session", "--dest=org.kde.KWin", "/KWin", "org.kde.KWin.reconfigure"])
                    .is_some_and(|o| o.status.success());
            format!(
                "enabled the KWin script '{KWIN_SCRIPT}'{}",
                if reconfigured { "" } else { " (KWin not reconfigured)" }
            )
        }
        Desktop::Other => return None,
    };
    fr.desktop_integration = Some(Record {
        ts: lindesk_core::clock::now_ms(),
        desktop: format!("{desktop:?}").to_lowercase(),
        action: action.clone(),
    });
    save(state, &fr);
    Some(action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktops() {
        assert_eq!(desktop_of(Some("ubuntu:GNOME")), Desktop::Gnome);
        assert_eq!(desktop_of(Some("KDE")), Desktop::Kde);
        assert_eq!(desktop_of(Some("COSMIC")), Desktop::Other);
        assert_eq!(desktop_of(None), Desktop::Other);
    }

    #[test]
    fn gnome_info_parsing() {
        assert_eq!(
            gnome_state("lindesk@lindesk.org\n  Name: lindesk\n  Enabled: No\n  State: INITIALIZED\n"),
            Some(false)
        );
        assert_eq!(gnome_state("x\n  Enabled: Yes\n"), Some(true));
        assert_eq!(gnome_state("x\n  State: ENABLED\n"), Some(true));
        assert_eq!(gnome_state("x\n  State: DISABLED\n"), Some(false));
        assert_eq!(gnome_state("Extension doesn't exist"), None);
    }

    #[test]
    fn recorded_runs_are_not_repeated() {
        let tmp = crate::common::test_dir();
        let path = tmp.path().join("lindesk/first-run.json");
        // Other desktops: nothing to do, nothing recorded.
        assert_eq!(first_run(Some("sway"), &path), None);
        assert!(!path.exists());
        save(
            &path,
            &FirstRun { desktop_integration: Some(Record { ts: 1, desktop: "gnome".into(), action: "x".into() }) },
        );
        assert_eq!(first_run(Some("GNOME"), &path), None, "already handled once");
        assert_eq!(load(&path).desktop_integration.unwrap().action, "x");
    }
}
