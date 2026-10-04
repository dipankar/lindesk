//! Lake statistics read from directory listings and file footers (no data is scanned).

use std::fs::File;

use lindesk_store::TABLES;
use lindesk_store::layout;
use parquet::file::reader::{FileReader, SerializedFileReader};
use serde::{Deserialize, Serialize};

use crate::{Lake, QueryError};

/// Statistics of one table.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct TableStats {
    /// Table name.
    pub table: String,
    /// `raw`, `rollup` or `bench`.
    pub kind: String,
    /// Partition directories.
    pub partitions: usize,
    /// Data files.
    pub files: usize,
    /// Rows, from file footers.
    pub rows: u64,
    /// Bytes on disk.
    pub bytes: u64,
    /// Files still in the pre-Vortex Parquet format (converted by compaction).
    pub legacy_files: usize,
    /// Oldest partition (`YYYY-MM-DD HH:00`, or `YYYY-MM-DD` for rollups).
    pub oldest: Option<String>,
    /// Newest partition.
    pub newest: Option<String>,
}

/// Statistics of the whole lake.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct LakeStats {
    /// Lake root.
    pub root: String,
    /// Per-table statistics, in [`TABLES`] order.
    pub tables: Vec<TableStats>,
    /// Total bytes.
    pub bytes: u64,
    /// Total rows.
    pub rows: u64,
}

impl Lake {
    pub(crate) fn compute_stats(&self) -> Result<LakeStats, QueryError> {
        let mut out = LakeStats { root: self.root().display().to_string(), ..Default::default() };
        let codec = lindesk_store::format::VortexCodec::new();
        for def in TABLES {
            let parts = layout::list_partitions(self.root(), def)?;
            let mut t = TableStats {
                table: def.name.into(),
                kind: match def.kind {
                    lindesk_store::TableKind::Raw => "raw",
                    lindesk_store::TableKind::Rollup { .. } => "rollup",
                    lindesk_store::TableKind::Bench => "bench",
                }
                .into(),
                partitions: parts.len(),
                oldest: parts.first().map(|p| p.key.to_string()),
                newest: parts.last().map(|p| p.key.to_string()),
                ..Default::default()
            };
            for f in parts.iter().flat_map(|p| &p.files) {
                // A file can vanish under a concurrent compaction; skip it.
                let rows = if f.is_legacy() {
                    let Ok(file) = File::open(&f.path) else { continue };
                    let reader = SerializedFileReader::new(file)?;
                    u64::try_from(reader.metadata().file_metadata().num_rows()).unwrap_or(0)
                } else {
                    match codec.row_count(&f.path) {
                        Ok(n) => n,
                        Err(_) if !f.path.exists() => continue,
                        Err(e) => return Err(crate::engine::store_err(e)),
                    }
                };
                t.rows += rows;
                t.files += 1;
                t.bytes += f.bytes;
                if f.is_legacy() {
                    t.legacy_files += 1;
                }
            }
            out.bytes += t.bytes;
            out.rows += t.rows;
            out.tables.push(t);
        }
        Ok(out)
    }
}
