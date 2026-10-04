//! `lindeskctl query` and `lindeskctl data …`: the telemetry lake through lindesk-query.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use lindesk_core::config::Config;
use lindesk_query::{CATALOG_HINT, DeleteRequest, Format, Lake, QueryError};
use serde_json::json;

use crate::cli::{BackendArg, DataCmd, QueryArgs, QueryFormat};
use crate::cmd::doctor::tilde;
use crate::paths;
use crate::ui::{Table, Ui, bytes, confirm, out};

fn config(ui: &Ui) -> Option<Config> {
    paths::load_config(&paths::config_path(ui.system)).ok()
}

/// The lake for the scope (or an explicit directory), read with the default backend.
pub fn open_lake(ui: &Ui, explicit: Option<&Path>) -> Result<Lake> {
    open_lake_with(ui, explicit, BackendArg::Auto)
}

/// [`open_lake`] with a backend choice (`--backend`).
pub fn open_lake_with(ui: &Ui, explicit: Option<&Path>, backend: BackendArg) -> Result<Lake> {
    let root = paths::lake_root(explicit, ui.system, config(ui).as_ref());
    if ui.system && root.exists() && std::fs::read_dir(&root).is_err() {
        bail!(
            "can't read the system lake at {}: it's readable by root and the lindesk group.\n  Join the group: sudo usermod -aG lindesk $USER (then log out and in), or use sudo.",
            root.display()
        );
    }
    Ok(Lake::open(&root)?.with_backend(backend.into()))
}

fn query_error(e: QueryError, lake: &Lake) -> anyhow::Error {
    match e {
        QueryError::Refused(m) => {
            anyhow!("{m}\n  lindeskctl query is read-only; to delete rows use: lindeskctl data delete --where …")
        }
        QueryError::DuckDb(d) => {
            let m = d.to_string();
            let hint = if m.contains("does not exist") || m.contains("not found") {
                "\n  Tables: pressure, units, events, metrics, bench_runs, bench_samples, pressure_1m, pressure_1h, units_1m, units_1h. Columns: lindeskctl query \"describe units\""
            } else {
                ""
            };
            anyhow!("{m}{hint}")
        }
        QueryError::Io(io) if io.kind() == std::io::ErrorKind::PermissionDenied => {
            anyhow!("permission denied reading {} ({io})", lake.root().display())
        }
        other => anyhow!(other),
    }
}

pub fn query(ui: &Ui, a: &QueryArgs) -> Result<()> {
    let lake = open_lake_with(ui, a.lake.as_deref(), a.backend)?;
    let fmt = if ui.json && a.format == QueryFormat::Table { QueryFormat::Json } else { a.format };
    let format = match fmt {
        QueryFormat::Table => Format::Table,
        QueryFormat::Csv => Format::Csv,
        QueryFormat::Json => Format::Json,
        QueryFormat::Parquet => {
            let out = a.out.clone().ok_or_else(|| crate::ui::Usage("--format parquet needs --out FILE".into()))?;
            Format::Parquet(std::path::absolute(out)?)
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
    let result = lake.query(&a.sql, &format).map_err(|e| query_error(e, &lake))?;
    match (&format, &a.out) {
        (Format::Parquet(p), _) => ui.info(format!("{} wrote {}", ui.ok_mark(), p.display())),
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

fn print_stats(ui: &Ui, s: &lindesk_query::LakeStats) {
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
            let p = lake.catalog().map_err(|e| {
                anyhow!("{e}\n  The catalog needs the DuckDB vortex extension: lindeskctl data install-extension")
            })?;
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
            let r = lake.compact(now())?;
            if ui.json {
                return ui.print_json(&r);
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
            let retention = config(ui).unwrap_or_default().telemetry.retention;
            let r = lake.prune(&retention, now())?;
            if ui.json {
                return ui.print_json(&r);
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
            let s = lake.stats()?;
            if ui.json {
                return ui.print_json(&s);
            }
            print_stats(ui, &s);
            Ok(())
        }
        DataCmd::InstallExtension(l) => {
            require_write(ui, "installing the DuckDB vortex extension for the system lake")?;
            let lake = open_lake_with(ui, l.lake.as_deref(), BackendArg::Extension)?;
            lake.install_extension().map_err(|e| {
                anyhow!("{e}\n  It downloads once from DuckDB's extension repository; without it lindesk uses its built-in Vortex reader.")
            })?;
            if ui.json {
                return ui.print_json(&json!({"installed": true, "dir": lake.extension_dir()}));
            }
            out(&format!(
                "{} DuckDB vortex extension installed in {} (queries and the catalog work offline now)",
                ui.ok_mark(),
                tilde(lake.extension_dir())
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
            let mut req = DeleteRequest {
                predicate: predicate.clone(),
                tables: tables.clone(),
                since: since.as_deref().map(|s| parse_time(s, n)).transpose()?,
                until: until.as_deref().map(|s| parse_time(s, n)).transpose()?,
                dry_run: true,
            };
            let count = lake.delete(&req).map_err(|e| query_error(e, &lake))?;
            let total = count.rows();
            if *dry_run || total == 0 {
                if ui.json {
                    return ui.print_json(&count);
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
            req.dry_run = false;
            let r = lake.delete(&req).map_err(|e| query_error(e, &lake))?;
            if ui.json {
                return ui.print_json(&r);
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
            let r = lake.purge()?;
            if ui.json {
                return ui.print_json(&r);
            }
            out(&format!("{} removed {} files ({})", ui.ok_mark(), r.files, bytes(r.bytes)));
            Ok(())
        }
        DataCmd::Shell(l) => shell(ui, l.lake.as_deref(), l.backend),
    }
}

fn shell(ui: &Ui, explicit: Option<&Path>, backend: BackendArg) -> Result<()> {
    let lake = open_lake_with(ui, explicit, backend)?;
    let Some(duckdb) = paths::which("duckdb") else {
        bail!(
            "the duckdb CLI isn't installed. Get it from https://duckdb.org/docs/installation (one static binary; e.g. `curl https://install.duckdb.org | sh`),\n  or query without it: lindeskctl query \"select …\""
        );
    };
    let catalog: PathBuf = lake.catalog_path().to_path_buf();
    if !catalog.exists() {
        lake.catalog().with_context(|| {
            format!(
                "{} doesn't exist and couldn't be generated (run lindeskctl data catalog{})",
                catalog.display(),
                if ui.system { " as root" } else { "" }
            )
        })?;
    }
    ui.info(ui.dim(format!(
        "opening {} read-only; tables are views over the Vortex lake (.tables lists them)",
        tilde(&catalog)
    )));
    // DuckDB doesn't autoload the extension the views need.
    let ext_dir = lake.extension_dir().to_string_lossy().replace('\'', "''");
    let init = format!("SET extension_directory = '{ext_dir}'; {CATALOG_HINT}");
    let err = Command::new(duckdb).arg("-readonly").arg("-cmd").arg(init).arg(&catalog).exec();
    Err(anyhow!("running duckdb: {err}"))
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
