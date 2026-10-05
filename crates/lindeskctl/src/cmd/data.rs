//! `lindeskctl query` and `lindeskctl data …`: the telemetry lake through `lindesk-data`
//! (the optional lindesk-telemetry package; see [`crate::telemetry`]).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use lindesk_core::config::Config;
use serde_json::json;

use crate::cli::{BackendArg, DataCmd, QueryArgs, QueryFormat};
use crate::cmd::doctor::tilde;
use crate::paths;
use crate::telemetry::{self, CATALOG_HINT, Data};
use crate::ui::{Table, Ui, bytes, confirm, out};

fn config(ui: &Ui) -> Option<Config> {
    paths::load_config(&paths::config_path(ui.system)).ok()
}

/// A lake: where it is and the `lindesk-data` that reads it.
pub struct Lake {
    root: PathBuf,
    pub data: Data,
}

impl Lake {
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// The lake for the scope (or an explicit directory), read with the default backend.
pub fn open_lake(ui: &Ui, explicit: Option<&Path>) -> Result<Lake> {
    open_lake_with(ui, explicit, BackendArg::Auto)
}

/// [`open_lake`] with a backend choice (`--backend`). Fails with the not-installed message
/// (exit 1) without `lindesk-data`.
pub fn open_lake_with(ui: &Ui, explicit: Option<&Path>, backend: BackendArg) -> Result<Lake> {
    let root = paths::lake_root(explicit, ui.system, config(ui).as_ref());
    let data = Data::new(ui, &root, &paths::config_path(ui.system), backend)?;
    if ui.system && root.exists() && std::fs::read_dir(&root).is_err() {
        bail!(
            "can't read the system lake at {}: it's readable by root and the lindesk group.\n  Join the group: sudo usermod -aG lindesk $USER (then log out and in), or use sudo.",
            root.display()
        );
    }
    Ok(Lake { root, data })
}

pub fn query(ui: &Ui, a: &QueryArgs) -> Result<()> {
    let lake = open_lake_with(ui, a.lake.as_deref(), a.backend)?;
    let fmt = if ui.json && a.format == QueryFormat::Table { QueryFormat::Json } else { a.format };
    let mut args = vec!["query".to_string(), a.sql.clone(), "--format".into()];
    let parquet = match fmt {
        QueryFormat::Table => {
            args.push("table".into());
            None
        }
        QueryFormat::Csv => {
            args.push("csv".into());
            None
        }
        QueryFormat::Json => {
            args.push("json".into());
            None
        }
        QueryFormat::Parquet => {
            let out = a.out.clone().ok_or_else(|| crate::ui::Usage("--format parquet needs --out FILE".into()))?;
            let out = std::path::absolute(out)?;
            args.extend(["parquet".into(), "--output".into(), out.display().to_string()]);
            Some(out)
        }
    };
    if !lake.root().is_dir() {
        ui.info(
            ui.dim(format!(
                "no telemetry yet in {} (tables are empty until lindesk records some).",
                tilde(lake.root())
            )),
        );
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = lake.data.output(&args)?;
    match (&parquet, &a.out) {
        (Some(p), _) => ui.info(format!("{} wrote {}", ui.ok_mark(), p.display())),
        (_, Some(path)) => {
            std::fs::write(path, &result).with_context(|| format!("writing {}", path.display()))?;
            ui.info(format!("{} wrote {}", ui.ok_mark(), path.display()));
        }
        _ => crate::ui::out_raw(&result),
    }
    Ok(())
}

/// Parse `--since`/`--until` and simulate times: RFC 3339, `YYYY-MM-DD[ HH:MM[:SS]]`
/// (local time), `now`, `-1h` / `1h` (that long ago), or epoch milliseconds.
pub fn parse_time(s: &str, now: i64) -> Result<i64> {
    use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
    let t = s.trim();
    if t.eq_ignore_ascii_case("now") {
        return Ok(now);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(t) {
        return Ok(dt.timestamp_millis());
    }
    for f in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(t, f)
            && let Some(l) = Local.from_local_datetime(&n).earliest()
        {
            return Ok(l.timestamp_millis());
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(t, "%Y-%m-%d")
        && let Some(l) = Local.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap_or_default()).earliest()
    {
        return Ok(l.timestamp_millis());
    }
    if t.len() >= 12 && t.chars().all(|c| c.is_ascii_digit()) {
        return Ok(t.parse()?);
    }
    let rel = t.strip_prefix('-').unwrap_or(t);
    let d = humantime::parse_duration(rel).map_err(|_| {
        anyhow!("'{s}' isn't a time: use RFC 3339, YYYY-MM-DD[ HH:MM], now, or a duration ago like -1h, -30m, 7d")
    })?;
    Ok(now - d.as_millis() as i64)
}

fn print_stats(ui: &Ui, s: &telemetry::LakeStats) {
    let mut t =
        Table::new(&["TABLE", "KIND", "PARTITIONS", "FILES", "ROWS", "SIZE", "OLDEST", "NEWEST"]).right(&[2, 3, 4, 5]);
    for x in &s.tables {
        if x.files == 0 && !ui.json {
            t.row(vec![
                ui.dim(&x.table),
                ui.dim(&x.kind),
                ui.dim("0"),
                ui.dim("0"),
                ui.dim("0"),
                ui.dim("-"),
                String::new(),
                String::new(),
            ]);
            continue;
        }
        t.row(vec![
            x.table.clone(),
            x.kind.clone(),
            x.partitions.to_string(),
            x.files.to_string(),
            x.rows.to_string(),
            bytes(x.bytes),
            x.oldest.clone().unwrap_or_default(),
            x.newest.clone().unwrap_or_default(),
        ]);
    }
    let mut o = format!(
        "{}  {}\n\n",
        ui.bold(tilde(Path::new(&s.root))),
        ui.dim(format!("{} rows, {}", s.rows, bytes(s.bytes)))
    );
    o.push_str(&t.render(ui));
    let legacy: usize = s.tables.iter().map(|x| x.legacy_files).sum();
    if legacy > 0 {
        o.push_str(&ui.yellow(format!(
            "\n{legacy} legacy parquet file{} (converted on next compact: lindeskctl data compact)",
            if legacy == 1 { "" } else { "s" }
        )));
    }
    out(&o);
}

fn now() -> i64 {
    lindesk_core::clock::now_ms()
}

fn require_write(ui: &Ui, what: &str) -> Result<()> {
    if ui.system {
        paths::require_root(what)?;
    }
    Ok(())
}

fn must_exist(lake: &Lake) -> Result<()> {
    if !lake.root().is_dir() {
        bail!("no telemetry lake at {} yet; nothing to do", tilde(lake.root()));
    }
    Ok(())
}

/// Ask before something destructive: on a TTY unless --yes; refuse without a TTY.
fn approve(ui: &Ui, yes: bool, prompt: &str) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !ui.stdin_tty {
        bail!("{prompt} Refusing without a terminal to confirm on; pass --yes.");
    }
    Ok(confirm(prompt))
}

pub fn data(ui: &Ui, cmd: &DataCmd) -> Result<()> {
    match cmd {
        DataCmd::Catalog(l) => {
            require_write(ui, "regenerating the system catalog")?;
            let lake = open_lake_with(ui, l.lake.as_deref(), l.backend)?;
            let (c, _) = lake.data.json::<telemetry::Catalog>(&["catalog"]).map_err(|e| {
                anyhow!("{e}\n  The catalog needs the DuckDB vortex extension: lindeskctl data install-extension")
            })?;
            let p = c.catalog;
            if ui.json {
                return ui.print_json(&json!({"catalog": p, "client_setup": CATALOG_HINT}));
            }
            ui.info(format!("{} wrote {}", ui.ok_mark(), tilde(&p)));
            ui.info(ui.dim(format!(
                "open it from any DuckDB client: duckdb -readonly {} -c \"{CATALOG_HINT} FROM lindesk_tables\"",
                p.display()
            )));
            ui.info(
                ui.dim(format!("(run `{CATALOG_HINT}` first in every DuckDB session; DuckDB doesn't autoload it)")),
            );
            Ok(())
        }
        DataCmd::Compact(l) => {
            require_write(ui, "compacting the system lake")?;
            let lake = open_lake_with(ui, l.lake.as_deref(), l.backend)?;
            must_exist(&lake)?;
            let (r, raw) = lake.data.json::<telemetry::CompactReport>(&["compact"])?;
            if ui.json {
                return ui.print_json(&raw);
            }
            out(&format!(
                "{} compacted {} partitions: {} → {} files, {} → {}; rollups refreshed for {} hours",
                ui.ok_mark(),
                r.partitions,
                r.files_before,
                r.files_after,
                bytes(r.bytes_before),
                bytes(r.bytes_after),
                r.rollup_hours
            ));
            Ok(())
        }
        DataCmd::Prune(l) => {
            require_write(ui, "pruning the system lake")?;
            let lake = open_lake_with(ui, l.lake.as_deref(), l.backend)?;
            must_exist(&lake)?;
            // Retention comes from the scope's config (lindesk-data --config).
            let (r, raw) = lake.data.json::<telemetry::PruneReport>(&["prune"])?;
            if ui.json {
                return ui.print_json(&raw);
            }
            out(&format!("{} pruned: {} → {}", ui.ok_mark(), bytes(r.bytes_before), bytes(r.bytes_after)));
            for t in &r.tables {
                out(&format!(
                    "  {:<14} expired {:>4}  evicted {:>4}  freed {}",
                    t.table,
                    t.expired,
                    t.evicted,
                    bytes(t.bytes)
                ));
            }
            Ok(())
        }
        DataCmd::Stats(l) => {
            let lake = open_lake_with(ui, l.lake.as_deref(), l.backend)?;
            let (s, raw) = lake.data.json::<telemetry::LakeStats>(&["stats"])?;
            if ui.json {
                return ui.print_json(&raw);
            }
            print_stats(ui, &s);
            Ok(())
        }
        DataCmd::InstallExtension(l) => {
            require_write(ui, "installing the DuckDB vortex extension for the system lake")?;
            let lake = open_lake_with(ui, l.lake.as_deref(), BackendArg::Extension)?;
            let ext_dir = lake.root().parent().unwrap_or(lake.root()).join("duckdb-ext");
            lake.data.output(&["install-extension"]).map_err(|e| {
                anyhow!("{e}\n  It downloads once from DuckDB's extension repository; without it lindesk uses its built-in Vortex reader.")
            })?;
            if ui.json {
                return ui.print_json(&json!({"installed": true, "dir": ext_dir}));
            }
            out(&format!(
                "{} DuckDB vortex extension installed in {} (queries and the catalog work offline now)",
                ui.ok_mark(),
                tilde(&ext_dir)
            ));
            Ok(())
        }
        DataCmd::Delete { predicate, since, until, tables, dry_run, yes, lake: l } => {
            if !dry_run {
                require_write(ui, "deleting from the system lake")?;
            }
            let lake = open_lake_with(ui, l.lake.as_deref(), l.backend)?;
            must_exist(&lake)?;
            let n = now();
            let mut args: Vec<String> = vec!["delete".into(), "--where".into(), predicate.clone()];
            for t in tables {
                args.extend(["--table".into(), t.clone()]);
            }
            if let Some(s) = since {
                args.extend(["--since".into(), parse_time(s, n)?.to_string()]);
            }
            if let Some(u) = until {
                args.extend(["--until".into(), parse_time(u, n)?.to_string()]);
            }
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            // Without --yes lindesk-data only counts.
            let (count, raw) = lake.data.json::<telemetry::DeleteReport>(&argv)?;
            let total = count.rows();
            if *dry_run || total == 0 {
                if ui.json {
                    return ui.print_json(&raw);
                }
                if total == 0 {
                    out("no matching rows");
                } else {
                    out(&format!("{total} matching rows (dry run, nothing deleted):"));
                    for t in &count.tables {
                        out(&format!("  {:<14} {:>8} rows", t.table, t.rows));
                    }
                }
                return Ok(());
            }
            let summary: Vec<String> = count.tables.iter().map(|t| format!("{} {}", t.table, t.rows)).collect();
            if !ui.json {
                out(&format!("{total} matching rows: {}", summary.join(", ")));
            }
            if !approve(ui, *yes, &format!("Delete {total} rows from {}?", tilde(lake.root())))? {
                out("nothing deleted");
                return Ok(());
            }
            let mut argv = argv;
            argv.push("--yes");
            let (r, raw) = lake.data.json::<telemetry::DeleteReport>(&argv)?;
            if ui.json {
                return ui.print_json(&raw);
            }
            let rewritten: usize = r.tables.iter().map(|t| t.partitions_rewritten + t.partitions_removed).sum();
            out(&format!("{} deleted {} rows ({rewritten} partitions rewritten)", ui.ok_mark(), r.rows()));
            Ok(())
        }
        DataCmd::Purge { yes, lake: l } => {
            require_write(ui, "purging the system lake")?;
            let lake = open_lake_with(ui, l.lake.as_deref(), l.backend)?;
            if !lake.root().is_dir() {
                out("no telemetry to purge");
                return Ok(());
            }
            let size = paths::dir_size(lake.root());
            if !approve(ui, *yes, &format!("Remove ALL telemetry in {} ({})?", tilde(lake.root()), bytes(size)))? {
                out("nothing removed");
                return Ok(());
            }
            let (r, raw) = lake.data.json::<telemetry::PurgeReport>(&["purge", "--yes"])?;
            if ui.json {
                return ui.print_json(&raw);
            }
            out(&format!("{} removed {} files ({})", ui.ok_mark(), r.files, bytes(r.bytes)));
            Ok(())
        }
        DataCmd::Shell(l) => {
            let lake = open_lake_with(ui, l.lake.as_deref(), l.backend)?;
            Err(lake.data.exec(&["shell"]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        let now = 1_800_000_000_000;
        assert_eq!(parse_time("now", now).unwrap(), now);
        assert_eq!(parse_time("-1h", now).unwrap(), now - 3_600_000);
        assert_eq!(parse_time("30m", now).unwrap(), now - 1_800_000);
        assert_eq!(parse_time("2026-10-04T10:00:00Z", now).unwrap(), 1_791_108_000_000);
        assert!(parse_time("2026-10-04", now).is_ok());
        assert!(parse_time("2026-10-04 13:45", now).is_ok());
        assert_eq!(parse_time("1791108000000", now).unwrap(), 1_791_108_000_000);
        assert!(parse_time("yesterday-ish", now).is_err());
    }
}
