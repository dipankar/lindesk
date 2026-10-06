//! `lindesk-data`: the telemetry lake's reader and maintainer, from the optional
//! lindesk-telemetry package. Run by `lindesk-compact.timer`, and by `lindeskctl query`,
//! `lindeskctl data`, `lindeskctl simulate`, `lindesk-mcp` and `lindesk-bench`, which link no
//! DuckDB themselves.
//!
//! ```text
//! lindesk-data maintain                     # compact + prune + catalog (the timer default)
//! lindesk-data query "select resource, avg(some_avg10) from pressure group by 1"
//! lindesk-data delete --where "app_id = 'org.mozilla.firefox'" --since 7d --yes
//! lindesk-data export --table pressure --from -1h --format jsonl
//! lindesk-data import-bench bench-results-20261005T101500Z.json
//! ```
//!
//! **The contract with the other binaries** is this command line plus `--json` (reports as
//! JSON on stdout; errors as `lindesk-data: <message>` on stderr, exit 1). The lake logic
//! lives in `lindesk-query` (DuckDB) and `lindesk-store` (Vortex writer).

mod bench;

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use lindesk_core::config::Config;
use lindesk_query::{BackendChoice, CATALOG_HINT, DeleteRequest, Format, Lake, QueryError, TABLES};
use lindesk_store::wall_clock_ms;

/// Maintain and query the lindesk telemetry lake.
#[derive(Parser, Debug)]
#[command(name = "lindesk-data", version = lindesk_version::LONG, about, long_about = None)]
struct Cli {
    /// Lake root. Default: `telemetry.dir` from the config, else /var/lib/lindesk/telemetry
    /// as root or $XDG_STATE_HOME/lindesk/telemetry otherwise.
    #[arg(long, global = true, env = "LINDESK_TELEMETRY_DIR")]
    root: Option<PathBuf>,
    /// lindesk.toml to read retention from. Default: /etc/lindesk/lindesk.toml as root,
    /// else $XDG_CONFIG_HOME/lindesk/lindesk.toml. A missing file means defaults.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Print reports as JSON.
    #[arg(long, global = true)]
    json: bool,
    /// How to read .vortex files: the DuckDB vortex extension (installed once into the
    /// extension directory), the native Rust reader, or auto (extension, else native).
    #[arg(long, global = true, value_enum, default_value_t = BackendArg::Auto)]
    backend: BackendArg,
    /// DuckDB extension directory. Default: <root>/../duckdb-ext.
    #[arg(long, global = true)]
    extension_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Merge sealed hour partitions and refresh rollups.
    Compact,
    /// Apply retention (TTL per tier and max_size).
    Prune,
    /// Compact, prune and regenerate the catalog.
    Maintain,
    /// Regenerate catalog.duckdb (views only) for external DuckDB clients.
    Catalog,
    /// Install the DuckDB vortex extension into the extension directory (needs network once).
    InstallExtension,
    /// Show per-table partitions, files, rows and bytes.
    Stats,
    /// Run a read-only SQL statement.
    Query {
        /// SELECT / WITH / DESCRIBE / SHOW / SUMMARIZE statement.
        sql: String,
        /// Output format.
        #[arg(long, short, value_enum, default_value_t = OutFormat::Table)]
        format: OutFormat,
        /// Output file for `--format parquet`.
        #[arg(long, short, required_if_eq("format", "parquet"))]
        output: Option<PathBuf>,
    },
    /// Delete rows matching a predicate from raw tables and rollups.
    Delete {
        /// SQL boolean expression, e.g. "app_id = 'org.mozilla.firefox'".
        #[arg(long = "where", value_name = "PREDICATE")]
        predicate: String,
        /// Restrict to these tables (repeatable). Default: every table the predicate fits.
        #[arg(long = "table")]
        tables: Vec<String>,
        /// Only rows at or after this time: RFC 3339, YYYY-MM-DD, or a duration ago ("7d").
        #[arg(long)]
        since: Option<String>,
        /// Only rows before this time (same forms as --since).
        #[arg(long)]
        until: Option<String>,
        /// Actually delete; without it only matching rows are counted.
        #[arg(long)]
        yes: bool,
    },
    /// Remove all telemetry.
    Purge {
        /// Confirm.
        #[arg(long)]
        yes: bool,
    },
    /// Print one table's rows in a time window, `ts` as epoch milliseconds.
    Export {
        /// Table name (pressure, units, events, metrics, …).
        #[arg(long)]
        table: String,
        /// Rows at or after this time: RFC 3339, YYYY-MM-DD, epoch ms, or a duration ago
        /// ("1h", "-1h"). Default: everything.
        #[arg(long, allow_hyphen_values = true)]
        from: Option<String>,
        /// Rows before this time (same forms). Default: now.
        #[arg(long, allow_hyphen_values = true)]
        to: Option<String>,
        /// `jsonl`: one JSON object per line; `json`: one array.
        #[arg(long, short, value_enum, default_value_t = ExportFormat::Jsonl)]
        format: ExportFormat,
    },
    /// Store a lindesk-bench results file in bench_runs / bench_samples.
    ImportBench {
        /// `bench-results-*.json` as written by `lindesk-bench run`.
        results: PathBuf,
    },
    /// Open the duckdb CLI read-only on the catalog (regenerated if missing).
    Shell,
    /// Write lindesk-data.1 into a directory (packaging).
    #[command(hide = true)]
    Man { dir: PathBuf },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum ExportFormat {
    Jsonl,
    Json,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum BackendArg {
    Auto,
    Extension,
    Native,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum OutFormat {
    Table,
    Csv,
    Json,
    Parquet,
}

fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}

fn default_config_path() -> PathBuf {
    if is_root() {
        return PathBuf::from("/etc/lindesk/lindesk.toml");
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("/etc"))
        .join("lindesk/lindesk.toml")
}

/// Parses `--since`/`--until`/`--from`/`--to`: RFC 3339, `YYYY-MM-DD` (UTC midnight), epoch
/// milliseconds (12+ digits), `now`, or a humantime duration meaning "that long before
/// `now`" (`7d`, `-7d`).
fn parse_time(s: &str, now: i64) -> Result<i64> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("now") {
        return Ok(now);
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(t.timestamp_millis());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc().timestamp_millis());
    }
    if s.len() >= 12 && s.chars().all(|c| c.is_ascii_digit()) {
        return Ok(s.parse()?);
    }
    let d = humantime::parse_duration(s.strip_prefix('-').unwrap_or(s))
        .with_context(|| format!("'{s}' is not an RFC 3339 time, a YYYY-MM-DD date, epoch ms or a duration like 7d"))?;
    Ok(now - i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Adds the hints `lindeskctl query` has always given to DuckDB errors.
fn query_error(e: QueryError, lake: &Lake) -> anyhow::Error {
    match e {
        QueryError::Refused(m) => {
            anyhow!("{m}\n  queries are read-only; to delete rows use: lindeskctl data delete --where …")
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

/// `export`: the window's rows of `table` with `ts` as epoch ms.
fn export(lake: &Lake, table: &str, from: Option<i64>, to: i64, format: ExportFormat) -> Result<()> {
    let def = TABLES.iter().find(|t| t.name == table).ok_or_else(|| {
        let names: Vec<&str> = TABLES.iter().map(|t| t.name).collect();
        anyhow!("unknown table '{table}' (tables: {})", names.join(", "))
    })?;
    let lower = from.map(|f| format!("epoch_ms(ts) >= {f} and ")).unwrap_or_default();
    let sql =
        format!("select * replace (epoch_ms(ts) as ts) from {} where {lower}epoch_ms(ts) < {to} order by ts", def.name);
    let json = lake.query(&sql, &Format::Json).map_err(|e| query_error(e, lake))?;
    let rows: Vec<serde_json::Value> =
        if json.trim().is_empty() { Vec::new() } else { serde_json::from_str(&json).context("query output")? };
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    use std::io::Write as _;
    match format {
        ExportFormat::Json => writeln!(out, "{}", serde_json::to_string(&rows)?)?,
        ExportFormat::Jsonl => {
            for r in &rows {
                writeln!(out, "{r}")?;
            }
        }
    }
    out.flush()?;
    Ok(())
}

/// `shell`: exec the duckdb CLI on the catalog with the extension preloaded.
fn shell(lake: &Lake) -> Result<()> {
    let Some(duckdb) = which("duckdb") else {
        bail!(
            "the duckdb CLI isn't installed. Get it from https://duckdb.org/docs/installation (one static binary; e.g. `curl https://install.duckdb.org | sh`),\n  or query without it: lindeskctl query \"select …\""
        );
    };
    let catalog = lake.catalog_path().to_path_buf();
    if !catalog.exists() {
        lake.catalog().with_context(|| {
            format!("{} doesn't exist and couldn't be generated (run lindesk-data catalog)", catalog.display())
        })?;
    }
    eprintln!("opening {} read-only; tables are views over the Vortex lake (.tables lists them)", catalog.display());
    // DuckDB doesn't autoload the extension the views need.
    let ext_dir = lake.extension_dir().to_string_lossy().replace('\'', "''");
    let init = format!("SET extension_directory = '{ext_dir}'; {CATALOG_HINT}");
    let err = std::process::Command::new(duckdb).arg("-readonly").arg("-cmd").arg(init).arg(&catalog).exec();
    Err(anyhow!("running duckdb: {err}"))
}

fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false))
}

/// The man page, shipped with this binary (not with lindeskctl).
const MAN_PAGE: &str = include_str!("../man/lindesk-data.1");

fn size(b: u64) -> String {
    bytesize::ByteSize::b(b).display().iec().to_string()
}

fn print_json(v: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn print_compact(r: &lindesk_query::CompactReport) {
    println!(
        "compact: {} partitions, {} -> {} files, {} -> {}, {} rows; rollups: {} day partitions, {} hours{}",
        r.partitions,
        r.files_before,
        r.files_after,
        size(r.bytes_before),
        size(r.bytes_after),
        r.rows,
        r.rollup_partitions,
        r.rollup_hours,
        if r.recovered > 0 { format!("; recovered {} interrupted operations", r.recovered) } else { String::new() }
    );
}

fn print_prune(r: &lindesk_query::PruneReport) {
    println!("prune: {} -> {}", size(r.bytes_before), size(r.bytes_after));
    for t in &r.tables {
        println!("  {:<14} expired {:>4}  evicted {:>4}  freed {}", t.table, t.expired, t.evicted, size(t.bytes));
    }
}

fn print_catalog(lake: &Lake, path: &Path) {
    println!("catalog: {}", path.display());
    println!("  views read Vortex files; DuckDB clients must load the extension first:");
    println!(
        "  duckdb -readonly {} -c \"{} SELECT * FROM lindesk_tables\"",
        path.display(),
        lindesk_query::CATALOG_HINT
    );
    println!("  (offline: SET extension_directory = '{}'; LOAD vortex;)", lake.extension_dir().display());
}

fn run(cli: Cli) -> Result<()> {
    if let Cmd::Man { dir } = &cli.cmd {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let p = dir.join("lindesk-data.1");
        std::fs::write(&p, MAN_PAGE).with_context(|| format!("writing {}", p.display()))?;
        println!("{}", p.display());
        return Ok(());
    }
    let config_path = cli.config.clone().unwrap_or_else(default_config_path);
    let config = Config::load(&config_path).with_context(|| format!("loading {}", config_path.display()))?;
    let root = cli
        .root
        .clone()
        .or_else(|| config.telemetry.dir.clone())
        .unwrap_or_else(|| lindesk_query::default_root(is_root()));
    let mut lake = Lake::open(&root)?.with_backend(match cli.backend {
        BackendArg::Auto => BackendChoice::Auto,
        BackendArg::Extension => BackendChoice::Extension,
        BackendArg::Native => BackendChoice::Native,
    });
    if let Some(dir) = &cli.extension_dir {
        lake = lake.with_extension_dir(dir);
    }
    let retention = &config.telemetry.retention;
    let now = wall_clock_ms();
    let needs_root = |lake: &Lake| -> Result<()> {
        if !lake.root().is_dir() {
            bail!("no telemetry lake at {}", lake.root().display());
        }
        Ok(())
    };
    match cli.cmd {
        Cmd::Compact => {
            needs_root(&lake)?;
            let r = lake.compact(now)?;
            if cli.json { print_json(&r)? } else { print_compact(&r) }
        }
        Cmd::Prune => {
            needs_root(&lake)?;
            let r = lake.prune(retention, now)?;
            if cli.json { print_json(&r)? } else { print_prune(&r) }
        }
        Cmd::Maintain => {
            if !lake.root().is_dir() {
                // Nothing recorded yet (or telemetry disabled): not an error for the timer.
                tracing::info!(root = %lake.root().display(), "no telemetry lake yet; nothing to do");
                return Ok(());
            }
            let r = lake.maintain(retention, now)?;
            let catalog = r.catalog.clone();
            if cli.json {
                print_json(&r)?
            } else {
                print_compact(&r.compact);
                print_prune(&r.prune);
                match &catalog {
                    Some(p) => print_catalog(&lake, p),
                    None => println!("catalog: not regenerated (DuckDB vortex extension unavailable)"),
                }
            }
        }
        Cmd::Catalog => {
            needs_root(&lake)?;
            let p = lake.catalog()?;
            if cli.json {
                print_json(&serde_json::json!({ "catalog": p, "setup": lindesk_query::CATALOG_HINT }))?
            } else {
                print_catalog(&lake, &p);
            }
        }
        Cmd::InstallExtension => {
            lake.install_extension()?;
            println!("DuckDB vortex extension ready in {}", lake.extension_dir().display());
        }
        Cmd::Stats => {
            let s = lake.stats()?;
            if cli.json {
                print_json(&s)?
            } else {
                println!("{}  ({} rows, {}, format vortex)", s.root, s.rows, size(s.bytes));
                let legacy: usize = s.tables.iter().map(|t| t.legacy_files).sum();
                if legacy > 0 {
                    println!("{legacy} legacy Parquet files remain; `lindesk-data compact` converts them");
                }
                println!(
                    "{:<14} {:<7} {:>6} {:>6} {:>10} {:>10}  {:<16} {:<16}",
                    "table", "kind", "parts", "files", "rows", "size", "oldest", "newest"
                );
                for t in &s.tables {
                    println!(
                        "{:<14} {:<7} {:>6} {:>6} {:>10} {:>10}  {:<16} {:<16}",
                        t.table,
                        t.kind,
                        t.partitions,
                        t.files,
                        t.rows,
                        size(t.bytes),
                        t.oldest.as_deref().unwrap_or("-"),
                        t.newest.as_deref().unwrap_or("-")
                    );
                }
            }
        }
        Cmd::Query { sql, format, output } => {
            let format = match format {
                OutFormat::Table if cli.json => Format::Json,
                OutFormat::Table => Format::Table,
                OutFormat::Csv => Format::Csv,
                OutFormat::Json => Format::Json,
                OutFormat::Parquet => Format::Parquet(output.context("--output is required with --format parquet")?),
            };
            print!("{}", lake.query(&sql, &format).map_err(|e| query_error(e, &lake))?);
        }
        Cmd::Delete { predicate, tables, since, until, yes } => {
            needs_root(&lake)?;
            let req = DeleteRequest {
                predicate,
                tables,
                since: since.as_deref().map(|s| parse_time(s, now)).transpose()?,
                until: until.as_deref().map(|s| parse_time(s, now)).transpose()?,
                dry_run: !yes,
            };
            let r = lake.delete(&req).map_err(|e| query_error(e, &lake))?;
            if cli.json {
                print_json(&r)?;
            } else {
                for t in &r.tables {
                    println!(
                        "  {:<14} {:>10} rows  ({} partitions rewritten, {} removed)",
                        t.table, t.rows, t.partitions_rewritten, t.partitions_removed
                    );
                }
                if r.dry_run {
                    println!("{} matching rows; nothing deleted (re-run with --yes to delete)", r.rows());
                } else {
                    println!("deleted {} rows", r.rows());
                    lake.refresh_catalog();
                }
            }
        }
        Cmd::Purge { yes } => {
            if !yes {
                bail!("refusing to purge {} without --yes", lake.root().display());
            }
            needs_root(&lake)?;
            let r = lake.purge()?;
            if cli.json {
                print_json(&r)?
            } else {
                println!("purged {} files ({}) from {}", r.files, size(r.bytes), lake.root().display());
            }
        }
        Cmd::Export { table, from, to, format } => {
            let from = from.as_deref().map(|s| parse_time(s, now)).transpose()?;
            let to = to.as_deref().map(|s| parse_time(s, now)).transpose()?.unwrap_or(now + 1);
            export(&lake, &table, from, to, format)?;
        }
        Cmd::ImportBench { results } => {
            let r = bench::import(lake.root(), &results)?;
            if cli.json {
                print_json(&r)?
            } else {
                println!(
                    "stored run {} ({} rows) in the telemetry lake at {}",
                    r.run_id,
                    r.rows,
                    lake.root().display()
                );
            }
        }
        Cmd::Shell => shell(&lake)?,
        Cmd::Man { .. } => unreachable!("handled above"),
    }
    Ok(())
}

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("LINDESK_LOG").unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .init();
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lindesk-data: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_times() {
        let now = 1_000_000_000;
        assert_eq!(parse_time("1h", now).unwrap(), now - 3_600_000);
        assert_eq!(parse_time("-1h", now).unwrap(), now - 3_600_000);
        assert_eq!(parse_time("now", now).unwrap(), now);
        assert_eq!(parse_time("1791108000000", now).unwrap(), 1_791_108_000_000);
        assert_eq!(parse_time("1970-01-02", now).unwrap(), 86_400_000);
        assert_eq!(parse_time("1970-01-01T00:00:01Z", now).unwrap(), 1000);
        assert!(parse_time("yesterday-ish", now).is_err());
    }

    #[test]
    fn cli_parses() {
        let cli =
            Cli::try_parse_from(["lindesk-data", "--root", "/x", "delete", "--where", "pid = 1", "--yes"]).unwrap();
        assert!(matches!(cli.cmd, Cmd::Delete { yes: true, .. }));
        assert!(Cli::try_parse_from(["lindesk-data", "query", "select 1", "--format", "parquet"]).is_err());
        let cli = Cli::try_parse_from(["lindesk-data", "export", "--table", "pressure", "--from", "-1h"]).unwrap();
        assert!(matches!(cli.cmd, Cmd::Export { format: ExportFormat::Jsonl, .. }));
    }
}
