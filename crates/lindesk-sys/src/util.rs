//! Small filesystem helpers shared by the modules.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

/// Build an `InvalidData` error.
pub(crate) fn invalid_data(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Build an `InvalidInput` error.
pub(crate) fn invalid_input(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

/// Read a whole small file as text, in as few system calls as it takes.
///
/// procfs and cgroupfs files report a size of 0, so `fs::read_to_string` stats the file and
/// then probes with a run of small reads (eight for `/proc/<pid>/status`). The daemons read
/// thousands of these a minute; this reads a page at a time and never stats.
pub fn read_file(path: &Path) -> io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut page = [0u8; 4096];
    let mut out = Vec::new();
    loop {
        match f.read(&mut page) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&page[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    String::from_utf8(out).map_err(|_| invalid_data(format!("{} is not UTF-8", path.display())))
}

/// Read a file and strip trailing whitespace.
pub(crate) fn read_trimmed(path: &Path) -> io::Result<String> {
    let mut s = read_file(path)?;
    s.truncate(s.trim_end().len());
    Ok(s)
}

/// Like [`read_trimmed`], but a missing file is `Ok(None)`.
pub(crate) fn read_opt(path: &Path) -> io::Result<Option<String>> {
    match read_trimmed(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Best-effort read: any error is `None`.
pub(crate) fn read_lossy(path: &Path) -> Option<String> {
    read_trimmed(path).ok()
}

/// Write a whole value with a single `write(2)`, the way kernfs/sysfs attribute files expect.
///
/// The file is opened without `O_CREAT` (attribute files must already exist) and with
/// `O_TRUNC`, which kernfs ignores and which keeps fixture files in tests faithful.
pub(crate) fn write_attr(path: &Path, value: &str) -> io::Result<()> {
    let mut f = OpenOptions::new().write(true).truncate(true).open(path)?;
    let n = f.write(value.as_bytes())?;
    if n != value.len() {
        return Err(io::Error::new(io::ErrorKind::WriteZero, format!("short write to {}", path.display())));
    }
    Ok(())
}

/// Join a possibly absolute path under `root`, refusing `..` so a crafted path can't
/// escape the root.
pub(crate) fn under(root: &Path, rel: impl AsRef<Path>) -> io::Result<PathBuf> {
    let mut out = root.to_path_buf();
    for c in rel.as_ref().components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(invalid_input(format!("path '{}' must not contain '..'", rel.as_ref().display())));
            }
        }
    }
    Ok(out)
}

/// Validate a cgroup attribute name: a single path component.
pub(crate) fn check_attr_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return Err(invalid_input(format!("invalid cgroup attribute name '{name}'")));
    }
    Ok(())
}

/// Validate a `MAJ:MIN` device string.
pub(crate) fn check_device(dev: &str) -> io::Result<()> {
    let ok = dev
        .split_once(':')
        .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty() && a.bytes().chain(b.bytes()).all(|c| c.is_ascii_digit()));
    if ok { Ok(()) } else { Err(invalid_input(format!("invalid block device '{dev}', expected MAJ:MIN"))) }
}

/// Names of the subdirectories of `dir`, sorted. A missing directory yields an empty list.
pub(crate) fn subdirs(dir: &Path) -> io::Result<Vec<String>> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in rd {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Some(name) = entry.file_name().to_str()
        {
            out.push(name.to_string());
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_rejects_parent() {
        let r = Path::new("/sys/fs/cgroup");
        assert_eq!(under(r, "/user.slice/a").unwrap(), PathBuf::from("/sys/fs/cgroup/user.slice/a"));
        assert_eq!(under(r, "").unwrap(), PathBuf::from("/sys/fs/cgroup"));
        assert_eq!(under(r, "/").unwrap(), PathBuf::from("/sys/fs/cgroup"));
        assert!(under(r, "/user.slice/../../etc").is_err());
    }

    #[test]
    fn device_and_attr_validation() {
        assert!(check_device("259:0").is_ok());
        assert!(check_device("259").is_err());
        assert!(check_device("a:b").is_err());
        assert!(check_device(":1").is_err());
        assert!(check_attr_name("io.max").is_ok());
        assert!(check_attr_name("../x").is_err());
        assert!(check_attr_name("").is_err());
    }
}
