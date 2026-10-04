//! The versioned change store.
//!
//! Layout of a store directory:
//!
//! ```text
//! .lock              flock: serializes every mutation, across handles and processes
//! HEAD               latest changeset id
//! 000001.json        one record per changeset (append-only; status updated in place)
//! config/000000.toml config file before the first tracked config edit
//! config/000003.toml config file after changeset 3 (written for config-editing changesets)
//! ```
//!
//! Write-ahead: a changeset is written as `applying` (with every op's previous value)
//! before the first write to the system, and each op is marked `done` after its write.
//! [`ChangeStore::recover`] rolls back any `applying` changeset left by a crash.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lindesk_core::clock::{Clock, SystemClock};
use lindesk_plugin_api::{Event, TimestampMs};
use serde_json::Value;

use crate::apply::{Applier, RealApplier};
use crate::guard::{self, GuardContext, Limits};
use crate::model::{
    Author, ChangeOp, ChangeSet, OpRecord, Origin, Plan, PlannedOp, Rejection, Risk, Rule, Scope, Status, keys_overlap,
};
use crate::roots::{DirLock, atomic_write, io_err, read_opt};
use crate::{ChangeError, Result};

type Sink = Arc<dyn Fn(&Event) + Send + Sync>;

/// How `revert` treats ops that later changesets touched again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConflictMode {
    Refuse,
    Force,
    /// Revert only the ops nobody touched since (dead-man expiry).
    Skip,
}

/// A directory of versioned changesets. All methods take `&self`; mutations hold an
/// exclusive file lock, so a store can be shared (`Arc`) between threads and several
/// handles/processes can use the same directory.
pub struct ChangeStore {
    dir: PathBuf,
    scope: Scope,
    applier: Arc<dyn Applier>,
    clock: Arc<dyn Clock>,
    limits: Limits,
    sink: Option<Sink>,
}

impl std::fmt::Debug for ChangeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangeStore").field("dir", &self.dir).field("scope", &self.scope).finish_non_exhaustive()
    }
}

impl ChangeStore {
    /// Open (creating) a store. Uses [`RealApplier::host`] for `scope` and the system
    /// clock; replace them with [`Self::with_applier`] / [`Self::with_clock`].
    pub fn open(dir: impl Into<PathBuf>, scope: Scope) -> Result<ChangeStore> {
        let dir = dir.into();
        std::fs::create_dir_all(dir.join("config")).map_err(|e| io_err(&dir, e))?;
        Ok(ChangeStore {
            dir,
            scope,
            applier: Arc::new(RealApplier::host(scope)),
            clock: Arc::new(SystemClock),
            limits: Limits::default(),
            sink: None,
        })
    }

    /// `/var/lib/lindesk/changes` (system) or `$XDG_STATE_HOME/lindesk/changes` (user).
    pub fn default_dir(scope: Scope) -> Option<PathBuf> {
        match scope {
            Scope::System => Some("/var/lib/lindesk/changes".into()),
            Scope::User => std::env::var_os("XDG_STATE_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
                .map(|b| b.join("lindesk/changes")),
        }
    }

    pub fn with_applier(mut self, applier: Arc<dyn Applier>) -> Self {
        self.applier = applier;
        self
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Called with every `change.*` notice (after the record is on disk).
    pub fn on_event(mut self, f: impl Fn(&Event) + Send + Sync + 'static) -> Self {
        self.sink = Some(Arc::new(f));
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn scope(&self) -> Scope {
        self.scope
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    fn emit(&self, cs: &ChangeSet, name: &str) {
        if let Some(s) = &self.sink {
            s(&cs.notice(name));
        }
    }

    fn lock(&self) -> Result<DirLock> {
        DirLock::acquire(&self.dir)
    }

    fn record_path(&self, id: u64) -> PathBuf {
        self.dir.join(format!("{id:06}.json"))
    }

    fn write_record(&self, cs: &ChangeSet) -> Result<()> {
        let mut text = serde_json::to_string_pretty(cs).map_err(|e| ChangeError::Apply(e.to_string()))?;
        text.push('\n');
        atomic_write(&self.record_path(cs.id), text.as_bytes(), 0o600)
    }

    fn ids(&self) -> Result<Vec<u64>> {
        let mut ids: Vec<u64> = std::fs::read_dir(&self.dir)
            .map_err(|e| io_err(&self.dir, e))?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name();
                let name = name.to_str()?;
                let stem = name.strip_suffix(".json")?;
                (stem.len() >= 6 && stem.bytes().all(|b| b.is_ascii_digit())).then(|| stem.parse().ok())?
            })
            .collect();
        ids.sort_unstable();
        Ok(ids)
    }

    /// Latest changeset id (0 when empty).
    pub fn head(&self) -> Result<u64> {
        let head = read_opt(&self.dir.join("HEAD"))?.and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        Ok(head.max(self.ids()?.last().copied().unwrap_or(0)))
    }

    pub fn get(&self, id: u64) -> Result<ChangeSet> {
        let p = self.record_path(id);
        let text = read_opt(&p)?.ok_or(ChangeError::NotFound(id))?;
        serde_json::from_str(&text).map_err(|e| ChangeError::Corrupt { path: p, message: e.to_string() })
    }

    /// Newest first.
    pub fn history(&self, limit: usize) -> Result<Vec<ChangeSet>> {
        self.ids()?.into_iter().rev().take(limit).map(|id| self.get(id)).collect()
    }

    fn all(&self) -> Result<Vec<ChangeSet>> {
        self.ids()?.into_iter().map(|id| self.get(id)).collect()
    }

    /// The earliest pending dead-man deadline (to arm a timer for [`Self::expire_due`]).
    pub fn next_expiry(&self) -> Result<Option<TimestampMs>> {
        Ok(self.all()?.into_iter().filter(|c| c.status == Status::Applied).filter_map(|c| c.ttl_until).min())
    }

    fn recent_count(&self, origin: Origin, now: TimestampMs) -> Result<usize> {
        let window = self.limits.rate_window.as_millis() as i64;
        Ok(self
            .all()?
            .iter()
            .filter(|c| c.origin == origin && !c.is_revert() && c.ts > now - window && c.ts <= now)
            .count())
    }

    // ── plan ──────────────────────────────────────────────────────────────────────────

    /// Validate `ops` and show what they would do. No side effects.
    pub fn plan(&self, ops: &[ChangeOp], ctx: &GuardContext) -> Plan {
        let mut rejections = Vec::new();
        if ops.is_empty() {
            rejections.push(Rejection::new(None, Rule::Empty, "no ops"));
        }
        if ops.len() > self.limits.max_ops {
            rejections.push(Rejection::new(
                None,
                Rule::TooManyOps,
                format!("{} ops; at most {} per changeset", ops.len(), self.limits.max_ops),
            ));
        }
        if ctx.scope != self.scope {
            rejections.push(Rejection::new(
                None,
                Rule::Scope,
                format!("guard context is {} scope but the store is {}", ctx.scope, self.scope),
            ));
        }
        let now = self.clock.now();
        if let Ok(n) = self.recent_count(ctx.origin, now)
            && n >= self.limits.rate_count
        {
            rejections.push(rate_rejection(&self.limits, ctx.origin));
        }
        let mut risk = Risk::Low;
        let mut planned = Vec::new();
        let mut runtime_diff = String::new();
        let mut valid = vec![true; ops.len()];
        for (i, op) in ops.iter().enumerate() {
            let op_risk = match guard::validate(op, ctx) {
                Ok(r) => r,
                Err(r) => {
                    rejections.push(Rejection { index: Some(i), ..r });
                    valid[i] = false;
                    op.risk()
                }
            };
            risk = risk.max(op_risk);
            let current =
                if op.scope() == self.scope { self.applier.read(op).unwrap_or(Value::Null) } else { Value::Null };
            let target = op.target_value();
            if !op.is_config() {
                runtime_diff.push_str(&runtime_diff_block(op, &current, &target));
            }
            planned.push(PlannedOp { op: op.clone(), risk: op_risk, current, target });
        }
        // The valid config ops together, in order: interactions (e.g. [safety] min/max) and
        // the combined diff.
        let config_ops: Vec<(usize, &ChangeOp)> =
            ops.iter().enumerate().filter(|(i, o)| valid[*i] && o.is_config() && o.scope() == ctx.scope).collect();
        let mut diff = String::new();
        if !config_ops.is_empty() && ctx.scope == self.scope {
            let check = guard::check_config(&config_ops, ctx);
            for r in check.rejections {
                if !rejections.contains(&r) {
                    rejections.push(r);
                }
            }
            let label = ctx.config_paths.get(ctx.scope).map(|p| p.display().to_string()).unwrap_or_default();
            diff.push_str(&crate::diff::unified(
                &check.before,
                &check.after,
                &format!("a{label}"),
                &format!("b{label}"),
                3,
            ));
        }
        diff.push_str(&runtime_diff);
        rejections.sort_by_key(|r| r.index.unwrap_or(usize::MAX));
        rejections.dedup();
        Plan {
            scope: self.scope,
            ops: planned,
            risk,
            diff,
            rejections,
            ttl_required: ctx.origin == Origin::Mcp && risk == Risk::High,
            validated: true,
        }
    }

    // ── apply ─────────────────────────────────────────────────────────────────────────

    /// Apply a plan from [`Self::plan`]. `ttl`: auto-revert unless confirmed in time
    /// (`None` = never; required for High risk from MCP). On a write failure every op
    /// already written is rolled back and `Err(ApplyFailed)` carries the failed changeset.
    pub fn apply(&self, plan: &Plan, author: Author, description: &str, ttl: Option<Duration>) -> Result<ChangeSet> {
        if !plan.validated {
            return Err(ChangeError::NotPlanned);
        }
        if !plan.rejections.is_empty() {
            return Err(ChangeError::Rejected(plan.rejections.clone()));
        }
        if plan.scope != self.scope {
            return Err(ChangeError::Rejected(vec![Rejection::new(None, Rule::Scope, "plan is for another store")]));
        }
        let ttl = ttl.filter(|t| !t.is_zero());
        if author.origin == Origin::Mcp && plan.risk == Risk::High && ttl.is_none() {
            return Err(ChangeError::Rejected(vec![Rejection::new(
                None,
                Rule::TtlRequired,
                "high-risk changes from MCP need a ttl (they auto-revert unless confirmed)",
            )]));
        }
        if let Some(t) = ttl
            && t > self.limits.max_ttl
        {
            return Err(ChangeError::Rejected(vec![Rejection::new(
                None,
                Rule::OutOfRange,
                format!(
                    "ttl {} exceeds {}",
                    humantime::format_duration(t),
                    humantime::format_duration(self.limits.max_ttl)
                ),
            )]));
        }
        let _lock = self.lock()?;
        let now = self.clock.now();
        if self.recent_count(author.origin, now)? >= self.limits.rate_count {
            return Err(ChangeError::Rejected(vec![rate_rejection(&self.limits, author.origin)]));
        }
        // Previous values: read now; an op on a place an earlier op of this changeset
        // already wrote gets that op's value.
        let mut ops: Vec<OpRecord> = Vec::with_capacity(plan.ops.len());
        for p in &plan.ops {
            let previous = self.previous_for(&ops, &p.op)?;
            ops.push(OpRecord { op: p.op.clone(), previous, applied: p.op.target_value(), done: false });
        }
        let cs = self.new_changeset(author, description, plan.risk, ops, now, ttl, None)?;
        self.execute(cs)
    }

    /// The value at `op`'s place just before it is written as part of `so_far`: the last
    /// value written there by an earlier op of the same changeset, else the system's.
    fn previous_for(&self, so_far: &[OpRecord], op: &ChangeOp) -> Result<Value> {
        match so_far.iter().rev().find(|r| same_place(&r.op, op)) {
            Some(r) => Ok(r.applied.clone()),
            None => self.applier.read(op),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn new_changeset(
        &self,
        author: Author,
        description: &str,
        risk: Risk,
        ops: Vec<OpRecord>,
        now: TimestampMs,
        ttl: Option<Duration>,
        reverts: Option<u64>,
    ) -> Result<ChangeSet> {
        let head = self.head()?;
        Ok(ChangeSet {
            id: head + 1,
            parent: (head > 0).then_some(head),
            ts: now,
            updated: now,
            origin: author.origin,
            author,
            scope: self.scope,
            description: description.to_string(),
            risk,
            ops,
            status: Status::Applying,
            ttl_until: ttl.map(|t| now + t.as_millis() as i64),
            reverts,
            revert_to: None,
            reverted_by: None,
            error: None,
            config_snapshot: None,
        })
    }

    /// WAL → write ops → applied (or roll back → failed). Caller holds the lock.
    fn execute(&self, mut cs: ChangeSet) -> Result<ChangeSet> {
        let touches_config = cs.ops.iter().any(|o| o.op.is_config());
        let config_before = if touches_config { self.config_text()? } else { None };
        self.write_record(&cs)?;
        atomic_write(&self.dir.join("HEAD"), format!("{}\n", cs.id).as_bytes(), 0o644)?;
        for k in 0..cs.ops.len() {
            let res = self.applier.write(&cs.ops[k].op, &cs.ops[k].applied);
            match res {
                Ok(()) => {
                    cs.ops[k].done = true;
                    self.write_record(&cs)?;
                }
                Err(e) => {
                    let mut msg = format!("op {k} ({}): {e}", cs.ops[k].op);
                    let errs = self.roll_back(&mut cs, k);
                    if !errs.is_empty() {
                        msg.push_str(&format!("; rollback errors: {}", errs.join("; ")));
                    }
                    cs.status = Status::Failed;
                    cs.error = Some(msg);
                    cs.updated = self.clock.now();
                    self.write_record(&cs)?;
                    self.emit(&cs, "change.failed");
                    return Err(ChangeError::ApplyFailed(Box::new(cs)));
                }
            }
        }
        if touches_config {
            self.snapshot_config(&mut cs, config_before)?;
        }
        cs.status = Status::Applied;
        cs.updated = self.clock.now();
        self.write_record(&cs)?;
        if !cs.is_revert() {
            self.emit(&cs, "change.applied");
        }
        Ok(cs)
    }

    /// Write back `previous` for ops `upto..=0` that were (or may have been) written:
    /// every `done` op plus op `upto` itself. Returns the errors (rollback continues).
    fn roll_back(&self, cs: &mut ChangeSet, upto: usize) -> Vec<String> {
        let mut errs = Vec::new();
        for k in (0..=upto.min(cs.ops.len().saturating_sub(1))).rev() {
            if k != upto && !cs.ops[k].done {
                continue;
            }
            if let Err(e) = self.applier.write(&cs.ops[k].op, &cs.ops[k].previous) {
                errs.push(format!("op {k}: {e}"));
            }
            cs.ops[k].done = false;
        }
        errs
    }

    fn config_text(&self) -> Result<Option<String>> {
        match self.applier.config_path(self.scope) {
            Some(p) => Ok(Some(read_opt(&p)?.unwrap_or_default())),
            None => Ok(None),
        }
    }

    fn snapshot_config(&self, cs: &mut ChangeSet, before: Option<String>) -> Result<()> {
        let Some(after) = self.config_text()? else { return Ok(()) };
        let dir = self.dir.join("config");
        let baseline = dir.join("000000.toml");
        if !baseline.exists() {
            atomic_write(&baseline, before.unwrap_or_default().as_bytes(), 0o600)?;
        }
        let name = format!("{:06}.toml", cs.id);
        atomic_write(&dir.join(&name), after.as_bytes(), 0o600)?;
        cs.config_snapshot = Some(format!("config/{name}"));
        Ok(())
    }

    // ── confirm / revert ──────────────────────────────────────────────────────────────

    /// Keep a changeset: cancels its dead-man ttl. Idempotent.
    pub fn confirm(&self, id: u64) -> Result<ChangeSet> {
        let _lock = self.lock()?;
        let mut cs = self.get(id)?;
        match cs.status {
            Status::Confirmed => Ok(cs),
            Status::Applied => {
                cs.status = Status::Confirmed;
                cs.updated = self.clock.now();
                self.write_record(&cs)?;
                self.emit(&cs, "change.confirmed");
                Ok(cs)
            }
            Status::Reverted => Err(ChangeError::AlreadyReverted { id, by: cs.reverted_by }),
            s => Err(ChangeError::InvalidState {
                id,
                status: s,
                message: "only applied changesets can be confirmed".into(),
            }),
        }
    }

    /// Undo a changeset by appending a new one that restores its previous values. Refused
    /// when a later applied changeset touched the same keys, unless `force` (which
    /// overwrites those later values too); [`Self::revert_to`] undoes them in order.
    pub fn revert(&self, id: u64, author: Author, force: bool) -> Result<ChangeSet> {
        let _lock = self.lock()?;
        self.revert_locked(id, author, if force { ConflictMode::Force } else { ConflictMode::Refuse })
    }

    fn revert_locked(&self, id: u64, author: Author, mode: ConflictMode) -> Result<ChangeSet> {
        let mut target = self.get(id)?;
        match target.status {
            Status::Applied | Status::Confirmed => {}
            Status::Reverted => return Err(ChangeError::AlreadyReverted { id, by: target.reverted_by }),
            s => {
                return Err(ChangeError::InvalidState {
                    id,
                    status: s,
                    message: "only applied changesets can be reverted".into(),
                });
            }
        }
        let later = self.later_effective(id)?;
        let mut conflicts: Vec<(u64, String)> = Vec::new();
        let mut skip = vec![false; target.ops.len()];
        for (k, rec) in target.ops.iter().enumerate() {
            for key in rec.op.keys() {
                for l in &later {
                    if l.keys().iter().any(|lk| keys_overlap(lk, &key)) {
                        conflicts.push((l.id, key.clone()));
                        skip[k] = true;
                    }
                }
            }
        }
        if !conflicts.is_empty() && mode == ConflictMode::Refuse {
            let mut later_ids: Vec<u64> = conflicts.iter().map(|c| c.0).collect();
            later_ids.sort_unstable();
            later_ids.dedup();
            let mut keys: Vec<String> = conflicts.into_iter().map(|c| c.1).collect();
            keys.sort();
            keys.dedup();
            return Err(ChangeError::Conflict { id, later: later_ids, keys });
        }
        let now = self.clock.now();
        let mut ops = Vec::new();
        for (k, rec) in target.ops.iter().enumerate().rev() {
            if mode == ConflictMode::Skip && skip[k] {
                continue;
            }
            let current = self.previous_for(&ops, &rec.op)?;
            // Describe the restore as an op where possible; `applied` is what gets written.
            let mut op = rec.op.with_value(&rec.previous);
            if !same_place(&op, &rec.op) {
                op = rec.op.clone();
            }
            ops.push(OpRecord { op, previous: current, applied: rec.previous.clone(), done: false });
        }
        let description = format!(
            "revert {id}{}",
            if target.description.is_empty() { String::new() } else { format!(": {}", target.description) }
        );
        let risk = target.risk;
        let cs = self.new_changeset(author, &description, risk, ops, now, None, Some(id))?;
        let cs = self.execute(cs)?;
        target.status = Status::Reverted;
        target.reverted_by = Some(cs.id);
        target.updated = self.clock.now();
        self.write_record(&target)?;
        self.emit(&cs, "change.reverted");
        Ok(cs)
    }

    /// Changesets after `id` whose values are still in effect, ignoring revert pairs that
    /// both lie after `id` (they cancel out).
    fn later_effective(&self, id: u64) -> Result<Vec<ChangeSet>> {
        Ok(self
            .all()?
            .into_iter()
            .filter(|c| c.id > id && c.is_effective() && c.reverts.is_none_or(|t| t <= id))
            .collect())
    }

    /// Return to the state at `version` by unwinding every later changeset, newest first:
    /// one new changeset writes back the recorded `previous` of every op applied after
    /// `version` (including reverts), in reverse order, which restores exactly the values
    /// lindesk saw at `version`. Every later changeset still in effect becomes `reverted`.
    /// Returns `None` when there is nothing to undo.
    pub fn revert_to(&self, version: u64, author: Author) -> Result<Option<ChangeSet>> {
        let _lock = self.lock()?;
        let later: Vec<ChangeSet> = self
            .all()?
            .into_iter()
            .filter(|c| c.id > version && matches!(c.status, Status::Applied | Status::Confirmed | Status::Reverted))
            .rev()
            .collect();
        if !later.iter().any(|c| c.is_effective()) {
            return Ok(None);
        }
        let mut ops: Vec<OpRecord> = Vec::new();
        for c in &later {
            for rec in c.ops.iter().rev() {
                let current = self.previous_for(&ops, &rec.op)?;
                let mut op = rec.op.with_value(&rec.previous);
                if !same_place(&op, &rec.op) {
                    op = rec.op.clone();
                }
                ops.push(OpRecord { op, previous: current, applied: rec.previous.clone(), done: false });
            }
        }
        let risk = later.iter().map(|c| c.risk).max().unwrap_or_default();
        let now = self.clock.now();
        let mut cs = self.new_changeset(author, &format!("revert to version {version}"), risk, ops, now, None, None)?;
        cs.revert_to = Some(version);
        let cs = self.execute(cs)?;
        for mut c in later.into_iter().filter(|c| c.is_effective()) {
            c.status = Status::Reverted;
            c.reverted_by = Some(cs.id);
            c.updated = self.clock.now();
            self.write_record(&c)?;
        }
        self.emit(&cs, "change.reverted");
        Ok(Some(cs))
    }

    /// Dead-man switch: revert every applied (unconfirmed) changeset whose ttl has passed,
    /// newest first. Ops that a later changeset changed again are left alone. Returns the
    /// expired changesets (now `reverted`, with `reverted_by` set).
    pub fn expire_due(&self, now: TimestampMs) -> Result<Vec<ChangeSet>> {
        let _lock = self.lock()?;
        let mut due: Vec<ChangeSet> = self
            .all()?
            .into_iter()
            .filter(|c| c.status == Status::Applied && c.ttl_until.is_some_and(|t| t <= now))
            .collect();
        due.sort_by_key(|c| std::cmp::Reverse(c.id));
        let mut out = Vec::new();
        for c in due {
            let author = Author::daemon("dead-man");
            match self.revert_locked(c.id, author, ConflictMode::Skip) {
                Ok(_) => {
                    let expired = self.get(c.id)?;
                    self.emit(&expired, "change.expired");
                    out.push(expired);
                }
                Err(e) => {
                    // Leave it applied; it will be retried on the next call.
                    let mut failed = c.clone();
                    failed.error = Some(format!("auto-revert failed: {e}"));
                    self.emit(&failed, "change.failed");
                }
            }
        }
        Ok(out)
    }

    /// Roll back changesets left `applying` by a crash (each becomes `failed`), and finish
    /// reverts that were applied but not yet linked. Call once at daemon startup, before
    /// serving requests. Returns the rolled-back changesets.
    pub fn recover(&self) -> Result<Vec<ChangeSet>> {
        let _lock = self.lock()?;
        let mut out = Vec::new();
        let mut all = self.all()?;
        all.sort_by_key(|c| std::cmp::Reverse(c.id));
        for mut cs in all.iter().filter(|c| c.status == Status::Applying).cloned() {
            // Roll back every done op and the first not-done one (it may be half-written).
            let inflight = cs.ops.iter().position(|o| !o.done).unwrap_or(cs.ops.len());
            let last = if inflight < cs.ops.len() { inflight } else { cs.ops.len().saturating_sub(1) };
            let errs = if cs.ops.is_empty() { Vec::new() } else { self.roll_back(&mut cs, last) };
            cs.status = Status::Failed;
            cs.error = Some(if errs.is_empty() {
                "interrupted; rolled back on recovery".into()
            } else {
                format!("interrupted; rollback errors: {}", errs.join("; "))
            });
            cs.updated = self.clock.now();
            self.write_record(&cs)?;
            self.emit(&cs, "change.failed");
            out.push(cs);
        }
        // Reverts that completed but whose targets weren't marked yet.
        let all = self.all()?;
        for r in all.iter().filter(|c| c.is_effective()) {
            if let Some(v) = r.revert_to {
                for mut c in all.iter().filter(|c| c.id > v && c.id < r.id && c.is_effective()).cloned() {
                    c.status = Status::Reverted;
                    c.reverted_by = Some(r.id);
                    c.updated = self.clock.now();
                    self.write_record(&c)?;
                }
            }
            if let Some(t) = r.reverts
                && let Ok(mut target) = self.get(t)
                && target.is_effective()
            {
                target.status = Status::Reverted;
                target.reverted_by = Some(r.id);
                target.updated = self.clock.now();
                self.write_record(&target)?;
            }
        }
        // Leftover temporary files from interrupted writes.
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                if e.file_name().to_str().is_some_and(|n| n.starts_with('.') && n.contains(".tmp-")) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        Ok(out)
    }

    // ── config snapshots ──────────────────────────────────────────────────────────────

    /// The config file as of version `id` (`None` when no config edit was ever tracked).
    pub fn config_at(&self, id: u64) -> Result<Option<String>> {
        let dir = self.dir.join("config");
        let mut best: Option<u64> = None;
        for e in std::fs::read_dir(&dir).map_err(|e| io_err(&dir, e))?.flatten() {
            let name = e.file_name();
            let Some(n) = name.to_str().and_then(|n| n.strip_suffix(".toml")).and_then(|n| n.parse::<u64>().ok())
            else {
                continue;
            };
            if n <= id && best.is_none_or(|b| n > b) {
                best = Some(n);
            }
        }
        match best {
            Some(n) => read_opt(&dir.join(format!("{n:06}.toml"))),
            None => Ok(None),
        }
    }

    /// Unified diff of the config file between versions `a` and `b`.
    pub fn diff(&self, a: u64, b: u64) -> Result<String> {
        let ta = self.config_at(a)?.unwrap_or_default();
        let tb = self.config_at(b)?.unwrap_or_default();
        Ok(crate::diff::unified(&ta, &tb, &format!("a/lindesk.toml@{a}"), &format!("b/lindesk.toml@{b}"), 3))
    }
}

fn rate_rejection(limits: &Limits, origin: Origin) -> Rejection {
    Rejection::new(
        None,
        Rule::RateLimited,
        format!(
            "rate limit: at most {} changesets per {} from {origin}",
            limits.rate_count,
            humantime::format_duration(limits.rate_window)
        ),
    )
}

/// Two ops designate the same place (same variant and location).
fn same_place(a: &ChangeOp, b: &ChangeOp) -> bool {
    use ChangeOp::*;
    match (a, b) {
        (
            ConfigSet { .. } | ConfigUnset { .. } | ProfileUse { .. },
            ConfigSet { .. } | ConfigUnset { .. } | ProfileUse { .. },
        ) => a.scope() == b.scope() && a.config_key() == b.config_key(),
        (ClassOverride { scope: s1, unit: u1, .. }, ClassOverride { scope: s2, unit: u2, .. }) => s1 == s2 && u1 == u2,
        (Sysctl { key: k1, persist: p1, .. }, Sysctl { key: k2, persist: p2, .. }) => k1 == k2 && p1 == p2,
        (IoScheduler { device: d1, persist: p1, .. }, IoScheduler { device: d2, persist: p2, .. }) => {
            d1 == d2 && p1 == p2
        }
        (Zswap { .. }, Zswap { .. }) => a.keys() == b.keys(),
        (CpuEpp { .. }, CpuEpp { .. }) | (CpuGovernor { .. }, CpuGovernor { .. }) => true,
        _ => false,
    }
}

fn runtime_diff_block(op: &ChangeOp, current: &Value, target: &Value) -> String {
    let label = match op {
        ChangeOp::Sysctl { key, .. } => format!("/proc/sys/{}", key.replace('.', "/")),
        ChangeOp::IoScheduler { device, .. } => format!("/sys/block/{device}/queue/scheduler"),
        ChangeOp::Zswap { .. } => "/sys/module/zswap/parameters".into(),
        ChangeOp::CpuEpp { .. } => "/sys/devices/system/cpu/cpufreq/policy*/energy_performance_preference".into(),
        ChangeOp::CpuGovernor { .. } => "/sys/devices/system/cpu/cpufreq/policy*/scaling_governor".into(),
        _ => op.kind().into(),
    };
    let lines = |v: &Value| -> Vec<String> {
        match v {
            Value::Null => vec![],
            Value::String(s) => vec![s.clone()],
            Value::Object(m) => m
                .iter()
                .filter(|(_, x)| !x.is_null())
                .map(|(k, x)| format!("{k}: {}", x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())))
                .collect(),
            other => vec![other.to_string()],
        }
    };
    // For per-policy maps vs a single target string, show the target per policy.
    let tgt = match (current, target) {
        (Value::Object(m), Value::String(s))
            if matches!(op, ChangeOp::CpuEpp { .. } | ChangeOp::CpuGovernor { .. }) =>
        {
            Value::Object(m.keys().map(|k| (k.clone(), Value::String(s.clone()))).collect())
        }
        _ => target.clone(),
    };
    let (a, b) = (lines(current), lines(&tgt));
    if a == b {
        return String::new();
    }
    let mut out = format!("--- a{label}\n+++ b{label}\n@@ {op} @@\n");
    for l in a.iter().filter(|l| !b.contains(l)) {
        out.push_str(&format!("-{l}\n"));
    }
    for l in b.iter().filter(|l| !a.contains(l)) {
        out.push_str(&format!("+{l}\n"));
    }
    out
}
