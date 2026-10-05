//! # lindesk-store: the telemetry writer
//!
//! lindesk keeps its telemetry in a hive-partitioned [Vortex](https://vortex.dev) lake that
//! DuckDB queries through its `vortex` extension. This crate is the *write* side, linked by
//! the `telemetry` sink plugin (`lindesk-telemetry-sink`) and `lindesk-data`, never by the
//! daemons. It depends on arrow-rs and the `vortex` crate only, never on DuckDB. Reading,
//! compaction, rollups, retention and deletion live
//! in `lindesk-query`.
//!
//! * [`schema`]: every table's columns ([`TABLES`], [`schema()`]).
//! * [`layout`]: partition paths, listing, locking and fsync helpers shared with the reader.
//! * [`Store`]: buffers [`Record`](lindesk_plugin_api::Record)s and benchmark rows and
//!   flushes them as immutable Vortex files (synchronous).
//! * [`format`]: the Vortex codec and file writer, shared with `lindesk-query`.
//! * [`AsyncStore`]: what daemons should use. `append` only enqueues into a bounded queue
//!   (dropping the oldest rows when full, counted in the `telemetry_dropped_rows` metric);
//!   a low-priority background thread owns the [`Store`] and does all I/O.
//!
//! ```no_run
//! # use lindesk_store::{Store, StoreOptions, wall_clock_ms};
//! let mut store = Store::open("/tmp/lake", StoreOptions::user())?;
//! store.append(&[/* records */]);
//! store.maybe_flush(wall_clock_ms())?;
//! # Ok::<(), lindesk_store::StoreError>(())
//! ```

mod async_writer;
pub mod config;
pub mod format;
pub mod layout;
mod rows;
pub mod schema;
mod writer;

pub use async_writer::{AsyncOptions, AsyncStore, DROPPED_METRIC, FlushOutcome, ThreadInit};
pub use config::SinkConfig;
pub use rows::{BenchPhase, BenchRun, BenchSample, flatten_target};
pub use schema::{SCHEMA_VERSION, TABLES, TableDef, TableKind, schema, table};
pub use writer::{FlushReport, Store, StoreOptions, wall_clock_ms};

/// The arrow-rs crates (version 59, the one `vortex` uses) that [`format`] speaks, re-exported
/// so readers use exactly the same version.
pub mod arrow {
    pub use arrow_array;
    pub use arrow_cast;
    pub use arrow_schema;
}

/// Errors from the writer.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Filesystem error.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// Arrow array construction failed.
    #[error("arrow error: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    /// Vortex encoding or decoding failed.
    #[error("vortex error: {0}")]
    Vortex(String),
    /// Invalid configuration.
    #[error("invalid configuration: {0}")]
    Config(String),
}

impl From<rustix::io::Errno> for StoreError {
    fn from(e: rustix::io::Errno) -> Self {
        StoreError::Io(e.into())
    }
}
