//! Filesystem roots and atomic file helpers. Everything is root-relative so tests run
//! against fixture trees.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::model::Scope;
use crate::{ChangeError, Result};

/// Where the kernel and OS interfaces live (same field names as `lindesk_sys::Roots`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Roots {
    /// procfs, usually `/proc`.
    pub proc: PathBuf,
    /// sysfs, usually `/sys`.
    pub sys: PathBuf,
    /// `/etc` (persisted sysctl.d and udev rules).
    pub etc: PathBuf,
}

impl Roots {
    pub fn host() -> Self {
        Roots { proc: "/proc".into(), sys: "/sys".into(), etc: "/etc".into() }
    }

    /// `prefix/proc`, `prefix/sys`, `prefix/etc`.
    pub fn under(prefix: impl AsRef<Path>) -> Self {
        let p = prefix.as_ref();
        Roots { proc: p.join("proc"), sys: p.join("sys"), etc: p.join("etc") }
    }

    /// `/etc/sysctl.d/90-lindesk.conf`.
    pub fn sysctl_conf(&self) -> PathBuf {
        self.etc.join("sysctl.d/90-lindesk.conf")
    }

    /// `/etc/udev/rules.d/90-lindesk-iosched.rules`.
    pub fn udev_rules(&self) -> PathBuf {
        self.etc.join("udev/rules.d/90-lindesk-iosched.rules")
    }
}

impl Default for Roots {
    fn default() -> Self {
        Roots::host()
    }
}

/// The lindesk.toml files config ops edit.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigPaths {
    pub user: Option<PathBuf>,
    pub system: Option<PathBuf>,
}

impl ConfigPaths {
    pub fn get(&self, scope: Scope) -> Option<&Path> {
        match scope {
            Scope::User => self.user.as_deref(),
            Scope::System => self.system.as_deref(),
        }
    }

    /// The real file for `scope`: `/etc/lindesk/lindesk.toml`, or
    /// `$XDG_CONFIG_HOME/lindesk/lindesk.toml` (falling back to `~/.config`).
    pub fn host(scope: Scope) -> Self {
        match scope {
            Scope::System => ConfigPaths { user: None, system: Some("/etc/lindesk/lindesk.toml".into()) },
            Scope::User => {
                let base = std::env::var_os("XDG_CONFIG_HOME")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
                    .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
                ConfigPaths { user: base.map(|b| b.join("lindesk/lindesk.toml")), system: None }
            }
        }
    }
}

pub(crate) fn io_err(path: &Path, source: std::io::Error) -> ChangeError {
    ChangeError::Io { path: path.to_path_buf(), source }
}

/// Read a file; `None` when it doesn't exist.
pub(crate) fn read_opt(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(path, e)),
    }
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Write via a temporary file in the same directory + fsync + rename, keeping the mode of
/// an existing file (`default_mode` for a new one).
pub(crate) fn atomic_write(path: &Path, contents: &[u8], default_mode: u32) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
    let mode = fs::metadata(path).map(|m| m.permissions().mode() & 0o7777).unwrap_or(default_mode);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.tmp-{}-{}", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
    let res = (|| -> std::io::Result<()> {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.set_permissions(fs::Permissions::from_mode(mode))?;
        f.write_all(contents)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res.map_err(|e| io_err(path, e))
}

/// Remove a file, ignoring "not found".
pub(crate) fn remove_opt(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err(path, e)),
    }
}

/// Write a kernel attribute in place (procfs/sysfs files can't be renamed over).
pub(crate) fn write_attr(path: &Path, value: &str) -> Result<()> {
    let mut f = OpenOptions::new().write(true).truncate(true).open(path).map_err(|e| io_err(path, e))?;
    f.write_all(value.as_bytes()).map_err(|e| io_err(path, e))
}

/// An exclusive `flock` on `<dir>/.lock`, released on drop. Works across processes and
/// across handles in one process (locks are per open file description).
pub(crate) struct DirLock(#[allow(dead_code)] File);

impl DirLock {
    pub(crate) fn acquire(dir: &Path) -> Result<DirLock> {
        let path = dir.join(".lock");
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| io_err(&path, e))?;
        rustix::fs::flock(&f, rustix::fs::FlockOperation::LockExclusive)
            .map_err(|e| io_err(&path, std::io::Error::from(e)))?;
        Ok(DirLock(f))
    }
}
