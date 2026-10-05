//! The flat per-phase summary (`metrics` in the results JSON, the rows of `bench_samples`)
//! and storing results in the telemetry lake.
//!
//! The results JSON is the primary output. lindesk-bench links no lake code: when
//! `lindesk-data` (the optional lindesk-telemetry package) is installed it runs
//! `lindesk-data import-bench <results.json>`, which stores the run in `bench_runs` /
//! `bench_samples`; without it the results stay in the JSON (importable later the same way).

use std::path::Path;

use anyhow::{Result, anyhow, bail};

use crate::results::{Metric, PhaseResult};

fn m(name: &str, value: f64, labels: &[(&str, &str)]) -> Metric {
    Metric {
        name: name.to_string(),
        value,
        labels: labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
    }
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

/// Stores the results file at `json` through `lindesk-data import-bench` (into `root`, else
/// the user's lake). Returns lindesk-data's summary line.
pub fn store(json: &Path, root: Option<&Path>) -> Result<String> {
    let Some(bin) = lindesk_sys::telemetry::data_binary() else {
        bail!(
            "{}\n  results kept in {}; import them later with: lindesk-data import-bench {}",
            lindesk_sys::telemetry::NOT_INSTALLED,
            json.display(),
            json.display()
        );
    };
    let mut c = std::process::Command::new(&bin);
    if let Some(r) = root {
        c.arg("--root").arg(r);
    }
    let o = c
        .arg("import-bench")
        .arg(json)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| anyhow!("running {}: {e}", bin.display()))?;
    if !o.status.success() {
        let err = String::from_utf8_lossy(&o.stderr);
        bail!("{}", err.trim().strip_prefix("lindesk-data: ").unwrap_or(err.trim()));
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
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

    /// Both halves in one test: they set LINDESK_DATA, which tests in other threads would see.
    #[test]
    fn stores_through_lindesk_data_or_explains() {
        use lindesk_sys::telemetry::DATA_ENV;
        let t = tempfile::tempdir().unwrap();
        let json = t.path().join("bench-results.json");
        std::fs::write(&json, serde_json::to_string(&sample_results()).unwrap()).unwrap();
        // SAFETY: only this test in this binary reads or writes LINDESK_DATA.
        unsafe { std::env::set_var(DATA_ENV, "/nonexistent/lindesk-data") };
        let e = store(&json, None).unwrap_err().to_string();
        assert!(e.starts_with("telemetry isn't installed"), "{e}");
        assert!(e.contains(&format!("lindesk-data import-bench {}", json.display())), "{e}");

        // The workspace's lindesk-data, when this test runs from `cargo test --workspace`.
        let exe = std::env::current_exe().unwrap();
        let data = exe.parent().and_then(Path::parent).map(|d| d.join("lindesk-data"));
        match data.filter(|d| d.is_file()) {
            Some(d) => {
                unsafe { std::env::set_var(DATA_ENV, &d) };
                let root = t.path().join("telemetry");
                let msg = store(&json, Some(&root));
                unsafe { std::env::remove_var(DATA_ENV) };
                let msg = msg.unwrap();
                assert!(msg.starts_with("stored run") && msg.contains("rows"), "{msg}");
                assert!(root.join("bench_runs").is_dir() && root.join("bench_samples").is_dir());
            }
            None => {
                unsafe { std::env::remove_var(DATA_ENV) };
                eprintln!("lindesk-data not built next to this test; skipped the import half");
            }
        }
    }
}
