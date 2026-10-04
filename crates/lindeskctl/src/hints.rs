//! First-run guidance: at most one actionable line, only when something installed isn't
//! running or enabled, and never twice for the same thing in a login session.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::bus::{self, Daemon};
use crate::paths;

/// One thing to fix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hint {
    /// Stable key, for "don't nag twice".
    pub key: &'static str,
    pub text: String,
}

fn unit_installed(system: bool, unit: &str) -> bool {
    let mut dirs: Vec<PathBuf> = if system {
        [
            "/etc/systemd/system",
            "/run/systemd/system",
            "/usr/local/lib/systemd/system",
            "/usr/lib/systemd/system",
            "/lib/systemd/system",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    } else {
        ["/etc/systemd/user", "/usr/local/lib/systemd/user", "/usr/lib/systemd/user", "/lib/systemd/user"]
            .iter()
            .map(PathBuf::from)
            .collect()
    };
    if !system {
        let cfg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
        if let Some(c) = cfg {
            dirs.push(c.join("systemd/user"));
        }
    }
    dirs.iter().any(|d| d.join(unit).exists())
}

fn data_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(h) = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute()) {
        v.push(h);
    } else if let Some(h) = std::env::var_os("HOME") {
        v.push(PathBuf::from(h).join(".local/share"));
    }
    let sys = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    v.extend(sys.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    v
}

fn gnome_extension_disabled() -> bool {
    const UUID: &str = "lindesk@lindesk.org";
    if !data_dirs().iter().any(|d| d.join("gnome-shell/extensions").join(UUID).is_dir()) {
        return false;
    }
    if paths::which("gnome-extensions").is_none() {
        return false;
    }
    match Command::new("gnome-extensions").args(["info", UUID]).output() {
        Ok(o) if o.status.success() => {
            let t = String::from_utf8_lossy(&o.stdout);
            let enabled = t.lines().any(|l| l.trim() == "Enabled: Yes");
            let active = t.lines().any(|l| matches!(l.trim(), "State: ACTIVE" | "State: ENABLED"));
            !(enabled || active)
        }
        _ => false,
    }
}

fn kwin_script_disabled() -> bool {
    if !data_dirs().iter().any(|d| d.join("kwin/scripts/lindesk").is_dir()) {
        return false;
    }
    let Some(k) = paths::which("kreadconfig6").or_else(|| paths::which("kreadconfig5")) else { return false };
    Command::new(k)
        .args(["--file", "kwinrc", "--group", "Plugins", "--key", "lindeskEnabled"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() != "true")
        .unwrap_or(false)
}

/// The single most important first-run fix, if any.
pub fn first_run() -> Option<Hint> {
    let session_down = !matches!(bus::reach(Daemon::Session), bus::Reach::Running);
    if session_down && unit_installed(false, "lindesk-session.service") {
        return Some(Hint {
            key: "session",
            text: format!("lindesk-session is installed but not running. Start it: {}", Daemon::Session.start_hint()),
        });
    }
    let broker_down = !matches!(bus::reach(Daemon::Broker), bus::Reach::Running);
    if broker_down && unit_installed(true, "lindeskd.service") {
        return Some(Hint {
            key: "broker",
            text: format!("lindeskd is installed but not running. Start it: {}", Daemon::Broker.start_hint()),
        });
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_lowercase();
    if desktop.contains("gnome") && gnome_extension_disabled() {
        return Some(Hint {
            key: "gnome",
            text: "the lindesk GNOME extension is off, so focus isn't tracked. Turn it on: gnome-extensions enable lindesk@lindesk.org".into(),
        });
    }
    if desktop.contains("kde") && kwin_script_disabled() {
        return Some(Hint {
            key: "kwin",
            text: "the lindesk KWin script is off, so focus isn't tracked. Enable it in System Settings → Window Management → KWin Scripts".into(),
        });
    }
    None
}

fn seen_file() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", paths::uid())));
    runtime.join("lindesk/lindeskctl-hints")
}

/// Has this hint already been shown in this login session ($XDG_RUNTIME_DIR)? Records it.
pub fn first_time(key: &str, file: &Path) -> bool {
    let seen = std::fs::read_to_string(file).unwrap_or_default();
    if seen.lines().any(|l| l == key) {
        return false;
    }
    if let Some(d) = file.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(file, format!("{seen}{key}\n"));
    true
}

/// The hint to show now (once per login session), if any.
pub fn once() -> Option<Hint> {
    let h = first_run()?;
    first_time(h.key, &seen_file()).then_some(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_show_once() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("x/hints");
        assert!(first_time("session", &f));
        assert!(!first_time("session", &f));
        assert!(first_time("gnome", &f));
        assert!(!first_time("gnome", &f));
    }
}
