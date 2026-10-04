//! Maintenance: compaction, rollups, retention, deletion and purge.
//!
//! Every operation holds the lake's exclusive lock (writers defer while it is held) and
//! works in a private staging directory, `<root>/.staging/<ulid>/`, on the same filesystem.
//! A rewritten partition is built there and swapped with the live directory using
//! `renameat2(RENAME_EXCHANGE)`, so a reader sees either the old files or the new ones,
//! never both. Afterwards the staging directory (now holding the old files) is removed;
//! readers that already opened an old file keep reading it.
//!
//! Crash safety: on start every operation recovers leftover staging directories. Because
//! the swap is atomic, a staged partition is always the *non-live* copy and is discarded,
//! except in the non-atomic fallback (filesystems without `RENAME_EXCHANGE`), where a
//! staged partition whose live directory is missing is moved back into place.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use lindesk_core::config::Retention;
use lindesk_plugin_api::TimestampMs;
use lindesk_store::layout::{self, LakeLock, LockMode, Partition, PartitionKey, STAGING_DIR};
use lindesk_store::schema::{Bucket, Granularity, TableDef, TableKind};
use lindesk_store::{TABLES, table};
use rustix::fs::{CWD, RenameFlags};
use serde::{Deserialize, Serialize};

use crate::engine::{self, Db, order_by};
use crate::rollup::{epoch_hour_expr, rollup_select, ts_lit};
use crate::{Lake, QueryError};

/// Name of the per-day manifest in rollup partitions recording which raw files each hour
/// was computed from. Rollups of an hour are recomputed when that set changes.
pub(crate) const ROLLUP_MANIFEST: &str = ".sources.json";

/// Marker written into a staged partition once it is complete, just before the swap.
/// Recovery only ever restores staged partitions that carry it.
const COMPLETE_MARKER: &str = ".complete";

/// Result of [`Lake::compact`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct CompactReport {
    /// Partitions rewritten: sealed raw and bench hours merged, plus partitions converted
    /// from legacy Parquet.
    pub partitions: usize,
    /// Data files in those partitions before compaction.
    pub files_before: usize,
    /// Data files in those partitions after compaction.
    pub files_after: usize,
    /// Bytes of those partitions before compaction.
    pub bytes_before: u64,
    /// Bytes of those partitions after compaction.
    pub bytes_after: u64,
    /// Rows rewritten by compaction.
    pub rows: u64,
    /// Rollup partitions (days) rewritten.
    pub rollup_partitions: usize,
    /// Raw hours (re)aggregated into rollups, summed over rollup tables.
    pub rollup_hours: usize,
    /// Crashed-operation leftovers cleaned up first.
    pub recovered: usize,
}

/// Partitions removed from one table by [`Lake::prune`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct TablePrune {
    /// Table name.
    pub table: String,
    /// Partitions removed because they outlived their TTL.
    pub expired: usize,
    /// Partitions removed to honour `max_size`.
    pub evicted: usize,
    /// Bytes freed.
    pub bytes: u64,
}

/// Result of [`Lake::prune`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Per-table results (tables with nothing removed are omitted).
    pub tables: Vec<TablePrune>,
    /// Lake size before pruning.
    pub bytes_before: u64,
    /// Lake size after pruning.
    pub bytes_after: u64,
}

/// What [`Lake::delete`] should remove.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct DeleteRequest {
    /// A SQL boolean expression over the table's columns, e.g. `app_id = 'org.mozilla.firefox'`.
    pub predicate: String,
    /// Tables to consider; empty means every table whose columns the predicate binds to.
    pub tables: Vec<String>,
    /// Only rows with `ts >= since` (ms since the epoch).
    pub since: Option<TimestampMs>,
    /// Only rows with `ts < until`.
    pub until: Option<TimestampMs>,
    /// Count matching rows without changing anything.
    pub dry_run: bool,
}

/// Rows removed from one table by [`Lake::delete`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct TableDelete {
    /// Table name.
    pub table: String,
    /// Matching rows (deleted unless dry run).
    pub rows: u64,
    /// Partitions rewritten without the matching rows.
    pub partitions_rewritten: usize,
    /// Partitions removed because every row matched.
    pub partitions_removed: usize,
}

/// Result of [`Lake::delete`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct DeleteReport {
    /// Tables with matching rows.
    pub tables: Vec<TableDelete>,
    /// Tables skipped because the predicate does not bind to their columns, with the reason.
    pub skipped: Vec<(String, String)>,
    /// Whether this was a dry run.
    pub dry_run: bool,
}

impl DeleteReport {
    /// Total matching rows over all tables.
    pub fn rows(&self) -> u64 {
        self.tables.iter().map(|t| t.rows).sum()
    }
}

/// Result of [`Lake::purge`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct PurgeReport {
    /// Data files removed.
    pub files: usize,
    /// Bytes removed.
    pub bytes: u64,
}

/// Result of [`Lake::maintain`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct MaintainReport {
    /// Compaction and rollups.
    pub compact: CompactReport,
    /// Retention.
    pub prune: PruneReport,
    /// The regenerated catalog, or `None` when the DuckDB vortex extension is unavailable.
    pub catalog: Option<PathBuf>,
}

/// A staging directory for one maintenance operation.
struct Op {
    dir: PathBuf,
    trash: usize,
}

impl Op {
    /// Where a staged copy of a live partition directory is built.
    fn staged(&self, root: &Path, live: &Path) -> PathBuf {
        self.dir.join(live.strip_prefix(root).unwrap_or(live))
    }

    fn next_trash(&mut self) -> PathBuf {
        self.trash += 1;
        self.dir.join(".trash").join(self.trash.to_string())
    }
}

impl Drop for Op {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir_all(&self.dir)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(dir = %self.dir.display(), error = %e, "could not remove staging directory");
        }
    }
}

/// Checks that a user predicate is a single expression: no statement separators, no
/// comments and balanced parentheses outside quotes.
fn check_predicate(pred: &str) -> Result<(), QueryError> {
    let bad = |m: &str| Err(QueryError::Invalid(format!("invalid predicate: {m}")));
    if pred.trim().is_empty() {
        return bad("empty");
    }
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut prev = '\0';
    for c in pred.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '\'' | '"' => quote = Some(c),
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth < 0 {
                        return bad("unbalanced ')'");
                    }
                }
                ';' => return bad("';' is not allowed"),
                '-' if prev == '-' => return bad("comments are not allowed"),
                '*' if prev == '/' => return bad("comments are not allowed"),
                _ => {}
            },
        }
        prev = c;
    }
    if quote.is_some() {
        return bad("unterminated quote");
    }
    if depth != 0 {
        return bad("unbalanced '('");
    }
    Ok(())
}

impl Lake {
    fn dir_mode(&self) -> u32 {
        fs::metadata(self.root()).map(|m| m.permissions().mode() & 0o7777).unwrap_or(0o700)
    }

    pub(crate) fn file_mode(&self) -> u32 {
        self.dir_mode() & 0o666
    }

    fn lock(&self) -> Result<LakeLock, QueryError> {
        Ok(LakeLock::acquire(self.root(), LockMode::Exclusive, self.file_mode())?)
    }

    fn staging(&self) -> PathBuf {
        self.root().join(STAGING_DIR)
    }

    fn begin_op(&self) -> Result<Op, QueryError> {
        let dir = self.staging().join(ulid::Ulid::new().to_string());
        layout::create_dir_all(&dir, self.dir_mode())?;
        Ok(Op { dir, trash: 0 })
    }

    fn maint_db(&self) -> Result<Db, QueryError> {
        let tmp = self.staging().join("duckdb_tmp");
        // Maintenance scans whole partitions, so filter pushdown buys little; it is off
        // because the vortex extension cannot translate every predicate a user may delete by.
        Db::open_with(self.backend()?, self.extension_dir(), &[self.root()], &[], Some(&tmp), false)
    }

    /// Cleans up staging directories left by a crashed operation. Must hold the lock.
    fn recover_staging(&self) -> Result<usize, QueryError> {
        let mut recovered = 0;
        for op in layout::read_dir_names(&self.staging())? {
            let op_dir = self.staging().join(&op);
            if !op_dir.is_dir() {
                continue;
            }
            for def in TABLES {
                for staged in layout::list_partitions(&op_dir, def)? {
                    let live = layout::partition_dir(self.root(), def.name, &staged.key);
                    if !live.exists() && staged.dir.join(COMPLETE_MARKER).exists() {
                        // Interrupted non-atomic swap: the staged copy is the newest data.
                        layout::create_dir_all(live.parent().unwrap_or(self.root()), self.dir_mode())?;
                        fs::rename(&staged.dir, &live)?;
                        let _ = fs::remove_file(live.join(COMPLETE_MARKER));
                        tracing::info!(partition = %live.display(), "restored partition from staging");
                    }
                }
            }
            fs::remove_dir_all(&op_dir)?;
            recovered += 1;
        }
        Ok(recovered)
    }

    /// Moves a fully built `staged` directory into place at `live`, atomically exchanging
    /// it with the existing directory when there is one.
    fn swap_in(&self, staged: &Path, live: &Path, op: &mut Op) -> Result<(), QueryError> {
        let parent = live.parent().unwrap_or(self.root());
        let marker = staged.join(COMPLETE_MARKER);
        fs::File::create(&marker)?.sync_all()?;
        layout::fsync_dir(staged)?;
        if live.exists() {
            let exchanged = self.use_exchange()
                && match rustix::fs::renameat_with(CWD, staged, CWD, live, RenameFlags::EXCHANGE) {
                    Ok(()) => true,
                    Err(rustix::io::Errno::INVAL | rustix::io::Errno::NOSYS | rustix::io::Errno::OPNOTSUPP) => false,
                    Err(e) => return Err(io::Error::from(e).into()),
                };
            if !exchanged {
                // Non-atomic fallback: readers may briefly see the partition missing, but
                // never see both copies.
                let trash = op.next_trash();
                layout::create_dir_all(trash.parent().unwrap_or(&op.dir), self.dir_mode())?;
                fs::rename(live, &trash)?;
                fs::rename(staged, live)?;
            }
        } else {
            layout::create_dir_all(parent, self.dir_mode())?;
            fs::rename(staged, live)?;
        }
        layout::fsync_dir(parent)?;
        // The marker has moved into the live directory; it is hidden from readers, so a
        // crash before this removal is harmless.
        let _ = fs::remove_file(live.join(COMPLETE_MARKER));
        Ok(())
    }

    /// Removes a live partition directory (and its date directory when that empties).
    fn remove_partition(&self, live: &Path, op: &mut Op) -> Result<(), QueryError> {
        let trash = op.next_trash();
        layout::create_dir_all(trash.parent().unwrap_or(&op.dir), self.dir_mode())?;
        fs::rename(live, &trash)?;
        let parent = live.parent().unwrap_or(self.root());
        layout::fsync_dir(parent)?;
        if parent.file_name().is_some_and(|n| n.to_string_lossy().starts_with("date="))
            && fs::remove_dir(parent).is_ok()
        {
            let _ = layout::fsync_dir(parent.parent().unwrap_or(self.root()));
        }
        fs::remove_dir_all(&trash)?;
        Ok(())
    }

    /// Runs `select` and writes its rows as a new Vortex file in `staged` (through the same
    /// writer the daemons use, so the file has exactly the table's schema), checking the
    /// row count when `expected` is given.
    fn copy_to(
        &self,
        db: &Db,
        def: &TableDef,
        select: &str,
        staged: &Path,
        expected: Option<u64>,
    ) -> Result<u64, QueryError> {
        layout::create_dir_all(staged, self.dir_mode())?;
        let n = db.write_vortex(def, select, staged, self.file_mode())?;
        if let Some(e) = expected
            && e != n
        {
            return Err(QueryError::Invalid(format!("rewrite produced {n} rows, expected {e}; aborting")));
        }
        Ok(n)
    }

    fn count(db: &Db, sql: &str) -> Result<u64, QueryError> {
        let n: i64 = db.conn.query_row(sql, [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }

    pub(crate) fn run_compact(&self, now: TimestampMs) -> Result<CompactReport, QueryError> {
        let _lock = self.lock()?;
        let mut report = CompactReport { recovered: self.recover_staging()?, ..Default::default() };
        let mut db = self.maint_db()?;
        for def in TABLES {
            for p in layout::list_partitions(self.root(), def)? {
                // Raw/bench hours are merged once sealed; any partition still holding legacy
                // Parquet (rollups included) is converted to Vortex.
                let merge = def.granularity() == Granularity::Hour && p.key.is_sealed(now) && p.files.len() > 1;
                if p.key.is_sealed(now) {
                    remove_stale_tmp(&p.dir)?;
                }
                if !merge && !p.has_legacy() {
                    continue;
                }
                let mut op = self.begin_op()?;
                let files: Vec<PathBuf> = p.files.iter().map(|f| f.path.clone()).collect();
                let src = db.relation(def, &files)?;
                let expected = Self::count(&db, &format!("SELECT count(*) FROM {src}"))?;
                let staged = op.staged(self.root(), &p.dir);
                let rows = self.copy_to(
                    &db,
                    def,
                    &format!("SELECT * FROM {src} ORDER BY {}", order_by(def)),
                    &staged,
                    Some(expected),
                )?;
                let manifest = p.dir.join(ROLLUP_MANIFEST);
                if manifest.exists() {
                    fs::copy(&manifest, staged.join(ROLLUP_MANIFEST))?;
                }
                let after: u64 = layout::read_dir_names(&staged)?
                    .iter()
                    .filter_map(|n| fs::metadata(staged.join(n)).ok())
                    .map(|m| m.len())
                    .sum();
                self.swap_in(&staged, &p.dir, &mut op)?;
                report.partitions += 1;
                report.files_before += p.files.len();
                report.files_after += 1;
                report.bytes_before += p.bytes();
                report.bytes_after += after;
                report.rows += rows;
            }
        }
        for raw in TABLES.iter().filter(|t| t.kind == TableKind::Raw) {
            for rollup in raw.rollups() {
                let (parts, hours) = self.refresh_rollup(&mut db, raw, rollup, now)?;
                report.rollup_partitions += parts;
                report.rollup_hours += hours;
            }
        }
        Ok(report)
    }

    /// Recomputes the rollup rows of every sealed raw hour whose source files changed since
    /// the rollup was last computed. Idempotent: an hour's rollup rows are replaced, never
    /// appended. Returns (day partitions rewritten, hours recomputed).
    fn refresh_rollup(
        &self,
        db: &mut Db,
        raw: &TableDef,
        rollup: &TableDef,
        now: TimestampMs,
    ) -> Result<(usize, usize), QueryError> {
        let mut days: BTreeMap<PartitionKey, Vec<Partition>> = BTreeMap::new();
        for p in layout::list_partitions(self.root(), raw)? {
            if p.key.is_sealed(now) && !p.files.is_empty() {
                days.entry(p.key.with_granularity(Granularity::Day)).or_default().push(p);
            }
        }
        let existing: BTreeMap<PartitionKey, Partition> =
            layout::list_partitions(self.root(), rollup)?.into_iter().map(|p| (p.key, p)).collect();
        let (mut parts, mut hours) = (0, 0);
        for (day, raw_hours) in days {
            let current = existing.get(&day);
            let mut manifest = current.map(|p| read_manifest(&p.dir)).unwrap_or_default();
            let stale: Vec<&Partition> =
                raw_hours.iter().filter(|h| manifest.get(&hour_label(&h.key)) != Some(&file_names(h))).collect();
            if stale.is_empty() {
                continue;
            }
            let raw_files: Vec<PathBuf> = stale.iter().flat_map(|h| h.files.iter().map(|f| f.path.clone())).collect();
            let fresh = rollup_select(rollup, &db.relation(raw, &raw_files)?);
            let body = match current.filter(|p| !p.files.is_empty()) {
                Some(cur) => {
                    let old: Vec<PathBuf> = cur.files.iter().map(|f| f.path.clone()).collect();
                    let stale_hours: Vec<String> =
                        stale.iter().map(|h| (h.key.start_ms() / 3_600_000).to_string()).collect();
                    format!(
                        "SELECT * FROM {} WHERE {} NOT IN ({}) UNION ALL BY NAME {fresh}",
                        db.relation(rollup, &old)?,
                        epoch_hour_expr(),
                        stale_hours.join(", ")
                    )
                }
                None => fresh,
            };
            for h in &stale {
                manifest.insert(hour_label(&h.key), file_names(h));
            }
            let live = layout::partition_dir(self.root(), rollup.name, &day);
            let mut op = self.begin_op()?;
            let staged = op.staged(self.root(), &live);
            self.copy_to(db, rollup, &format!("SELECT * FROM ({body}) ORDER BY {}", order_by(rollup)), &staged, None)?;
            write_manifest(&staged, &manifest, self.file_mode())?;
            self.swap_in(&staged, &live, &mut op)?;
            parts += 1;
            hours += stale.len();
        }
        Ok((parts, hours))
    }

    pub(crate) fn run_prune(&self, retention: &Retention, now: TimestampMs) -> Result<PruneReport, QueryError> {
        let _lock = self.lock()?;
        self.recover_staging()?;
        let mut op = self.begin_op()?;
        let mut stats: BTreeMap<&'static str, TablePrune> = BTreeMap::new();
        let mut all: Vec<Partition> = Vec::new();
        for def in TABLES {
            all.extend(layout::list_partitions(self.root(), def)?);
        }
        let bytes_before: u64 = all.iter().map(Partition::bytes).sum();
        let mut total = bytes_before;
        let mut kept = Vec::new();
        for p in all {
            let def = table(p.table).expect("listed table");
            let ttl = match def.kind {
                TableKind::Raw => Some(retention.raw),
                TableKind::Rollup { bucket: Bucket::Minute, .. } => Some(retention.rollup_1m),
                TableKind::Rollup { bucket: Bucket::Hour, .. } => Some(retention.rollup_1h),
                TableKind::Bench => None,
            };
            let cutoff = ttl.map(|t| now.saturating_sub(i64::try_from(t.as_millis()).unwrap_or(i64::MAX)));
            if cutoff.is_some_and(|c| p.key.end_ms() <= c) {
                self.remove_partition(&p.dir, &mut op)?;
                let s =
                    stats.entry(p.table).or_insert_with(|| TablePrune { table: p.table.into(), ..Default::default() });
                s.expired += 1;
                s.bytes += p.bytes();
                total -= p.bytes();
            } else {
                kept.push(p);
            }
        }
        if retention.max_size > 0 && total > retention.max_size {
            // Oldest raw partitions go first, then 1m rollups, then 1h rollups. Benchmark
            // data and partitions that are still being written are never evicted.
            let tier = |p: &Partition| match table(p.table).map(|t| t.kind) {
                Some(TableKind::Raw) => Some(0),
                Some(TableKind::Rollup { bucket: Bucket::Minute, .. }) => Some(1),
                Some(TableKind::Rollup { bucket: Bucket::Hour, .. }) => Some(2),
                _ => None,
            };
            let mut candidates: Vec<(u8, &Partition)> =
                kept.iter().filter(|p| p.key.is_sealed(now)).filter_map(|p| tier(p).map(|t| (t, p))).collect();
            candidates.sort_by_key(|(t, p)| (*t, p.key.start_ms(), p.table));
            for (_, p) in candidates {
                if total <= retention.max_size {
                    break;
                }
                self.remove_partition(&p.dir, &mut op)?;
                let s =
                    stats.entry(p.table).or_insert_with(|| TablePrune { table: p.table.into(), ..Default::default() });
                s.evicted += 1;
                s.bytes += p.bytes();
                total -= p.bytes();
            }
        }
        Ok(PruneReport { tables: stats.into_values().collect(), bytes_before, bytes_after: total })
    }

    pub(crate) fn run_delete(&self, req: &DeleteRequest) -> Result<DeleteReport, QueryError> {
        check_predicate(&req.predicate)?;
        let explicit: BTreeSet<&str> = req.tables.iter().map(String::as_str).collect();
        for t in &explicit {
            if table(t).is_none() {
                return Err(QueryError::Invalid(format!("unknown table '{t}'")));
            }
        }
        let mut pred = format!("({})", req.predicate);
        if let Some(s) = req.since {
            pred.push_str(&format!(" AND ts >= {}", ts_lit(s)));
        }
        if let Some(u) = req.until {
            pred.push_str(&format!(" AND ts < {}", ts_lit(u)));
        }
        let _lock = self.lock()?;
        self.recover_staging()?;
        let mut db = self.maint_db()?;
        let mut report = DeleteReport { dry_run: req.dry_run, ..Default::default() };

        // Phase 1: validate the predicate against every table and count matches per file,
        // before anything is modified.
        let mut plan: Vec<(&TableDef, Vec<(Partition, u64)>)> = Vec::new();
        let mut bound_any = false;
        for def in TABLES.iter().filter(|t| explicit.is_empty() || explicit.contains(t.name)) {
            let parts: Vec<Partition> = layout::list_partitions(self.root(), def)?
                .into_iter()
                .filter(|p| !p.files.is_empty())
                .filter(|p| {
                    req.since.is_none_or(|s| p.key.end_ms() > s) && req.until.is_none_or(|u| p.key.start_ms() < u)
                })
                .collect();
            // Bind against the typed empty view first so tables without data still
            // validate the predicate's syntax and columns.
            let probe = format!("SELECT count(*) FROM ({}) WHERE {pred}", engine::typed_empty(def, false));
            if let Err(e) = Self::count(&db, &probe) {
                let msg = e.to_string();
                if msg.contains("Binder Error") && !explicit.contains(def.name) {
                    report.skipped.push((def.name.to_string(), msg.lines().next().unwrap_or("").to_string()));
                    continue;
                }
                return Err(e);
            }
            bound_any = true;
            if parts.is_empty() {
                continue;
            }
            // Count matches per partition; only partitions with matches are rewritten.
            let mut affected: Vec<(Partition, u64)> = Vec::new();
            for p in parts {
                let files: Vec<PathBuf> = p.files.iter().map(|f| f.path.clone()).collect();
                let src = db.relation(def, &files)?;
                let n = Self::count(&db, &format!("SELECT count(*) FROM {src} WHERE {pred}"))?;
                if n > 0 {
                    affected.push((p, n));
                }
            }
            if !affected.is_empty() {
                plan.push((def, affected));
            }
        }
        if !bound_any {
            return Err(QueryError::Invalid(format!(
                "the predicate does not apply to any table: {}",
                report.skipped.first().map_or("", |s| s.1.as_str())
            )));
        }

        // Phase 2: rewrite or remove the affected partitions.
        for (def, affected) in plan {
            let mut td = TableDelete { table: def.name.into(), ..Default::default() };
            for (p, matched) in affected {
                td.rows += matched;
                if req.dry_run {
                    continue;
                }
                let files: Vec<PathBuf> = p.files.iter().map(|f| f.path.clone()).collect();
                let src = db.relation(def, &files)?;
                let total = Self::count(&db, &format!("SELECT count(*) FROM {src}"))?;
                let mut op = self.begin_op()?;
                if matched >= total {
                    self.remove_partition(&p.dir, &mut op)?;
                    td.partitions_removed += 1;
                    continue;
                }
                let staged = op.staged(self.root(), &p.dir);
                let select = format!("SELECT * FROM {src} WHERE ({pred}) IS NOT TRUE ORDER BY {}", order_by(def));
                self.copy_to(&db, def, &select, &staged, Some(total - matched))?;
                let manifest = p.dir.join(ROLLUP_MANIFEST);
                if manifest.exists() {
                    fs::copy(&manifest, staged.join(ROLLUP_MANIFEST))?;
                }
                self.swap_in(&staged, &p.dir, &mut op)?;
                td.partitions_rewritten += 1;
            }
            report.tables.push(td);
        }
        Ok(report)
    }

    pub(crate) fn run_purge(&self) -> Result<PurgeReport, QueryError> {
        let _lock = self.lock()?;
        let mut report = PurgeReport::default();
        let mut op = self.begin_op()?;
        for def in TABLES {
            for p in layout::list_partitions(self.root(), def)? {
                report.files += p.files.len();
                report.bytes += p.bytes();
            }
            let dir = layout::table_dir(self.root(), def.name);
            if dir.exists() {
                let trash = op.next_trash();
                layout::create_dir_all(trash.parent().unwrap_or(&op.dir), self.dir_mode())?;
                fs::rename(&dir, &trash)?;
            }
        }
        layout::fsync_dir(self.root())?;
        drop(op);
        // Anything else in staging belongs to crashed operations of the purged data.
        match fs::remove_dir_all(self.staging()) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        Ok(report)
    }
}

fn hour_label(key: &PartitionKey) -> String {
    format!("{:02}", key.hour.unwrap_or(0))
}

fn file_names(p: &Partition) -> Vec<String> {
    p.files.iter().filter_map(|f| f.path.file_name().map(|n| n.to_string_lossy().into_owned())).collect()
}

fn read_manifest(dir: &Path) -> BTreeMap<String, Vec<String>> {
    fs::read(dir.join(ROLLUP_MANIFEST)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_manifest(dir: &Path, m: &BTreeMap<String, Vec<String>>, mode: u32) -> Result<(), QueryError> {
    let path = dir.join(ROLLUP_MANIFEST);
    fs::write(&path, serde_json::to_vec_pretty(m).unwrap_or_default())?;
    fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
    fs::File::open(&path)?.sync_all()?;
    Ok(())
}

/// Removes `.tmp` files a crashed writer left behind. Only called under the exclusive
/// lock, when no writer can be mid-write.
fn remove_stale_tmp(dir: &Path) -> Result<(), QueryError> {
    for name in layout::read_dir_names(dir)? {
        if name.starts_with('.') && name.ends_with(".tmp") {
            match fs::remove_file(dir.join(&name)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicates() {
        assert!(check_predicate("app_id = 'org.mozilla.firefox'").is_ok());
        assert!(check_predicate("unit LIKE '%;%' AND (pid = 1 OR pid = 2)").is_ok());
        for bad in ["", "1=1; drop table x", "1=1) OR (1=1", "x = 1 -- c", "x /* y */", "(x = 1", "x = 'a"] {
            assert!(check_predicate(bad).is_err(), "{bad}");
        }
    }
}
