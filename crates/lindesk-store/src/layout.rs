//! On-disk layout of the telemetry lake, shared by the writer and by `lindesk-query`.
//!
//! ```text
//! <root>/<table>/date=YYYY-MM-DD/hour=HH/part-<ulid>.vortex    raw and bench tables
//! <root>/<table>/date=YYYY-MM-DD/part-<ulid>.vortex            rollup tables
//! <root>/_format.json                                          format marker (vortex + version)
//! <root>/.staging/                                             maintenance scratch space
//! <root>/.lock                                                 flock(2) coordination file
//! <root>/../catalog.duckdb                                     DuckDB catalog (views only)
//! ```
//!
//! Partition keys are UTC. Files are immutable: they are written as hidden `.tmp` files,
//! fsynced, and renamed into place. Maintenance replaces whole partition directories.
//!
//! **Locking.** Writers hold a *shared* lock on `.lock` while they add files; maintenance
//! (compaction, prune, delete, purge) holds it *exclusively*. Writers never wait for it:
//! when maintenance is running they keep rows buffered and retry on the next flush.
//! Readers never lock; they rely on the atomic partition swap instead.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, NaiveDate, Timelike, Utc};
use lindesk_plugin_api::TimestampMs;
use rustix::fs::FlockOperation;

pub use crate::format::{LEGACY_PARQUET_EXT, VORTEX_EXT};
use crate::schema::{Granularity, TableDef};

/// Name of the staging directory under the lake root.
pub const STAGING_DIR: &str = ".staging";
/// Name of the lock file under the lake root.
pub const LOCK_FILE: &str = ".lock";
/// File name of the DuckDB catalog, stored next to the lake root.
pub const CATALOG_FILE: &str = "catalog.duckdb";

const MS_PER_HOUR: i64 = 3_600_000;
const MS_PER_DAY: i64 = 24 * MS_PER_HOUR;

/// Default lake root: `/var/lib/lindesk/telemetry` for the system store, otherwise
/// `$XDG_STATE_HOME/lindesk/telemetry` (falling back to `~/.local/state/lindesk/telemetry`).
pub fn default_root(system: bool) -> PathBuf {
    if system {
        return PathBuf::from("/var/lib/lindesk/telemetry");
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    state.join("lindesk/telemetry")
}

/// Path of the DuckDB catalog for a lake root (`<root>/../catalog.duckdb`).
pub fn catalog_path(root: &Path) -> PathBuf {
    root.parent().unwrap_or(root).join(CATALOG_FILE)
}

/// Identifies one partition directory of a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PartitionKey {
    /// UTC date.
    pub date: NaiveDate,
    /// UTC hour, or `None` for day-partitioned tables.
    pub hour: Option<u8>,
}

impl PartitionKey {
    /// The partition that holds a row with timestamp `ts` at the given granularity.
    pub fn for_ts(ts: TimestampMs, granularity: Granularity) -> PartitionKey {
        let dt = DateTime::<Utc>::from_timestamp_millis(ts).unwrap_or_default();
        let hour = match granularity {
            Granularity::Hour => Some(dt.hour() as u8),
            Granularity::Day => None,
        };
        PartitionKey { date: dt.date_naive(), hour }
    }

    /// Converts to a key of another granularity (e.g. the day partition of an hour).
    pub fn with_granularity(self, granularity: Granularity) -> PartitionKey {
        PartitionKey::for_ts(self.start_ms(), granularity)
    }

    /// Inclusive start of the partition, in ms since the epoch.
    pub fn start_ms(&self) -> TimestampMs {
        let day = self.date.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc().timestamp_millis();
        day + i64::from(self.hour.unwrap_or(0)) * MS_PER_HOUR
    }

    /// Exclusive end of the partition, in ms since the epoch.
    pub fn end_ms(&self) -> TimestampMs {
        self.start_ms() + if self.hour.is_some() { MS_PER_HOUR } else { MS_PER_DAY }
    }

    /// True when the partition lies entirely before the hour (or day) containing `now`,
    /// so the daemons no longer write to it in normal operation.
    pub fn is_sealed(&self, now: TimestampMs) -> bool {
        let g = if self.hour.is_some() { Granularity::Hour } else { Granularity::Day };
        self.end_ms() <= PartitionKey::for_ts(now, g).start_ms()
    }

    /// Path relative to the table directory, e.g. `date=2026-10-03/hour=07`.
    pub fn rel_path(&self) -> PathBuf {
        let mut p = PathBuf::from(format!("date={}", self.date.format("%Y-%m-%d")));
        if let Some(h) = self.hour {
            p.push(format!("hour={h:02}"));
        }
        p
    }

    /// Parses `date=YYYY-MM-DD` and `hour=HH` directory names.
    pub fn parse(date_dir: &str, hour_dir: Option<&str>) -> Option<PartitionKey> {
        let date = NaiveDate::parse_from_str(date_dir.strip_prefix("date=")?, "%Y-%m-%d").ok()?;
        let hour = match hour_dir {
            Some(h) => {
                let h: u8 = h.strip_prefix("hour=")?.parse().ok()?;
                (h < 24).then_some(h)?.into()
            }
            None => None,
        };
        Some(PartitionKey { date, hour })
    }
}

impl std::fmt::Display for PartitionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.hour {
            Some(h) => write!(f, "{} {h:02}:00", self.date),
            None => write!(f, "{}", self.date),
        }
    }
}

/// Directory of a table under the lake root.
pub fn table_dir(root: &Path, table: &str) -> PathBuf {
    root.join(table)
}

/// Directory of one partition.
pub fn partition_dir(root: &Path, table: &str, key: &PartitionKey) -> PathBuf {
    table_dir(root, table).join(key.rel_path())
}

/// A data file in a partition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataFile {
    /// Absolute path.
    pub path: PathBuf,
    /// Size in bytes.
    pub bytes: u64,
    /// Modification time.
    pub mtime: SystemTime,
}

impl DataFile {
    /// True for a `.parquet` file written before lindesk switched to Vortex.
    pub fn is_legacy(&self) -> bool {
        self.path.extension().is_some_and(|e| e == LEGACY_PARQUET_EXT)
    }
}

/// A partition directory and its data files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Partition {
    /// Table name.
    pub table: &'static str,
    /// Partition key.
    pub key: PartitionKey,
    /// Partition directory.
    pub dir: PathBuf,
    /// Committed data files (`.vortex`, plus legacy `.parquet`), sorted by name (ULIDs sort
    /// by creation time).
    pub files: Vec<DataFile>,
}

impl Partition {
    /// Total bytes of the partition's files.
    pub fn bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes).sum()
    }

    /// True when the partition still holds files from the pre-Vortex Parquet writer.
    pub fn has_legacy(&self) -> bool {
        self.files.iter().any(DataFile::is_legacy)
    }

    /// Newest modification time among the partition's files.
    pub fn newest_mtime(&self) -> Option<SystemTime> {
        self.files.iter().map(|f| f.mtime).max()
    }
}

/// Lists a table's partitions in key order. Missing directories yield an empty list;
/// unparseable names and in-progress `.tmp` files are ignored.
pub fn list_partitions(root: &Path, table: &TableDef) -> io::Result<Vec<Partition>> {
    let mut out = Vec::new();
    let tdir = table_dir(root, table.name);
    for date in read_dir_names(&tdir)? {
        let date_dir = tdir.join(&date);
        match table.granularity() {
            Granularity::Day => {
                if let Some(key) = PartitionKey::parse(&date, None) {
                    out.push(load_partition(table.name, key, date_dir)?);
                }
            }
            Granularity::Hour => {
                for hour in read_dir_names(&date_dir)? {
                    if let Some(key) = PartitionKey::parse(&date, Some(&hour)) {
                        out.push(load_partition(table.name, key, date_dir.join(&hour))?);
                    }
                }
            }
        }
    }
    out.sort_by_key(|p| p.key);
    Ok(out)
}

/// Lists one partition (empty when its directory does not exist).
pub fn partition(root: &Path, table: &TableDef, key: PartitionKey) -> io::Result<Partition> {
    load_partition(table.name, key, partition_dir(root, table.name, &key))
}

fn load_partition(table: &'static str, key: PartitionKey, dir: PathBuf) -> io::Result<Partition> {
    let mut files = Vec::new();
    for name in read_dir_names(&dir)? {
        let data = name.ends_with(&format!(".{VORTEX_EXT}")) || name.ends_with(&format!(".{LEGACY_PARQUET_EXT}"));
        if name.starts_with('.') || !data {
            continue;
        }
        let path = dir.join(&name);
        match fs::metadata(&path) {
            Ok(m) if m.is_file() => {
                files.push(DataFile { path, bytes: m.len(), mtime: m.modified()? });
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Partition { table, key, dir, files })
}

/// Names of the entries of a directory, sorted; a missing directory is empty.
pub fn read_dir_names(dir: &Path) -> io::Result<Vec<String>> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut names = Vec::new();
    for e in rd {
        if let Ok(n) = e?.file_name().into_string() {
            names.push(n);
        }
    }
    names.sort();
    Ok(names)
}

/// A fresh data file name, `part-<ulid>.vortex`.
pub fn new_part_name() -> String {
    format!("part-{}.{VORTEX_EXT}", ulid::Ulid::new())
}

/// fsyncs a directory so renames and creations inside it are durable.
pub fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Creates `dir` and any missing parents with `mode`, fsyncing each parent after a creation.
pub fn create_dir_all(dir: &Path, mode: u32) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        create_dir_all(parent, mode)?;
    }
    match fs::DirBuilder::new().mode(mode).create(dir) {
        Ok(()) => {
            if let Some(parent) = dir.parent() {
                fsync_dir(parent)?;
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
        Err(e) => Err(e),
    }
}

/// Creates the lake root with `dir_mode` and enforces that mode on it, preserving a
/// setgid bit set by packaging (so new files inherit e.g. the `lindesk` group).
pub fn prepare_root(root: &Path, dir_mode: u32) -> io::Result<()> {
    create_dir_all(root, dir_mode)?;
    let current = fs::metadata(root)?.permissions().mode();
    let wanted = (dir_mode & 0o777) | (current & 0o2000);
    if current & 0o7777 != wanted {
        fs::set_permissions(root, fs::Permissions::from_mode(wanted))?;
    }
    Ok(())
}

/// Lock mode on the lake's `.lock` file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockMode {
    /// Held by writers while adding files.
    Shared,
    /// Held by maintenance.
    Exclusive,
}

/// A held `flock(2)` on `<root>/.lock`, released on drop.
#[derive(Debug)]
pub struct LakeLock {
    file: File,
}

impl LakeLock {
    fn open(root: &Path, file_mode: u32) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(file_mode)
            .open(root.join(LOCK_FILE))
    }

    /// Tries to take the lock without blocking. `Ok(None)` means someone else holds it
    /// in a conflicting mode.
    pub fn try_acquire(root: &Path, mode: LockMode, file_mode: u32) -> io::Result<Option<LakeLock>> {
        let file = Self::open(root, file_mode)?;
        let op = match mode {
            LockMode::Shared => FlockOperation::NonBlockingLockShared,
            LockMode::Exclusive => FlockOperation::NonBlockingLockExclusive,
        };
        match rustix::fs::flock(&file, op) {
            Ok(()) => Ok(Some(LakeLock { file })),
            Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Takes the lock, blocking until it is available.
    pub fn acquire(root: &Path, mode: LockMode, file_mode: u32) -> io::Result<LakeLock> {
        let file = Self::open(root, file_mode)?;
        let op = match mode {
            LockMode::Shared => FlockOperation::LockShared,
            LockMode::Exclusive => FlockOperation::LockExclusive,
        };
        loop {
            match rustix::fs::flock(&file, op) {
                Ok(()) => return Ok(LakeLock { file }),
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Waits up to `timeout` for the lock. The wait is a real blocking `flock(2)` on a
    /// helper thread, so the kernel hands the lock over as soon as the holder releases it
    /// (polling could starve behind maintenance that re-locks quickly). On timeout the
    /// helper keeps waiting and releases the lock immediately if it gets it later.
    pub fn acquire_timeout(
        root: &Path,
        mode: LockMode,
        file_mode: u32,
        timeout: Duration,
    ) -> io::Result<Option<LakeLock>> {
        if let Some(l) = Self::try_acquire(root, mode, file_mode)? {
            return Ok(Some(l));
        }
        let root = root.to_path_buf();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new().name("lindesk-lake-lock".into()).spawn(move || {
            // If the receiver gave up, the send fails and the lock is dropped (released).
            let _ = tx.send(Self::acquire(&root, mode, file_mode));
        })?;
        match rx.recv_timeout(timeout) {
            Ok(r) => r.map(Some),
            Err(_) => Ok(None),
        }
    }
}

impl Drop for LakeLock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.file, FlockOperation::Unlock);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_keys() {
        // 2026-10-03T07:15:00Z
        let ts = 1_791_011_700_000;
        let k = PartitionKey::for_ts(ts, Granularity::Hour);
        assert_eq!(k.rel_path(), PathBuf::from("date=2026-10-03/hour=07"));
        assert!(k.start_ms() <= ts && ts < k.end_ms());
        assert_eq!(k.end_ms() - k.start_ms(), MS_PER_HOUR);
        assert_eq!(PartitionKey::parse("date=2026-10-03", Some("hour=07")), Some(k));
        assert_eq!(PartitionKey::parse("date=2026-10-03", Some("hour=24")), None);
        let d = k.with_granularity(Granularity::Day);
        assert_eq!(d.rel_path(), PathBuf::from("date=2026-10-03"));
        assert!(!k.is_sealed(ts));
        assert!(k.is_sealed(k.end_ms()));
        assert!(!d.is_sealed(k.end_ms()));
        assert!(d.is_sealed(d.end_ms()));
    }

    #[test]
    fn locks_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let ex = LakeLock::try_acquire(dir.path(), LockMode::Exclusive, 0o600).unwrap().unwrap();
        assert!(LakeLock::try_acquire(dir.path(), LockMode::Shared, 0o600).unwrap().is_none());
        drop(ex);
        let sh = LakeLock::try_acquire(dir.path(), LockMode::Shared, 0o600).unwrap().unwrap();
        let sh2 = LakeLock::try_acquire(dir.path(), LockMode::Shared, 0o600).unwrap();
        assert!(sh2.is_some());
        assert!(LakeLock::try_acquire(dir.path(), LockMode::Exclusive, 0o600).unwrap().is_none());
        let t = Duration::from_millis(50);
        assert!(LakeLock::acquire_timeout(dir.path(), LockMode::Exclusive, 0o600, t).unwrap().is_none());
        drop((sh, sh2));
        // A blocked waiter gets the lock as soon as it is released.
        let ex = LakeLock::try_acquire(dir.path(), LockMode::Exclusive, 0o600).unwrap().unwrap();
        let root = dir.path().to_path_buf();
        let waiter = std::thread::spawn(move || {
            LakeLock::acquire_timeout(&root, LockMode::Shared, 0o600, Duration::from_secs(30)).unwrap().is_some()
        });
        std::thread::sleep(Duration::from_millis(50));
        drop(ex);
        assert!(waiter.join().unwrap());
    }
}
