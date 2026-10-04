//! `lindesk-data`: telemetry lake maintenance, run by `lindesk-compact.timer`.
//!
//! ```text
//! lindesk-data maintain                     # compact + prune + catalog (the timer default)
//! lindesk-data query "select resource, avg(some_avg10) from pressure group by 1"
//! lindesk-data delete --where "app_id = 'org.mozilla.firefox'" --since 7d --yes
//! ```
//!
//! All logic lives in `lindesk-query`; this binary only parses arguments and prints reports.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use lindesk_core::config::Config;
use lindesk_query::{BackendChoice, DeleteRequest, Format, Lake};
use lindesk_store::wall_clock_ms;

/// Maintain and query the lindesk telemetry lake.
#[derive(Parser, Debug)]
#[command(name = "lindesk-data", version, about, long_about = None)]
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

/// Parses `--since`/`--until`: RFC 3339, `YYYY-MM-DD` (UTC midnight), or a humantime
/// duration meaning "that long before `now`".
fn parse_time(s: &str, now: i64) -> Result<i64> {
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(t.timestamp_millis());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc().timestamp_millis());
    }
    let d = humantime::parse_duration(s)
        .with_context(|| format!("'{s}' is not an RFC 3339 time, a YYYY-MM-DD date or a duration like 7d"))?;
    Ok(now - i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

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
            print!("{}", lake.query(&sql, &format)?);
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
            let r = lake.delete(&req)?;
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
    }
    Ok(())
}

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
    }
}
