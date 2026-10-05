//! The telemetry lake through `lindesk-data`.
//!
//! lindeskctl links no DuckDB or Vortex: `query`, `data …`, `simulate` and the `events`
//! fallback run `lindesk-data` (the optional lindesk-telemetry package) with `--json` where
//! they need structured results, and render those exactly as before. Without it they print
//! [`NOT_INSTALLED`] and exit 1.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde_json::Value;

use crate::cli::BackendArg;
use crate::ui::{Exit, Ui};

pub use lindesk_sys::telemetry::NOT_INSTALLED;

/// What a DuckDB client must run before using the catalog's views.
pub const CATALOG_HINT: &str = "INSTALL vortex; LOAD vortex;";

/// Prints the not-installed message (no "error:" prefix: nothing went wrong) and yields
/// exit status 1.
pub fn not_installed(ui: &Ui) -> anyhow::Error {
    let eui = Ui { color: ui.err_color, ..ui.clone() };
    eprintln!("{}", eui.yellow(NOT_INSTALLED));
    Exit(1).into()
}

/// One `lindesk-data` invocation for a lake.
pub struct Data {
    bin: PathBuf,
    global: Vec<String>,
}

impl Data {
    /// `lindesk-data --root ROOT --config CONFIG --backend B`, or the not-installed exit.
    pub fn new(ui: &Ui, root: &Path, config: &Path, backend: BackendArg) -> Result<Data> {
        let bin = lindesk_sys::telemetry::data_binary().ok_or_else(|| not_installed(ui))?;
        let backend = match backend {
            BackendArg::Auto => "auto",
            BackendArg::Extension => "extension",
            BackendArg::Native => "native",
        };
        let global = vec![
            "--root".into(),
            root.display().to_string(),
            "--config".into(),
            config.display().to_string(),
            "--backend".into(),
            backend.into(),
        ];
        Ok(Data { bin, global })
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(&self.bin);
        c.args(&self.global).args(args).stdin(Stdio::null());
        c
    }

    /// Runs it and returns stdout. stderr is passed through on success (warnings); on
    /// failure its message becomes the error, so lindeskctl prints it the usual way.
    pub fn output(&self, args: &[&str]) -> Result<String> {
        let o = self
            .command(args)
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| anyhow!("running {}: {e}", self.bin.display()))?;
        let stderr = String::from_utf8_lossy(&o.stderr);
        if o.status.success() {
            if !stderr.trim().is_empty() {
                eprint!("{stderr}");
            }
            return Ok(String::from_utf8_lossy(&o.stdout).into_owned());
        }
        let msg = stderr.trim().strip_prefix("lindesk-data: ").unwrap_or(stderr.trim()).to_string();
        Err(anyhow!(if msg.is_empty() { format!("lindesk-data failed ({})", o.status) } else { msg }))
    }

    /// `--json` output, parsed as `T` (and kept raw, for `lindeskctl --json`).
    pub fn json<T: for<'de> Deserialize<'de>>(&self, args: &[&str]) -> Result<(T, Value)> {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        let out = self.output(&a)?;
        let raw: Value =
            serde_json::from_str(out.trim()).map_err(|e| anyhow!("lindesk-data printed invalid JSON: {e}"))?;
        let t = serde_json::from_value(raw.clone()).map_err(|e| anyhow!("unexpected lindesk-data output: {e}"))?;
        Ok((t, raw))
    }

    /// Replaces this process (`data shell`).
    pub fn exec(&self, args: &[&str]) -> anyhow::Error {
        use std::os::unix::process::CommandExt;
        let err = self.command(args).stdin(Stdio::inherit()).exec();
        anyhow!("running {}: {err}", self.bin.display())
    }

    /// `export --table T --from F --to T`: the window's rows, `ts` in epoch ms.
    pub fn export(&self, table: &str, from: i64, to: i64) -> Result<Vec<Value>> {
        let (f, t) = (from.to_string(), to.to_string());
        let out = self.output(&["export", "--table", table, "--from", &f, "--to", &t, "--format", "jsonl"])?;
        out.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).map_err(|e| anyhow!("lindesk-data export printed invalid JSON: {e}")))
            .collect()
    }
}

// The report shapes lindeskctl renders (lindesk-query's, fields as serialised).

#[derive(Deserialize, Debug)]
pub struct CompactReport {
    pub partitions: usize,
    pub files_before: usize,
    pub files_after: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub rollup_hours: usize,
}

#[derive(Deserialize, Debug)]
pub struct TablePrune {
    pub table: String,
    pub expired: usize,
    pub evicted: usize,
    pub bytes: u64,
}

#[derive(Deserialize, Debug)]
pub struct PruneReport {
    pub tables: Vec<TablePrune>,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

#[derive(Deserialize, Debug)]
pub struct TableStats {
    pub table: String,
    pub kind: String,
    pub partitions: usize,
    pub files: usize,
    pub rows: u64,
    pub bytes: u64,
    #[serde(default)]
    pub legacy_files: usize,
    pub oldest: Option<String>,
    pub newest: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct LakeStats {
    pub root: String,
    pub tables: Vec<TableStats>,
    pub bytes: u64,
    pub rows: u64,
}

#[derive(Deserialize, Debug)]
pub struct TableDelete {
    pub table: String,
    pub rows: u64,
    pub partitions_rewritten: usize,
    pub partitions_removed: usize,
}

#[derive(Deserialize, Debug)]
pub struct DeleteReport {
    pub tables: Vec<TableDelete>,
}

impl DeleteReport {
    pub fn rows(&self) -> u64 {
        self.tables.iter().map(|t| t.rows).sum()
    }
}

#[derive(Deserialize, Debug)]
pub struct PurgeReport {
    pub files: usize,
    pub bytes: u64,
}

#[derive(Deserialize, Debug)]
pub struct Catalog {
    pub catalog: PathBuf,
}
