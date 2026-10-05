//! `lindesk-telemetry-sink`: the `telemetry` plugin. It writes every record the daemons
//! send into the hive-partitioned Vortex telemetry lake.
//!
//! An ordinary external sink plugin (stdio JSON-RPC, built on `lindesk-plugin`), shipped by
//! the optional `lindesk-telemetry` package with two manifests that run this binary:
//! `/usr/lib/lindesk/plugins/telemetry/` (`privilege = "system"`, loaded by `lindeskd`) and
//! `/usr/lib/lindesk/plugins/telemetry-user/` (`privilege = "user"`, loaded by
//! `lindesk-session`). Without the package the daemons simply run without telemetry.
//!
//! Config (`[plugins.telemetry.config]`; `[plugins.parquet-store.config]`, the built-in
//! sink's former name, is still read; the daemons fill in `dir`, `flush` and `mode` from
//! `[telemetry]` and `--telemetry-dir`):
//!
//! ```toml
//! dir = "/var/lib/lindesk/telemetry"   # default: system or user lake root by privilege
//! flush = "60s"                        # flush interval
//! mode = "system"                      # "user" (0700/0600), "system" (0750/0640) or octal
//! queue_rows = 100000                  # bounded queue; the oldest rows are dropped beyond it
//! flush_timeout = "5s"                 # how long sink.flush and shutdown wait for the writer
//! ```
//!
//! Never blocking, end to end: the host sends `sink.record` as a notification through a
//! bounded queue (a slow plugin loses batches, never stalls the engine); here the main
//! thread only parses each batch and enqueues it into [`AsyncStore`], whose own thread,
//! at idle I/O priority and nice 10, creates directories, encodes, fsyncs and renames.
//! When the queue is full the oldest rows are dropped and counted (the
//! `telemetry_dropped_rows` metric). The whole process runs at nice 10.

use lindesk_plugin::prelude::*;
use lindesk_store::{AsyncOptions, AsyncStore, FlushOutcome, SinkConfig, StoreOptions};

/// Plugin name (and the `[plugins.<name>]` config key).
pub const NAME: &str = "telemetry";

/// How long `shutdown` waits for the writer (the host's grace period is 500 ms).
const SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_millis(350);

/// Niceness of the plugin process and its writer thread.
const NICE: i8 = 10;

/// Lowers the calling thread's CPU and I/O priority so telemetry never competes with the
/// foreground: idle I/O class and nice 10. Failures are logged and ignored.
pub fn lower_priority() {
    let tid = rustix::thread::gettid().as_raw_nonzero().get();
    if let Err(e) = lindesk_sys::syscalls::ioprio_set(tid, IoPrioClass::Idle, 7) {
        tracing::debug!(error = %e, "could not set idle I/O priority");
    }
    if let Err(e) = lindesk_sys::syscalls::set_nice(tid, NICE) {
        tracing::debug!(error = %e, "could not renice");
    }
}

/// The telemetry sink.
#[derive(Debug, Default)]
pub struct TelemetrySink {
    store: Option<AsyncStore>,
}

impl TelemetrySink {
    /// A sink writing through an already started writer (tests).
    pub fn with_writer(store: AsyncStore) -> Self {
        TelemetrySink { store: Some(store) }
    }

    /// Starts a low-priority writer for `root` without any filesystem work on this thread.
    pub fn spawn_writer(
        root: &std::path::Path,
        opts: StoreOptions,
        mut aopts: AsyncOptions,
    ) -> PluginResult<AsyncStore> {
        if aopts.thread_init.is_none() {
            aopts.thread_init = Some(Box::new(lower_priority));
        }
        aopts.metric_plugin = NAME.into();
        AsyncStore::spawn(root, opts, aopts).map_err(PluginError::from)
    }

    /// Telemetry rows dropped so far because the writer could not keep up.
    pub fn dropped_rows(&self) -> u64 {
        self.store.as_ref().map_or(0, AsyncStore::dropped_rows)
    }

    fn writer(&self) -> PluginResult<&AsyncStore> {
        self.store.as_ref().ok_or_else(|| PluginError::Failed("telemetry used before initialize".into()))
    }
}

impl Plugin for TelemetrySink {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Sink],
            description: "Telemetry lake: hive-partitioned Vortex files, queried with lindesk-data (DuckDB)".into(),
        }
    }

    /// Parses the config and starts the writer thread. No directories are created here; the
    /// writer does that on its own thread, so `initialize` answers at once.
    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
        if self.store.is_some() {
            return Ok(());
        }
        let config = SinkConfig::from_json(&ctx.config).map_err(PluginError::failed)?;
        let (dir, opts) = config.resolve(ctx.privilege == Privilege::System).map_err(PluginError::failed)?;
        let aopts = config.async_options().map_err(PluginError::failed)?;
        tracing::info!("writing telemetry to {}", dir.display());
        self.store = Some(Self::spawn_writer(&dir, opts, aopts)?);
        Ok(())
    }

    /// Stops the writer. The host sends `sink.flush` (which waits up to `flush_timeout`)
    /// right before `shutdown` and kills a plugin that isn't gone 500 ms later, so this
    /// waits at most [`SHUTDOWN_WAIT`] for the rest.
    fn shutdown(&mut self) {
        if let Some(mut store) = self.store.take() {
            store.set_drop_timeout(SHUTDOWN_WAIT);
            if store.dropped_rows() > 0 {
                tracing::warn!("{} telemetry rows were dropped this session", store.dropped_rows());
            }
            drop(store);
        }
    }
}

impl Sink for TelemetrySink {
    /// Enqueues the records; never blocks on I/O.
    fn record(&mut self, records: &[Record]) -> PluginResult<()> {
        self.writer()?.append(records);
        Ok(())
    }

    /// Asks the writer to flush and waits up to the configured timeout. A timeout is
    /// logged, not reported as a failure: the writer keeps going in the background.
    fn flush(&mut self) -> PluginResult<()> {
        let store = self.writer()?;
        if store.flush() == FlushOutcome::TimedOut {
            tracing::warn!("telemetry flush timed out with {} rows queued; still writing", store.queued_rows());
        }
        if let Some(e) = store.last_error() {
            return Err(PluginError::Io(e));
        }
        Ok(())
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("lindesk-telemetry-sink {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    // stderr is forwarded to the host's log with this plugin's name; the host adds times.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("LINDESK_LOG").unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .init();
    // The main thread only parses and enqueues, but it should not compete either.
    lower_priority();
    lindesk_plugin::run(Server::new(TelemetrySink::default()).sink())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

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
        let mut sink = TelemetrySink::default();
        assert!(sink.record(&[]).is_err(), "record before init must fail");
        sink.init(&InitContext {
            api_version: API_VERSION,
            host: "test".into(),
            privilege: Privilege::User,
            uid: Some(1000),
            caps: HostCaps::default(),
            config: json!({ "dir": root, "flush": "1h", "mode": "user", "flush_timeout": "30s" }),
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
        let writer = TelemetrySink::spawn_writer(
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
        let mut sink = TelemetrySink::with_writer(writer);
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
