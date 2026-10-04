//! Markdown rendering of results: one column per phase, plus a delta column when there are
//! exactly two.

use std::fmt::Write;

use crate::results::{PhaseResult, Results};
use crate::safety::MIB;

#[derive(Clone, Copy, PartialEq)]
enum Better {
    Lower,
    Higher,
}

#[derive(Clone, Copy)]
enum Unit {
    /// Microseconds, shown as µs / ms / s.
    Micros,
    Percent,
    MiB,
    MiBps,
    Giga,
    Count,
}

struct Row {
    label: &'static str,
    unit: Unit,
    better: Better,
    get: fn(&PhaseResult) -> Option<f64>,
}

fn written(p: &PhaseResult) -> u64 {
    p.load.window.bytes_written + p.load.window.bytes_written_direct
}

fn rate(bytes: u64, secs: f64) -> Option<f64> {
    (secs > 0.0).then(|| bytes as f64 / MIB as f64 / secs)
}

const ROWS: &[Row] = &[
    Row {
        label: "Wakeup latency p50",
        unit: Unit::Micros,
        better: Better::Lower,
        get: |p| Some(p.probe.wakeup.p50_us),
    },
    Row {
        label: "Wakeup latency p99",
        unit: Unit::Micros,
        better: Better::Lower,
        get: |p| Some(p.probe.wakeup.p99_us),
    },
    Row {
        label: "Wakeup latency max",
        unit: Unit::Micros,
        better: Better::Lower,
        get: |p| Some(p.probe.wakeup.max_us),
    },
    Row {
        label: "4K read latency p50",
        unit: Unit::Micros,
        better: Better::Lower,
        get: |p| io(p).map(|_| p.probe.io.p50_us),
    },
    Row {
        label: "4K read latency p99",
        unit: Unit::Micros,
        better: Better::Lower,
        get: |p| io(p).map(|_| p.probe.io.p99_us),
    },
    Row {
        label: "4K read latency max",
        unit: Unit::Micros,
        better: Better::Lower,
        get: |p| io(p).map(|_| p.probe.io.max_us),
    },
    Row { label: "Page fault p99", unit: Unit::Micros, better: Better::Lower, get: |p| Some(p.probe.fault.p99_us) },
    Row { label: "Page fault max", unit: Unit::Micros, better: Better::Lower, get: |p| Some(p.probe.fault.max_us) },
    Row {
        label: "Foreground CPU stall",
        unit: Unit::Percent,
        better: Better::Lower,
        get: |p| p.probe.psi_cgroup.cpu.as_ref().map(|s| s.some_pct),
    },
    Row {
        label: "Foreground I/O stall",
        unit: Unit::Percent,
        better: Better::Lower,
        get: |p| p.probe.psi_cgroup.io.as_ref().map(|s| s.some_pct),
    },
    Row {
        label: "Foreground memory stall",
        unit: Unit::Percent,
        better: Better::Lower,
        get: |p| p.probe.psi_cgroup.memory.as_ref().map(|s| s.some_pct),
    },
    Row {
        label: "System I/O stall (some)",
        unit: Unit::Percent,
        better: Better::Lower,
        get: |p| p.probe.psi_system.io.as_ref().map(|s| s.some_pct),
    },
    Row {
        label: "System memory stall (some)",
        unit: Unit::Percent,
        better: Better::Lower,
        get: |p| p.probe.psi_system.memory.as_ref().map(|s| s.some_pct),
    },
    Row {
        label: "Background written",
        unit: Unit::MiB,
        better: Better::Higher,
        get: |p| (p.load.config.write || p.load.config.direct).then(|| written(p) as f64 / MIB as f64),
    },
    Row {
        label: "Background write rate",
        unit: Unit::MiBps,
        better: Better::Higher,
        get: |p| if p.load.config.write || p.load.config.direct { rate(written(p), p.load.window_s) } else { None },
    },
    Row {
        label: "Background read",
        unit: Unit::MiB,
        better: Better::Higher,
        get: |p| p.load.config.read.then(|| p.load.window.bytes_read as f64 / MIB as f64),
    },
    Row {
        label: "Background CPU work (ops)",
        unit: Unit::Giga,
        better: Better::Higher,
        get: |p| (p.load.config.cpu > 0).then(|| p.load.window.cpu_ops as f64 / 1e9),
    },
    Row {
        // One write per 4 KiB page; a pass covers the whole region once.
        label: "Background memory passes",
        unit: Unit::Count,
        better: Better::Higher,
        get: |p| {
            (p.load.mem_peak > 0).then(|| p.load.window.mem_pages_touched as f64 * 4096.0 / p.load.mem_peak as f64)
        },
    },
];

fn io(p: &PhaseResult) -> Option<()> {
    (p.probe.io.count > 0).then_some(())
}

/// `87 µs`, `1.24 ms`, `2.31 s`.
pub fn fmt_micros(us: f64) -> String {
    if us < 10.0 {
        format!("{us:.1} µs")
    } else if us < 1000.0 {
        format!("{us:.0} µs")
    } else if us < 1_000_000.0 {
        let ms = us / 1000.0;
        if ms < 100.0 { format!("{ms:.2} ms") } else { format!("{ms:.0} ms") }
    } else {
        format!("{:.2} s", us / 1e6)
    }
}

fn fmt(unit: Unit, v: f64) -> String {
    match unit {
        Unit::Micros => fmt_micros(v),
        Unit::Percent => format!("{v:.1}%"),
        Unit::MiB => format!("{v:.0} MiB"),
        Unit::MiBps => format!("{v:.0} MiB/s"),
        Unit::Giga => format!("{v:.1} G"),
        Unit::Count => format!("{v:.1}"),
    }
}

/// Relative change from `a` to `b`, with a verdict.
fn delta(a: f64, b: f64, better: Better) -> String {
    if a == 0.0 && b == 0.0 {
        return "=".into();
    }
    if a == 0.0 {
        return "n/a".into();
    }
    let d = (b - a) / a * 100.0;
    let verdict = if d.abs() < 5.0 {
        ""
    } else if (d < 0.0) == (better == Better::Lower) {
        " better"
    } else {
        " worse"
    };
    format!("{d:+.0}%{verdict}")
}

/// One column of the table.
pub struct Column<'a> {
    pub title: String,
    pub phase: &'a PhaseResult,
}

/// The comparison table. With two columns, a `Δ` column compares the second to the first.
pub fn table(cols: &[Column<'_>]) -> String {
    let mut out = String::new();
    let with_delta = cols.len() == 2;
    let _ = write!(out, "| Metric |");
    for c in cols {
        let _ = write!(out, " {} |", c.title);
    }
    if with_delta {
        out.push_str(" Δ |");
    }
    out.push_str("\n|---|");
    for _ in cols {
        out.push_str("---:|");
    }
    if with_delta {
        out.push_str("---:|");
    }
    out.push('\n');
    for row in ROWS {
        let vals: Vec<Option<f64>> = cols.iter().map(|c| (row.get)(c.phase)).collect();
        if vals.iter().all(Option::is_none) {
            continue;
        }
        let _ = write!(out, "| {} |", row.label);
        for v in &vals {
            let _ = write!(out, " {} |", v.map_or_else(|| "–".into(), |v| fmt(row.unit, v)));
        }
        if with_delta {
            let d = match (vals[0], vals[1]) {
                (Some(a), Some(b)) => delta(a, b, row.better),
                _ => "–".into(),
            };
            let _ = write!(out, " {d} |");
        }
        out.push('\n');
    }
    // Informational rows.
    if cols.iter().any(|c| !c.phase.levels.is_empty()) {
        out.push_str("| lindesk level (s green/yellow/red) |");
        for c in cols {
            let l = &c.phase.levels;
            let g = |k: &str| l.get(k).copied().unwrap_or(0);
            if l.is_empty() {
                out.push_str(" – |");
            } else {
                let _ = write!(out, " {}/{}/{} |", g("green"), g("yellow"), g("red"));
            }
        }
        if with_delta {
            out.push_str(" |");
        }
        out.push('\n');
    }
    out
}

/// A short header describing the run(s) the columns come from.
pub fn header(results: &[&Results]) -> String {
    let mut out = String::new();
    for r in results {
        let h = &r.host;
        let c = &r.config;
        let l = &c.load;
        let mut load = Vec::new();
        if l.cpu > 0 {
            load.push(format!("{} CPU workers", l.cpu));
        }
        if l.write || l.direct {
            load.push(format!(
                "{} writes (cap {})",
                match (l.write, l.direct) {
                    (true, true) => "buffered+direct",
                    (true, false) => "buffered",
                    _ => "direct",
                },
                crate::safety::fmt_bytes(l.write_cap)
            ));
        }
        if l.read {
            load.push("sequential reads".into());
        }
        if let Some(m) = &l.mem {
            load.push(format!("memory {m}"));
        }
        if let Some(b) = &l.build_cmd {
            load.push(format!("build `{b}`"));
        }
        let _ = writeln!(
            out,
            "**{}** ({}): {} · kernel {} · {} CPUs · {} RAM · lindesk support {} · {}",
            r.label,
            r.started,
            h.hostname.as_deref().unwrap_or("?"),
            h.kernel.as_deref().unwrap_or("?"),
            h.nproc,
            crate::safety::fmt_bytes(h.mem_total),
            h.support_level,
            h.desktop.as_deref().unwrap_or("no desktop"),
        );
        let _ = writeln!(
            out,
            "Profile `{}`: {:.0}s measured per phase in `{}`; load: {}.",
            c.profile,
            c.duration_s,
            c.slice,
            if load.is_empty() { "none".into() } else { load.join(", ") }
        );
        out.push('\n');
    }
    out
}

/// Header and table for the phases of one results file.
pub fn render_results(r: &Results) -> String {
    let cols: Vec<Column<'_>> =
        r.phases.iter().map(|p| Column { title: format!("lindesk {}", p.phase.as_str()), phase: p }).collect();
    format!("{}{}", header(&[r]), table(&cols))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::tests::sample_results;

    #[test]
    fn micros_formatting() {
        assert_eq!(fmt_micros(4.26), "4.3 µs");
        assert_eq!(fmt_micros(87.4), "87 µs");
        assert_eq!(fmt_micros(1240.0), "1.24 ms");
        assert_eq!(fmt_micros(215_000.0), "215 ms");
        assert_eq!(fmt_micros(2_310_000.0), "2.31 s");
    }

    #[test]
    fn deltas() {
        assert_eq!(delta(100.0, 25.0, Better::Lower), "-75% better");
        assert_eq!(delta(100.0, 90.0, Better::Higher), "-10% worse");
        assert_eq!(delta(100.0, 102.0, Better::Lower), "+2%");
        assert_eq!(delta(0.0, 0.0, Better::Lower), "=");
        assert_eq!(delta(0.0, 3.0, Better::Lower), "n/a");
    }

    #[test]
    fn renders_ab_table() {
        let r = sample_results();
        let md = render_results(&r);
        let lines: Vec<&str> = md.lines().filter(|l| l.starts_with('|')).collect();
        assert_eq!(lines[0], "| Metric | lindesk off | lindesk on | Δ |");
        assert_eq!(lines[1], "|---|---:|---:|---:|");
        let row = |name: &str| *lines.iter().find(|l| l.starts_with(&format!("| {name} |"))).unwrap();
        assert_eq!(row("Wakeup latency p99"), "| Wakeup latency p99 | 2.40 ms | 600 µs | -75% better |");
        assert_eq!(row("4K read latency p99"), "| 4K read latency p99 | 45.00 ms | 11.25 ms | -75% better |");
        assert_eq!(row("Foreground I/O stall"), "| Foreground I/O stall | 20.0% | 5.0% | -75% better |");
        // Background progress: the on phase did half as much (scale clamps at 0.5).
        assert_eq!(row("Background CPU work (ops)"), "| Background CPU work (ops) | 9.0 G | 18.0 G | +100% better |");
        assert!(row("lindesk level (s green/yellow/red)").contains("6/0/4"));
        // Rows for loads that were not configured are omitted.
        assert!(!md.contains("Background read"));
        assert!(md.contains("lindesk support IO-PARTIAL"));
    }

    #[test]
    fn single_phase_has_no_delta() {
        let mut r = sample_results();
        r.phases.truncate(1);
        let md = table(&[Column { title: "off".into(), phase: &r.phases[0] }]);
        assert!(md.starts_with("| Metric | off |\n|---|---:|\n"));
        assert!(!md.contains('Δ'));
    }
}
