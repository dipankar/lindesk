//! `catalog.duckdb`: a DuckDB database file holding only views over the lake, so any
//! DuckDB client can open it with `duckdb -readonly catalog.duckdb` and, after
//! `INSTALL vortex; LOAD vortex;`, query every table.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use lindesk_store::TABLES;
use lindesk_store::layout;

use crate::engine::{Backend, Db, ident, lit, path_lit};
use crate::{Lake, QueryError};

/// Storage version the catalog is written with, so DuckDB clients from 1.0 on can open it.
const STORAGE_VERSION: &str = "v1.0.0";

impl Lake {
    pub(crate) fn write_catalog(&self) -> Result<PathBuf, QueryError> {
        let path = self.catalog_path().to_path_buf();
        let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        // The catalog lives next to the lake root; give its directory the root's mode.
        let dir_mode = fs::metadata(self.root()).map(|m| m.permissions().mode() & 0o7777).unwrap_or(0o700);
        layout::create_dir_all(&dir, dir_mode)?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        // DuckDB writes the database plus WAL/checkpoint side files; give it a private
        // scratch directory next to the catalog and move only the database out of it.
        let scratch = dir.join(format!(".{name}.{}.tmp", ulid::Ulid::new()));
        layout::create_dir_all(&scratch, 0o700)?;
        let tmp = scratch.join(&name);
        let result = (|| -> Result<(), QueryError> {
            if self.backend()? != Backend::Extension {
                return Err(QueryError::Invalid(format!(
                    "catalog.duckdb needs the DuckDB vortex extension (its views use read_vortex); \
                     it could not be loaded from or installed into {}",
                    self.extension_dir().display()
                )));
            }
            let mut db = Db::open(Backend::Extension, self.extension_dir(), &[self.root(), &scratch], &[], None)?;
            db.conn.execute_batch(&format!(
                "ATTACH {} AS cat (STORAGE_VERSION {})",
                path_lit(&tmp),
                lit(STORAGE_VERSION)
            ))?;
            for def in TABLES {
                let body = db.table_body(self.root(), def)?;
                db.conn.execute_batch(&format!("CREATE VIEW cat.main.{} AS {body}", ident(def.name)))?;
            }
            let conn = &db.conn;
            let rows: Vec<String> = TABLES
                .iter()
                .map(|t| {
                    let kind = match t.kind {
                        lindesk_store::TableKind::Raw => "raw",
                        lindesk_store::TableKind::Rollup { .. } => "rollup",
                        lindesk_store::TableKind::Bench => "bench",
                    };
                    format!("({}, {}, {})", lit(t.name), lit(kind), lit(t.doc))
                })
                .collect();
            conn.execute_batch(&format!(
                "CREATE VIEW cat.main.lindesk_tables AS SELECT * FROM (VALUES {}) AS t(name, kind, description);\n\
                 CREATE VIEW cat.main.lindesk_info AS SELECT {} AS lake_root, {} AS schema_version, \
                 {} AS generated_at, 'vortex' AS format, {} AS required_setup;\n\
                 DETACH cat;",
                rows.join(", "),
                path_lit(self.root()),
                lindesk_store::SCHEMA_VERSION,
                lit(&chrono::Utc::now().to_rfc3339()),
                lit(crate::CATALOG_HINT),
            ))?;
            Ok(())
        })();
        let finish = result.and_then(|()| {
            fs::set_permissions(&tmp, fs::Permissions::from_mode(dir_mode & 0o666))?;
            fs::File::open(&tmp)?.sync_all()?;
            fs::rename(&tmp, &path)?;
            layout::fsync_dir(&dir)?;
            Ok(())
        });
        let _ = fs::remove_dir_all(&scratch);
        finish.map(|()| path)
    }
}
