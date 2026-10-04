//! Table definitions for the telemetry lake.
//!
//! This module is the single source of truth for every table's columns. The writer builds
//! Arrow schemas from it, and `lindesk-query` builds DuckDB types (for empty views) and
//! rollup SQL from it, so the two never drift apart.
//!
//! Every table starts with `schema_version INT` and `ts TIMESTAMP WITH TIME ZONE`
//! (microsecond precision, UTC; the daemons sample at millisecond resolution).
//!
//! Every data file of a table has exactly this schema: all `.vortex` files are written by
//! [`crate::format::write_vortex_file`] from these definitions, including the files that
//! maintenance rewrites. That matters because DuckDB's `read_vortex` requires one schema
//! per scan (it has no `union_by_name`). A future [`SCHEMA_VERSION`] that changes columns
//! therefore migrates partitions during compaction rather than mixing schemas.

use std::sync::Arc;

use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};

/// Version written into the `schema_version` column of every row.
pub const SCHEMA_VERSION: i32 = 1;

/// Timezone attached to every `ts` column, which DuckDB reads as `TIMESTAMP WITH TIME ZONE`.
pub const TS_TIMEZONE: &str = "UTC";

/// Logical column types used by the lake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColType {
    /// 32-bit signed integer (`INTEGER`).
    Int32,
    /// 64-bit signed integer (`BIGINT`). Unsigned kernel counters are stored here; they
    /// saturate at `i64::MAX`, which is never reached in practice.
    Int64,
    /// 64-bit float (`DOUBLE`).
    Float64,
    /// Boolean (`BOOLEAN`).
    Bool,
    /// UTF-8 string (`VARCHAR`).
    Utf8,
    /// Microsecond UTC timestamp (`TIMESTAMP WITH TIME ZONE`).
    Timestamp,
}

impl ColType {
    /// The Arrow type used when writing files.
    pub fn arrow_type(self) -> DataType {
        match self {
            ColType::Int32 => DataType::Int32,
            ColType::Int64 => DataType::Int64,
            ColType::Float64 => DataType::Float64,
            ColType::Bool => DataType::Boolean,
            ColType::Utf8 => DataType::Utf8,
            ColType::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, Some(TS_TIMEZONE.into())),
        }
    }

    /// The DuckDB SQL type of the column as DuckDB reads it back.
    pub fn duckdb_type(self) -> &'static str {
        match self {
            ColType::Int32 => "INTEGER",
            ColType::Int64 => "BIGINT",
            ColType::Float64 => "DOUBLE",
            ColType::Bool => "BOOLEAN",
            ColType::Utf8 => "VARCHAR",
            ColType::Timestamp => "TIMESTAMP WITH TIME ZONE",
        }
    }
}

/// One column of a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Column {
    /// Column name.
    pub name: &'static str,
    /// Column type.
    pub ty: ColType,
    /// Whether the column may contain NULLs.
    pub nullable: bool,
    /// One-line description, shown by `DESCRIBE`-like tooling.
    pub doc: &'static str,
}

const fn col(name: &'static str, ty: ColType, doc: &'static str) -> Column {
    Column { name, ty, nullable: false, doc }
}

const fn opt(name: &'static str, ty: ColType, doc: &'static str) -> Column {
    Column { name, ty, nullable: true, doc }
}

/// How a table's files are partitioned on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Granularity {
    /// `<table>/date=YYYY-MM-DD/hour=HH/`.
    Hour,
    /// `<table>/date=YYYY-MM-DD/`. Used by rollups, which are small, so that a year of
    /// hourly rollups is 365 files rather than 8760.
    Day,
}

/// Rollup bucket width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bucket {
    /// One row per group per minute.
    Minute,
    /// One row per group per hour.
    Hour,
}

impl Bucket {
    /// Bucket width in seconds.
    pub fn seconds(self) -> i64 {
        match self {
            Bucket::Minute => 60,
            Bucket::Hour => 3600,
        }
    }
}

/// What a table holds, which decides how maintenance treats it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableKind {
    /// Raw samples written by the daemons; retention tier `raw`.
    Raw,
    /// Aggregates of `source` computed by compaction; retention tier `rollup_1m`/`rollup_1h`.
    Rollup {
        /// The raw table aggregated.
        source: &'static str,
        /// Bucket width.
        bucket: Bucket,
    },
    /// Benchmark results. Never expired by TTL or size pruning; removed only by an
    /// explicit `delete` or `purge`.
    Bench,
}

/// A table in the lake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableDef {
    /// Table (and directory) name.
    pub name: &'static str,
    /// Kind of table.
    pub kind: TableKind,
    /// Columns in file order.
    pub columns: &'static [Column],
    /// Sort order used when compaction or deletion rewrites a partition.
    pub sort_key: &'static [&'static str],
    /// One-line description.
    pub doc: &'static str,
}

impl TableDef {
    /// On-disk partitioning of this table.
    pub fn granularity(&self) -> Granularity {
        match self.kind {
            TableKind::Rollup { .. } => Granularity::Day,
            TableKind::Raw | TableKind::Bench => Granularity::Hour,
        }
    }

    /// Looks up a column by name.
    pub fn column(&self, name: &str) -> Option<&'static Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// The Arrow schema used to write this table.
    pub fn arrow_schema(&self) -> SchemaRef {
        Arc::new(Schema::new(
            self.columns.iter().map(|c| Field::new(c.name, c.ty.arrow_type(), c.nullable)).collect::<Vec<_>>(),
        ))
    }

    /// Rollup tables derived from this raw table.
    pub fn rollups(&self) -> impl Iterator<Item = &'static TableDef> + '_ {
        TABLES.iter().filter(move |t| matches!(t.kind, TableKind::Rollup { source, .. } if source == self.name))
    }
}

use ColType::*;

const HEAD: [Column; 2] = [
    col("schema_version", Int32, "lindesk store schema version of the row"),
    col("ts", Timestamp, "sample time (UTC); for rollups, the bucket start"),
];

const PRESSURE: &[Column] = &[
    HEAD[0],
    HEAD[1],
    col("scope", Utf8, "'system' or a cgroup path relative to the cgroup mount"),
    col("resource", Utf8, "cpu | io | memory"),
    col("some_avg10", Float64, "PSI some avg10 (percent)"),
    opt("full_avg10", Float64, "PSI full avg10 (percent); NULL for system cpu"),
    col("some_delta_us", Int64, "some stall time accrued during the interval (µs)"),
    opt("full_delta_us", Int64, "full stall time accrued during the interval (µs)"),
    col("interval_ms", Int64, "wall time covered by the deltas (ms)"),
];

const UNITS: &[Column] = &[
    HEAD[0],
    HEAD[1],
    opt("manager", Utf8, "'system' or 'user' for unit targets, NULL otherwise"),
    opt("uid", Int64, "owning uid for user-manager units"),
    col("unit", Utf8, "unit name, or 'cgroup:<path>' / 'pid:<pid>' for other targets"),
    col("class", Utf8, "QoS class"),
    col("foreground", Bool, "target held input focus"),
    col("cpu_usage_us", Int64, "CPU time used during the interval (µs)"),
    col("io_rbytes", Int64, "bytes read during the interval"),
    col("io_wbytes", Int64, "bytes written during the interval"),
    col("io_rios", Int64, "read I/Os during the interval"),
    col("io_wios", Int64, "write I/Os during the interval"),
    col("mem_current", Int64, "memory.current at sample time (bytes)"),
    col("interval_ms", Int64, "wall time covered by the deltas (ms)"),
    col("applied", Utf8, "JSON array of settings lindesk had applied"),
];

const EVENTS: &[Column] = &[
    HEAD[0],
    HEAD[1],
    col("kind", Utf8, "focus | interaction | class_set | level | action_applied | action_reverted | plugin_error"),
    opt("unit", Utf8, "target unit (flattened like units.unit)"),
    opt("uid", Int64, "owning uid of a user-manager target"),
    opt("app_id", Utf8, "desktop application id"),
    opt("pid", Int32, "process id"),
    opt("plugin", Utf8, "plugin, policy or adapter that produced the event"),
    opt("reason", Utf8, "short human-readable reason or message"),
    col("payload", Utf8, "the complete event as JSON"),
];

const METRICS: &[Column] = &[
    HEAD[0],
    HEAD[1],
    col("plugin", Utf8, "plugin that emitted the metric"),
    col("name", Utf8, "metric name"),
    col("value", Float64, "metric value"),
    col("labels", Utf8, "metric labels as a JSON object; query with labels->>'key'"),
];

const BENCH_RUNS: &[Column] = &[
    HEAD[0],
    HEAD[1],
    col("run_id", Utf8, "unique run id (ULID)"),
    col("label", Utf8, "human label for the run"),
    opt("host", Utf8, "hostname"),
    opt("kernel", Utf8, "kernel release"),
    col("config", Utf8, "benchmark configuration as JSON"),
    opt("notes", Utf8, "free-form notes"),
];

const BENCH_SAMPLES: &[Column] = &[
    HEAD[0],
    HEAD[1],
    col("run_id", Utf8, "bench_runs.run_id"),
    col("phase", Utf8, "lindesk_on | lindesk_off"),
    col("metric", Utf8, "measurement name, e.g. wakeup_overshoot_us"),
    col("value", Float64, "measured value"),
    opt("iteration", Int64, "repetition number within the phase"),
    col("labels", Utf8, "extra dimensions as a JSON object (device, probe, ...)"),
];

/// Columns of `pressure_1m` / `pressure_1h`.
const PRESSURE_ROLLUP: &[Column] = &[
    HEAD[0],
    HEAD[1],
    col("scope", Utf8, "see pressure.scope"),
    col("resource", Utf8, "see pressure.resource"),
    col("samples", Int64, "raw rows aggregated"),
    col("some_avg10_min", Float64, "min of some_avg10"),
    col("some_avg10_avg", Float64, "mean of some_avg10"),
    col("some_avg10_max", Float64, "max of some_avg10"),
    col("some_avg10_p95", Float64, "95th percentile (continuous) of some_avg10"),
    opt("full_avg10_min", Float64, "min of full_avg10"),
    opt("full_avg10_avg", Float64, "mean of full_avg10"),
    opt("full_avg10_max", Float64, "max of full_avg10"),
    opt("full_avg10_p95", Float64, "95th percentile (continuous) of full_avg10"),
    col("some_delta_us", Int64, "sum of some_delta_us"),
    opt("full_delta_us", Int64, "sum of full_delta_us"),
    col("interval_ms", Int64, "sum of interval_ms"),
];

/// Columns of `units_1m` / `units_1h`.
const UNITS_ROLLUP: &[Column] = &[
    HEAD[0],
    HEAD[1],
    opt("manager", Utf8, "see units.manager"),
    opt("uid", Int64, "see units.uid"),
    col("unit", Utf8, "see units.unit"),
    col("class", Utf8, "class of the latest sample in the bucket"),
    col("foreground", Bool, "foreground in any sample of the bucket"),
    col("samples", Int64, "raw rows aggregated"),
    col("cpu_usage_us", Int64, "sum of cpu_usage_us"),
    col("io_rbytes", Int64, "sum of io_rbytes"),
    col("io_wbytes", Int64, "sum of io_wbytes"),
    col("io_rios", Int64, "sum of io_rios"),
    col("io_wios", Int64, "sum of io_wios"),
    col("mem_current_min", Int64, "min of mem_current"),
    col("mem_current_avg", Float64, "mean of mem_current"),
    col("mem_current_max", Int64, "max of mem_current"),
    col("mem_current_p95", Float64, "95th percentile (continuous) of mem_current"),
    col("interval_ms", Int64, "sum of interval_ms"),
];

/// Every table in the lake, raw tables first.
pub const TABLES: &[TableDef] = &[
    TableDef {
        name: "pressure",
        kind: TableKind::Raw,
        columns: PRESSURE,
        sort_key: &["scope", "resource", "ts"],
        doc: "PSI samples",
    },
    TableDef {
        name: "units",
        kind: TableKind::Raw,
        columns: UNITS,
        sort_key: &["unit", "ts"],
        doc: "per-target resource usage",
    },
    TableDef {
        name: "events",
        kind: TableKind::Raw,
        columns: EVENTS,
        sort_key: &["unit", "ts"],
        doc: "focus, interaction, class, level, action and plugin events",
    },
    TableDef {
        name: "metrics",
        kind: TableKind::Raw,
        columns: METRICS,
        sort_key: &["plugin", "name", "ts"],
        doc: "plugin-defined metrics",
    },
    TableDef {
        name: "bench_runs",
        kind: TableKind::Bench,
        columns: BENCH_RUNS,
        sort_key: &["run_id", "ts"],
        doc: "benchmark runs",
    },
    TableDef {
        name: "bench_samples",
        kind: TableKind::Bench,
        columns: BENCH_SAMPLES,
        sort_key: &["run_id", "phase", "metric", "ts"],
        doc: "benchmark measurements",
    },
    TableDef {
        name: "pressure_1m",
        kind: TableKind::Rollup { source: "pressure", bucket: Bucket::Minute },
        columns: PRESSURE_ROLLUP,
        sort_key: &["scope", "resource", "ts"],
        doc: "pressure per minute",
    },
    TableDef {
        name: "pressure_1h",
        kind: TableKind::Rollup { source: "pressure", bucket: Bucket::Hour },
        columns: PRESSURE_ROLLUP,
        sort_key: &["scope", "resource", "ts"],
        doc: "pressure per hour",
    },
    TableDef {
        name: "units_1m",
        kind: TableKind::Rollup { source: "units", bucket: Bucket::Minute },
        columns: UNITS_ROLLUP,
        sort_key: &["unit", "ts"],
        doc: "units per minute",
    },
    TableDef {
        name: "units_1h",
        kind: TableKind::Rollup { source: "units", bucket: Bucket::Hour },
        columns: UNITS_ROLLUP,
        sort_key: &["unit", "ts"],
        doc: "units per hour",
    },
];

/// Looks up a table definition by name.
pub fn table(name: &str) -> Option<&'static TableDef> {
    TABLES.iter().find(|t| t.name == name)
}

/// The Arrow schema of a table, or `None` for an unknown table.
pub fn schema(name: &str) -> Option<SchemaRef> {
    table(name).map(TableDef::arrow_schema)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_starts_with_version_and_ts_and_sorts_by_known_columns() {
        for t in TABLES {
            assert_eq!(t.columns[0].name, "schema_version", "{}", t.name);
            assert_eq!(t.columns[1].name, "ts", "{}", t.name);
            for k in t.sort_key {
                assert!(t.column(k).is_some(), "{}: sort key {k}", t.name);
            }
            if let TableKind::Rollup { source, .. } = t.kind {
                assert!(table(source).is_some_and(|s| s.kind == TableKind::Raw));
            }
            assert_eq!(t.arrow_schema().fields().len(), t.columns.len());
        }
        assert_eq!(table("pressure").unwrap().rollups().count(), 2);
    }
}
