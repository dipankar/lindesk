//! `parquet-store`: writes every record into the hive-partitioned Parquet telemetry lake.
//!
//! Config (the plugin's `[config]` table, all optional):
//!
//! ```toml
//! [plugins.parquet-store.config]
//! dir = "/var/lib/lindesk/telemetry"   # default: system or user lake root by privilege
//! flush = "60s"                        # flush interval
//! mode = "system"                      # "user" (0700/0600), "system" (0750/0640) or octal
//! queue_rows = 100000                  # bounded queue; the oldest rows are dropped beyond it
//! flush_timeout = "5s"                 # how long flush()/shutdown wait for the writer
//! ```
//!
//! The engine thread must never wait for the disk, so [`ParquetSink::record`] only enqueues
//! into a bounded queue. A dedicated writer thread, running at idle I/O priority and nice
//! 10, creates directories, encodes, fsyncs and renames. When the queue is full the oldest
//! telemetry is dropped and counted (`dropped_rows()`, and the `telemetry_dropped_rows`
//! metric in the lake). `flush()` and shutdown wait for the writer with a bounded timeout.

use lindesk_plugin_api::{
    InitContext, IoPrioClass, Plugin, PluginError, PluginInfo, PluginKind, PluginResult, Privilege, Record, Sink,
};
use lindesk_store::{AsyncOptions, AsyncStore, FlushOutcome, SinkConfig, StoreOptions};

/// Niceness of the telemetry writer thread.
const WRITER_NICE: i8 = 10;

/// Lowers the calling thread's CPU and I/O priority so telemetry never competes with the
/// foreground: idle I/O class and nice 10. Failures are logged and ignored.
pub fn lower_writer_priority() {
    let tid = rustix::thread::gettid().as_raw_nonzero().get();
    if let Err(e) = lindesk_sys::syscalls::ioprio_set(tid, IoPrioClass::Idle, 7) {
        tracing::debug!(error = %e, "could not set idle I/O priority on the telemetry writer");
    }
    if let Err(e) = lindesk_sys::syscalls::set_nice(tid, WRITER_NICE) {
        tracing::debug!(error = %e, "could not renice the telemetry writer");
    }
}

/// The built-in Parquet lake sink.
#[derive(Debug, Default)]
pub struct ParquetSink {
    store: Option<AsyncStore>,
}

impl ParquetSink {
    /// Plugin name.
    pub const NAME: &'static str = "parquet-store";

    /// An uninitialised sink; [`Plugin::init`] starts the writer from the config.
    pub fn new() -> Self {
        Self::default()
    }

    /// A sink writing through an already started writer (for embedding and tests).
    pub fn with_writer(store: AsyncStore) -> Self {
        ParquetSink { store: Some(store) }
    }

    /// Starts a low-priority writer for `root` without any filesystem work on this thread.
    pub fn spawn_writer(
        root: &std::path::Path,
        opts: StoreOptions,
        mut aopts: AsyncOptions,
    ) -> PluginResult<AsyncStore> {
        if aopts.thread_init.is_none() {
            aopts.thread_init = Some(Box::new(lower_writer_priority));
        }
        aopts.metric_plugin = Self::NAME.into();
        AsyncStore::spawn(root, opts, aopts).map_err(PluginError::from)
    }

    /// The writer, once initialised.
    pub fn writer(&self) -> Option<&AsyncStore> {
        self.store.as_ref()
    }

    /// Telemetry rows dropped so far because the writer could not keep up.
    pub fn dropped_rows(&self) -> u64 {
        self.store.as_ref().map_or(0, AsyncStore::dropped_rows)
    }

    fn writer_or_err(&self) -> PluginResult<&AsyncStore> {
        self.store.as_ref().ok_or_else(|| PluginError::Failed("parquet-store used before init".into()))
    }
}

impl Plugin for ParquetSink {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: Self::NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Sink],
            description: "Telemetry lake: hive-partitioned Parquet, queryable with DuckDB".into(),
        }
    }

    /// Parses the config and starts the writer thread. No directories are created here;
    /// the writer does that on its own thread.
    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
        if self.store.is_some() {
            return Ok(());
        }
        let config = SinkConfig::from_json(&ctx.config).map_err(PluginError::failed)?;
        let (dir, opts) = config.resolve(ctx.privilege == Privilege::System).map_err(PluginError::failed)?;
        let aopts = config.async_options().map_err(PluginError::failed)?;
        tracing::info!(dir = %dir.display(), "parquet-store writing telemetry");
        self.store = Some(Self::spawn_writer(&dir, opts, aopts)?);
        Ok(())
    }

    /// Flushes and stops the writer, waiting at most the configured flush timeout.
    fn shutdown(&mut self) {
        if let Some(store) = self.store.take() {
            if store.dropped_rows() > 0 {
                tracing::warn!(rows = store.dropped_rows(), "telemetry rows were dropped this session");
            }
            drop(store);
        }
    }
}

impl Sink for ParquetSink {
    /// Enqueues the records; never blocks on I/O.
    fn record(&mut self, records: &[Record]) -> PluginResult<()> {
        self.writer_or_err()?.append(records);
        Ok(())
    }

    /// Asks the writer to flush and waits up to the configured timeout. A timeout is
    /// logged, not reported as a plugin failure: the writer keeps going in the background.
    fn flush(&mut self) -> PluginResult<()> {
        let store = self.writer_or_err()?;
        if store.flush() == FlushOutcome::TimedOut {
            tracing::warn!(queued = store.queued_rows(), "telemetry flush timed out; writer continues in background");
        }
        if let Some(e) = store.last_error() {
            return Err(PluginError::Io(e));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use lindesk_plugin_api::{API_VERSION, HostCaps, Metric};
    use lindesk_store::wall_clock_ms;

    use super::*;

    fn metric() -> Record {
        Record::Metric(Metric {
            ts: wall_clock_ms(),
            name: "frames".into(),
            value: 1.0,
            labels: Default::default(),
            plugin: "test".into(),
        })
    }

    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() { out.extend(walk(&p)) } else { out.push(p) }
        }
        out
    }

    #[test]
    fn writes_records_to_the_lake() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("telemetry");
        let mut sink = ParquetSink::new();
        assert!(sink.record(&[]).is_err(), "record before init must fail");
        sink.init(&InitContext {
            api_version: API_VERSION,
            host: "test".into(),
            privilege: Privilege::User,
            uid: Some(1000),
            caps: HostCaps::default(),
            config: serde_json::json!({ "dir": root, "flush": "1h", "mode": "user", "flush_timeout": "30s" }),
        })
        .unwrap();
        assert!(!root.exists(), "init does no filesystem work on the caller's thread");
        sink.record(&[metric(), metric()]).unwrap();
        sink.flush().unwrap();
        let files = walk(&root.join("metrics"));
        assert_eq!(files.len(), 1, "{files:?}");
        assert!(files[0].extension().is_some_and(|e| e == "vortex"));
        sink.record(&[metric()]).unwrap();
        sink.shutdown();
        assert_eq!(walk(&root.join("metrics")).len(), 2, "shutdown flushes");
        assert_eq!(sink.dropped_rows(), 0);
    }

    #[test]
    fn a_slow_writer_never_blocks_record() {
        let dir = tempfile::tempdir().unwrap();
        let writer = ParquetSink::spawn_writer(
            &dir.path().join("telemetry"),
            StoreOptions::user(),
            AsyncOptions {
                queue_rows: 500,
                flush_timeout: Duration::from_millis(100),
                write_delay: Duration::from_secs(2),
                ..Default::default()
            },
        )
        .unwrap();
        let mut sink = ParquetSink::with_writer(writer);
        let batch: Vec<Record> = (0..50).map(|_| metric()).collect();
        let mut took: Vec<Duration> = (0..100)
            .map(|_| {
                let t = Instant::now();
                sink.record(&batch).unwrap();
                t.elapsed()
            })
            .collect();
        took.sort();
        // The writer sleeps 2 s per batch; record() must not notice. The p95 bound is the
        // real check, the max only tolerates scheduler preemption on a loaded machine.
        assert!(took[94] < Duration::from_millis(2), "p95 record() {:?}", took[94]);
        assert!(took[99] < Duration::from_millis(100), "max record() {:?}", took[99]);
        assert!(sink.dropped_rows() >= 4_000, "oldest rows are dropped: {}", sink.dropped_rows());
        let t = Instant::now();
        sink.flush().unwrap();
        sink.shutdown();
        assert!(t.elapsed() < Duration::from_secs(1), "flush and shutdown are bounded");
        // Let the detached writer finish before the tempdir goes away.
        std::thread::sleep(Duration::from_millis(2_500));
    }
}
