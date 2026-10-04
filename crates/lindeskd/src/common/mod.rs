//! Code shared by `lindeskd` and `lindesk-session` (the session crate includes this
//! directory with `#[path]`, so there is one copy without an extra crate).

pub mod calibration;
pub mod defaults;
pub mod engine_thread;
pub mod logging;
pub mod observe;
pub mod profiles;
pub mod restore;
pub mod setup;
pub mod startup;
pub mod what;

use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// The signals the daemons handle, installed early (before the engine applies anything) so
/// a SIGTERM during start-up still goes through the orderly shutdown.
pub struct Signals {
    pub term: tokio::signal::unix::Signal,
    pub int: tokio::signal::unix::Signal,
    pub hup: tokio::signal::unix::Signal,
}

impl Signals {
    /// Must run inside the tokio runtime (`rt.block_on(async { Signals::install() })`).
    pub fn install() -> std::io::Result<Signals> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Signals {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
            hup: signal(SignalKind::hangup())?,
        })
    }
}

/// Owner uid of a live process (`/proc/<pid>`), `None` when it doesn't exist.
pub fn proc_uid(proc_root: &Path, pid: i32) -> Option<u32> {
    if pid <= 0 {
        return None;
    }
    std::fs::metadata(proc_root.join(pid.to_string())).ok().map(|m| m.uid())
}

/// A temporary directory on tmpfs when available (tests): on a busy ext4 disk, replacing a
/// file by rename can stall for seconds (`auto_da_alloc` writeback).
#[cfg(test)]
pub fn test_dir() -> tempfile::TempDir {
    let uid = rustix::process::getuid().as_raw();
    [std::path::PathBuf::from("/dev/shm"), std::path::PathBuf::from(format!("/run/user/{uid}"))]
        .iter()
        .filter(|d| d.is_dir())
        .find_map(|d| tempfile::tempdir_in(d).ok())
        .unwrap_or_else(|| tempfile::tempdir().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_uid() {
        let me = rustix::process::getuid().as_raw();
        assert_eq!(proc_uid(Path::new("/proc"), std::process::id() as i32), Some(me));
        assert_eq!(proc_uid(Path::new("/proc"), 0), None);
        assert_eq!(proc_uid(Path::new("/proc"), i32::MAX), None);
    }
}
