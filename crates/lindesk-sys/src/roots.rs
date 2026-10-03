//! Explicit filesystem roots and environment, so nothing reads global state implicitly.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where the kernel and OS interfaces are mounted. [`Roots::host`] is the real machine;
/// tests point every root into a fixture tree with [`Roots::under`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Roots {
    /// sysfs, usually `/sys`.
    pub sys: PathBuf,
    /// procfs, usually `/proc`.
    pub proc: PathBuf,
    /// The cgroup mount, usually `/sys/fs/cgroup`.
    pub cgroup: PathBuf,
    /// `/etc`.
    pub etc: PathBuf,
    /// `/usr`.
    pub usr: PathBuf,
    /// `/boot` (kernel config files).
    pub boot: PathBuf,
    /// `/run` (systemd runtime markers).
    pub run: PathBuf,
}

impl Roots {
    /// The roots of the running system.
    pub fn host() -> Self {
        Roots {
            sys: "/sys".into(),
            proc: "/proc".into(),
            cgroup: "/sys/fs/cgroup".into(),
            etc: "/etc".into(),
            usr: "/usr".into(),
            boot: "/boot".into(),
            run: "/run".into(),
        }
    }

    /// Every root placed under `prefix`, mirroring the host layout (`prefix/sys`,
    /// `prefix/sys/fs/cgroup`, `prefix/proc`, ...). Used for fixtures and chroots.
    pub fn under(prefix: impl AsRef<Path>) -> Self {
        let p = prefix.as_ref();
        Roots {
            sys: p.join("sys"),
            proc: p.join("proc"),
            cgroup: p.join("sys/fs/cgroup"),
            etc: p.join("etc"),
            usr: p.join("usr"),
            boot: p.join("boot"),
            run: p.join("run"),
        }
    }

    /// `/proc/self/mountinfo` under these roots.
    pub fn self_mountinfo(&self) -> PathBuf {
        self.proc.join("self/mountinfo")
    }
}

impl Default for Roots {
    fn default() -> Self {
        Roots::host()
    }
}

/// The desktop-session environment variables lindesk cares about.
///
/// Only the variables in [`Env::DESKTOP_VARS`] are ever captured, so an `Env` built from a
/// process's `environ` never carries secrets.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Env {
    vars: BTreeMap<String, String>,
}

impl Env {
    /// Variables captured by [`Env::from_process`] and [`Env::from_environ`].
    pub const DESKTOP_VARS: &'static [&'static str] = &[
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
        "XDG_SESSION_TYPE",
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "DESKTOP_SESSION",
    ];

    /// An empty environment.
    pub fn new() -> Self {
        Env::default()
    }

    /// Capture [`Env::DESKTOP_VARS`] from this process's environment.
    pub fn from_process() -> Self {
        Self::DESKTOP_VARS.iter().filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v))).collect()
    }

    /// Capture [`Env::DESKTOP_VARS`] from the raw contents of `/proc/<pid>/environ`
    /// (NUL-separated `KEY=VALUE` pairs).
    pub fn from_environ(raw: &[u8]) -> Self {
        raw.split(|b| *b == 0)
            .filter_map(|kv| {
                let kv = std::str::from_utf8(kv).ok()?;
                let (k, v) = kv.split_once('=')?;
                Self::DESKTOP_VARS.contains(&k).then(|| (k.to_string(), v.to_string()))
            })
            .collect()
    }

    /// Builder-style setter.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.set(key, value);
        self
    }

    /// Set a variable.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.vars.insert(key.into(), value.into());
    }

    /// A variable's value. Empty values count as unset.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str).filter(|v| !v.is_empty())
    }

    /// All captured variables.
    pub fn vars(&self) -> &BTreeMap<String, String> {
        &self.vars
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for Env {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Env { vars: iter.into_iter().map(|(k, v)| (k.into(), v.into())).collect() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_under_mirrors_host() {
        let r = Roots::under("/tmp/x");
        assert_eq!(r.cgroup, PathBuf::from("/tmp/x/sys/fs/cgroup"));
        assert_eq!(r.self_mountinfo(), PathBuf::from("/tmp/x/proc/self/mountinfo"));
        assert_eq!(Roots::host().cgroup, PathBuf::from("/sys/fs/cgroup"));
    }

    #[test]
    fn environ_keeps_only_desktop_vars() {
        let raw = b"HOME=/home/u\0WAYLAND_DISPLAY=wayland-0\0SECRET_TOKEN=abc\0DISPLAY=:0\0XDG_SESSION_TYPE=\0";
        let env = Env::from_environ(raw);
        assert_eq!(env.get("WAYLAND_DISPLAY"), Some("wayland-0"));
        assert_eq!(env.get("DISPLAY"), Some(":0"));
        assert_eq!(env.get("SECRET_TOKEN"), None);
        assert_eq!(env.get("HOME"), None);
        assert_eq!(env.get("XDG_SESSION_TYPE"), None, "empty counts as unset");
        assert_eq!(env.vars().len(), 3);
    }

    #[test]
    fn builder() {
        let env = Env::new().with("XDG_CURRENT_DESKTOP", "KDE");
        assert_eq!(env.get("XDG_CURRENT_DESKTOP"), Some("KDE"));
    }
}
