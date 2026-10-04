//! The on-disk file format: [Vortex](https://vortex.dev), via the Rust `vortex` crate.
//!
//! Every `.vortex` file in the lake, whether flushed by a daemon or rewritten by maintenance,
//! is produced by [`write_vortex_file`] from Arrow batches cast to the table's declared
//! schema, so all files of a table are mutually compatible (DuckDB's `read_vortex` needs one
//! schema per scan). The lake root carries a `_format.json` marker naming the format and the
//! pinned crate version, and each file carries the same information in a metadata segment.
//!
//! Lakes written by earlier lindesk versions contain `.parquet` files; they stay readable
//! and `lindesk-data compact` converts them (see `lindesk-query`).

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::{RecordBatch, RecordBatchReader};
use arrow_schema::SchemaRef;
use rustix::fs::{Mode, OFlags};
use vortex::VortexSessionDefault;
use vortex::array::ArrayRef;
use vortex::array::IntoArray;
use vortex::array::arrays::ChunkedArray;
use vortex::arrow::ArrowSessionExt;
use vortex::compressor::BtrBlocksCompressorBuilder;
use vortex::file::{OpenOptionsSessionExt, WriteOptionsSessionExt, WriteStrategyBuilder};
use vortex::io::runtime::BlockingRuntime;
use vortex::io::runtime::current::CurrentThreadRuntime;
use vortex::io::session::RuntimeSessionExt;
use vortex::layout::LayoutStrategy;
use vortex::session::VortexSession;

use crate::StoreError;
use crate::layout;
use crate::schema::{SCHEMA_VERSION, TableDef};

/// Extension of data files.
pub const VORTEX_EXT: &str = "vortex";
/// Extension of data files written by lindesk before the switch to Vortex.
pub const LEGACY_PARQUET_EXT: &str = "parquet";
/// The exact `vortex` crate version files are written with.
pub const VORTEX_CRATE_VERSION: &str = "0.87.0";
/// Name of the format marker at the lake root.
pub const FORMAT_MARKER: &str = "_format.json";
/// Key of the metadata segment lindesk writes into every file.
const METADATA_KEY: &str = "lindesk";

/// Contents of `_format.json` and of each file's `lindesk` metadata segment.
pub fn format_descriptor() -> serde_json::Value {
    serde_json::json!({
        "format": "vortex",
        "extension": VORTEX_EXT,
        "vortex_crate": VORTEX_CRATE_VERSION,
        "schema_version": SCHEMA_VERSION,
        "compression": "btrblocks-compact",
        "writer": format!("lindesk-store {}", env!("CARGO_PKG_VERSION")),
    })
}

/// Writes `<root>/_format.json` if it is missing or out of date (atomically).
pub fn write_format_marker(root: &Path, file_mode: u32) -> io::Result<()> {
    let path = root.join(FORMAT_MARKER);
    let body = serde_json::to_vec_pretty(&format_descriptor()).unwrap_or_default();
    if fs::read(&path).is_ok_and(|b| b == body) {
        return Ok(());
    }
    let tmp = root.join(format!(".{FORMAT_MARKER}.{}.tmp", ulid::Ulid::new()));
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(file_mode).open(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &path)?;
    layout::fsync_dir(root)
}

/// A Vortex session plus the single-threaded runtime that drives it. Creating one is cheap
/// but not free (it registers every encoding), so long-lived writers keep one around.
pub struct VortexCodec {
    // Declared before the runtime: the session holds a weak handle to it.
    session: VortexSession,
    runtime: CurrentThreadRuntime,
    strategy: Arc<dyn LayoutStrategy>,
}

impl std::fmt::Debug for VortexCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VortexCodec")
    }
}

impl Default for VortexCodec {
    fn default() -> Self {
        Self::new()
    }
}

fn vx<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Vortex(e.to_string())
}

impl VortexCodec {
    /// A codec with the default session (all maintained encodings) and BtrBlocks compression
    /// in its compact preset.
    pub fn new() -> Self {
        let runtime = CurrentThreadRuntime::new();
        let session = VortexSession::default().with_handle(runtime.handle());
        // The compact preset adds zstd for strings and Pco for numbers to the lightweight
        // encodings: telemetry is written once and scanned for analysis, so size matters
        // more than the last bit of decode speed.
        let strategy = WriteStrategyBuilder::from_session(&session)
            .with_btrblocks_builder(BtrBlocksCompressorBuilder::from_session(&session).with_compact())
            .build();
        VortexCodec { session, runtime, strategy }
    }

    /// Converts batches (already in the table schema) into one Vortex array.
    fn to_vortex(&self, schema: &SchemaRef, batches: Vec<RecordBatch>) -> Result<ArrayRef, StoreError> {
        let arrow = self.session.arrow();
        let dtype = arrow.from_arrow_schema(schema).map_err(vx)?;
        let chunks = batches
            .into_iter()
            .map(|b| arrow.from_arrow_record_batch(b, schema))
            .collect::<Result<Vec<_>, _>>()
            .map_err(vx)?;
        Ok(ChunkedArray::try_new(chunks, dtype).map_err(vx)?.into_array())
    }

    /// Writes batches as a Vortex file to `out`.
    pub fn write(&self, out: &File, schema: &SchemaRef, batches: Vec<RecordBatch>) -> Result<(), StoreError> {
        let array = self.to_vortex(schema, batches)?;
        let meta = serde_json::to_vec(&format_descriptor()).unwrap_or_default();
        self.session
            .write_options()
            .with_strategy(Arc::clone(&self.strategy))
            .with_metadata_segment(METADATA_KEY, meta)
            .blocking(&self.runtime)
            .write(out, array.to_array_iterator())
            .map_err(vx)?;
        Ok(())
    }

    /// Reads a whole Vortex file as Arrow batches with `schema` (the table's declared schema).
    pub fn read(&self, path: &Path, schema: SchemaRef) -> Result<Vec<RecordBatch>, StoreError> {
        let bytes = fs::read(path)?;
        let file = self.session.open_options().open_buffer(bytes).map_err(vx)?;
        let reader = file.scan().map_err(vx)?.into_record_batch_reader(schema, &self.runtime).map_err(vx)?;
        let _: &dyn RecordBatchReader = &reader;
        reader.collect::<Result<Vec<_>, _>>().map_err(StoreError::Arrow)
    }

    /// Number of rows in a Vortex file, from its footer.
    pub fn row_count(&self, path: &Path) -> Result<u64, StoreError> {
        let open = self.session.open_options().open_path(path.to_path_buf());
        let file = self.runtime.block_on(open).map_err(vx)?;
        Ok(file.row_count())
    }
}

/// Casts a batch to the table's declared schema (column order, types and nullability), so
/// every file of a table has an identical Vortex dtype.
pub fn cast_to_table(def: &TableDef, batch: &RecordBatch) -> Result<RecordBatch, StoreError> {
    let schema = def.arrow_schema();
    let mut cols = Vec::with_capacity(schema.fields().len());
    for field in schema.fields() {
        let col = batch.column_by_name(field.name()).ok_or_else(|| {
            StoreError::Config(format!("{}: column '{}' missing from rewritten data", def.name, field.name()))
        })?;
        cols.push(arrow_cast::cast(col, field.data_type())?);
    }
    Ok(RecordBatch::try_new(Arc::clone(&schema), cols)?)
}

/// Writes `batches` (any column order or compatible types) as a new
/// `<dir>/part-<ulid>.vortex`: hidden `.tmp` file, fsync, rename, fsync of the directory,
/// all relative to the directory fd. Returns the path and the row count.
pub fn write_vortex_file(
    codec: &VortexCodec,
    dir: &Path,
    def: &TableDef,
    batches: &[RecordBatch],
    file_mode: u32,
) -> Result<(PathBuf, u64), StoreError> {
    let schema = def.arrow_schema();
    let batches = batches.iter().map(|b| cast_to_table(def, b)).collect::<Result<Vec<_>, _>>()?;
    let rows: u64 = batches.iter().map(|b| b.num_rows() as u64).sum();
    let dirfd = rustix::fs::open(dir, OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())?;
    let name = layout::new_part_name();
    let tmp = format!(".{name}.tmp");
    let fd = rustix::fs::openat(
        &dirfd,
        tmp.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::from_raw_mode(file_mode),
    )?;
    let file = File::from(fd);
    let result = (|| -> Result<(), StoreError> {
        rustix::fs::fchmod(&file, Mode::from_raw_mode(file_mode))?;
        codec.write(&file, &schema, batches)?;
        file.sync_all()?;
        rustix::fs::renameat(&dirfd, tmp.as_str(), &dirfd, name.as_str())?;
        rustix::fs::fsync(&dirfd)?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = rustix::fs::unlinkat(&dirfd, tmp.as_str(), rustix::fs::AtFlags::empty());
        return Err(e);
    }
    Ok((dir.join(name), rows))
}
