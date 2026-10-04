//! A non-blocking front end for [`Store`]: callers enqueue, a background thread writes.
//!
//! The daemons call into telemetry from their control loop, which must never wait for the
//! disk (the disk is often exactly what is under pressure). [`AsyncStore::append`] only
//! takes a short, uncontended lock and pushes onto a bounded in-memory queue; when the
//! queue is full the *oldest* rows are dropped and counted. A dedicated writer thread,
//! whose priority the owner can lower via [`AsyncOptions::thread_init`], opens the store
//! lazily (directory creation included), encodes, fsyncs and renames.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lindesk_plugin_api::{Metric, Record};

use crate::rows::{BenchRun, BenchSample};
use crate::writer::{Store, StoreOptions, wall_clock_ms};

/// Name of the metric (table `metrics`, plugin [`AsyncOptions::metric_plugin`]) recording
/// the total number of telemetry rows dropped since the writer started.
pub const DROPPED_METRIC: &str = "telemetry_dropped_rows";

/// A hook run once on the writer thread before any I/O, e.g. to lower its priority.
pub type ThreadInit = Box<dyn FnOnce() + Send + 'static>;

/// Options of the asynchronous writer.
pub struct AsyncOptions {
    /// Maximum rows waiting in the queue; beyond it the oldest rows are dropped.
    pub queue_rows: usize,
    /// How long [`AsyncStore::flush`] (and `Drop`) wait for the writer by default.
    pub flush_timeout: Duration,
    /// Plugin name used for the [`DROPPED_METRIC`] rows.
    pub metric_plugin: String,
    /// Runs first on the writer thread (priority lowering).
    pub thread_init: Option<ThreadInit>,
    /// Artificial delay before every write batch. Only for tests that simulate a slow disk.
    #[doc(hidden)]
    pub write_delay: Duration,
}

impl Default for AsyncOptions {
    fn default() -> Self {
        AsyncOptions {
            queue_rows: 100_000,
            flush_timeout: Duration::from_secs(5),
            metric_plugin: "parquet-store".into(),
            thread_init: None,
            write_delay: Duration::ZERO,
        }
    }
}

impl std::fmt::Debug for AsyncOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncOptions")
            .field("queue_rows", &self.queue_rows)
            .field("flush_timeout", &self.flush_timeout)
            .field("metric_plugin", &self.metric_plugin)
            .field("thread_init", &self.thread_init.is_some())
            .finish()
    }
}

enum Item {
    Record(Record),
    BenchRun(BenchRun),
    BenchSample(BenchSample),
}

#[derive(Default)]
struct State {
    queue: VecDeque<Item>,
    /// Rows dropped from the queue because it was full.
    queue_dropped: u64,
    /// Rows the store itself dropped (its buffer cap while maintenance held the lock).
    store_dropped: u64,
    flush_requested: u64,
    flush_completed: u64,
    shutdown: bool,
    exited: bool,
    last_error: Option<String>,
}

struct Shared {
    state: Mutex<State>,
    /// Wakes the writer.
    wake: Condvar,
    /// Signals flush completion and thread exit.
    done: Condvar,
    capacity: usize,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Outcome of [`AsyncStore::flush`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushOutcome {
    /// Everything enqueued before the call was handed to the store and flushed (or
    /// deferred by the store because maintenance holds the lake lock).
    Flushed,
    /// The writer did not finish within the timeout; it keeps working in the background.
    TimedOut,
}

/// The asynchronous writer. Cheap to call from latency-sensitive threads.
pub struct AsyncStore {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    flush_timeout: Duration,
}

impl std::fmt::Debug for AsyncStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncStore").field("queued_rows", &self.queued_rows()).finish()
    }
}

impl AsyncStore {
    /// Starts the writer thread. No filesystem work happens on the caller's thread: the
    /// store (and its directories) are created by the writer on first use.
    pub fn spawn(root: impl Into<PathBuf>, opts: StoreOptions, mut aopts: AsyncOptions) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            done: Condvar::new(),
            capacity: aopts.queue_rows.max(1),
        });
        let worker = Worker {
            shared: shared.clone(),
            root: root.into(),
            opts,
            store: None,
            write_delay: aopts.write_delay,
            flush_timeout: aopts.flush_timeout,
            metric_plugin: std::mem::take(&mut aopts.metric_plugin),
            reported_dropped: 0,
        };
        let init = aopts.thread_init.take();
        let thread = std::thread::Builder::new().name("lindesk-telemetry".into()).spawn(move || {
            if let Some(init) = init {
                init();
            }
            worker.run();
        })?;
        Ok(AsyncStore { shared, thread: Some(thread), flush_timeout: aopts.flush_timeout })
    }

    /// Enqueues records. Never blocks on I/O; drops the oldest queued rows when full.
    pub fn append(&self, records: &[Record]) {
        self.push(records.iter().cloned().map(Item::Record));
    }

    /// Enqueues a benchmark run and its samples.
    pub fn append_bench(&self, run: &BenchRun, samples: &[BenchSample]) {
        self.push(std::iter::once(Item::BenchRun(run.clone())).chain(samples.iter().cloned().map(Item::BenchSample)));
    }

    fn push(&self, items: impl Iterator<Item = Item>) {
        let mut st = self.shared.lock();
        st.queue.extend(items);
        let excess = st.queue.len().saturating_sub(self.shared.capacity);
        if excess > 0 {
            st.queue.drain(..excess);
            st.queue_dropped += excess as u64;
        }
        drop(st);
        self.shared.wake.notify_one();
    }

    /// Asks the writer to write everything enqueued so far and waits up to the configured
    /// flush timeout.
    pub fn flush(&self) -> FlushOutcome {
        self.flush_timeout(self.flush_timeout)
    }

    /// Like [`AsyncStore::flush`] with an explicit timeout.
    pub fn flush_timeout(&self, timeout: Duration) -> FlushOutcome {
        let deadline = Instant::now() + timeout;
        let mut st = self.shared.lock();
        st.flush_requested += 1;
        let ticket = st.flush_requested;
        self.shared.wake.notify_one();
        while st.flush_completed < ticket && !st.exited {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return FlushOutcome::TimedOut;
            }
            st = self.shared.done.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        if st.flush_completed >= ticket { FlushOutcome::Flushed } else { FlushOutcome::TimedOut }
    }

    /// Rows waiting in the queue.
    pub fn queued_rows(&self) -> usize {
        self.shared.lock().queue.len()
    }

    /// Rows dropped so far (queue overflow plus store buffer overflow).
    pub fn dropped_rows(&self) -> u64 {
        let st = self.shared.lock();
        st.queue_dropped + st.store_dropped
    }

    /// The most recent write error, if the last attempt failed.
    pub fn last_error(&self) -> Option<String> {
        self.shared.lock().last_error.clone()
    }
}

impl Drop for AsyncStore {
    /// Asks the writer to flush and exit, waiting at most the flush timeout. If the disk is
    /// stuck the thread is left to finish on its own.
    fn drop(&mut self) {
        let deadline = Instant::now() + self.flush_timeout;
        let mut st = self.shared.lock();
        st.shutdown = true;
        self.shared.wake.notify_one();
        while !st.exited {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                tracing::warn!(rows = st.queue.len(), "telemetry writer did not finish in time; detaching");
                return;
            }
            st = self.shared.done.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        drop(st);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Worker {
    shared: Arc<Shared>,
    root: PathBuf,
    opts: StoreOptions,
    store: Option<Store>,
    write_delay: Duration,
    flush_timeout: Duration,
    metric_plugin: String,
    reported_dropped: u64,
}

impl Worker {
    fn run(mut self) {
        // Wake at least this often so time-based flushes happen without new rows.
        let tick = self.opts.flush_interval.clamp(Duration::from_millis(100), Duration::from_secs(1));
        loop {
            let (items, ticket, shutdown) = {
                let mut st = self.shared.lock();
                if st.queue.is_empty() && st.flush_requested == st.flush_completed && !st.shutdown {
                    st = self.shared.wake.wait_timeout(st, tick).unwrap_or_else(|e| e.into_inner()).0;
                }
                (std::mem::take(&mut st.queue), st.flush_requested, st.shutdown)
            };
            let flush_now = shutdown || ticket > self.shared.lock().flush_completed;
            let result = self.write(items, flush_now);
            let mut st = self.shared.lock();
            st.last_error = result.err();
            if let Some(s) = &self.store {
                st.store_dropped = s.dropped_rows();
            }
            if flush_now {
                st.flush_completed = st.flush_completed.max(ticket);
                self.shared.done.notify_all();
            }
            if shutdown {
                st.exited = true;
                self.shared.done.notify_all();
                return;
            }
        }
    }

    fn write(&mut self, items: VecDeque<Item>, flush_now: bool) -> Result<(), String> {
        if !items.is_empty() && !self.write_delay.is_zero() {
            std::thread::sleep(self.write_delay);
        }
        if self.store.is_none() && items.is_empty() && !flush_now {
            // Nothing to write yet: do not create the lake just because time passed.
            return Ok(());
        }
        if self.store.is_none() {
            match Store::open(&self.root, self.opts.clone()) {
                Ok(s) => self.store = Some(s),
                Err(e) => {
                    // Without a store the rows cannot be kept anywhere bounded; count them.
                    self.shared.lock().queue_dropped += items.len() as u64;
                    tracing::warn!(root = %self.root.display(), error = %e, "cannot open telemetry store");
                    return Err(e.to_string());
                }
            }
        }
        let dropped = {
            let st = self.shared.lock();
            st.queue_dropped + st.store_dropped
        };
        let Some(store) = self.store.as_mut() else { return Ok(()) };
        let mut records = Vec::new();
        for item in items {
            match item {
                Item::Record(r) => records.push(r),
                Item::BenchRun(r) => store.append_bench(&r, &[]),
                Item::BenchSample(s) => store.append_bench_samples(std::slice::from_ref(&s)),
            }
        }
        if dropped > self.reported_dropped {
            records.push(Record::Metric(Metric {
                ts: wall_clock_ms(),
                name: DROPPED_METRIC.into(),
                value: dropped as f64,
                labels: Default::default(),
                plugin: self.metric_plugin.clone(),
            }));
            self.reported_dropped = dropped;
        }
        store.append(&records);
        let now = wall_clock_ms();
        let r = if flush_now { store.flush_wait(now, self.flush_timeout).map(Some) } else { store.maybe_flush(now) };
        r.map(|_| ()).map_err(|e| {
            tracing::warn!(error = %e, "telemetry flush failed; rows stay buffered");
            e.to_string()
        })
    }
}

#[cfg(test)]
mod tests {
    use lindesk_plugin_api::{Resource, Sample, Scope};

    use super::*;

    fn sample(ts: i64) -> Record {
        Record::Pressure(Sample {
            ts,
            scope: Scope::System,
            resource: Resource::Io,
            some_avg10: 1.0,
            full_avg10: None,
            some_delta_us: 1,
            full_delta_us: None,
            interval_ms: 1000,
        })
    }

    fn files(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| {
                let p = e.path();
                if p.is_dir() { files(&p) } else { usize::from(p.extension().is_some_and(|x| x == "vortex")) }
            })
            .sum()
    }

    #[test]
    fn writes_in_the_background_and_flushes_on_request() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lake");
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ran.clone();
        let store = AsyncStore::spawn(
            &root,
            StoreOptions { flush_interval: Duration::from_secs(3600), ..StoreOptions::user() },
            AsyncOptions {
                thread_init: Some(Box::new(move || flag.store(true, std::sync::atomic::Ordering::SeqCst))),
                // fsync can take seconds on a saturated disk; this test is about ordering.
                flush_timeout: Duration::from_secs(120),
                ..Default::default()
            },
        )
        .unwrap();
        store.append(&[sample(1), sample(2)]);
        assert_eq!(store.flush(), FlushOutcome::Flushed);
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(files(&root.join("pressure")), 1);
        assert_eq!(store.dropped_rows(), 0);
        store.append(&[sample(3)]);
        drop(store);
        assert_eq!(files(&root.join("pressure")), 2, "drop flushes");
    }

    #[test]
    fn a_slow_disk_never_blocks_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lake");
        let store = AsyncStore::spawn(
            &root,
            StoreOptions::user(),
            AsyncOptions {
                queue_rows: 1_000,
                flush_timeout: Duration::from_millis(200),
                write_delay: Duration::from_secs(2),
                ..Default::default()
            },
        )
        .unwrap();
        let batch: Vec<Record> = (0..100).map(sample).collect();
        let mut took: Vec<Duration> = (0..200)
            .map(|_| {
                let t = Instant::now();
                store.append(&batch);
                t.elapsed()
            })
            .collect();
        took.sort();
        // 20k rows into a 1k queue while the writer sleeps: the oldest are dropped.
        assert!(store.queued_rows() <= 1_000);
        assert!(store.dropped_rows() >= 18_000, "{}", store.dropped_rows());
        // p95 is the real bound; the max only tolerates preemption on a loaded machine.
        assert!(took[189] < Duration::from_millis(2), "p95 append {:?}", took[189]);
        assert!(took[199] < Duration::from_millis(100), "max append {:?}", took[199]);
        // flush and drop are bounded by the timeout even though the writer is stuck.
        let t = Instant::now();
        assert_eq!(store.flush(), FlushOutcome::TimedOut);
        assert!(t.elapsed() < Duration::from_secs(1));
        let t = Instant::now();
        drop(store);
        assert!(t.elapsed() < Duration::from_secs(1));
        // Let the detached writer finish before the tempdir goes away.
        std::thread::sleep(Duration::from_millis(2_500));
    }

    #[test]
    fn drops_are_recorded_as_a_metric() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lake");
        let store = AsyncStore::spawn(
            &root,
            StoreOptions::user(),
            AsyncOptions { queue_rows: 10, write_delay: Duration::from_millis(300), ..Default::default() },
        )
        .unwrap();
        store.append(&[sample(1)]);
        // While the writer sleeps on the first batch, overflow the queue.
        std::thread::sleep(Duration::from_millis(50));
        store.append(&(0..25).map(sample).collect::<Vec<_>>());
        assert_eq!(store.dropped_rows(), 15);
        assert_eq!(store.flush_timeout(Duration::from_secs(120)), FlushOutcome::Flushed);
        assert_eq!(files(&root.join("metrics")), 1);
    }
}
