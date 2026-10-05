//! Build identity shared by every lindesk binary: the workspace version (the single source
//! of truth, `[workspace.package] version` in Cargo.toml), the git commit it was built from
//! and the build date. See `build.rs` for where the commit and date come from.
//!
//! ```text
//! $ lindeskctl --version
//! lindeskctl 0.1.0 (2c021b4, 2026-10-05)
//! ```
//!
//! Binaries use [`LONG`] as clap's `version`; the daemons put [`VERSION`] and [`git`] into
//! their status JSON (`daemon.version`, `daemon.git`).

/// The workspace version, e.g. `0.1.0`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short commit hash (7 hex digits), or empty when unknown.
pub const GIT_HASH: &str = env!("LINDESK_GIT_HASH");

/// `YYYY-MM-DD` (UTC): the commit date unless the build overrode it.
pub const BUILD_DATE: &str = env!("LINDESK_BUILD_DATE");

/// `0.1.0 (2c021b4, 2026-10-05)`, or `0.1.0 (2026-10-05)` without a commit.
pub const LONG: &str = env!("LINDESK_VERSION_LONG");

/// The commit hash, if known.
pub fn git() -> Option<&'static str> {
    (!GIT_HASH.is_empty()).then_some(GIT_HASH)
}

/// `<binary> 0.1.0 (2c021b4, 2026-10-05)`.
pub fn line(binary: &str) -> String {
    format!("{binary} {LONG}")
}

/// Why a client of version [`VERSION`] should warn about a daemon reporting `daemon`
/// (`daemon.version` of its status), or `None` when they match. A missing version means a
/// daemon too old to report one.
pub fn mismatch(client: &str, daemon_name: &str, daemon: Option<&str>) -> Option<String> {
    match daemon {
        Some(v) if v == VERSION => None,
        Some(v) => Some(format!(
            "{daemon_name} runs version {v} but {client} is {VERSION}: restart it to use the installed version \
             (a partial upgrade, or the daemon wasn't restarted)"
        )),
        None => Some(format!("{daemon_name} doesn't report its version (older than {client} {VERSION}); restart it")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_version_has_the_shape_we_document() {
        assert!(LONG.starts_with(VERSION), "{LONG}");
        let inner = LONG[VERSION.len()..].trim().trim_start_matches('(').trim_end_matches(')');
        let date = inner.rsplit(", ").next().unwrap();
        assert_eq!(date, BUILD_DATE);
        assert_eq!(date.len(), 10, "{date}");
        assert_eq!(&date[4..5], "-");
        if let Some(h) = git() {
            assert!(h.len() >= 7 && h.chars().all(|c| c.is_ascii_hexdigit()), "{h}");
            assert!(LONG.contains(h));
        }
        assert_eq!(line("lindeskctl"), format!("lindeskctl {LONG}"));
    }

    #[test]
    fn mismatch_messages() {
        assert_eq!(mismatch("lindeskctl", "lindesk-session", Some(VERSION)), None);
        let m = mismatch("lindeskctl", "lindesk-session", Some("0.0.1")).unwrap();
        assert!(m.contains("0.0.1") && m.contains(VERSION) && m.contains("restart"), "{m}");
        assert!(mismatch("lindeskctl", "lindeskd", None).unwrap().contains("doesn't report"));
    }
}
