//! Shared helpers for the lake integration tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use lindesk_plugin_api::*;
use lindesk_query::{Backend, BackendChoice, Format, Lake};
use lindesk_store::{Store, StoreOptions};

/// 2026-01-01T00:00:00Z.
pub const BASE: i64 = 1_767_225_600_000;
pub const HOUR: i64 = 3_600_000;
pub const DAY: i64 = 24 * HOUR;

pub fn store(root: &Path) -> Store {
    Store::open(root, StoreOptions::user()).unwrap()
}

pub fn pressure(ts: i64, scope: &str, resource: Resource, some: f32, delta: u64) -> Record {
    Record::Pressure(Sample {
        ts,
        scope: if scope == "system" { Scope::System } else { Scope::Cgroup(scope.into()) },
        resource,
        some_avg10: some,
        full_avg10: (resource != Resource::Cpu).then_some(some / 2.0),
        some_delta_us: delta,
        full_delta_us: (resource != Resource::Cpu).then_some(delta / 2),
        interval_ms: 1000,
    })
}

pub fn unit(ts: i64, name: &str, class: QosClass, foreground: bool, cpu: u64, mem: u64) -> Record {
    Record::Unit {
        ts,
        target: Target::user_unit(1000, name),
        class,
        foreground,
        usage: Usage {
            cpu_usage_us: cpu,
            io_rbytes: cpu * 2,
            io_wbytes: cpu * 3,
            io_rios: 1,
            io_wios: 2,
            mem_current: mem,
            interval_ms: 1000,
        },
        applied: vec![Setting::CpuWeight { value: 200 }],
    }
}

pub fn focus(ts: i64, unit: &str, app_id: &str) -> Record {
    Record::Event(Event::Focus {
        ts,
        pid: Some(4242),
        app_id: Some(app_id.into()),
        target: Some(Target::user_unit(1000, unit)),
        source: "x11".into(),
    })
}

pub fn metric(ts: i64, name: &str, value: f64, host: &str) -> Record {
    Record::Metric(Metric {
        ts,
        name: name.into(),
        value,
        labels: [("host".to_string(), host.to_string())].into(),
        plugin: "gnome".into(),
    })
}

/// A realistic mix for `hours` hours starting at `start`: every `step_s` seconds one
/// pressure sample per resource, two units and, every minute, a focus event and a metric.
pub fn mix(start: i64, hours: i64, step_s: i64) -> Vec<Record> {
    let mut out = Vec::new();
    let mut t = start;
    let mut i: u64 = 0;
    while t < start + hours * HOUR {
        for (k, r) in Resource::ALL.into_iter().enumerate() {
            out.push(pressure(t, "system", r, ((i * 7 + k as u64) % 100) as f32 / 3.0, i * 13 % 5000));
        }
        out.push(unit(t, "app-firefox.scope", QosClass::Interactive, i.is_multiple_of(2), i * 11 % 9000, 1 << 30 | i));
        out.push(unit(t, "app-make.scope", QosClass::Batch, false, i * 17 % 9000, 1 << 28 | (i * 3)));
        if (t / 1000) % 60 == 0 {
            out.push(focus(t, "app-firefox.scope", "org.mozilla.firefox"));
            out.push(focus(t, "app-gnome-terminal.scope", "org.gnome.Terminal"));
            out.push(metric(t, "frame_overruns", (i % 4) as f64, "laptop"));
        }
        t += step_s * 1000;
        i += 1;
    }
    out
}

/// `SELECT count(*)`-style scalar query.
pub fn scalar(lake: &Lake, sql: &str) -> i64 {
    let out = lake.query(sql, &Format::Csv).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let v = out.lines().nth(1).unwrap_or_else(|| panic!("{sql}: no rows in {out:?}"));
    v.trim().parse().unwrap_or_else(|_| panic!("{sql}: not an integer: {v:?}"))
}

pub fn count(lake: &Lake, table: &str) -> i64 {
    scalar(lake, &format!("SELECT count(*) FROM {table}"))
}

/// All data files (`.vortex`, and legacy `.parquet`) under a directory.
pub fn data_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(data_files(&p));
        } else if p.extension().is_some_and(|e| e == "vortex" || e == "parquet") {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// Shared DuckDB extension directory, so the vortex extension is downloaded once per build
/// rather than once per test.
pub fn ext_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("duckdb-ext")
}

static INSTALL: std::sync::Once = std::sync::Once::new();

/// Opens a lake with the given backend choice and the shared extension directory.
pub fn lake(root: &Path, choice: BackendChoice) -> Lake {
    INSTALL.call_once(|| {
        let probe = Lake::open(root).unwrap().with_extension_dir(ext_dir());
        if let Err(e) = probe.install_extension() {
            eprintln!("vortex extension unavailable ({e}); extension tests use the native fallback");
        }
    });
    let lake = Lake::open(root).unwrap().with_extension_dir(ext_dir()).with_backend(choice);
    if choice == BackendChoice::Native {
        assert_eq!(lake.backend().unwrap(), Backend::Native);
    }
    lake
}

/// Runs a test body once with the extension (when available) and once natively.
#[macro_export]
macro_rules! both_backends {
    ($name:ident, $body:ident) => {
        mod $name {
            #[test]
            fn extension() {
                super::$body(lindesk_query::BackendChoice::Auto);
            }
            #[test]
            fn native() {
                super::$body(lindesk_query::BackendChoice::Native);
            }
        }
    };
}
