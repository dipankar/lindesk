//! Flattening of [`Record`]s and bench data into table rows, and rows into Arrow batches.

use std::collections::BTreeMap;
use std::sync::Arc;

use arrow_array::builder::{BooleanBuilder, Float64Builder, Int32Builder, Int64Builder, StringBuilder};
use arrow_array::types::TimestampMicrosecondType;
use arrow_array::{ArrayRef, PrimitiveArray, RecordBatch};
use arrow_schema::ArrowError;
use lindesk_plugin_api::{Event, Manager, Record, Target, TimestampMs};
use serde::{Deserialize, Serialize};

use crate::schema::{ColType, SCHEMA_VERSION, TS_TIMEZONE, TableDef, table};

/// One value of a row, before it is turned into an Arrow column.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Cell {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
}

impl From<Option<String>> for Cell {
    fn from(v: Option<String>) -> Self {
        v.map_or(Cell::Null, Cell::Str)
    }
}

/// A row destined for one table.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Row {
    pub table: &'static TableDef,
    pub ts: TimestampMs,
    pub cells: Vec<Cell>,
}

/// Labels are stored as a JSON object string (Vortex has no MAP type); query them with
/// `labels->>'key'`.
fn labels_json(labels: &BTreeMap<String, String>) -> Cell {
    Cell::Str(serde_json::to_string(labels).unwrap_or_else(|_| "{}".into()))
}

fn sat(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn def(name: &str) -> &'static TableDef {
    table(name).expect("built-in table")
}

/// `(manager, uid, unit)` for a target, as stored in the `units` and `events` tables.
pub fn flatten_target(target: &Target) -> (Option<&'static str>, Option<i64>, String) {
    match target {
        Target::Unit { manager: Manager::System, name } => (Some("system"), None, name.clone()),
        Target::Unit { manager: Manager::User(uid), name } => (Some("user"), Some(i64::from(*uid)), name.clone()),
        other => (None, None, other.to_string()),
    }
}

/// Converts a record into a row of its table.
pub(crate) fn record_row(record: &Record) -> Row {
    let v = Cell::Int(i64::from(SCHEMA_VERSION));
    match record {
        Record::Pressure(s) => Row {
            table: def("pressure"),
            ts: s.ts,
            cells: vec![
                v,
                Cell::Int(s.ts),
                Cell::Str(s.scope.to_string()),
                Cell::Str(s.resource.as_str().into()),
                Cell::Float(f64::from(s.some_avg10)),
                s.full_avg10.map_or(Cell::Null, |f| Cell::Float(f64::from(f))),
                Cell::Int(sat(s.some_delta_us)),
                s.full_delta_us.map_or(Cell::Null, |d| Cell::Int(sat(d))),
                Cell::Int(i64::from(s.interval_ms)),
            ],
        },
        Record::Unit { ts, target, class, foreground, usage, applied } => {
            let (manager, uid, unit) = flatten_target(target);
            Row {
                table: def("units"),
                ts: *ts,
                cells: vec![
                    v,
                    Cell::Int(*ts),
                    manager.map(String::from).into(),
                    uid.map_or(Cell::Null, Cell::Int),
                    Cell::Str(unit),
                    Cell::Str(class.as_str().into()),
                    Cell::Bool(*foreground),
                    Cell::Int(sat(usage.cpu_usage_us)),
                    Cell::Int(sat(usage.io_rbytes)),
                    Cell::Int(sat(usage.io_wbytes)),
                    Cell::Int(sat(usage.io_rios)),
                    Cell::Int(sat(usage.io_wios)),
                    Cell::Int(sat(usage.mem_current)),
                    Cell::Int(i64::from(usage.interval_ms)),
                    Cell::Str(serde_json::to_string(applied).unwrap_or_else(|_| "[]".into())),
                ],
            }
        }
        Record::Event(e) => event_row(e),
        Record::Metric(m) => Row {
            table: def("metrics"),
            ts: m.ts,
            cells: vec![
                v,
                Cell::Int(m.ts),
                Cell::Str(m.plugin.clone()),
                Cell::Str(m.name.clone()),
                Cell::Float(m.value),
                labels_json(&m.labels),
            ],
        },
    }
}

/// Columns of the `events` table that are lifted out of the JSON payload.
#[derive(Default)]
struct EventCols {
    target: Option<Target>,
    app_id: Option<String>,
    pid: Option<i32>,
    plugin: Option<String>,
    reason: Option<String>,
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

fn event_row(e: &Event) -> Row {
    let c = match e {
        Event::Focus { pid, app_id, target, source, .. } => EventCols {
            target: target.clone(),
            app_id: app_id.clone(),
            pid: *pid,
            plugin: non_empty(source),
            reason: None,
        },
        Event::Interaction { pid, target, end, source, .. } => EventCols {
            target: target.clone(),
            pid: *pid,
            plugin: non_empty(source),
            reason: end.then(|| "end".to_string()),
            ..Default::default()
        },
        Event::ClassSet { target, class, source, .. } => EventCols {
            target: Some(target.clone()),
            plugin: non_empty(source),
            reason: Some(class.as_str().to_string()),
            ..Default::default()
        },
        Event::LevelChanged { from, to, policy, .. } => {
            EventCols { plugin: non_empty(policy), reason: Some(format!("{from}->{to}")), ..Default::default() }
        }
        Event::ActionApplied { target, setting, provenance, .. } => EventCols {
            target: Some(target.clone()),
            plugin: non_empty(&provenance.plugin),
            reason: Some(if provenance.reason.is_empty() {
                setting.to_string()
            } else {
                format!("{setting}: {}", provenance.reason)
            }),
            ..Default::default()
        },
        Event::ActionReverted { target, knob, reason, .. } => EventCols {
            target: Some(target.clone()),
            reason: Some(if reason.is_empty() { knob.to_string() } else { format!("{knob}: {reason}") }),
            ..Default::default()
        },
        Event::PluginError { plugin, message, .. } => {
            EventCols { plugin: non_empty(plugin), reason: Some(message.clone()), ..Default::default() }
        }
        Event::Notice { name, source, message, .. } => {
            EventCols { plugin: non_empty(source), reason: Some(format!("{name}: {message}")), ..Default::default() }
        }
        // `Event` is non-exhaustive; unknown kinds still land with their payload JSON.
        _ => EventCols::default(),
    };
    let (_, uid, unit) = c.target.as_ref().map(flatten_target).map_or((None, None, None), |(m, u, n)| (m, u, Some(n)));
    Row {
        table: def("events"),
        ts: e.ts(),
        cells: vec![
            Cell::Int(i64::from(SCHEMA_VERSION)),
            Cell::Int(e.ts()),
            Cell::Str(e.kind().into()),
            unit.into(),
            uid.map_or(Cell::Null, Cell::Int),
            c.app_id.into(),
            c.pid.map_or(Cell::Null, |p| Cell::Int(i64::from(p))),
            c.plugin.into(),
            c.reason.into(),
            Cell::Str(serde_json::to_string(e).unwrap_or_default()),
        ],
    }
}

/// Which side of an A/B benchmark a sample belongs to.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum BenchPhase {
    /// lindesk running.
    LindeskOn,
    /// lindesk stopped (baseline).
    LindeskOff,
}

impl BenchPhase {
    /// Stored string, `lindesk_on` or `lindesk_off`.
    pub fn as_str(self) -> &'static str {
        match self {
            BenchPhase::LindeskOn => "lindesk_on",
            BenchPhase::LindeskOff => "lindesk_off",
        }
    }
}

/// One benchmark run (a row of `bench_runs`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BenchRun {
    /// Unique id; [`BenchRun::new_id`] makes a ULID.
    pub run_id: String,
    /// Start time.
    pub ts: TimestampMs,
    /// Human label, e.g. `torture-fio-on-gnome`.
    pub label: String,
    /// Hostname.
    #[serde(default)]
    pub host: Option<String>,
    /// Kernel release.
    #[serde(default)]
    pub kernel: Option<String>,
    /// The benchmark configuration (load mix, durations, lindesk config, ...).
    #[serde(default)]
    pub config: serde_json::Value,
    /// Free-form notes.
    #[serde(default)]
    pub notes: Option<String>,
}

impl BenchRun {
    /// A fresh run id.
    pub fn new_id() -> String {
        ulid::Ulid::new().to_string()
    }
}

/// One measurement (a row of `bench_samples`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BenchSample {
    /// Measurement time.
    pub ts: TimestampMs,
    /// The run it belongs to.
    pub run_id: String,
    /// A/B phase.
    pub phase: BenchPhase,
    /// Measurement name, e.g. `wakeup_overshoot_us` or `read_latency_us`.
    pub metric: String,
    /// Value.
    pub value: f64,
    /// Repetition number within the phase.
    #[serde(default)]
    pub iteration: Option<u32>,
    /// Extra dimensions (device, probe, percentile, ...).
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

pub(crate) fn bench_run_row(r: &BenchRun) -> Row {
    Row {
        table: def("bench_runs"),
        ts: r.ts,
        cells: vec![
            Cell::Int(i64::from(SCHEMA_VERSION)),
            Cell::Int(r.ts),
            Cell::Str(r.run_id.clone()),
            Cell::Str(r.label.clone()),
            r.host.clone().into(),
            r.kernel.clone().into(),
            Cell::Str(if r.config.is_null() { "{}".into() } else { r.config.to_string() }),
            r.notes.clone().into(),
        ],
    }
}

pub(crate) fn bench_sample_row(s: &BenchSample) -> Row {
    Row {
        table: def("bench_samples"),
        ts: s.ts,
        cells: vec![
            Cell::Int(i64::from(SCHEMA_VERSION)),
            Cell::Int(s.ts),
            Cell::Str(s.run_id.clone()),
            Cell::Str(s.phase.as_str().into()),
            Cell::Str(s.metric.clone()),
            Cell::Float(s.value),
            s.iteration.map_or(Cell::Null, |i| Cell::Int(i64::from(i))),
            labels_json(&s.labels),
        ],
    }
}

/// Builds a record batch from rows of one table. Rows must have been produced for `def`.
pub(crate) fn to_batch(def: &TableDef, rows: &[Vec<Cell>]) -> Result<RecordBatch, ArrowError> {
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(def.columns.len());
    for (i, col) in def.columns.iter().enumerate() {
        let cells = rows.iter().map(|r| r.get(i).unwrap_or(&Cell::Null));
        let array: ArrayRef = match col.ty {
            ColType::Int32 => {
                let mut b = Int32Builder::with_capacity(rows.len());
                for c in cells {
                    b.append_option(match c {
                        Cell::Int(v) => Some(i32::try_from(*v).unwrap_or(i32::MAX)),
                        _ => None,
                    });
                }
                Arc::new(b.finish())
            }
            ColType::Int64 => {
                let mut b = Int64Builder::with_capacity(rows.len());
                for c in cells {
                    b.append_option(match c {
                        Cell::Int(v) => Some(*v),
                        _ => None,
                    });
                }
                Arc::new(b.finish())
            }
            ColType::Timestamp => {
                // Rows carry milliseconds; files store microseconds like DuckDB's TIMESTAMPTZ.
                let vals: Vec<Option<i64>> = cells
                    .map(|c| match c {
                        Cell::Int(v) => Some(v.saturating_mul(1000)),
                        _ => None,
                    })
                    .collect();
                Arc::new(PrimitiveArray::<TimestampMicrosecondType>::from(vals).with_timezone(TS_TIMEZONE))
            }
            ColType::Float64 => {
                let mut b = Float64Builder::with_capacity(rows.len());
                for c in cells {
                    b.append_option(match c {
                        Cell::Float(v) => Some(*v),
                        _ => None,
                    });
                }
                Arc::new(b.finish())
            }
            ColType::Bool => {
                let mut b = BooleanBuilder::with_capacity(rows.len());
                for c in cells {
                    b.append_option(match c {
                        Cell::Bool(v) => Some(*v),
                        _ => None,
                    });
                }
                Arc::new(b.finish())
            }
            ColType::Utf8 => {
                let mut b = StringBuilder::new();
                for c in cells {
                    match c {
                        Cell::Str(s) => b.append_value(s),
                        _ => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
        };
        arrays.push(array);
    }
    RecordBatch::try_new(def.arrow_schema(), arrays)
}

#[cfg(test)]
mod tests {
    use lindesk_plugin_api::*;

    use super::*;

    #[test]
    fn rows_match_their_schema() {
        let target = Target::user_unit(1000, "app-firefox.scope");
        let records = vec![
            Record::Pressure(Sample {
                ts: 1,
                scope: Scope::System,
                resource: Resource::Io,
                some_avg10: 1.5,
                full_avg10: None,
                some_delta_us: 10,
                full_delta_us: Some(u64::MAX),
                interval_ms: 1000,
            }),
            Record::Unit {
                ts: 2,
                target: target.clone(),
                class: QosClass::Interactive,
                foreground: true,
                usage: Usage::default(),
                applied: vec![Setting::CpuWeight { value: 200 }],
            },
            Record::Event(Event::Focus {
                ts: 3,
                pid: Some(42),
                app_id: Some("org.mozilla.firefox".into()),
                target: Some(target),
                source: "x11".into(),
            }),
            Record::Metric(Metric {
                ts: 4,
                name: "frames".into(),
                value: 2.0,
                labels: [("k".to_string(), "v".to_string())].into(),
                plugin: "gnome".into(),
            }),
        ];
        for r in &records {
            let row = record_row(r);
            assert_eq!(row.table.name, r.table());
            assert_eq!(row.cells.len(), row.table.columns.len());
            let batch = to_batch(row.table, std::slice::from_ref(&row.cells)).unwrap();
            assert_eq!(batch.num_rows(), 1);
        }
        let ev = record_row(&records[2]);
        assert_eq!(ev.cells[3], Cell::Str("app-firefox.scope".into()));
        assert_eq!(ev.cells[4], Cell::Int(1000));
        assert_eq!(ev.cells[5], Cell::Str("org.mozilla.firefox".into()));
        assert_eq!(flatten_target(&Target::Process { pid: 7 }), (None, None, "pid:7".into()));
    }
}
