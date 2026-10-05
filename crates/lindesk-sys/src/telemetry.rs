//! Finding the optional telemetry component from the core binaries.
//!
//! The core package (`lindeskd`, `lindesk-session`, `lindeskctl`, `lindesk-mcp`,
//! `lindesk-bench`) links neither DuckDB nor Vortex. Reading and maintaining the telemetry
//! lake is `lindesk-data`'s job, and writing it is the `telemetry` sink plugin's; both ship
//! in the optional `lindesk-telemetry` package. The core binaries run `lindesk-data` as a
//! subprocess and treat its absence as "telemetry isn't installed".

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The lake tool's executable name.
pub const DATA_BIN: &str = "lindesk-data";

/// Overrides where `lindesk-data` is looked for: a path to the executable. Set it to a
/// path that doesn't exist (or to an empty string) to behave as if it weren't installed.
pub const DATA_ENV: &str = "LINDESK_DATA";

/// What every core tool says when a telemetry feature is used without the package.
pub const NOT_INSTALLED: &str =
    "telemetry isn't installed: sudo apt install lindesk-telemetry (or ./packaging/install.sh --with-telemetry)";

fn executable(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// `lindesk-data`: `$LINDESK_DATA` when set, else next to the running executable (a cargo
/// build directory, or `/usr/bin` beside the core binaries), else on `PATH`.
pub fn data_binary() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os(DATA_ENV) {
        let p = PathBuf::from(v);
        return (!p.as_os_str().is_empty() && executable(&p)).then_some(p);
    }
    let sibling = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.join(DATA_BIN)));
    if let Some(p) = sibling.filter(|p| executable(p)) {
        return Some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(DATA_BIN)).find(|p| executable(p))
}

/// True when the telemetry component is installed.
pub fn installed() -> bool {
    data_binary().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_wins() {
        // Only this test touches LINDESK_DATA in this crate.
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("lindesk-data");
        std::fs::write(&fake, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: single-threaded use of this variable within the test binary.
        unsafe { std::env::set_var(DATA_ENV, &fake) };
        assert_eq!(data_binary(), Some(fake.clone()));
        unsafe { std::env::set_var(DATA_ENV, tmp.path().join("missing")) };
        assert_eq!(data_binary(), None);
        assert!(!installed());
        unsafe { std::env::remove_var(DATA_ENV) };
    }
}
