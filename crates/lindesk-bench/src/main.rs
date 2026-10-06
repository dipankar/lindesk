//! lindesk-bench: foreground latency probe, built-in background load and the A/B harness
//! that shows what lindesk does for responsiveness.
//!
//! ```text
//! lindesk-bench probe [--duration 10s] [--stream]        # run as the foreground app
//! lindesk-bench load --cpu 8 --write --mem 30%           # background load, bounded
//! lindesk-bench run [--quick] [--phases off,on]          # A/B: lindesk off vs on
//! lindesk-bench compare a.json [b.json]                  # markdown table from results
//! ```

mod harness;
mod hist;
mod lake;
mod lindesk;
mod load;
mod probe;
mod report;
mod results;
mod safety;
mod sys;

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};

use crate::results::{Phase, Results};

#[derive(Parser)]
#[command(name = "lindesk-bench", version = lindesk_version::LONG, about = "lindesk foreground latency probe and A/B torture harness")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Measure foreground latency: wakeup overshoot, 4K read latency, page-fault latency, PSI.
    Probe(probe::ProbeArgs),
    /// Generate bounded background load (CPU, writes, reads, memory, a real build).
    Load(load::LoadArgs),
    /// A/B harness: run the probe against the load with lindesk off and on.
    Run(harness::RunArgs),
    /// Render the comparison table from saved results.
    ///
    /// With one file: its phases side by side. With two: the first file's `off` phase (or
    /// its only phase) against the second file's `on` phase (or its only phase). Pick a
    /// phase explicitly with `file.json:on`.
    Compare { a: String, b: Option<String> },
}

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    sys::install_signal_handlers();
    let cli = Cli::parse();
    let r = match cli.cmd {
        Cmd::Probe(a) => probe::run(a),
        Cmd::Load(a) => load::run(a),
        Cmd::Run(a) => harness::run(a),
        Cmd::Compare { a, b } => compare(&a, b.as_deref()),
    };
    match r {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("lindesk-bench: {e:#}");
            std::process::exit(1);
        }
    }
}

/// `path` or `path:phase`.
fn parse_selector(s: &str) -> Result<(PathBuf, Option<Phase>)> {
    if Path::new(s).exists() {
        return Ok((s.into(), None));
    }
    if let Some((path, ph)) = s.rsplit_once(':')
        && let Some(ph) = Phase::parse(ph)
    {
        return Ok((path.into(), Some(ph)));
    }
    Ok((s.into(), None))
}

fn pick(r: &Results, sel: Option<Phase>, prefer: Phase, path: &Path) -> Result<usize> {
    let want = sel.or_else(|| (r.phases.len() > 1).then_some(prefer));
    match want {
        Some(p) => r
            .phases
            .iter()
            .position(|x| x.phase == p)
            .ok_or_else(|| anyhow!("{} has no '{}' phase", path.display(), p.as_str())),
        None if r.phases.is_empty() => Err(anyhow!("{} has no phases", path.display())),
        None => Ok(0),
    }
}

fn compare(a: &str, b: Option<&str>) -> Result<i32> {
    let (pa, sa) = parse_selector(a)?;
    let ra = Results::load(&pa)?;
    let Some(b) = b else {
        if let Some(p) = sa {
            let i = pick(&ra, Some(p), p, &pa)?;
            let col = report::Column { title: format!("lindesk {}", p.as_str()), phase: &ra.phases[i] };
            println!("{}{}", report::header(&[&ra]), report::table(&[col]));
        } else {
            println!("{}", report::render_results(&ra));
        }
        return Ok(0);
    };
    let (pb, sb) = parse_selector(b)?;
    let rb = Results::load(&pb)?;
    let ia = pick(&ra, sa, Phase::Off, &pa)?;
    let ib = pick(&rb, sb, Phase::On, &pb)?;
    let title = |r: &Results, i: usize| format!("{} ({})", r.label, r.phases[i].phase.as_str());
    let cols = [
        report::Column { title: title(&ra, ia), phase: &ra.phases[ia] },
        report::Column { title: title(&rb, ib), phase: &rb.phases[ib] },
    ];
    let headers: Vec<&Results> = if ra.run_id == rb.run_id { vec![&ra] } else { vec![&ra, &rb] };
    println!("{}{}", report::header(&headers), report::table(&cols));
    Ok(0)
}
