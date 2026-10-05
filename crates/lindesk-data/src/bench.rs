//! `lindesk-data import-bench`: a `lindesk-bench` results file into `bench_runs` and
//! `bench_samples`.
//!
//! Reads only what it stores, so results files of any version import: `run_id`, `label`,
//! `host.{hostname,kernel}`, `config` and per phase `phase` (`off`/`on`), `started_ms`,
//! `metrics` (`[{name, value, labels}]`: the flat summary lindesk-bench computes) and
//! `timeline` (`[{t_s, level, psi: {cpu|io|memory: {some_pct}}}]`, stored as the
//! `psi_some_pct` series with `series = timeline`).

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use lindesk_store::{BenchPhase, BenchRun, BenchSample, Store, StoreOptions, wall_clock_ms};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize)]
struct Results {
    run_id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    host: Value,
    #[serde(default)]
    config: Value,
    #[serde(default)]
    phases: Vec<Phase>,
}

#[derive(Deserialize)]
struct Phase {
    phase: String,
    started_ms: i64,
    #[serde(default)]
    metrics: Vec<Metric>,
    #[serde(default)]
    timeline: Vec<Point>,
}

#[derive(Deserialize)]
struct Metric {
    name: String,
    value: f64,
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Point {
    t_s: f64,
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    psi: BTreeMap<String, Option<Stall>>,
}

#[derive(Deserialize)]
struct Stall {
    some_pct: f64,
}

/// What was stored.
#[derive(Serialize, Debug)]
pub struct Imported {
    pub run_id: String,
    pub rows: usize,
}

/// The run row and sample rows of a results document.
fn rows(r: &Results) -> Result<(BenchRun, Vec<BenchSample>)> {
    let started_ms = r.phases.first().map_or_else(wall_clock_ms, |p| p.started_ms);
    let host = |k: &str| r.host.get(k).and_then(Value::as_str).map(str::to_string);
    let run = BenchRun {
        run_id: r.run_id.clone(),
        ts: started_ms,
        label: r.label.clone(),
        host: host("hostname"),
        kernel: host("kernel"),
        config: serde_json::json!({ "config": r.config, "host": r.host }),
        notes: None,
    };
    let mut samples = Vec::new();
    for p in &r.phases {
        let phase = match p.phase.as_str() {
            "off" => BenchPhase::LindeskOff,
            "on" => BenchPhase::LindeskOn,
            other => bail!("unknown phase '{other}' (expected off or on)"),
        };
        for m in &p.metrics {
            samples.push(BenchSample {
                ts: p.started_ms,
                run_id: r.run_id.clone(),
                phase,
                metric: m.name.clone(),
                value: m.value,
                iteration: None,
                labels: m.labels.clone(),
            });
        }
        // The once-a-second timeline, as a series.
        for (i, t) in p.timeline.iter().enumerate() {
            let ts = p.started_ms + (t.t_s * 1000.0) as i64;
            for (res, stall) in &t.psi {
                let Some(s) = stall else { continue };
                let mut labels = BTreeMap::from([
                    ("series".to_string(), "timeline".to_string()),
                    ("scope".into(), "system".into()),
                    ("resource".into(), res.clone()),
                ]);
                if let Some(l) = &t.level {
                    labels.insert("level".into(), l.clone());
                }
                samples.push(BenchSample {
                    ts,
                    run_id: r.run_id.clone(),
                    phase,
                    metric: "psi_some_pct".into(),
                    value: s.some_pct,
                    iteration: Some(i as u32),
                    labels,
                });
            }
        }
    }
    Ok((run, samples))
}

/// Stores `results` into the lake at `root`.
pub fn import(root: &Path, results: &Path) -> Result<Imported> {
    let text = std::fs::read_to_string(results).with_context(|| format!("reading {}", results.display()))?;
    let r: Results = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a lindesk-bench results file", results.display()))?;
    let (run, samples) = rows(&r)?;
    let mut store = Store::open(root, StoreOptions::user()).with_context(|| format!("opening {}", root.display()))?;
    store.append_bench(&run, &samples);
    let rep = store.flush_wait(wall_clock_ms(), Duration::from_secs(10))?;
    if rep.deferred_rows > 0 {
        bail!("the lake at {} is locked by maintenance; {} rows not written", root.display(), rep.deferred_rows);
    }
    Ok(Imported { run_id: r.run_id, rows: rep.rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"{
        "version": 1, "tool": "lindesk-bench", "run_id": "01RUN", "label": "t",
        "host": {"hostname": "box", "kernel": "6.8"}, "config": {"profile": "quick"},
        "phases": [
            {"phase": "off", "started_ms": 1000,
             "metrics": [{"name": "wakeup_overshoot_us", "value": 2400.0, "labels": {"stat": "p99"}}],
             "timeline": [{"t_s": 1.0, "psi": {"cpu": {"some_pct": 3.0, "some_us": 1}, "io": null}, "mem_available": 1}]},
            {"phase": "on", "started_ms": 5000, "timeline": [{"t_s": 0.5, "level": "red", "psi": {"io": {"some_pct": 9.0}}}]}
        ]
    }"#;

    #[test]
    fn flattens_a_results_file() {
        let r: Results = serde_json::from_str(DOC).unwrap();
        let (run, s) = rows(&r).unwrap();
        assert_eq!((run.ts, run.host.as_deref(), run.kernel.as_deref()), (1000, Some("box"), Some("6.8")));
        assert_eq!(s.len(), 3);
        assert_eq!(
            (s[0].metric.as_str(), s[0].value, s[0].phase),
            ("wakeup_overshoot_us", 2400.0, BenchPhase::LindeskOff)
        );
        assert_eq!((s[1].ts, s[1].labels["resource"].as_str()), (2000, "cpu"));
        assert_eq!((s[2].phase, s[2].labels["level"].as_str(), s[2].ts), (BenchPhase::LindeskOn, "red", 5500));
    }

    #[test]
    fn imports_into_a_lake() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("results.json");
        std::fs::write(&f, DOC).unwrap();
        let root = t.path().join("telemetry");
        let r = import(&root, &f).unwrap();
        assert_eq!((r.run_id.as_str(), r.rows), ("01RUN", 4));
        let lake = lindesk_query::Lake::open(&root).unwrap().with_backend(lindesk_query::BackendChoice::Native);
        let out = lindesk_query::Format::Json;
        let n = lake.query("select count(*) as n from bench_samples", &out).unwrap();
        assert!(n.contains("\"n\":3") || n.contains("\"n\": 3"), "{n}");
        std::fs::write(&f, "{}").unwrap();
        assert!(import(&root, &f).is_err());
    }
}
