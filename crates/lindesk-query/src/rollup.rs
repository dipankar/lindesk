//! SQL for the `_1m` / `_1h` rollups.
//!
//! The SELECT list is generated from the rollup table's column definitions in
//! `lindesk-store`, with every expression cast to its declared type, so rollup files always
//! match the declared schema (and the typed empty views).

use lindesk_store::SCHEMA_VERSION;
use lindesk_store::schema::{Bucket, TableDef, TableKind};

use crate::engine::ident;

/// SQL expression truncating `ts` to the start of its bucket.
pub(crate) fn bucket_expr(bucket: Bucket) -> String {
    let us = bucket.seconds() * 1_000_000;
    format!("make_timestamp((epoch_us(ts) // {us}) * {us})::TIMESTAMPTZ")
}

/// SQL expression for "the UTC hour number (hours since the epoch) of `ts`".
pub(crate) fn epoch_hour_expr() -> &'static str {
    "(epoch_us(ts) // 3600000000)"
}

/// SQL timestamp literal for a ms-since-epoch value.
pub(crate) fn ts_lit(ms: i64) -> String {
    format!("make_timestamp({}::BIGINT)::TIMESTAMPTZ", ms.saturating_mul(1000))
}

/// Aggregate expression for one rollup column.
fn expr(source: &str, col: &str, bucket: Bucket) -> Option<String> {
    let q = |c: &str| ident(c);
    let p95 = |c: &str| format!("quantile_cont({}, 0.95)", q(c));
    Some(match (source, col) {
        (_, "schema_version") => SCHEMA_VERSION.to_string(),
        (_, "ts") => bucket_expr(bucket),
        (_, "samples") => "count(*)".into(),
        ("pressure", "scope" | "resource") => q(col),
        ("pressure", c) if c.ends_with("_min") => format!("min({})", q(&c[..c.len() - 4])),
        ("pressure", c) if c.ends_with("_avg") && c != "some_avg10" && c != "full_avg10" => {
            format!("avg({})", q(&c[..c.len() - 4]))
        }
        ("pressure", c) if c.ends_with("_max") => format!("max({})", q(&c[..c.len() - 4])),
        ("pressure", c) if c.ends_with("_p95") => p95(&c[..c.len() - 4]),
        ("pressure", "some_delta_us" | "full_delta_us" | "interval_ms") => format!("sum({})", q(col)),
        ("units", "manager" | "uid" | "unit") => q(col),
        ("units", "class") => "arg_max(class, ts)".into(),
        ("units", "foreground") => "bool_or(foreground)".into(),
        ("units", "mem_current_min") => "min(mem_current)".into(),
        ("units", "mem_current_avg") => "avg(mem_current)".into(),
        ("units", "mem_current_max") => "max(mem_current)".into(),
        ("units", "mem_current_p95") => p95("mem_current"),
        ("units", "cpu_usage_us" | "io_rbytes" | "io_wbytes" | "io_rios" | "io_wios" | "interval_ms") => {
            format!("sum({})", q(col))
        }
        _ => return None,
    })
}

/// `SELECT … FROM <from> GROUP BY …` computing `rollup` from raw rows read by `from`.
pub(crate) fn rollup_select(rollup: &TableDef, from: &str) -> String {
    let TableKind::Rollup { source, bucket } = rollup.kind else {
        panic!("{} is not a rollup table", rollup.name);
    };
    let group: &[&str] = match source {
        "pressure" => &["scope", "resource"],
        _ => &["manager", "uid", "unit"],
    };
    let cols: Vec<String> = rollup
        .columns
        .iter()
        .map(|c| {
            let e = expr(source, c.name, bucket).unwrap_or_else(|| panic!("no rollup expression for {}", c.name));
            format!("CAST({e} AS {}) AS {}", c.ty.duckdb_type(), ident(c.name))
        })
        .collect();
    let keys: Vec<String> = group.iter().map(|g| ident(g)).collect();
    format!("SELECT {} FROM {from} GROUP BY {}, {}", cols.join(", "), bucket_expr(bucket), keys.join(", "))
}

#[cfg(test)]
mod tests {
    use lindesk_store::TABLES;

    use super::*;

    #[test]
    fn every_rollup_column_has_an_expression() {
        for t in TABLES {
            if matches!(t.kind, TableKind::Rollup { .. }) {
                let sql = rollup_select(t, "x");
                assert!(sql.contains("GROUP BY"), "{sql}");
            }
        }
    }
}
