//! # lindesk-query: reading and maintaining the telemetry lake with DuckDB
//!
//! The daemons write an immutable, hive-partitioned [Vortex](https://vortex.dev) lake with
//! `lindesk-store`. This crate is everything else. DuckDB reads the files through its
//! `vortex` core extension ([`Backend::Extension`]), which lindesk loads from (and if needed
//! installs once into) `<root>/../duckdb-ext`. When the extension is unavailable (offline,
//! unsupported platform) the Rust `vortex` crate reads the files instead
//! ([`Backend::Native`]); every operation except [`Lake::catalog`] works either way.
//! Lakes from earlier versions hold `.parquet` files: views read both formats, and
//! [`Lake::compact`] converts sealed Parquet partitions to Vortex.
//!
//!
//! * [`Lake::query`]: read-only SQL over views of every table, in a sandboxed in-memory
//!   DuckDB (only the lake directory is reachable, only SELECT-like statements run).
//! * [`Lake::catalog`]: writes `catalog.duckdb`, a views-only database any DuckDB client
//!   can open read-only.
//! * [`Lake::compact`]: merges each sealed hour's files and refreshes the 1m/1h rollups.
//! * [`Lake::prune`]: TTL per tier and a total size cap.
//! * [`Lake::delete`]: rewrites only the partitions containing matching rows, raw and rollup.
//! * [`Lake::purge`] and [`Lake::stats`].
//!
//! All maintenance holds the lake's exclusive lock and swaps rewritten partitions in with
//! `renameat2(RENAME_EXCHANGE)`, so concurrent readers never see duplicated rows.

mod catalog;
mod engine;
mod maint;
mod query;
mod rollup;
mod stats;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub use engine::{Backend, BackendChoice};
use lindesk_core::config::Retention;
use lindesk_plugin_api::TimestampMs;
pub use lindesk_store::{TABLES, TableDef, layout::catalog_path, layout::default_root};
pub use maint::{
    CompactReport, DeleteReport, DeleteRequest, MaintainReport, PruneReport, PurgeReport, TableDelete, TablePrune,
};
pub use query::Format;
pub use stats::{LakeStats, TableStats};

/// What a DuckDB client must run before using the catalog's views.
pub const CATALOG_HINT: &str = "INSTALL vortex; LOAD vortex;";

/// How many times [`Lake::query`] retries after a file vanished under it.
const QUERY_RETRIES: usize = 2;

/// Errors from the query layer.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// DuckDB reported an error.
    #[error("{0}")]
    DuckDb(#[from] duckdb::Error),
    /// Filesystem error.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// Reading legacy Parquet metadata failed.
    #[error("parquet error: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
    /// The statement is not allowed on the read-only query path.
    #[error("refused: {0}")]
    Refused(String),
    /// Invalid request.
    #[error("{0}")]
    Invalid(String),
}

impl From<rustix::io::Errno> for QueryError {
    fn from(e: rustix::io::Errno) -> Self {
        QueryError::Io(e.into())
    }
}

/// A telemetry lake on disk.
#[derive(Clone, Debug)]
pub struct Lake {
    root: PathBuf,
    catalog: PathBuf,
    exchange: bool,
    choice: BackendChoice,
    ext_dir: PathBuf,
    resolved: Arc<OnceLock<Result<Backend, String>>>,
}

impl Lake {
    /// Opens the lake at `root`. The directory need not exist yet: queries then see empty
    /// tables. The catalog defaults to `<root>/../catalog.duckdb`.
    pub fn open(root: impl AsRef<Path>) -> Result<Lake, QueryError> {
        let root = std::path::absolute(root.as_ref())?;
        let root = root.canonicalize().unwrap_or(root);
        let catalog = catalog_path(&root);
        let ext_dir = engine::default_extension_dir(&root);
        Ok(Lake {
            root,
            catalog,
            exchange: true,
            choice: BackendChoice::Auto,
            ext_dir,
            resolved: Arc::new(OnceLock::new()),
        })
    }

    /// Chooses how `.vortex` files are read (default [`BackendChoice::Auto`]).
    pub fn with_backend(mut self, choice: BackendChoice) -> Lake {
        self.choice = choice;
        self.resolved = Arc::new(OnceLock::new());
        self
    }

    /// Uses a different DuckDB extension directory (default `<root>/../duckdb-ext`).
    pub fn with_extension_dir(mut self, dir: impl Into<PathBuf>) -> Lake {
        self.ext_dir = dir.into();
        self.resolved = Arc::new(OnceLock::new());
        self
    }

    /// The DuckDB extension directory lindesk loads `vortex` from.
    pub fn extension_dir(&self) -> &Path {
        &self.ext_dir
    }

    /// The backend in use, resolving it on first call: with [`BackendChoice::Auto`] the
    /// extension is loaded, or installed once into [`Lake::extension_dir`] if that directory
    /// is writable, and otherwise the native reader is used.
    pub fn backend(&self) -> Result<Backend, QueryError> {
        let r = self.resolved.get_or_init(|| match self.choice {
            BackendChoice::Native => Ok(Backend::Native),
            BackendChoice::Extension => engine::probe_extension(&self.ext_dir, true).map(|()| Backend::Extension),
            BackendChoice::Auto => match engine::probe_extension(&self.ext_dir, true) {
                Ok(()) => Ok(Backend::Extension),
                Err(e) => {
                    tracing::info!(reason = %e, "DuckDB vortex extension unavailable; using the native reader");
                    Ok(Backend::Native)
                }
            },
        });
        r.clone().map_err(|e| QueryError::Invalid(format!("DuckDB vortex extension unavailable: {e}")))
    }

    /// Installs the DuckDB `vortex` extension into [`Lake::extension_dir`] (needs network
    /// access once) and switches this lake to it.
    pub fn install_extension(&self) -> Result<(), QueryError> {
        engine::probe_extension(&self.ext_dir, true).map_err(QueryError::Invalid)
    }

    /// Uses a different catalog file.
    pub fn with_catalog_path(mut self, path: impl Into<PathBuf>) -> Lake {
        self.catalog = path.into();
        self
    }

    /// Disables `RENAME_EXCHANGE` and always uses the two-rename fallback. Only useful for
    /// testing the fallback; the exchange is used automatically when supported.
    pub fn with_exchange(mut self, enabled: bool) -> Lake {
        self.exchange = enabled;
        self
    }

    pub(crate) fn use_exchange(&self) -> bool {
        self.exchange
    }

    /// The (absolute) lake root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of `catalog.duckdb`.
    pub fn catalog_path(&self) -> &Path {
        &self.catalog
    }

    /// Runs one read-only statement and renders the result.
    ///
    /// Allowed: SELECT, WITH, FROM-first, VALUES, DESCRIBE, SHOW, SUMMARIZE and
    /// `PRAGMA show_tables`. The statement runs in a fresh in-memory DuckDB whose only
    /// objects are one view per table (see [`TABLES`]; hive columns `date` and `hour` are
    /// included), with external access disabled except for the lake directory (and the
    /// output file for [`Format::Parquet`]) and the configuration locked. A query that fails
    /// because concurrent compaction removed a file is retried (twice at most).
    pub fn query(&self, sql: &str, format: &Format) -> Result<String, QueryError> {
        let mut attempt = 0;
        loop {
            match self.run_query(sql, format) {
                Err(e) if attempt < QUERY_RETRIES && query::is_vanished_file(&e) => {
                    attempt += 1;
                    tracing::debug!(error = %e, attempt, "file vanished during query; retrying");
                    std::thread::sleep(Duration::from_millis(20 * attempt as u64));
                }
                other => return other,
            }
        }
    }

    /// (Re)generates `catalog.duckdb`: one view per table over absolute `read_vortex` globs
    /// (plus `read_parquet` for legacy files, or a typed empty view for tables without
    /// files yet), and `lindesk_tables` / `lindesk_info`. It is written to a temporary file
    /// and renamed into place, so clients holding the old catalog open are unaffected.
    ///
    /// Requires [`Backend::Extension`]. Clients must run `INSTALL vortex; LOAD vortex;`
    /// before using the views (DuckDB does not autoload it); see [`CATALOG_HINT`].
    pub fn catalog(&self) -> Result<PathBuf, QueryError> {
        self.write_catalog()
    }

    /// Merges every sealed hour partition (older than the hour containing `now`) that has
    /// more than one file into a single ZSTD file sorted by the table's sort key, then
    /// refreshes the `_1m`/`_1h` rollups of sealed hours whose raw files changed.
    pub fn compact(&self, now: TimestampMs) -> Result<CompactReport, QueryError> {
        self.run_compact(now)
    }

    /// Drops whole partitions past their tier's TTL (`raw` for raw tables, `rollup_1m`,
    /// `rollup_1h`), then, while the lake exceeds `max_size` (0 = unlimited), the oldest
    /// sealed raw partitions, then 1m and 1h rollups. Bench tables are never pruned.
    pub fn prune(&self, retention: &Retention, now: TimestampMs) -> Result<PruneReport, QueryError> {
        self.run_prune(retention, now)
    }

    /// Deletes rows matching `req.predicate` from every selected table whose columns the
    /// predicate binds to, raw and rollup alike, rewriting only affected partitions.
    pub fn delete(&self, req: &DeleteRequest) -> Result<DeleteReport, QueryError> {
        self.run_delete(req)
    }

    /// Removes every table's data, then regenerates the (now empty) catalog.
    pub fn purge(&self) -> Result<PurgeReport, QueryError> {
        let r = self.run_purge()?;
        self.refresh_catalog();
        Ok(r)
    }

    /// Partitions, files, rows, bytes and oldest/newest partition per table.
    pub fn stats(&self) -> Result<LakeStats, QueryError> {
        self.compute_stats()
    }

    /// The timer job: compact (merging and converting legacy Parquet), prune, and regenerate
    /// the catalog when the vortex extension is available.
    pub fn maintain(&self, retention: &Retention, now: TimestampMs) -> Result<MaintainReport, QueryError> {
        let compact = self.compact(now)?;
        let prune = self.prune(retention, now)?;
        let catalog = self.refresh_catalog();
        Ok(MaintainReport { compact, prune, catalog })
    }

    /// Regenerates the catalog if possible. Without the vortex extension a stale catalog
    /// would point at the wrong files, so it is removed instead. Returns the path when written.
    pub fn refresh_catalog(&self) -> Option<PathBuf> {
        match self.catalog() {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!(error = %e, "catalog.duckdb not regenerated");
                let _ = std::fs::remove_file(&self.catalog);
                None
            }
        }
    }
}
