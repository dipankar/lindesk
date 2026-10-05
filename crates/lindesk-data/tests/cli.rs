//! The command line the core binaries rely on (`lindeskctl`, `lindesk-mcp`, `lindesk-bench`
//! run `lindesk-data` instead of linking DuckDB): `--json` reports, `query --format json`,
//! `export`, `import-bench` and `man`.

use std::path::Path;
use std::process::{Command, Output};

use lindesk_plugin_api::{Event, Level, Record, Resource, Sample, Scope};

const T0: i64 = 1_791_000_000_000;

fn data(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lindesk-data"))
        .arg("--root")
        .arg(root)
        .arg("--config")
        .arg(root.join("none.toml"))
        .arg("--backend")
        .arg("native")
        .args(args)
        .output()
        .expect("run lindesk-data")
}

fn ok(o: &Output) -> String {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn write_lake(root: &Path) {
    let mut store = lindesk_store::Store::open(root, lindesk_store::StoreOptions::user()).unwrap();
    let mut recs: Vec<Record> = (0..10)
        .flat_map(|i| {
            Resource::ALL.into_iter().map(move |r| {
                Record::Pressure(Sample {
                    ts: T0 + i * 1000,
                    scope: Scope::System,
                    resource: r,
                    some_avg10: if r == Resource::Io { 20.0 } else { 2.0 },
                    full_avg10: None,
                    some_delta_us: 1000,
                    full_delta_us: None,
                    interval_ms: 1000,
                })
            })
        })
        .collect();
    recs.push(Record::Event(Event::LevelChanged {
        ts: T0 + 5000,
        from: Level::Green,
        to: Level::Red,
        policy: "threshold".into(),
    }));
    store.append(&recs);
    store.flush(T0 + 100_000).unwrap();
}

#[test]
fn export_query_and_reports() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("telemetry");
    write_lake(&root);

    // export: one JSON object per line, ts in epoch ms, window half-open.
    let out = ok(&data(
        &root,
        &["export", "--table", "pressure", "--from", &(T0 + 2000).to_string(), "--to", &(T0 + 4000).to_string()],
    ));
    let rows: Vec<serde_json::Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(rows.len(), 6, "{out}");
    assert_eq!(rows[0]["ts"], T0 + 2000);
    assert!(rows.iter().all(|r| r["resource"].is_string() && r["some_avg10"].is_number()), "{out}");
    let ev = ok(&data(&root, &["export", "--table", "events", "--format", "json"]));
    let ev: serde_json::Value = serde_json::from_str(&ev).unwrap();
    assert_eq!(ev[0]["kind"], "level");
    let payload: Event = serde_json::from_str(ev[0]["payload"].as_str().unwrap()).unwrap();
    assert!(matches!(payload, Event::LevelChanged { to: Level::Red, .. }));
    let o = data(&root, &["export", "--table", "nope"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("unknown table 'nope'"));

    // query --format json, and the hints on errors.
    let q = ok(&data(&root, &["query", "select count(*) as n from pressure", "--format", "json"]));
    let q: serde_json::Value = serde_json::from_str(&q).unwrap();
    assert_eq!(q[0]["n"], 30);
    let o = data(&root, &["query", "delete from pressure"]);
    assert!(String::from_utf8_lossy(&o.stderr).contains("read-only"));

    // --json reports parse as what lindeskctl renders.
    let s: serde_json::Value = serde_json::from_str(&ok(&data(&root, &["--json", "stats"]))).unwrap();
    assert!(s["tables"].as_array().unwrap().iter().any(|t| t["table"] == "pressure" && t["rows"] == 30), "{s}");
    let del: serde_json::Value = serde_json::from_str(&ok(&data(
        &root,
        &["--json", "delete", "--where", "resource = 'cpu'", "--since", &T0.to_string()],
    )))
    .unwrap();
    assert_eq!(del["dry_run"], true);
    assert_eq!(del["tables"][0]["rows"], 10);
}

#[test]
fn import_bench_and_man() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("telemetry");
    let results = d.path().join("bench-results.json");
    std::fs::write(
        &results,
        r#"{"run_id": "01TEST", "label": "ci", "host": {"hostname": "h"}, "config": {},
            "phases": [{"phase": "on", "started_ms": 1791000000000,
                        "metrics": [{"name": "read_latency_us", "value": 950.0, "labels": {"stat": "p99"}}],
                        "timeline": []}]}"#,
    )
    .unwrap();
    let out = ok(&data(&root, &["import-bench", results.to_str().unwrap()]));
    assert!(out.starts_with("stored run 01TEST (2 rows)"), "{out}");
    let q = ok(&data(&root, &["query", "select metric, value from bench_samples", "--format", "json"]));
    assert!(q.contains("read_latency_us") && q.contains("950"), "{q}");

    let man = d.path().join("man");
    ok(&data(&root, &["man", man.to_str().unwrap()]));
    assert!(std::fs::read_to_string(man.join("lindesk-data.1")).unwrap().starts_with(".TH LINDESK\\-DATA 1"));
}
