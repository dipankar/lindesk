//! DuckDB plumbing: backend selection (the DuckDB `vortex` extension or the native Rust
//! reader), sandboxed connections, SQL quoting, relations over data files and the
//! Arrow bridge used to write query results as Vortex files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use duckdb::Connection;
use duckdb::arrow as a58;
use duckdb::arrow::array::Array as _;
use lindesk_store::arrow::arrow_array as a59;
use lindesk_store::arrow::arrow_array::Array as _;
use lindesk_store::format::{self, LEGACY_PARQUET_EXT, VORTEX_EXT, VortexCodec};
use lindesk_store::layout;
use lindesk_store::schema::{ColType, Granularity, TableDef};

use crate::QueryError;

/// How `.vortex` files are read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// DuckDB's `vortex` core extension (`read_vortex`). Preferred: scans stream and
    /// push down filters, and `catalog.duckdb` needs it.
    Extension,
    /// The Rust `vortex` crate reads files into Arrow and loads them into DuckDB tables.
    /// Works offline and on platforms without the extension; slower and memory-bound.
    Native,
}

/// Which backend [`crate::Lake`] should use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackendChoice {
    /// Use the extension if it loads (installing it once into the lindesk extension
    /// directory if needed), otherwise fall back to [`Backend::Native`].
    #[default]
    Auto,
    /// Always use the extension; fail if it cannot be loaded.
    Extension,
    /// Never touch the extension.
    Native,
}

/// Quotes a SQL string literal.
pub(crate) fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Quotes a path as a SQL string literal.
pub(crate) fn path_lit(p: &Path) -> String {
    lit(&p.to_string_lossy())
}

/// Quotes a SQL identifier.
pub(crate) fn ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// A SQL list literal of file paths.
pub(crate) fn file_list<'a>(files: impl IntoIterator<Item = &'a PathBuf>) -> String {
    let items: Vec<String> = files.into_iter().map(|p| path_lit(p)).collect();
    format!("[{}]", items.join(", "))
}

/// `ORDER BY` list for a table's sort key.
pub(crate) fn order_by(def: &TableDef) -> String {
    def.sort_key.iter().map(|k| ident(k)).collect::<Vec<_>>().join(", ")
}

/// Hive partition columns that views expose, with their types.
pub(crate) fn hive_columns(def: &TableDef) -> &'static [(&'static str, &'static str)] {
    match def.granularity() {
        Granularity::Hour => &[("date", "DATE"), ("hour", "INTEGER")],
        Granularity::Day => &[("date", "DATE")],
    }
}

/// The glob matching a table's data files with extension `ext`.
pub(crate) fn table_glob(root: &Path, def: &TableDef, ext: &str) -> PathBuf {
    layout::table_dir(root, def.name).join(format!("**/*.{ext}"))
}

/// A typed relation with no rows and the table's columns (plus the hive partition columns
/// when `hive`), used for tables without files and for validating predicates.
pub(crate) fn typed_empty(def: &TableDef, hive: bool) -> String {
    let hive_cols = if hive { hive_columns(def) } else { &[] };
    let cols: Vec<String> = def
        .columns
        .iter()
        .map(|c| (c.name, c.ty.duckdb_type()))
        .chain(hive_cols.iter().copied())
        .map(|(n, t)| format!("CAST(NULL AS {t}) AS {}", ident(n)))
        .collect();
    format!("SELECT {} WHERE false", cols.join(", "))
}

/// `SELECT` list casting a relation's columns to the declared types. Legacy Parquet files
/// stored labels as `MAP(VARCHAR, VARCHAR)`; they become the JSON objects Vortex files hold.
fn normalized_cols(def: &TableDef, legacy: bool, hive: bool) -> String {
    let mut cols: Vec<String> = def
        .columns
        .iter()
        .map(|c| {
            let src = if legacy && c.name == "labels" && c.ty == ColType::Utf8 {
                "to_json(\"labels\")".to_string()
            } else {
                ident(c.name)
            };
            format!("CAST({src} AS {}) AS {}", c.ty.duckdb_type(), ident(c.name))
        })
        .collect();
    if hive {
        cols.extend(hive_columns(def).iter().map(|(n, t)| format!("CAST({} AS {t}) AS {}", ident(n), ident(n))));
    }
    cols.join(", ")
}

/// Splits files into (vortex, legacy parquet).
fn split(files: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    files.iter().cloned().partition(|f| f.extension().is_none_or(|e| e != LEGACY_PARQUET_EXT))
}

/// True for errors where the vortex extension could not translate a pushed-down filter.
pub(crate) fn is_pushdown_error(e: &QueryError) -> bool {
    matches!(e, QueryError::DuckDb(d) if d.to_string().contains("DUCKDB_VX_EXPR"))
}

/// Settings applied to every connection before anything else.
const BASE_SETTINGS: &str = "SET autoinstall_known_extensions = false;\nSET autoload_known_extensions = false;\n";

/// Default directory for the extension: `<lake root>/../duckdb-ext`.
pub(crate) fn default_extension_dir(root: &Path) -> PathBuf {
    root.parent().unwrap_or(root).join("duckdb-ext")
}

/// Tries to load (and, when `install`, install) the vortex extension on a scratch
/// connection. Returns the error text when it is unavailable.
pub(crate) fn probe_extension(ext_dir: &Path, install: bool) -> Result<(), String> {
    let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
    conn.execute_batch(&format!("{BASE_SETTINGS}SET extension_directory = {};", path_lit(ext_dir)))
        .map_err(|e| e.to_string())?;
    match conn.execute_batch("LOAD vortex;") {
        Ok(()) => Ok(()),
        Err(load) if install => {
            std::fs::create_dir_all(ext_dir)
                .map_err(|e| format!("{load}; cannot create {}: {e}", ext_dir.display()))?;
            conn.execute_batch("INSTALL vortex; LOAD vortex;").map_err(|e| format!("install failed: {e}"))?;
            tracing::info!(dir = %ext_dir.display(), "installed the DuckDB vortex extension");
            Ok(())
        }
        Err(load) => Err(load.to_string()),
    }
}

/// A sandboxed in-memory DuckDB plus whatever the backend needs to read and write files.
pub(crate) struct Db {
    pub conn: Connection,
    backend: Backend,
    codec: VortexCodec,
    /// Native backend: tables already loaded for a given file list.
    loaded: HashMap<Vec<PathBuf>, String>,
    next: usize,
}

impl Db {
    /// Opens an in-memory DuckDB that can only touch `dirs` (recursively) and `paths`.
    /// The vortex extension (if used) is loaded first; then external access is disabled
    /// and the configuration locked, so SQL on the connection cannot widen its access,
    /// install extensions or reach the network.
    pub fn open(
        backend: Backend,
        ext_dir: &Path,
        dirs: &[&Path],
        paths: &[&Path],
        temp_dir: Option<&Path>,
    ) -> Result<Db, QueryError> {
        Self::open_with(backend, ext_dir, dirs, paths, temp_dir, true)
    }

    /// Like [`Db::open`]; `pushdown = false` disables DuckDB's filter pushdown, which the
    /// vortex extension cannot always translate (e.g. `IS DISTINCT FROM`).
    pub fn open_with(
        backend: Backend,
        ext_dir: &Path,
        dirs: &[&Path],
        paths: &[&Path],
        temp_dir: Option<&Path>,
        pushdown: bool,
    ) -> Result<Db, QueryError> {
        let conn = Connection::open_in_memory()?;
        let list = |ps: &[&Path], dir: bool| {
            let items: Vec<String> = ps
                .iter()
                .map(|p| {
                    let mut s = p.to_string_lossy().into_owned();
                    if dir && !s.ends_with('/') {
                        s.push('/');
                    }
                    lit(&s)
                })
                .collect();
            format!("[{}]", items.join(", "))
        };
        let mut sql = String::from(BASE_SETTINGS);
        if backend == Backend::Extension {
            sql.push_str(&format!("SET extension_directory = {};\nLOAD vortex;\n", path_lit(ext_dir)));
        }
        if let Some(t) = temp_dir {
            sql.push_str(&format!("SET temp_directory = {};\n", path_lit(t)));
        }
        if !pushdown {
            sql.push_str("SET disabled_optimizers = 'filter_pushdown';\n");
        }
        sql.push_str(&format!(
            "SET allowed_directories = {};\nSET allowed_paths = {};\n\
             SET enable_external_access = false;\nSET lock_configuration = true;",
            list(dirs, true),
            list(paths, false)
        ));
        conn.execute_batch(&sql)?;
        Ok(Db { conn, backend, codec: VortexCodec::new(), loaded: HashMap::new(), next: 0 })
    }

    /// A relation (usable after `FROM`) over explicit data files of one table, with exactly
    /// the declared columns and no hive columns.
    pub fn relation(&mut self, def: &TableDef, files: &[PathBuf]) -> Result<String, QueryError> {
        let (vortex, legacy) = split(files);
        let mut parts = Vec::new();
        if !vortex.is_empty() {
            let src = match self.backend {
                Backend::Extension => {
                    format!("read_vortex({}, hive_partitioning = false)", file_list(&vortex))
                }
                Backend::Native => self.load_native(def, &vortex, None)?,
            };
            parts.push(format!("SELECT {} FROM {src}", normalized_cols(def, false, false)));
        }
        if !legacy.is_empty() {
            parts.push(format!(
                "SELECT {} FROM read_parquet({}, union_by_name = true, hive_partitioning = false)",
                normalized_cols(def, true, false),
                file_list(&legacy)
            ));
        }
        Ok(match parts.len() {
            0 => format!("({})", typed_empty(def, false)),
            _ => format!("({})", parts.join(" UNION ALL ")),
        })
    }

    /// Creates the user-facing view (or, for the native backend, table) `def.name` over all
    /// of the table's files, with hive columns `date` (and `hour`).
    pub fn create_table_view(&mut self, root: &Path, def: &TableDef) -> Result<(), QueryError> {
        let body = self.table_body(root, def)?;
        self.conn.execute_batch(&format!("CREATE VIEW {} AS {body}", ident(def.name)))?;
        Ok(())
    }

    /// SELECT body over every file of a table (see [`Db::create_table_view`]).
    pub fn table_body(&mut self, root: &Path, def: &TableDef) -> Result<String, QueryError> {
        let parts_list = layout::list_partitions(root, def)?;
        let has =
            |ext: &str| parts_list.iter().flat_map(|p| &p.files).any(|f| f.path.extension().is_some_and(|e| e == ext));
        let mut parts = Vec::new();
        if has(VORTEX_EXT) {
            let src = match self.backend {
                Backend::Extension => {
                    format!("read_vortex({}, hive_partitioning = true)", path_lit(&table_glob(root, def, VORTEX_EXT)))
                }
                Backend::Native => {
                    let mut tables = Vec::new();
                    for p in &parts_list {
                        if let Some(t) = self.load_partition_native(root, def, p.key)? {
                            tables.push(t);
                        }
                    }
                    let union: Vec<String> = tables.iter().map(|t| format!("SELECT * FROM {t}")).collect();
                    format!("({})", union.join(" UNION ALL "))
                }
            };
            parts.push(format!("SELECT {} FROM {src}", normalized_cols(def, false, true)));
        }
        if has(LEGACY_PARQUET_EXT) {
            parts.push(format!(
                "SELECT {} FROM read_parquet({}, hive_partitioning = true, union_by_name = true)",
                normalized_cols(def, true, true),
                path_lit(&table_glob(root, def, LEGACY_PARQUET_EXT))
            ));
        }
        Ok(if parts.is_empty() { typed_empty(def, true) } else { parts.join(" UNION ALL ") })
    }

    /// Native backend: loads one partition (with hive columns). Maintenance may swap the
    /// partition directory while it is read; since the swap is atomic, a vanished file means
    /// "list again", and the half-loaded table is dropped so rows are never counted twice.
    fn load_partition_native(
        &mut self,
        root: &Path,
        def: &TableDef,
        key: layout::PartitionKey,
    ) -> Result<Option<String>, QueryError> {
        const ATTEMPTS: usize = 20;
        for attempt in 1..=ATTEMPTS {
            let part = layout::partition(root, def, key)?;
            let files: Vec<PathBuf> = part.files.iter().filter(|f| !f.is_legacy()).map(|f| f.path.clone()).collect();
            if files.is_empty() {
                return Ok(None);
            }
            match self.load_native(def, &files, Some(key)) {
                Err(QueryError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound && attempt < ATTEMPTS => {
                    std::thread::sleep(std::time::Duration::from_millis(5 * attempt as u64));
                }
                other => return other.map(Some),
            }
        }
        Ok(None)
    }

    /// Native backend: reads `files` with the Rust codec into a table and returns its name.
    /// With `key`, the hive columns of that partition are added. On error the partly filled
    /// table is dropped.
    fn load_native(
        &mut self,
        def: &TableDef,
        files: &[PathBuf],
        key: Option<layout::PartitionKey>,
    ) -> Result<String, QueryError> {
        if let Some(t) = self.loaded.get(files) {
            return Ok(t.clone());
        }
        self.next += 1;
        let name = format!("__lindesk_native_{}", self.next);
        self.conn.execute_batch(&format!("CREATE TABLE {name} AS {}", typed_empty(def, key.is_some())))?;
        if let Err(e) = self.fill_native(&name, def, files, key) {
            let _ = self.conn.execute_batch(&format!("DROP TABLE IF EXISTS {name}"));
            return Err(e);
        }
        self.loaded.insert(files.to_vec(), name.clone());
        Ok(name)
    }

    fn fill_native(
        &self,
        name: &str,
        def: &TableDef,
        files: &[PathBuf],
        key: Option<layout::PartitionKey>,
    ) -> Result<(), QueryError> {
        let schema = def.arrow_schema();
        {
            let mut app = self.conn.appender(name)?;
            for f in files {
                let batches = self.codec.read(f, Arc::clone(&schema)).map_err(store_err)?;
                for b in batches {
                    let mut b = to_a58(b)?;
                    if let Some(k) = key {
                        b = with_hive(b, &k)?;
                    }
                    app.append_record_batch(b)?;
                }
            }
            app.flush()?;
        }
        Ok(())
    }

    /// Runs `select`, writes its result as a new Vortex file in `dir` (through the same
    /// writer the daemons use) and returns the row count.
    pub fn write_vortex(&self, def: &TableDef, select: &str, dir: &Path, file_mode: u32) -> Result<u64, QueryError> {
        let mut stmt = self.conn.prepare(select)?;
        let batches: Vec<a59::RecordBatch> =
            stmt.query_arrow([])?.map(to_a59).collect::<Result<Vec<_>, QueryError>>()?;
        layout::create_dir_all(dir, 0o700)?;
        let (_, rows) = format::write_vortex_file(&self.codec, dir, def, &batches, file_mode).map_err(store_err)?;
        Ok(rows)
    }
}

pub(crate) fn store_err(e: lindesk_store::StoreError) -> QueryError {
    match e {
        lindesk_store::StoreError::Io(e) => QueryError::Io(e),
        other => QueryError::Invalid(other.to_string()),
    }
}

/// Moves a record batch from DuckDB's arrow (58) to the writer's arrow (59) through the
/// Arrow C data interface, which both versions implement with identical `#[repr(C)]` structs.
fn to_a59(batch: a58::record_batch::RecordBatch) -> Result<a59::RecordBatch, QueryError> {
    let data = a58::array::StructArray::from(batch).into_data();
    let (array, schema) = a58::ffi::to_ffi(&data).map_err(|e| QueryError::Invalid(format!("arrow export: {e}")))?;
    // SAFETY: FFI_ArrowArray / FFI_ArrowSchema are the C ABI structs of the Arrow C data
    // interface in both crate versions; ownership (and the release callback) moves with them.
    let (array, schema) = unsafe {
        (
            std::mem::transmute::<a58::ffi::FFI_ArrowArray, a59::ffi::FFI_ArrowArray>(array),
            std::mem::transmute::<a58::ffi::FFI_ArrowSchema, a59::ffi::FFI_ArrowSchema>(schema),
        )
    };
    let data =
        unsafe { a59::ffi::from_ffi(array, &schema) }.map_err(|e| QueryError::Invalid(format!("arrow import: {e}")))?;
    Ok(a59::RecordBatch::from(a59::StructArray::from(data)))
}

/// The reverse of [`to_a59`].
fn to_a58(batch: a59::RecordBatch) -> Result<a58::record_batch::RecordBatch, QueryError> {
    let data = a59::StructArray::from(batch).into_data();
    let (array, schema) = a59::ffi::to_ffi(&data).map_err(|e| QueryError::Invalid(format!("arrow export: {e}")))?;
    // SAFETY: see `to_a59`.
    let (array, schema) = unsafe {
        (
            std::mem::transmute::<a59::ffi::FFI_ArrowArray, a58::ffi::FFI_ArrowArray>(array),
            std::mem::transmute::<a59::ffi::FFI_ArrowSchema, a58::ffi::FFI_ArrowSchema>(schema),
        )
    };
    let data =
        unsafe { a58::ffi::from_ffi(array, &schema) }.map_err(|e| QueryError::Invalid(format!("arrow import: {e}")))?;
    Ok(a58::record_batch::RecordBatch::from(a58::array::StructArray::from(data)))
}

/// Appends constant hive columns (`date`, and `hour` for hourly tables) to a batch.
fn with_hive(
    batch: a58::record_batch::RecordBatch,
    key: &layout::PartitionKey,
) -> Result<a58::record_batch::RecordBatch, QueryError> {
    use a58::array::{ArrayRef, Date32Array, Int32Array};
    use a58::datatypes::{DataType, Field, Schema};
    let n = batch.num_rows();
    let days = (key.start_ms().div_euclid(86_400_000)) as i32;
    let mut fields: Vec<Field> = batch.schema().fields().iter().map(|f| f.as_ref().clone()).collect();
    let mut cols: Vec<ArrayRef> = batch.columns().to_vec();
    fields.push(Field::new("date", DataType::Date32, true));
    cols.push(Arc::new(Date32Array::from(vec![days; n])));
    if let Some(h) = key.hour {
        fields.push(Field::new("hour", DataType::Int32, true));
        cols.push(Arc::new(Int32Array::from(vec![i32::from(h); n])));
    }
    a58::record_batch::RecordBatch::try_new(Arc::new(Schema::new(fields)), cols)
        .map_err(|e| QueryError::Invalid(e.to_string()))
}

/// Creates one view per table, skipping the expensive work for tables the SQL cannot
/// reference (`wanted` returns false), which get the empty typed view instead.
pub(crate) fn create_views(
    db: &mut Db,
    root: &Path,
    tables: &[TableDef],
    wanted: impl Fn(&TableDef) -> bool,
) -> Result<(), QueryError> {
    for def in tables {
        if wanted(def) {
            db.create_table_view(root, def)?;
        } else {
            db.conn.execute_batch(&format!("CREATE VIEW {} AS {}", ident(def.name), typed_empty(def, true)))?;
        }
    }
    Ok(())
}
