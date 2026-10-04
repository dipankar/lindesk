//! Storing results in the telemetry lake (`bench_runs` / `bench_samples`) through
//! lindesk-store. Optional: the results JSON is the primary output, and builds without the
//! `lake` feature skip this.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;

use crate::results::{PhaseResult, Results};

/// One summary measurement of a phase.
#[derive(Clone, Debug, PartialEq)]
pub struct Metric {
    pub name: &'static str,
    pub value: f64,
    pub labels: BTreeMap<String, String>,
}

fn m(name: &'static str, value: f64, labels: &[(&str, &str)]) -> Metric {
    Metric { name, value, labels: labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
}

/// Flattens a phase into named metrics (the rows of `bench_samples`).
pub fn phase_metrics(p: &PhaseResult) -> Vec<Metric> {
    let mut out = Vec::new();
    for (name, h) in [
        ("wakeup_overshoot_us", &p.probe.wakeup),
        ("read_latency_us", &p.probe.io),
        ("fault_latency_us", &p.probe.fault),
    ] {
        if h.count == 0 {
            continue;
        }
        for (stat, v) in [
            ("p50", h.p50_us),
            ("p90", h.p90_us),
            ("p99", h.p99_us),
            ("p999", h.p999_us),
            ("max", h.max_us),
            ("mean", h.mean_us),
        ] {
            out.push(m(name, v, &[("stat", stat)]));
        }
        out.push(m(name, h.count as f64, &[("stat", "count")]));
    }
    for (scope, w) in [("probe", &p.probe.psi_cgroup), ("system", &p.probe.psi_system)] {
        for (res, s) in [("cpu", &w.cpu), ("io", &w.io), ("memory", &w.memory)] {
            if let Some(s) = s {
                out.push(m("psi_some_pct", s.some_pct, &[("scope", scope), ("resource", res)]));
                if let Some(f) = s.full_pct {
                    out.push(m("psi_full_pct", f, &[("scope", scope), ("resource", res)]));
                }
            }
        }
    }
    let w = &p.load.window;
    for (name, v) in [
        ("bg_bytes_written", w.bytes_written),
        ("bg_bytes_written_direct", w.bytes_written_direct),
        ("bg_bytes_read", w.bytes_read),
        ("bg_cpu_ops", w.cpu_ops),
        ("bg_mem_pages_touched", w.mem_pages_touched),
        ("bg_fsyncs", w.fsyncs),
    ] {
        out.push(m(name, v as f64, &[]));
    }
    out.push(m("bg_window_s", p.load.window_s, &[]));
    for (level, secs) in &p.levels {
        out.push(m("level_seconds", f64::from(*secs), &[("level", level)]));
    }
    out
}

#[cfg(feature = "lake")]
pub fn store(r: &Results, root: Option<&Path>) -> Result<String> {
    use anyhow::Context;
    use lindesk_store::{BenchPhase, BenchRun, BenchSample, Store, StoreOptions, layout};

    let root = root.map_or_else(|| layout::default_root(false), Path::to_path_buf);
    let started_ms = r.phases.first().map_or_else(lindesk_store::wall_clock_ms, |p| p.started_ms);
    let run = BenchRun {
        run_id: r.run_id.clone(),
        ts: started_ms,
        label: r.label.clone(),
        host: r.host.hostname.clone(),
        kernel: r.host.kernel.clone(),
        config: serde_json::json!({ "config": r.config, "host": r.host }),
        notes: None,
    };
    let mut samples = Vec::new();
    for p in &r.phases {
        let phase = match p.phase {
            crate::results::Phase::Off => BenchPhase::LindeskOff,
            crate::results::Phase::On => BenchPhase::LindeskOn,
        };
        for x in phase_metrics(p) {
            samples.push(BenchSample {
                ts: p.started_ms,
                run_id: r.run_id.clone(),
                phase,
                metric: x.name.into(),
                value: x.value,
                iteration: None,
                labels: x.labels,
            });
        }
        // The once-a-second timeline, as a series.
        for (i, t) in p.timeline.iter().enumerate() {
            let ts = p.started_ms + (t.t_s * 1000.0) as i64;
            for (res, s) in [("cpu", &t.psi.cpu), ("io", &t.psi.io), ("memory", &t.psi.memory)] {
                if let Some(s) = s {
                    let mut labels = BTreeMap::from([
                        ("series".to_string(), "timeline".to_string()),
                        ("scope".into(), "system".into()),
                        ("resource".into(), res.into()),
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
    }
    let mut store = Store::open(&root, StoreOptions::user()).with_context(|| format!("opening {}", root.display()))?;
    store.append_bench(&run, &samples);
    let rep = store.flush_wait(lindesk_store::wall_clock_ms(), std::time::Duration::from_secs(10))?;
    if rep.deferred_rows > 0 {
        anyhow::bail!(
            "the lake at {} is locked by maintenance; {} rows not written",
            root.display(),
            rep.deferred_rows
        );
    }
    Ok(format!("stored run {} ({} rows) in the telemetry lake at {}", r.run_id, rep.rows, root.display()))
}

#[cfg(not(feature = "lake"))]
pub fn store(_: &Results, _: Option<&Path>) -> Result<String> {
    anyhow::bail!("built without the `lake` feature")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::tests::sample_results;

    #[test]
    fn metrics_cover_the_table() {
        let r = sample_results();
        let ms = phase_metrics(&r.phases[0]);
        let get = |n: &str, k: &str, v: &str| {
            ms.iter().find(|x| x.name == n && x.labels.get(k).map(String::as_str) == Some(v)).map(|x| x.value)
        };
        assert_eq!(get("wakeup_overshoot_us", "stat", "p99"), Some(2400.0));
        assert_eq!(get("psi_some_pct", "resource", "io"), Some(20.0));
        assert!(ms.iter().any(|x| x.name == "bg_bytes_written" && x.value > 0.0));
        assert_eq!(get("level_seconds", "level", "red"), Some(4.0));
    }

    #[cfg(feature = "lake")]
    #[test]
    fn stores_into_a_lake() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("telemetry");
        let msg = store(&sample_results(), Some(&root)).unwrap();
        assert!(msg.contains("rows"), "{msg}");
        let has_parquet = |table: &str| {
            fn walk(d: &Path) -> bool {
                std::fs::read_dir(d).into_iter().flatten().flatten().any(|e| {
                    let p = e.path();
                    if p.is_dir() { walk(&p) } else { p.extension().is_some_and(|x| x == "parquet") }
                })
            }
            walk(&root.join(table))
        };
        assert!(has_parquet("bench_runs"));
        assert!(has_parquet("bench_samples"));
    }
}
