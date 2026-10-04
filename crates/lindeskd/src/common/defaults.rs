//! Zero-configuration defaults: built-in rules when none are installed, directories with
//! the right modes, and the reasons a daemon runs degraded.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use lindesk_builtins::classifier::rules::{ADMIN_DIR, USER_DIR, VENDOR_DIR};
use lindesk_core::config::Config;
use lindesk_plugin_api::Privilege;
use serde_json::json;

/// The vendor rule files, compiled in as a fallback for hosts where
/// `/usr/share/lindesk/rules.d` is missing (e.g. running from a cargo build).
pub const EMBEDDED_RULES: &[(&str, &str)] = &[
    ("00-slices.toml", include_str!("../../../../packaging/config/rules.d/00-slices.toml")),
    ("10-critical.toml", include_str!("../../../../packaging/config/rules.d/10-critical.toml")),
    ("50-background.toml", include_str!("../../../../packaging/config/rules.d/50-background.toml")),
    ("60-batch.toml", include_str!("../../../../packaging/config/rules.d/60-batch.toml")),
];

// Profiles need no fallback here: `lindesk_core::profile::builtin_profiles` embeds
// `packaging/config/profiles.d` and `ProfileSet::load` layers the directories on top.

/// Where the `rules` classifier's vendor rules come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RulesSource {
    /// `[plugins.rules.config] dirs` is set.
    Configured,
    /// `/usr/share/lindesk/rules.d` exists.
    System,
    /// Built into the binary, written to `<state-dir>/rules.d.builtin`.
    Embedded,
}

impl RulesSource {
    pub fn as_str(self) -> &'static str {
        match self {
            RulesSource::Configured => "configured",
            RulesSource::System => "system",
            RulesSource::Embedded => "embedded",
        }
    }
}

/// Make sure the `rules` classifier has vendor rules. When the vendor directory is missing
/// and no dirs are configured, write the embedded rules to `<state_dir>/rules.d.builtin` and
/// point the classifier at it (then the admin dir, then, for users, their own dir).
pub fn ensure_rules(config: &mut Config, state_dir: &Path, privilege: Privilege) -> RulesSource {
    ensure_rules_in(config, state_dir, privilege, Path::new(VENDOR_DIR))
}

fn ensure_rules_in(config: &mut Config, state_dir: &Path, privilege: Privilege, vendor: &Path) -> RulesSource {
    let pc = config.plugins.entry("rules".to_string()).or_default();
    if let Some(dirs) = pc.config.get("dirs") {
        // Our own earlier fallback (ensure_rules is idempotent).
        let ours = dirs.get(0).and_then(|d| d.as_str()).is_some_and(|d| d.ends_with("rules.d.builtin"));
        return if ours { RulesSource::Embedded } else { RulesSource::Configured };
    }
    if vendor.is_dir() {
        return RulesSource::System;
    }
    let dir = state_dir.join("rules.d.builtin");
    let written = std::fs::create_dir_all(&dir)
        .and_then(|()| EMBEDDED_RULES.iter().try_for_each(|(name, text)| std::fs::write(dir.join(name), text)));
    if let Err(e) = written {
        tracing::warn!("cannot write the built-in rules to {}: {e}; classifying without vendor rules", dir.display());
        return RulesSource::System;
    }
    let mut dirs = vec![dir.display().to_string(), ADMIN_DIR.to_string()];
    if privilege == Privilege::User {
        dirs.push(USER_DIR.to_string());
    }
    if !pc.config.is_object() {
        pc.config = json!({});
    }
    pc.config["dirs"] = json!(dirs);
    tracing::info!("{VENDOR_DIR} not found; using the built-in rules");
    RulesSource::Embedded
}

/// Gid of a group from `/etc/group`.
pub fn group_gid(etc: &Path, name: &str) -> Option<u32> {
    let text = std::fs::read_to_string(etc.join("group")).ok()?;
    text.lines().find_map(|l| {
        let mut f = l.split(':');
        (f.next()? == name).then_some(())?;
        f.next()?;
        f.next()?.parse().ok()
    })
}

/// Create `dir` (and parents) with `mode`; when running as root, hand it to `group`
/// (falling back to root's group when that group doesn't exist).
pub fn ensure_dir(dir: &Path, mode: u32, group: Option<&str>) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))?;
    if rustix::process::geteuid().is_root()
        && let Some(g) = group
    {
        let gid = group_gid(Path::new("/etc"), g).unwrap_or(0);
        rustix::fs::chown(dir, None, Some(rustix::fs::Gid::from_raw(gid)))?;
    }
    Ok(dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_rules_parse() {
        let files: Vec<(&str, &str)> = EMBEDDED_RULES.to_vec();
        lindesk_builtins::classifier::RulesClassifier::from_files(files).expect("the shipped rules are valid");
    }

    #[test]
    fn rules_fallback() {
        let tmp = crate::common::test_dir();
        let mut c = Config::default();
        let src = ensure_rules_in(&mut c, tmp.path(), Privilege::User, Path::new("/nonexistent/rules.d"));
        assert_eq!(src, RulesSource::Embedded);
        let dirs = c.plugin("rules").config["dirs"].clone();
        assert_eq!(dirs[0], tmp.path().join("rules.d.builtin").display().to_string());
        assert_eq!(dirs[1], ADMIN_DIR);
        assert_eq!(dirs[2], USER_DIR);
        assert!(tmp.path().join("rules.d.builtin/10-critical.toml").is_file());

        // Idempotent; configured dirs and an installed vendor dir are left alone.
        assert_eq!(ensure_rules_in(&mut c, tmp.path(), Privilege::User, Path::new("/x")), RulesSource::Embedded);
        c.plugins.get_mut("rules").unwrap().config = json!({"dirs": ["/mine"]});
        assert_eq!(ensure_rules_in(&mut c, tmp.path(), Privilege::User, Path::new("/x")), RulesSource::Configured);
        let mut c = Config::default();
        assert_eq!(ensure_rules_in(&mut c, tmp.path(), Privilege::System, tmp.path()), RulesSource::System);
        assert!(c.plugin("rules").config.get("dirs").is_none());
    }

    #[test]
    fn groups_and_dirs() {
        let tmp = crate::common::test_dir();
        std::fs::write(tmp.path().join("group"), "root:x:0:\nlindesk:x:987:alice\n").unwrap();
        assert_eq!(group_gid(tmp.path(), "lindesk"), Some(987));
        assert_eq!(group_gid(tmp.path(), "nope"), None);
        let d = ensure_dir(&tmp.path().join("a/b"), 0o750, Some("lindesk")).unwrap();
        assert_eq!(std::fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o750);
    }
}
