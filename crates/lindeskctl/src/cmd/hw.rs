//! `lindeskctl hardware`, `swap` and `calibrate`: what lindesk knows about this machine
//! (docs/CONTROL-PLANE.md §1, §3). Each asks the daemon first and falls back to probing
//! locally, so they work out of the box.

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use lindesk_plugin_api::{CpuLatency, Hardware, HostCaps, LatencyMap, PowerSource, StorageKind};
use lindesk_sys::doctor::Severity;
use lindesk_sys::swap::{SwapOptions, SwapReport};
use lindesk_sys::{Env, Roots, calibrate, hardware, swap};
use serde_json::{Value, json};

use crate::bus::{self, Daemon};
use crate::cmd::changes;
use crate::cmd::doctor::tilde;
use crate::ui::{self, Exit, Table, Ui, bar_cells, out};

// ------------------------------------------------------------------------------ local data

/// A saved latency map: the scope's own first, then the other one.
pub fn saved_calibration(system: bool) -> Option<(LatencyMap, std::path::PathBuf)> {
    [system, !system].into_iter().find_map(|s| {
        let p = calibrate::default_path(s);
        calibrate::load(&p).ok().map(|m| (m, p))
    })
}

/// Hardware detected here, with the saved latency map attached.
pub fn local_hardware(system: bool) -> Hardware {
    let mut hw = hardware::detect(&Roots::host());
    if let Some((map, _)) = saved_calibration(system) {
        calibrate::attach(&mut hw, map);
    }
    hw
}

/// Host capabilities probed here (managed devices only), with the saved latency map.
pub fn local_caps(system: bool) -> HostCaps {
    let probe = lindesk_sys::probe(&Roots::host(), &Env::from_process());
    let mut caps = probe.host_caps();
    if let Some((map, _)) = saved_calibration(system) {
        calibrate::attach(&mut caps.hardware, map);
    }
    caps
}

/// From the daemon when one answers, else local. Returns the source name too.
fn fetch_hardware(system: bool) -> (Hardware, String) {
    let order: &[Daemon] = if system { &[Daemon::Broker] } else { &[Daemon::Session, Daemon::Broker] };
    for &d in order {
        if !matches!(bus::reach(d), bus::Reach::Running) {
            continue;
        }
        if let Ok(h) = bus::get_hardware(d).and_then(|j| Ok(serde_json::from_str::<Hardware>(&j)?)) {
            let mut h = h;
            if h.latency.is_none()
                && let Some((map, _)) = saved_calibration(system)
            {
                calibrate::attach(&mut h, map);
            }
            return (h, d.name().to_string());
        }
    }
    (local_hardware(system), "local".into())
}

// ------------------------------------------------------------------------------- rendering

/// `0-7,16-23`.
pub fn cpulist(cpus: &[u32]) -> String {
    let mut v = cpus.to_vec();
    v.sort_unstable();
    v.dedup();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let start = v[i];
        let mut end = start;
        while i + 1 < v.len() && v[i + 1] == end + 1 {
            i += 1;
            end = v[i];
        }
        out.push(if start == end { start.to_string() } else { format!("{start}-{end}") });
        i += 1;
    }
    out.join(",")
}

/// `4.1µs`, `1.25ms`.
pub fn us(v: f64) -> String {
    if v >= 1000.0 {
        format!("{:.2}ms", v / 1000.0)
    } else if v >= 10.0 {
        format!("{v:.0}µs")
    } else {
        format!("{v:.1}µs")
    }
}

/// `2.9 GiB/s`.
pub fn bw(bps: u64) -> String {
    if bps == 0 { "-".into() } else { format!("{}/s", hardware::fmt_bytes(bps)) }
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

/// CPU types for the summary: P/E (or big/LITTLE) on hybrid CPUs, else one group.
fn cpu_groups<'a>(map: &'a LatencyMap, hw: Option<&Hardware>) -> Vec<(&'static str, Vec<&'a CpuLatency>)> {
    let hybrid = hw.filter(|h| h.is_hybrid());
    match hybrid {
        Some(h) => {
            let arm = h.arch.starts_with("aarch") || h.arch.starts_with("arm");
            let (p, e) = if arm { ("big", "LITTLE") } else { ("P-cores", "E-cores") };
            let pick = |set: &[u32]| map.cpus.iter().filter(|c| set.contains(&c.cpu)).collect::<Vec<_>>();
            let mut v = vec![(p, pick(&h.performance_cpus)), (e, pick(&h.efficiency_cpus))];
            let rest: Vec<&CpuLatency> = map
                .cpus
                .iter()
                .filter(|c| !h.performance_cpus.contains(&c.cpu) && !h.efficiency_cpus.contains(&c.cpu))
                .collect();
            if !rest.is_empty() {
                v.push(("other", rest));
            }
            v.retain(|(_, c)| !c.is_empty());
            v
        }
        None => vec![("all CPUs", map.cpus.iter().collect())],
    }
}

fn age(ts: i64) -> String {
    let now = lindesk_core::clock::now_ms();
    if ts <= 0 {
        return "unknown time".into();
    }
    format!("{} ({})", ui::datetime(ts).get(..16).unwrap_or_default(), changes::ago(ts, now))
}

/// The latency map: CPU types, memory, disks, swap, and a NOISY warning.
pub fn render_latency(ui: &Ui, map: &LatencyMap, hw: Option<&Hardware>) -> String {
    let mut s = format!(
        "{}  {}\n",
        ui.bold("Latency map"),
        ui.dim(format!("measured {} in {:.1} s", age(map.measured_at), map.duration_ms as f64 / 1000.0))
    );
    if map.noisy {
        s.push_str(&format!(
            "  {}\n",
            ui.paint(
                "1;33",
                "⚠ NOISY: the machine was busy while measuring, so these numbers are less reliable. Re-run when idle: lindeskctl calibrate"
            )
        ));
    }
    if !map.cpus.is_empty() {
        let groups = cpu_groups(map, hw);
        let mut t = Table::new(&["  CPU TYPE", "CPUS", "SPEED", "", "WAKEUP p50", "p99"]).right(&[3, 4, 5]);
        let mut speeds = Vec::new();
        for (name, cpus) in &groups {
            let ids: Vec<u32> = cpus.iter().map(|c| c.cpu).collect();
            let perf = median(cpus.iter().map(|c| c.relative_perf).collect());
            let p50 = median(cpus.iter().map(|c| c.wakeup_p50_us).collect());
            let p99 = median(cpus.iter().map(|c| c.wakeup_p99_us).collect());
            speeds.push((*name, perf));
            let p99s = if p99 >= 100.0 { ui.yellow(us(p99)) } else { us(p99) };
            t.row(vec![
                format!("  {name}"),
                cpulist(&ids),
                ui.cyan(bar_cells(perf * 100.0, 10)),
                format!("{perf:.2}×"),
                us(p50),
                p99s,
            ]);
        }
        s.push_str(&t.render(ui));
        if let [(p, pp), (e, ep), ..] = speeds.as_slice()
            && *pp > 0.0
        {
            s.push_str(&format!(
                "  {}\n",
                ui.dim(format!("{e} run at {:.0}% of {p} speed (single thread, measured)", ep / pp * 100.0))
            ));
        }
    }
    let m = &map.memory;
    if m.load_latency_ns > 0.0 || m.copy_bps > 0 {
        s.push_str(&format!(
            "  {:<10}load {:.1} ns · copy {} · first-touch fault {}\n",
            "memory",
            m.load_latency_ns,
            bw(m.copy_bps),
            us(m.fault_us)
        ));
    }
    if map.storage.is_empty() {
        s.push_str(&format!("  {:<10}{}\n", "disks", ui.dim("not measured (nominal values are used)")));
    } else {
        let mut t = Table::new(&["  DISK", "4K READ p50", "p99", "SEQ READ", "SEQ WRITE"]).right(&[1, 2, 3, 4]);
        for d in &map.storage {
            let name = hw
                .and_then(|h| h.storage_for(&d.dev))
                .map(|x| format!("{} ({})", x.name, hardware::kind_label(x.kind)))
                .unwrap_or_else(|| d.dev.clone());
            t.row(vec![
                format!("  {name}"),
                us(d.read_4k_p50_us),
                us(d.read_4k_p99_us),
                bw(d.seq_read_bps),
                bw(d.seq_write_bps),
            ]);
        }
        s.push_str(&t.render(ui));
    }
    for w in &map.swap {
        s.push_str(&format!("  {:<10}{} ({}): page-in ≈ {}\n", "swap", w.path, w.kind, us(w.page_in_us)));
    }
    s
}

fn yes_no(ui: &Ui, b: bool) -> String {
    if b { ui.green("yes") } else { ui.dim("no") }
}

/// How lindesk adapts to this hardware (architecture-specific behaviour).
pub fn adaptations(hw: &Hardware) -> Vec<String> {
    let mut v = Vec::new();
    let arm = hw.arch.starts_with("aarch") || hw.arch.starts_with("arm");
    if hw.is_hybrid() && arm {
        v.push(if hw.features.eas {
            "big.LITTLE with EAS: uclamp steers the foreground to big cores and batch work to LITTLE ones (profile arm-eas)".into()
        } else {
            "big.LITTLE without EAS: uclamp only shapes frequency; cpu.weight does most of the work".into()
        });
    } else if hw.is_hybrid() {
        v.push("hybrid P/E cores: foreground boosts (uclamp.min) land on P-cores, batch caps (uclamp.max) on E-cores (profile x86-hybrid)".into());
    } else if arm && hw.features.eas {
        v.push("EAS active: uclamp steers tasks between core types (profile arm-eas)".into());
    }
    if hw.storage.iter().any(|d| d.kind == StorageKind::Hdd) {
        v.push("rotational disk: earlier I/O intervention and gentler io.max steps (profile hdd)".into());
    }
    if hw.power == PowerSource::Battery {
        v.push("on battery: shorter, smaller boosts and harder background yielding (profile battery)".into());
    }
    if hw.compressed_swap {
        v.push("compressed swap: reclaim costs CPU, so memory pressure is treated as CPU work too".into());
    }
    if hw.features.cpufreq_driver.is_none() && hw.cpus > 0 {
        v.push("no cpufreq: uclamp affects placement only, not frequency".into());
    }
    if let Some(virt) = &hw.virtualization {
        v.push(format!(
            "virtualized ({virt}): pressure and cpufreq reflect this guest only; I/O control may be limited"
        ));
    }
    match &hw.latency {
        Some(m) if !m.storage.is_empty() => {
            v.push("calibrated: I/O throttles scale from measured disk bandwidth instead of nominal values".into())
        }
        Some(_) => v.push("calibrated without the disks: I/O throttles use nominal bandwidth".into()),
        None => {}
    }
    v
}

/// `lindeskctl hardware`.
pub fn render_hardware(ui: &Ui, hw: &Hardware) -> String {
    let f = &hw.features;
    let row = |k: &str, v: String| format!("{}  {v}\n", ui.bold(format!("{k:<9}")));
    let cont = |v: String| format!("{:<9}  {v}\n", "");
    let mut s = String::new();
    s.push_str(&row("CPU", hardware::describe_cpu(hw)));
    let mut topo = vec![
        if hw.arch.is_empty() { "unknown arch".to_string() } else { hw.arch.clone() },
        hardware::vendor_name(hw.vendor).to_string(),
    ];
    if hw.cores > 0 {
        topo.push(format!("{} cores / {} threads", hw.cores, hw.cpus));
    }
    topo.push(if hw.smt { "SMT on".into() } else { "no SMT".into() });
    if hw.is_hybrid() {
        topo.push(format!("P {} · E {}", cpulist(&hw.performance_cpus), cpulist(&hw.efficiency_cpus)));
    }
    s.push_str(&cont(topo.join(" · ")));
    let mut freq = Vec::new();
    freq.push(f.cpufreq_driver.clone().unwrap_or_else(|| "no cpufreq driver".into()));
    if let Some(g) = &f.governor {
        freq.push(format!("governor {g}"));
    }
    if let Some(e) = &f.epp {
        freq.push(format!("EPP {e}"));
    }
    s.push_str(&row("cpufreq", freq.join(" · ")));
    if !f.governors_available.is_empty() {
        s.push_str(&cont(ui.dim(format!("governors: {}", f.governors_available.join(" ")))));
    }
    if !f.epp_available.is_empty() {
        s.push_str(&cont(ui.dim(format!("EPP choices: {}", f.epp_available.join(" ")))));
    }
    s.push_str(&row(
        "scheduler",
        format!(
            "EAS {} · ITMT {} · {} NUMA node{}",
            yes_no(ui, f.eas),
            yes_no(ui, f.itmt),
            f.numa_nodes.max(1),
            if f.numa_nodes > 1 { "s" } else { "" }
        ),
    ));
    s.push_str(&row("memory", hardware::describe_memory(hw)));
    if hw.storage.is_empty() {
        s.push_str(&row("storage", ui.dim("no managed disk")));
    }
    for (i, d) in hw.storage.iter().enumerate() {
        let sched = if d.scheduler.is_empty() { "?".into() } else { d.scheduler.clone() };
        let line = format!(
            "{} {}  {}  scheduler {}  {}",
            ui.bold(&d.name),
            ui.dim(&d.dev),
            hardware::kind_label(d.kind),
            sched,
            ui.dim(format!("~{} nominal", bw(d.nominal_bps)))
        );
        s.push_str(&if i == 0 { row("storage", line) } else { cont(line) });
    }
    s.push_str(&row("power", hardware::describe_power(hw)));
    s.push_str(&row(
        "virtual",
        hw.virtualization.clone().map(|v| ui.yellow(v)).unwrap_or_else(|| ui.dim("bare metal")),
    ));
    let a = adaptations(hw);
    if !a.is_empty() {
        s.push_str(&format!("\n{}\n", ui.bold("How lindesk adapts here")));
        for x in a {
            s.push_str(&format!("  {} {x}\n", ui.cyan("→")));
        }
    }
    s.push('\n');
    match &hw.latency {
        Some(m) => s.push_str(&render_latency(ui, m, Some(hw))),
        None => s.push_str(&format!(
            "{}  {}\n",
            ui.bold("Latency map"),
            ui.dim("not calibrated yet: lindesk uses nominal values. Measure it (≈15 s): lindeskctl calibrate")
        )),
    }
    s
}

pub fn hardware_cmd(ui: &Ui) -> Result<()> {
    let (hw, source) = fetch_hardware(ui.system);
    if ui.json {
        return ui.print_json(&hw);
    }
    let mut s = render_hardware(ui, &hw);
    s.push_str(&ui.dim(format!(
        "source: {}",
        if source == "local" { "probed locally (no daemon answered)".to_string() } else { source }
    )));
    out(&s);
    Ok(())
}

// ------------------------------------------------------------------------------------ swap

fn severity_mark(ui: &Ui, s: Severity) -> String {
    match s {
        Severity::Ok => ui.green("✓"),
        Severity::Info => ui.cyan("•"),
        Severity::Warn => ui.yellow("⚠"),
        Severity::Error => ui.red("✗"),
    }
}

pub fn render_swap(ui: &Ui, r: &SwapReport) -> String {
    let mut s = String::new();
    let pct = (r.swap_used * 100).checked_div(r.swap_total).unwrap_or(0);
    let head = if r.swap_total == 0 {
        format!("no swap · {} RAM", hardware::fmt_bytes(r.mem_total))
    } else {
        format!(
            "{} swap, {} used ({pct}%) · {} RAM",
            hardware::fmt_bytes(r.swap_total),
            hardware::fmt_bytes(r.swap_used),
            hardware::fmt_bytes(r.mem_total)
        )
    };
    s.push_str(&format!("{}  {head}\n", ui.bold("Swap")));
    for d in &r.devices {
        s.push_str(&format!("  {}\n", swap::describe_device(d)));
    }
    let z = match &r.zswap {
        Some(z) if z.enabled => ui.green(format!(
            "on ({}, pool ≤ {}%)",
            z.compressor.clone().unwrap_or_else(|| "?".into()),
            z.max_pool_percent.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
        )),
        Some(_) => ui.dim("off"),
        None => ui.dim("not available"),
    };
    let mglru = match &r.mglru {
        Some(m) if m.enabled => ui.green(format!("on ({})", m.raw)),
        Some(m) => ui.yellow(format!("off ({})", m.raw)),
        None => ui.dim("not available"),
    };
    s.push_str(&format!("  {:<10}{z} · MGLRU {mglru}\n", "zswap"));
    s.push_str(&format!("  {:<10}{}\n", "vm", swap::describe_tunables(&r.vm)));
    if let Some(rt) = &r.rates {
        let line = format!(
            "{:.0} pages/s in · {:.0} pages/s out (over {} ms)",
            rt.pswpin_per_s, rt.pswpout_per_s, rt.interval_ms
        );
        let line = if rt.pswpin_per_s > swap::THRASH_PAGES_PER_S { ui.red(line) } else { line };
        s.push_str(&format!("  {:<10}{line}\n", "activity"));
    }
    if !r.top_units.is_empty() {
        let top: Vec<String> = r
            .top_units
            .iter()
            .take(5)
            .map(|u| format!("{} {}", u.cgroup.rsplit('/').next().unwrap_or(&u.cgroup), ui::short_bytes(u.swap_bytes)))
            .collect();
        s.push_str(&format!("  {:<10}{}\n", "swapped", top.join(" · ")));
    }
    s.push('\n');
    if r.findings.is_empty() {
        s.push_str(&format!("{} {}\n", ui.ok_mark(), ui.green("swap looks well tuned for this machine")));
        return s;
    }
    s.push_str(&format!("{}\n", ui.bold(format!("Findings  {}", r.findings.len()))));
    for f in &r.findings {
        s.push_str(&format!("{} {}\n", severity_mark(ui, f.severity), ui.bold(&f.title)));
        s.push_str(&format!("  {}\n", ui.dim(&f.detail)));
        if f.proposed_ops.is_empty() {
            s.push_str(&format!("  {}\n", ui.dim("(advice only: lindesk doesn't change this itself)")));
        } else {
            match changes::suggest_apply(&f.proposed_ops, &f.title) {
                Some(cmd) => s.push_str(&format!("  {} {}\n", ui.cyan("fix:"), ui.cyan(cmd))),
                None => s.push_str(&format!(
                    "  {} {}\n",
                    ui.cyan("ops:"),
                    serde_json::to_string(&f.proposed_ops).unwrap_or_default()
                )),
            }
        }
    }
    s.push_str(&ui.dim(
        "\nfixes are guardrailed and versioned: --ttl 10m auto-reverts unless you `lindeskctl changes confirm ID`",
    ));
    s.push('\n');
    s
}

fn fetch_swap(system: bool) -> (SwapReport, String) {
    let order: &[Daemon] = if system { &[Daemon::Broker] } else { &[Daemon::Session, Daemon::Broker] };
    for &d in order {
        if !matches!(bus::reach(d), bus::Reach::Running) {
            continue;
        }
        if let Ok(r) = bus::swap_report(d).and_then(|j| Ok(serde_json::from_str::<SwapReport>(&j)?)) {
            return (r, d.name().to_string());
        }
    }
    let roots = Roots::host();
    let hw = hardware::detect(&roots);
    let opts = SwapOptions { interval: Some(Duration::from_millis(500)), previous: None };
    (swap::analyze_with(&roots, &hw, &opts), "local".into())
}

pub fn swap_cmd(ui: &Ui) -> Result<()> {
    let (r, source) = fetch_swap(ui.system);
    if ui.json {
        return ui.print_json(&r);
    }
    let mut s = render_swap(ui, &r);
    s.push_str(&ui.dim(format!(
        "source: {}",
        if source == "local" { "analyzed locally (no daemon answered)".to_string() } else { source }
    )));
    out(&s);
    Ok(())
}

// ------------------------------------------------------------------------------- calibrate

/// A spinner on stderr while something slow runs (only on a terminal).
pub struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    pub fn start(ui: &Ui, msg: String) -> Spinner {
        let stop = Arc::new(AtomicBool::new(false));
        let on = !ui.quiet && !ui.json && std::io::stderr().is_terminal();
        let handle = on.then(|| {
            let stop = stop.clone();
            let color = ui.err_color;
            std::thread::spawn(move || {
                const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
                let t0 = Instant::now();
                let mut i = 0;
                while !stop.load(Ordering::Relaxed) {
                    let f = if ui::ascii() { ["-", "\\", "|", "/"][i % 4] } else { FRAMES[i % FRAMES.len()] };
                    eprint!(
                        "\r{} {msg} {}\x1b[K",
                        ui::paint(color, "36", f),
                        ui::paint(color, "2", format!("{:.0}s", t0.elapsed().as_secs_f64()))
                    );
                    let _ = std::io::stderr().flush();
                    i += 1;
                    std::thread::sleep(Duration::from_millis(100));
                }
                eprint!("\r\x1b[K");
                let _ = std::io::stderr().flush();
            })
        });
        Spinner { stop, handle }
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub fn calibrate_cmd(ui: &Ui, quick: bool, force: bool) -> Result<()> {
    let budget = if quick { "≈3 s" } else { "≈15 s" };
    let broker_up = matches!(bus::reach(Daemon::Broker), bus::Reach::Running);
    if ui.system && !broker_up {
        return Err(Daemon::Broker.not_running().context("system calibration is done by lindeskd"));
    }
    if broker_up {
        let spin = Spinner::start(ui, format!("lindeskd is calibrating ({budget}; keep the machine idle)"));
        let res = bus::calibrate(&json!({"quick": quick, "force": force}).to_string());
        spin.stop();
        match res {
            Ok(raw) => {
                let map: LatencyMap = serde_json::from_str(&raw)
                    .map_err(|e| anyhow!("lindeskd returned a latency map this lindeskctl can't read: {e}"))?;
                let path = calibrate::default_path(true);
                if ui.json {
                    return ui.print_json(&json!({"source": "lindeskd", "path": path, "map": map, "notes": []}));
                }
                let (hw, _) = fetch_hardware(true);
                out(&format!(
                    "{}\n{} {}",
                    render_latency(ui, &map, Some(&hw)).trim_end(),
                    ui.ok_mark(),
                    ui.dim(format!(
                        "measured by lindeskd and saved to {} (used by every lindesk daemon)",
                        path.display()
                    ))
                ));
                return Ok(());
            }
            Err(e) if ui.system => return Err(e),
            Err(e) => {
                ui.warn(format!(
                    "{}; measuring locally as your user instead",
                    e.to_string().lines().next().unwrap_or_default()
                ));
            }
        }
    }
    let opts = calibrate::CalibrateOptions { quick, force, ..Default::default() };
    let spin = Spinner::start(ui, format!("calibrating ({budget}; keep the machine idle)"));
    let res = calibrate::run_detailed(&Roots::host(), &opts);
    spin.stop();
    let c = match res {
        Ok(c) => c,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("busy") && !force {
                return Err(anyhow!(
                    "{msg}\n  Retry when idle, or now with: lindeskctl calibrate --force{}",
                    if quick { " --quick" } else { "" }
                ));
            }
            return Err(anyhow!("calibration failed: {msg}"));
        }
    };
    let path = calibrate::default_path(false);
    let saved = calibrate::save(&path, &c.map);
    if ui.json {
        ui.print_json(&json!({
            "source": "local",
            "path": path,
            "saved": saved.is_ok(),
            "map": c.map,
            "notes": c.notes,
        }))?;
        return saved.map_err(|e| anyhow!("saving {}: {e}", path.display()));
    }
    let hw = hardware::detect(&Roots::host());
    let mut s = render_latency(ui, &c.map, Some(&hw));
    for n in &c.notes {
        s.push_str(&format!("  {} {}\n", ui.dim("note"), ui.dim(n)));
    }
    match &saved {
        Ok(()) => s.push_str(&format!(
            "{} saved to {} (your user calibration; lindesk-session uses it)\n{}",
            ui.ok_mark(),
            tilde(&path),
            ui.dim(
                "lindeskd wasn't running, so this ran as you. With lindeskd running, `lindeskctl calibrate` stores the system map instead."
            )
        )),
        Err(e) => s.push_str(&format!("{} saving {}: {e}", ui.fail_mark(), path.display())),
    }
    out(&s);
    if saved.is_err() { Err(Exit(1).into()) } else { Ok(()) }
}

/// For `doctor`: is there a latency map, how old, how trustworthy.
pub fn calibration_summary() -> Value {
    match saved_calibration(true) {
        Some((m, p)) => json!({
            "present": true,
            "path": p,
            "measured_at": m.measured_at,
            "noisy": m.noisy,
            "disks": m.storage.len(),
        }),
        None => json!({"present": false}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_plugin_api::{MemoryLatency, StorageDevice, StorageLatency, SwapLatency};
    use lindesk_sys::swap::{SwapDevice, SwapFinding, SwapKind, VmTunables, Zswap};

    pub fn fixture_hw() -> Hardware {
        serde_json::from_value(json!({
            "cpus": 16, "cores": 12, "smt": true,
            "performance_cpus": [0,1,2,3,4,5,6,7], "efficiency_cpus": [8,9,10,11,12,13,14,15],
            "cpu_model": "Intel Core Ultra 7 155H", "mem_total": 33285996544u64, "swap_total": 8589934592u64,
            "compressed_swap": true,
            "storage": [{"dev": "259:0", "name": "nvme0n1", "kind": "nvme", "scheduler": "none", "nominal_bps": 2147483648u64}],
            "power": "battery", "portable": true, "arch": "x86_64", "vendor": "intel",
            "features": {"eas": false, "itmt": true, "cpufreq_driver": "intel_pstate", "governor": "powersave",
                         "epp": "balance_performance", "epp_available": ["default", "performance", "balance_performance", "power"],
                         "governors_available": ["performance", "powersave"], "numa_nodes": 1}
        }))
        .unwrap()
    }

    pub fn fixture_map() -> LatencyMap {
        let mut cpus = Vec::new();
        for c in 0..16u32 {
            let p = c < 8;
            cpus.push(CpuLatency {
                cpu: c,
                wakeup_p50_us: if p { 4.0 } else { 5.5 },
                wakeup_p99_us: if p { 12.0 } else { 21.0 },
                relative_perf: if p { 1.0 } else { 0.62 },
            });
        }
        LatencyMap {
            measured_at: 1_790_000_000_000,
            duration_ms: 3120,
            cpus,
            memory: MemoryLatency { load_latency_ns: 92.3, copy_bps: 18 << 30, fault_us: 0.31 },
            storage: vec![StorageLatency {
                dev: "259:0".into(),
                read_4k_p50_us: 82.0,
                read_4k_p99_us: 240.0,
                seq_read_bps: 3 << 30,
                seq_write_bps: 1800 << 20,
            }],
            swap: vec![SwapLatency { path: "/dev/zram0".into(), kind: "zram".into(), page_in_us: 3.2 }],
            noisy: true,
        }
    }

    #[test]
    fn renders_latency_map() {
        let s = render_latency(&Ui::plain(), &fixture_map(), Some(&fixture_hw()));
        assert!(s.contains("⚠ NOISY"), "{s}");
        assert!(s.contains("P-cores   0-7   ██████████  1.00×") && s.contains("4.0µs") && s.contains("12µs"), "{s}");
        assert!(s.contains("E-cores   8-15  ██████▎░░░  0.62×") && s.contains("21µs"), "{s}");
        assert!(s.contains("E-cores run at 62% of P-cores speed"), "{s}");
        assert!(s.contains("memory    load 92.3 ns · copy 18 GiB/s · first-touch fault 0.3µs"), "{s}");
        assert!(
            s.contains("nvme0n1 (NVMe)") && s.contains("82µs") && s.contains("240µs") && s.contains("3 GiB/s"),
            "{s}"
        );
        assert!(s.contains("swap      /dev/zram0 (zram): page-in ≈ 3.2µs"), "{s}");
        // Uniform CPUs: one group.
        let s = render_latency(&Ui::plain(), &fixture_map(), None);
        assert!(s.contains("all CPUs") && s.contains("0-15") && !s.contains("NOT"), "{s}");
    }

    #[test]
    fn renders_hardware_view() {
        let mut hw = fixture_hw();
        let s = render_hardware(&Ui::plain(), &hw);
        assert!(s.starts_with("CPU        Intel Core Ultra 7 155H — hybrid"), "{s}");
        assert!(s.contains("x86_64 · intel · 12 cores / 16 threads · SMT on · P 0-7 · E 8-15"), "{s}");
        assert!(s.contains("cpufreq    intel_pstate · governor powersave · EPP balance_performance"), "{s}");
        assert!(s.contains("EPP choices: default performance balance_performance power"), "{s}");
        assert!(s.contains("scheduler  EAS no · ITMT yes · 1 NUMA node"), "{s}");
        assert!(s.contains("storage    nvme0n1 259:0  NVMe  scheduler none"), "{s}");
        assert!(s.contains("power      battery (laptop)") && s.contains("virtual    bare metal"), "{s}");
        assert!(s.contains("hybrid P/E cores") && s.contains("on battery"), "{s}");
        assert!(s.contains("not calibrated yet") && s.contains("lindeskctl calibrate"), "{s}");
        hw.latency = Some(fixture_map());
        let s = render_hardware(&Ui::plain(), &hw);
        assert!(s.contains("Latency map  measured") && s.contains("calibrated: I/O throttles"), "{s}");
        hw.latency.as_mut().unwrap().storage.clear();
        assert!(render_hardware(&Ui::plain(), &hw).contains("calibrated without the disks"));
        assert_eq!(cpulist(&[0, 1, 2, 3, 8, 10, 11]), "0-3,8,10-11");
    }

    #[test]
    fn renders_swap_report() {
        let r = SwapReport {
            mem_total: 16 << 30,
            swap_total: 8 << 30,
            swap_used: 1 << 30,
            devices: vec![SwapDevice {
                path: "/dev/sda2".into(),
                kind: SwapKind::Partition,
                backing_dev: Some("8:0".into()),
                backing: Some(StorageKind::Hdd),
                compressor: None,
                size_bytes: 8 << 30,
                used_bytes: 1 << 30,
                priority: -2,
            }],
            zswap: Some(Zswap { enabled: false, ..Default::default() }),
            vm: VmTunables { swappiness: Some(60), page_cluster: Some(3), ..Default::default() },
            findings: vec![
                SwapFinding {
                    severity: Severity::Warn,
                    title: "Swap on a hard disk".into(),
                    detail: "sda2 is on a rotational disk".into(),
                    proposed_ops: vec![
                        json!({"op": "sysctl", "key": "vm.swappiness", "value": "10"}),
                        json!({"op": "zswap", "enabled": true, "compressor": "zstd", "max_pool_percent": 20}),
                    ],
                },
                SwapFinding {
                    severity: Severity::Info,
                    title: "MGLRU off".into(),
                    detail: "echo y".into(),
                    proposed_ops: vec![],
                },
            ],
            ..Default::default()
        };
        let s = render_swap(&Ui::plain(), &r);
        assert!(s.starts_with("Swap  8 GiB swap, 1 GiB used (12%) · 16 GiB RAM"), "{s}");
        assert!(s.contains("sda2 partition on HDD 8 GiB"), "{s}");
        assert!(s.contains("zswap     off"), "{s}");
        assert!(s.contains("swappiness 60, page-cluster 3"), "{s}");
        assert!(s.contains("Findings  2") && s.contains("⚠ Swap on a hard disk"), "{s}");
        assert!(
            s.contains("fix: lindeskctl changes apply --sysctl vm.swappiness=10 --zswap on --zswap-compressor zstd --zswap-max-pool 20 --ttl 10m -m 'Swap on a hard disk'"),
            "{s}"
        );
        assert!(s.contains("• MGLRU off") && s.contains("advice only"), "{s}");
        let healthy = SwapReport { findings: vec![], ..r };
        assert!(render_swap(&Ui::plain(), &healthy).contains("swap looks well tuned"));
    }

    #[test]
    fn helpers() {
        assert_eq!(us(4.06), "4.1µs");
        assert_eq!(us(240.0), "240µs");
        assert_eq!(us(1500.0), "1.50ms");
        assert_eq!(median(vec![3.0, 1.0, 2.0]), 2.0);
        assert_eq!(bw(0), "-");
        let _ = StorageDevice::default();
    }
}
