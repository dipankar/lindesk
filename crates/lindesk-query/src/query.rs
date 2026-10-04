//! Read-only SQL over the lake.

use std::fmt::Write as _;
use std::path::PathBuf;

use chrono::{DateTime, NaiveDate};
use duckdb::types::{TimeUnit, Value};
use lindesk_store::TABLES;

use crate::engine::{self, lit, path_lit};
use crate::{Lake, QueryError};

/// Output format of [`Lake::query`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Format {
    /// An aligned ASCII table.
    Table,
    /// RFC 4180 CSV with a header row.
    Csv,
    /// A JSON array of objects.
    Json,
    /// Write the result to a ZSTD Parquet file (SELECT-like statements only).
    Parquet(PathBuf),
}

impl std::str::FromStr for Format {
    type Err = String;

    /// Parses `table`, `csv`, `json` or `parquet:<path>`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "table" => Ok(Format::Table),
            "csv" => Ok(Format::Csv),
            "json" => Ok(Format::Json),
            _ => match s.strip_prefix("parquet:") {
                Some(p) if !p.is_empty() => Ok(Format::Parquet(PathBuf::from(p))),
                _ => Err(format!("unknown format '{s}' (table|csv|json|parquet:<path>)")),
            },
        }
    }
}

/// The kind of statement a query is, as far as the read-only gate is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatementKind {
    /// SELECT, WITH, FROM-first, VALUES, or a parenthesised query.
    Select,
    /// DESCRIBE, SHOW, SUMMARIZE or `PRAGMA show_tables`.
    Inspect,
}

/// Strips leading whitespace, comments and opening parentheses.
fn skip_trivia(mut s: &str) -> &str {
    loop {
        let t = s.trim_start();
        if let Some(rest) = t.strip_prefix("--") {
            s = rest.split_once('\n').map_or("", |(_, r)| r);
        } else if let Some(rest) = t.strip_prefix("/*") {
            s = rest.split_once("*/").map_or("", |(_, r)| r);
        } else if let Some(rest) = t.strip_prefix('(') {
            s = rest;
        } else {
            return t;
        }
    }
}

fn first_words(s: &str, n: usize) -> Vec<String> {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .take(n)
        .map(|w| w.to_ascii_uppercase())
        .collect()
}

/// Classifies `sql` or refuses it. DuckDB's parser double-checks SELECT-like statements
/// (see [`Lake::query`]); this lexical gate also admits a few read-only utility statements.
pub(crate) fn classify(sql: &str) -> Result<StatementKind, QueryError> {
    let body = skip_trivia(sql);
    let words = first_words(body, 2);
    let refuse = || {
        QueryError::Refused(format!(
            "only read-only statements are allowed (SELECT, WITH, FROM, VALUES, DESCRIBE, SHOW, SUMMARIZE, \
             PRAGMA show_tables); got '{}'",
            words.first().map(String::as_str).unwrap_or("")
        ))
    };
    match words.first().map(String::as_str) {
        Some("SELECT" | "WITH" | "FROM" | "VALUES") => Ok(StatementKind::Select),
        Some("DESCRIBE" | "SHOW" | "SUMMARIZE") => Ok(StatementKind::Inspect),
        Some("PRAGMA") if matches!(words.get(1).map(String::as_str), Some("SHOW_TABLES" | "SHOW_TABLES_EXPANDED")) => {
            Ok(StatementKind::Inspect)
        }
        _ => Err(refuse()),
    }
}

/// True for errors caused by a file disappearing under a running query (concurrent
/// compaction swapped its partition); such queries are retried.
pub(crate) fn is_vanished_file(e: &QueryError) -> bool {
    if let QueryError::Io(io) = e {
        return io.kind() == std::io::ErrorKind::NotFound;
    }
    let QueryError::DuckDb(e) = e else { return false };
    let m = e.to_string();
    m.contains("No such file or directory")
        || m.contains("No files found")
        || m.contains("Cannot open file")
        || m.contains("NotFound")
}

/// True when `sql` may reference `table` (a cheap word match used to skip globbing
/// tables the query cannot touch).
pub(crate) fn mentions(sql: &str, table: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    lower.match_indices(table).any(|(i, _)| {
        let before = lower[..i].chars().next_back();
        let after = lower[i + table.len()..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
        boundary(before) && boundary(after)
    })
}

/// Fully materialised result set.
pub(crate) struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

impl Lake {
    pub(crate) fn run_query(&self, sql: &str, format: &Format) -> Result<String, QueryError> {
        match self.run_query_with(sql, format, true) {
            // The vortex extension cannot translate some pushed-down filters; run without.
            Err(e) if engine::is_pushdown_error(&e) => self.run_query_with(sql, format, false),
            other => other,
        }
    }

    fn run_query_with(&self, sql: &str, format: &Format, pushdown: bool) -> Result<String, QueryError> {
        let kind = classify(sql)?;
        if matches!(format, Format::Parquet(_)) && kind != StatementKind::Select {
            return Err(QueryError::Refused("parquet output needs a SELECT-like statement".into()));
        }
        let out_path = match format {
            Format::Parquet(p) => Some(std::path::absolute(p)?),
            _ => None,
        };
        let paths: Vec<&std::path::Path> = out_path.iter().map(PathBuf::as_path).collect();
        let mut db =
            engine::Db::open_with(self.backend()?, self.extension_dir(), &[self.root()], &paths, None, pushdown)?;
        engine::create_views(&mut db, self.root(), TABLES, |t| mentions(sql, t.name))?;
        let conn = &db.conn;
        if kind == StatementKind::Select {
            // Parser-level proof that this is exactly one SELECT statement.
            let ast: String =
                conn.query_row(&format!("SELECT json_serialize_sql({})::VARCHAR", lit(sql)), [], |r| r.get(0))?;
            let ast: serde_json::Value = serde_json::from_str(&ast).unwrap_or_default();
            let n = ast["statements"].as_array().map_or(0, Vec::len);
            if ast["error"].as_bool() != Some(false) || n != 1 {
                let msg = ast["error_message"].as_str().unwrap_or("expected exactly one SELECT statement");
                return Err(QueryError::Refused(msg.to_string()));
            }
        }
        if let Some(p) = &out_path {
            let body = sql.trim().trim_end_matches(';');
            let n =
                conn.execute(&format!("COPY ({body}) TO {} (FORMAT parquet, COMPRESSION zstd)", path_lit(p)), [])?;
            return Ok(format!("wrote {n} rows to {}\n", p.display()));
        }
        let rs = fetch(conn, sql)?;
        Ok(match format {
            Format::Table => render_table(&rs),
            Format::Csv => render_csv(&rs),
            Format::Json => render_json(&rs),
            Format::Parquet(_) => unreachable!(),
        })
    }
}

/// Runs one statement and materialises its rows.
pub(crate) fn fetch(conn: &duckdb::Connection, sql: &str) -> Result<ResultSet, QueryError> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query([])?;
    let columns: Vec<String> = rows.as_ref().map(|s| s.column_names()).unwrap_or_default();
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut vals = Vec::with_capacity(columns.len());
        for i in 0..columns.len() {
            vals.push(row.get::<_, Value>(i)?);
        }
        out.push(vals);
    }
    Ok(ResultSet { columns, rows: out })
}

fn fmt_ts(unit: TimeUnit, v: i64) -> String {
    let micros = match unit {
        TimeUnit::Second => v.saturating_mul(1_000_000),
        TimeUnit::Millisecond => v.saturating_mul(1_000),
        TimeUnit::Microsecond => v,
        TimeUnit::Nanosecond => v / 1_000,
    };
    DateTime::from_timestamp_micros(micros)
        .map_or_else(|| v.to_string(), |d| d.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
}

/// Renders a value the way DuckDB's CLI roughly would; `None` for NULL.
pub(crate) fn value_to_string(v: &Value) -> Option<String> {
    Some(match v {
        Value::Null => return None,
        Value::Boolean(b) => b.to_string(),
        Value::TinyInt(i) => i.to_string(),
        Value::SmallInt(i) => i.to_string(),
        Value::Int(i) => i.to_string(),
        Value::BigInt(i) => i.to_string(),
        Value::HugeInt(i) => i.to_string(),
        Value::UHugeInt(i) => i.to_string(),
        Value::UTinyInt(i) => i.to_string(),
        Value::USmallInt(i) => i.to_string(),
        Value::UInt(i) => i.to_string(),
        Value::UBigInt(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(f) => f.to_string(),
        Value::Decimal(d) => d.to_string(),
        Value::Timestamp(u, t) => fmt_ts(*u, *t),
        Value::Text(s) | Value::Enum(s) => s.clone(),
        Value::Blob(b) | Value::Geometry(b) => b.iter().fold(String::from("\\x"), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        }),
        Value::Date32(d) => {
            NaiveDate::from_num_days_from_ce_opt(*d + 719_163).map_or_else(|| d.to_string(), |d| d.to_string())
        }
        Value::Time64(u, t) => {
            let us = match u {
                TimeUnit::Second => t * 1_000_000,
                TimeUnit::Millisecond => t * 1_000,
                TimeUnit::Microsecond => *t,
                TimeUnit::Nanosecond => t / 1_000,
            };
            format!(
                "{:02}:{:02}:{:02}.{:06}",
                us / 3_600_000_000,
                us / 60_000_000 % 60,
                us / 1_000_000 % 60,
                us % 1_000_000
            )
        }
        Value::Interval { months, days, nanos } => format!("{months} months {days} days {}us", nanos / 1_000),
        Value::List(items) | Value::Array(items) => {
            let parts: Vec<String> =
                items.iter().map(|v| value_to_string(v).unwrap_or_else(|| "NULL".into())).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Struct(m) => {
            let parts: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("'{k}': {}", value_to_string(v).unwrap_or_else(|| "NULL".into())))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        Value::Map(m) => {
            let parts: Vec<String> = m
                .iter()
                .map(|(k, v)| {
                    let s = |v| value_to_string(v).unwrap_or_else(|| "NULL".into());
                    format!("{}={}", s(k), s(v))
                })
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        Value::Union(v) => return value_to_string(v),
        other => format!("{other:?}"),
    })
}

/// Converts a value to JSON, keeping numbers and booleans native.
pub(crate) fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Null => J::Null,
        Value::Boolean(b) => J::Bool(*b),
        Value::TinyInt(i) => J::from(*i),
        Value::SmallInt(i) => J::from(*i),
        Value::Int(i) => J::from(*i),
        Value::BigInt(i) => J::from(*i),
        Value::UTinyInt(i) => J::from(*i),
        Value::USmallInt(i) => J::from(*i),
        Value::UInt(i) => J::from(*i),
        Value::UBigInt(i) => J::from(*i),
        Value::HugeInt(i) => i64::try_from(*i).map_or_else(|_| J::String(i.to_string()), J::from),
        Value::UHugeInt(i) => u64::try_from(*i).map_or_else(|_| J::String(i.to_string()), J::from),
        Value::Float(f) => serde_json::Number::from_f64(f64::from(*f)).map_or(J::Null, J::Number),
        Value::Double(f) => serde_json::Number::from_f64(*f).map_or(J::Null, J::Number),
        Value::List(items) | Value::Array(items) => J::Array(items.iter().map(value_to_json).collect()),
        Value::Struct(m) => J::Object(m.iter().map(|(k, v)| (k.clone(), value_to_json(v))).collect()),
        Value::Map(m) => J::Object(
            m.iter().map(|(k, v)| (value_to_string(k).unwrap_or_else(|| "null".into()), value_to_json(v))).collect(),
        ),
        Value::Union(v) => value_to_json(v),
        other => value_to_string(other).map_or(J::Null, J::String),
    }
}

fn is_numeric(v: &Value) -> bool {
    matches!(
        v,
        Value::TinyInt(_)
            | Value::SmallInt(_)
            | Value::Int(_)
            | Value::BigInt(_)
            | Value::HugeInt(_)
            | Value::UHugeInt(_)
            | Value::UTinyInt(_)
            | Value::USmallInt(_)
            | Value::UInt(_)
            | Value::UBigInt(_)
            | Value::Float(_)
            | Value::Double(_)
            | Value::Decimal(_)
    )
}

/// Renders an aligned ASCII table; numbers are right-aligned.
pub(crate) fn render_table(rs: &ResultSet) -> String {
    let cells: Vec<Vec<String>> = rs
        .rows
        .iter()
        .map(|r| r.iter().map(|v| value_to_string(v).unwrap_or_else(|| "NULL".into()).replace('\n', "\\n")).collect())
        .collect();
    let mut widths: Vec<usize> = rs.columns.iter().map(|c| c.chars().count()).collect();
    for row in &cells {
        for (w, c) in widths.iter_mut().zip(row) {
            *w = (*w).max(c.chars().count());
        }
    }
    let rule = widths.iter().fold(String::from("+"), |mut s, w| {
        s.push_str(&"-".repeat(w + 2));
        s.push('+');
        s
    });
    let mut out = String::new();
    let _ = writeln!(out, "{rule}");
    let mut line = String::from("|");
    for (c, w) in rs.columns.iter().zip(&widths) {
        let _ = write!(line, " {c:<w$} |");
    }
    let _ = writeln!(out, "{line}\n{rule}");
    for (row, vals) in cells.iter().zip(&rs.rows) {
        let mut line = String::from("|");
        for ((c, w), v) in row.iter().zip(&widths).zip(vals) {
            if is_numeric(v) {
                let _ = write!(line, " {c:>w$} |");
            } else {
                let _ = write!(line, " {c:<w$} |");
            }
        }
        let _ = writeln!(out, "{line}");
    }
    if !cells.is_empty() {
        let _ = writeln!(out, "{rule}");
    }
    let n = cells.len();
    let _ = writeln!(out, "({n} row{})", if n == 1 { "" } else { "s" });
    out
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
}

/// Renders RFC 4180 CSV; NULL is an empty field.
pub(crate) fn render_csv(rs: &ResultSet) -> String {
    let mut out = rs.columns.iter().map(|c| csv_field(c)).collect::<Vec<_>>().join(",");
    out.push('\n');
    for row in &rs.rows {
        let line: Vec<String> =
            row.iter().map(|v| value_to_string(v).map(|s| csv_field(&s)).unwrap_or_default()).collect();
        out.push_str(&line.join(","));
        out.push('\n');
    }
    out
}

/// Renders a JSON array of objects.
pub(crate) fn render_json(rs: &ResultSet) -> String {
    let rows: Vec<serde_json::Value> = rs
        .rows
        .iter()
        .map(|r| serde_json::Value::Object(rs.columns.iter().cloned().zip(r.iter().map(value_to_json)).collect()))
        .collect();
    let mut s = serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".into());
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate() {
        assert_eq!(classify("  -- hi\n /* x */ (select 1)").unwrap(), StatementKind::Select);
        assert_eq!(classify("with a as (select 1) select * from a").unwrap(), StatementKind::Select);
        assert_eq!(classify("FROM pressure").unwrap(), StatementKind::Select);
        assert_eq!(classify("describe pressure").unwrap(), StatementKind::Inspect);
        assert_eq!(classify("PRAGMA show_tables").unwrap(), StatementKind::Inspect);
        for bad in [
            "pragma enable_profiling",
            "copy pressure to '/tmp/x.csv'",
            "attach '/tmp/x.db'",
            "create table t as select 1",
            "install httpfs",
            "set enable_external_access = true",
            "",
        ] {
            assert!(matches!(classify(bad), Err(QueryError::Refused(_))), "{bad}");
        }
    }

    #[test]
    fn mentions_is_word_based() {
        assert!(mentions("select * from Pressure", "pressure"));
        assert!(mentions("select * from pressure_1m", "pressure_1m"));
        assert!(!mentions("select * from pressure_1m", "pressure"));
        assert!(!mentions("select * from units_1h", "units"));
    }

    #[test]
    fn csv_quotes() {
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("plain"), "plain");
    }
}
