//! The buffered Vortex writer used by the daemons.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lindesk_plugin_api::{Record, TimestampMs};

use crate::StoreError;
use crate::format::{self, VortexCodec};
use crate::layout::{self, LakeLock, LockMode, PartitionKey};
use crate::rows::{self, BenchRun, BenchSample, Cell, Row};
use crate::schema::TableDef;

/// How long `Drop` waits for running maintenance before giving up on the final flush.
const DROP_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// Writer options.
#[derive(Clone, Debug, PartialEq)]
pub struct StoreOptions {
    /// [`Store::maybe_flush`] writes buffered rows once this much time has passed since
    /// the previous flush.
    pub flush_interval: Duration,
    /// [`Store::maybe_flush`] also writes once this many rows are buffered. The buffer is
    /// hard-capped at ten times this; beyond that the oldest rows are dropped (counted in
    /// [`Store::dropped_rows`]) rather than stalling the daemon.
    pub max_buffer_rows: usize,
    /// Mode of directories the writer creates (the lake root is forced to it).
    pub dir_mode: u32,
    /// Mode of files the writer creates.
    pub file_mode: u32,
}

impl StoreOptions {
    /// Per-user store: only the owner can read (0700 / 0600).
    pub fn user() -> Self {
        StoreOptions {
            flush_interval: Duration::from_secs(60),
            max_buffer_rows: 50_000,
            dir_mode: 0o700,
            file_mode: 0o600,
        }
    }

    /// System store: root writes, the `lindesk` group reads (0750 / 0640).
    pub fn system() -> Self {
        StoreOptions { dir_mode: 0o750, file_mode: 0o640, ..Self::user() }
    }
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self::user()
    }
}

/// Result of a flush.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FlushReport {
    /// Files written.
    pub files: Vec<PathBuf>,
    /// Rows written.
    pub rows: usize,
    /// Rows kept in the buffer because maintenance holds the lake lock.
    pub deferred_rows: usize,
}

type BufKey = (PartitionKey, &'static str);

/// A buffered, hive-partitioned Parquet writer.
///
/// Rows are buffered in memory per (table, hour partition). Each flush writes one new
/// Vortex file per buffered partition (`.tmp`, fsync, rename, fsync dir), so
/// readers only ever see complete files. Rows are partitioned by their own timestamp, so
/// a late row lands in the right (possibly sealed) partition; compaction merges it later.
#[derive(Debug)]
pub struct Store {
    root: PathBuf,
    opts: StoreOptions,
    buffers: BTreeMap<BufKey, Vec<(TimestampMs, Vec<Cell>)>>,
    buffered: usize,
    last_flush: Option<TimestampMs>,
    dropped: u64,
    codec: VortexCodec,
}

impl Store {
    /// Opens (creating if needed) a lake at `root` and enforces `opts.dir_mode` on it.
    pub fn open(root: impl AsRef<Path>, opts: StoreOptions) -> Result<Store, StoreError> {
        let root = root.as_ref().to_path_buf();
        layout::prepare_root(&root, opts.dir_mode)?;
        format::write_format_marker(&root, opts.file_mode)?;
        Ok(Store {
            root,
            opts,
            buffers: BTreeMap::new(),
            buffered: 0,
            last_flush: None,
            dropped: 0,
            codec: VortexCodec::new(),
        })
    }

    /// The lake root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The options the store was opened with.
    pub fn options(&self) -> &StoreOptions {
        &self.opts
    }

    /// Rows currently buffered.
    pub fn buffered_rows(&self) -> usize {
        self.buffered
    }

    /// Rows dropped because the buffer hit its hard cap.
    pub fn dropped_rows(&self) -> u64 {
        self.dropped
    }

    /// Buffers records. Nothing is written until a flush.
    pub fn append(&mut self, records: &[Record]) {
        for r in records {
            self.push(rows::record_row(r));
        }
        self.enforce_cap();
    }

    /// Buffers a benchmark run and its samples.
    pub fn append_bench(&mut self, run: &BenchRun, samples: &[BenchSample]) {
        self.push(rows::bench_run_row(run));
        self.append_bench_samples(samples);
    }

    /// Buffers benchmark samples (for runs recorded separately).
    pub fn append_bench_samples(&mut self, samples: &[BenchSample]) {
        for s in samples {
            self.push(rows::bench_sample_row(s));
        }
        self.enforce_cap();
    }

    fn push(&mut self, row: Row) {
        let key = PartitionKey::for_ts(row.ts, row.table.granularity());
        self.buffers.entry((key, row.table.name)).or_default().push((row.ts, row.cells));
        self.buffered += 1;
    }

    fn enforce_cap(&mut self) {
        let cap = self.opts.max_buffer_rows.saturating_mul(10).max(1);
        while self.buffered > cap {
            let Some((key, rows)) = self.buffers.pop_first() else { break };
            self.buffered -= rows.len();
            self.dropped += rows.len() as u64;
            tracing::warn!(table = key.1, partition = %key.0, rows = rows.len(), "telemetry buffer full, dropping rows");
        }
    }

    /// Flushes if the flush interval has elapsed since the last flush, or the buffer holds
    /// at least `max_buffer_rows`. Returns `None` when no flush was due.
    pub fn maybe_flush(&mut self, now: TimestampMs) -> Result<Option<FlushReport>, StoreError> {
        let interval = i64::try_from(self.opts.flush_interval.as_millis()).unwrap_or(i64::MAX);
        let last = *self.last_flush.get_or_insert(now);
        let due = now.saturating_sub(last) >= interval || self.buffered >= self.opts.max_buffer_rows;
        if !due || self.buffered == 0 {
            if due {
                self.last_flush = Some(now);
            }
            return Ok(None);
        }
        self.flush(now).map(Some)
    }

    /// Writes all buffered rows, one file per (table, partition). If maintenance currently
    /// holds the lake lock, nothing is written and the rows stay buffered
    /// ([`FlushReport::deferred_rows`]); the next flush retries.
    pub fn flush(&mut self, now: TimestampMs) -> Result<FlushReport, StoreError> {
        if self.buffered == 0 {
            self.last_flush = Some(now);
            return Ok(FlushReport::default());
        }
        match LakeLock::try_acquire(&self.root, LockMode::Shared, self.opts.file_mode)? {
            Some(lock) => self.write_all(now, lock),
            None => Ok(FlushReport { deferred_rows: self.buffered, ..Default::default() }),
        }
    }

    /// Like [`Store::flush`], but waits up to `timeout` for running maintenance.
    pub fn flush_wait(&mut self, now: TimestampMs, timeout: Duration) -> Result<FlushReport, StoreError> {
        if self.buffered == 0 {
            return self.flush(now);
        }
        match LakeLock::acquire_timeout(&self.root, LockMode::Shared, self.opts.file_mode, timeout)? {
            Some(lock) => self.write_all(now, lock),
            None => Ok(FlushReport { deferred_rows: self.buffered, ..Default::default() }),
        }
    }

    fn write_all(&mut self, now: TimestampMs, _lock: LakeLock) -> Result<FlushReport, StoreError> {
        let mut report = FlushReport::default();
        let mut pending = std::mem::take(&mut self.buffers).into_iter();
        while let Some(((key, table), mut rows)) = pending.next() {
            let def = crate::schema::table(table).expect("buffered table exists");
            rows.sort_by_key(|(ts, _)| *ts);
            match self.write_file(def, &key, &rows) {
                Ok(path) => {
                    self.buffered -= rows.len();
                    report.rows += rows.len();
                    report.files.push(path);
                }
                Err(e) => {
                    // Keep this and every unwritten partition for the next attempt.
                    self.buffers.insert((key, table), rows);
                    self.buffers.extend(pending);
                    return Err(e);
                }
            }
        }
        self.last_flush = Some(now);
        Ok(report)
    }

    fn write_file(
        &self,
        def: &TableDef,
        key: &PartitionKey,
        rows: &[(TimestampMs, Vec<Cell>)],
    ) -> Result<PathBuf, StoreError> {
        let cells: Vec<Vec<Cell>> = rows.iter().map(|(_, c)| c.clone()).collect();
        let batch = rows::to_batch(def, &cells)?;
        let dir = layout::partition_dir(&self.root, def.name, key);
        layout::create_dir_all(&dir, self.opts.dir_mode)?;
        let (path, _) = format::write_vortex_file(&self.codec, &dir, def, &[batch], self.opts.file_mode)?;
        Ok(path)
    }
}

/// Current wall-clock time in ms since the epoch.
pub fn wall_clock_ms() -> TimestampMs {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)).unwrap_or(0)
}

impl Drop for Store {
    /// Best-effort final flush, waiting a few seconds for running maintenance.
    fn drop(&mut self) {
        if self.buffered == 0 {
            return;
        }
        match self.flush_wait(wall_clock_ms(), DROP_FLUSH_TIMEOUT) {
            Ok(r) if r.deferred_rows > 0 => {
                tracing::warn!(rows = r.deferred_rows, "telemetry lake busy; dropping unflushed rows")
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, rows = self.buffered, "final telemetry flush failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use lindesk_plugin_api::{Metric, Record};

    use super::*;
    use crate::format::{FORMAT_MARKER, VortexCodec};
    use crate::schema::table;

    #[test]
    fn flush_writes_readable_vortex_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path(), StoreOptions::user()).unwrap();
        assert!(dir.path().join(FORMAT_MARKER).exists());
        let metric = |ts, v| {
            Record::Metric(Metric {
                ts,
                name: "m".into(),
                value: v,
                labels: [("host".to_string(), "box".to_string())].into(),
                plugin: "p".into(),
            })
        };
        store.append(&[metric(1_000, 1.0), metric(2_000, 2.0)]);
        let r = store.flush(5_000).unwrap();
        assert_eq!((r.rows, r.files.len(), r.deferred_rows), (2, 1, 0));
        assert!(r.files[0].extension().is_some_and(|e| e == "vortex"));
        let codec = VortexCodec::new();
        assert_eq!(codec.row_count(&r.files[0]).unwrap(), 2);
        let def = table("metrics").unwrap();
        let batches = codec.read(&r.files[0], def.arrow_schema()).unwrap();
        let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(rows, 2);
        let labels = batches[0].column_by_name("labels").unwrap();
        let labels = labels.as_any().downcast_ref::<arrow_array::StringArray>().unwrap();
        assert_eq!(labels.value(0), r#"{"host":"box"}"#);
    }
}
