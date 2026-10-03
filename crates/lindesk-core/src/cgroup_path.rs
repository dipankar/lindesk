//! Interpret cgroup paths in systemd's hierarchy without talking to systemd.

use lindesk_plugin_api::{Manager, Target};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CgroupInfo {
    /// Deepest `.service`/`.scope` unit on the path.
    pub unit: Option<String>,
    /// Slice directly containing that unit.
    pub slice: Option<String>,
    /// Set when the unit belongs to a `user@UID.service` manager.
    pub user: Option<u32>,
}

impl CgroupInfo {
    pub fn manager(&self) -> Manager {
        self.user.map_or(Manager::System, Manager::User)
    }

    pub fn target(&self) -> Option<Target> {
        self.unit.as_ref().map(|u| Target::Unit { manager: self.manager(), name: u.clone() })
    }

    pub fn slice_target(&self) -> Option<Target> {
        self.slice.as_ref().map(|s| Target::Unit { manager: self.manager(), name: s.clone() })
    }
}

/// Parse a path like `/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-firefox-123.scope`.
pub fn parse(path: &str) -> CgroupInfo {
    let mut info = CgroupInfo::default();
    let mut user: Option<u32> = None;
    let mut slice: Option<String> = None;
    for comp in path.split('/').filter(|c| !c.is_empty()) {
        if let Some(uid) = comp.strip_prefix("user@").and_then(|r| r.strip_suffix(".service")).and_then(|u| u.parse().ok()) {
            // Entering a user manager: its own units are user units; the manager service itself
            // is a system unit that only matters if nothing deeper exists.
            info = CgroupInfo { unit: Some(comp.to_string()), slice: slice.clone(), user: None };
            user = Some(uid);
            slice = None;
            continue;
        }
        if comp.ends_with(".slice") {
            slice = Some(comp.to_string());
        } else if comp.ends_with(".service") || comp.ends_with(".scope") {
            info = CgroupInfo { unit: Some(comp.to_string()), slice: slice.clone(), user };
        }
    }
    info
}

/// Extract the cgroup v2 path from the contents of `/proc/<pid>/cgroup`.
pub fn from_proc_cgroup(contents: &str) -> Option<String> {
    contents.lines().find_map(|l| l.strip_prefix("0::")).map(|p| p.trim().to_string())
}

/// Best-effort desktop app id from a unit name created by a desktop launcher, e.g.
/// `app-gnome-org.mozilla.firefox-1234.scope` → `org.mozilla.firefox`,
/// `app-flatpak-org.gimp.GIMP-5678.scope` → `org.gimp.GIMP`,
/// `app-org.kde.konsole@abc.service` → `org.kde.konsole`.
pub fn app_id_from_unit(unit: &str) -> Option<String> {
    let stem = unit.strip_suffix(".scope").or_else(|| unit.strip_suffix(".service"))?;
    let rest = stem.strip_prefix("app-")?;
    // Optional launcher prefix.
    let rest = ["gnome-", "flatpak-", "cosmic-", "kde-", "dbus-:1.", "snap."]
        .iter()
        .find_map(|p| rest.strip_prefix(p))
        .unwrap_or(rest);
    // Strip "@instance" and a trailing "-<digits|random>" suffix.
    let rest = rest.split('@').next()?;
    let id = match rest.rsplit_once('-') {
        Some((head, tail)) if !head.is_empty() && (tail.chars().all(|c| c.is_ascii_digit()) || tail.len() >= 8) => head,
        _ => rest,
    };
    let id = id.replace("\\x2d", "-");
    if id.is_empty() { None } else { Some(id) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_app_scope() {
        let i = parse("/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-firefox-1234.scope");
        assert_eq!(i.unit.as_deref(), Some("app-gnome-firefox-1234.scope"));
        assert_eq!(i.slice.as_deref(), Some("app.slice"));
        assert_eq!(i.user, Some(1000));
        assert_eq!(i.target(), Some(Target::user_unit(1000, "app-gnome-firefox-1234.scope")));
    }

    #[test]
    fn nested_subgroups_and_sessions() {
        let i = parse("/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.gnome.Terminal.slice/vte-spawn-abc.scope");
        assert_eq!(i.unit.as_deref(), Some("vte-spawn-abc.scope"));
        assert_eq!(i.slice.as_deref(), Some("app-org.gnome.Terminal.slice"));
        let s = parse("/user.slice/user-1000.slice/session-2.scope");
        assert_eq!(s.user, None);
        assert_eq!(s.unit.as_deref(), Some("session-2.scope"));
        let d = parse("/system.slice/docker-0123abcd.scope");
        assert_eq!(d.target(), Some(Target::system_unit("docker-0123abcd.scope")));
        let m = parse("/user.slice/user-1000.slice/user@1000.service/init.scope");
        assert_eq!(m.target(), Some(Target::user_unit(1000, "init.scope")));
    }

    #[test]
    fn proc_cgroup() {
        assert_eq!(from_proc_cgroup("0::/system.slice/sshd.service\n").as_deref(), Some("/system.slice/sshd.service"));
        assert_eq!(from_proc_cgroup("12:cpu:/foo\n"), None);
    }

    #[test]
    fn app_ids() {
        assert_eq!(app_id_from_unit("app-gnome-org.mozilla.firefox-1234.scope").as_deref(), Some("org.mozilla.firefox"));
        assert_eq!(app_id_from_unit("app-flatpak-org.gimp.GIMP-5678.scope").as_deref(), Some("org.gimp.GIMP"));
        assert_eq!(app_id_from_unit("app-org.kde.konsole@0a1b2c.service").as_deref(), Some("org.kde.konsole"));
        assert_eq!(app_id_from_unit("app-gnome-code-99.scope").as_deref(), Some("code"));
        assert_eq!(app_id_from_unit("sshd.service"), None);
    }
}
