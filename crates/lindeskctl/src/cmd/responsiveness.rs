use crate::{
    bus::{self, Daemon},
    cli::LatencyCmd,
    ui::{Ui, out},
};
use anyhow::{Result, bail};
use std::time::{Duration, Instant};

pub fn depend(ui: &Ui, client: i32, server: i32, ttl: Duration, cancel: bool) -> Result<()> {
    if !cancel && (ttl > Duration::from_secs(30) || ttl < Duration::from_millis(1)) {
        bail!("lease must be between 1ms and 30s");
    }
    let ttl = if cancel { 0 } else { ttl.as_millis() as u32 };
    let d = Daemon::for_scope(ui.system);
    let conn = bus::running(d)?;
    let result = match d {
        Daemon::Session => bus::session_proxy(&conn)?.report_dependency(client, server, ttl),
        Daemon::Broker => bus::broker_proxy(&conn)?.report_dependency(client, server, ttl),
    };
    result.map_err(|e| bus::call_error(d, "ReportDependency", e))?;
    if ui.json {
        ui.print_json(&serde_json::json!({"client": client, "server": server, "ttl_ms": ttl}))?;
    } else {
        ui.info(format!(
            "Dependency {client} → {server}: {}",
            if cancel {
                "cancelled".into()
            } else {
                format!("lease accepted for {ttl}ms; protection requires an interactive client")
            }
        ));
    }
    Ok(())
}

pub fn latency(ui: &Ui, cmd: &LatencyCmd) -> Result<()> {
    let d = Daemon::for_scope(ui.system);
    if !matches!(cmd, LatencyCmd::Show) && ui.system {
        bail!("application observations belong to the session; omit --system");
    }
    let conn = bus::running(d)?;
    match cmd {
        LatencyCmd::Show => {
            let raw = match d {
                Daemon::Session => bus::session_proxy(&conn)?.latency_report(),
                Daemon::Broker => bus::broker_proxy(&conn)?.latency_report(),
            }
            .map_err(|e| bus::call_error(d, "LatencyReport", e))?;
            let v: serde_json::Value = serde_json::from_str(&raw)?;
            if ui.json {
                return ui.print_json(&v);
            }
            if let Some(summary) = v["summary"].as_object() {
                if summary.is_empty() {
                    out("No application observations yet. Try: lindeskctl latency probe");
                }
                for (name, s) in summary {
                    out(&format!("{name}: n={} p95={} p99={} max={}", s["count"], s["p95"], s["p99"], s["max"]));
                }
            }
            out(
                "Use --json to export observations and recent pressure/engine context. Timer jitter is not input-to-display latency.",
            );
        }
        LatencyCmd::Report { kind, value } => {
            if !value.is_finite() || *value < 0.0 {
                bail!("value must be finite and nonnegative");
            }
            let name = match kind.as_str() {
                "frame" => "desktop.frame_ms",
                "input" => "desktop.input_ms",
                "runqueue" => "desktop.runqueue_ms",
                "audio-xruns" => "desktop.audio_xruns",
                _ => "desktop.timer_late_ms",
            };
            bus::session_proxy(&conn)?.report_metric(name, *value, Default::default())?;
        }
        LatencyCmd::Probe { duration } => {
            if *duration > Duration::from_secs(60) {
                bail!("probe duration must be at most 60s");
            }
            let p = bus::session_proxy(&conn)?;
            let end = Instant::now() + *duration;
            let mut observations = 0;
            while Instant::now() < end {
                let deadline = (Instant::now() + Duration::from_millis(10)).min(end);
                std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                let late = Instant::now().saturating_duration_since(deadline).as_secs_f64() * 1000.0;
                // Publish after the measured wait; the next wait starts after the call.
                p.report_metric("desktop.timer_late_ms", late, Default::default())?;
                observations += 1;
            }
            ui.info(format!("Recorded {observations} timer wakeups; run lindeskctl latency show"));
        }
    }
    Ok(())
}

pub fn gpu(ui: &Ui, pid: i32, interval: Duration) -> Result<()> {
    if !(Duration::from_millis(100)..=Duration::from_secs(10)).contains(&interval) {
        bail!("interval must be between 100ms and 10s");
    }
    let root = std::path::Path::new("/proc");
    let before = lindesk_sys::gpu::clients(root, pid)?;
    let start = Instant::now();
    std::thread::sleep(interval);
    let after = lindesk_sys::gpu::clients(root, pid)?;
    let ns = start.elapsed().as_nanos() as f64;
    let rows: Vec<_> = after
        .iter()
        .map(|(key, c)| {
            let mut busy = serde_json::Map::new();
            for (engine, value) in &c.engines_ns {
                let pct =
                    before.get(key).and_then(|b| b.engines_ns.get(engine)).and_then(|old| value.checked_sub(*old)).map(
                        |delta| (delta as f64 * 100.0 / ns / *c.capacity.get(engine).unwrap_or(&1) as f64).min(100.0),
                    );
                busy.insert(engine.clone(), serde_json::json!(pct));
            }
            serde_json::json!({"client": key, "busy_pct": busy, "resident_bytes": c.resident_bytes})
        })
        .collect();
    let v = serde_json::json!({"pid": pid, "interval_ms": ns / 1e6, "clients": rows,
        "available": !rows.is_empty(), "note": "Per-client engine activity, not GPU queue latency. Missing counters are unknown; shared memory may overlap across clients. Scan limited to 1024 descriptors."});
    if ui.json {
        return ui.print_json(&v);
    }
    if rows.is_empty() {
        out(
            "No DRM client counters available for this process (unsupported driver, no GPU descriptors, or inaccessible fdinfo).",
        );
    } else {
        for r in rows {
            out(&format!("{}: engines {}%, resident bytes {}", r["client"], r["busy_pct"], r["resident_bytes"]));
        }
    }
    Ok(())
}
