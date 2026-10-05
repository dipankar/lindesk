//! `lindesk-bench run`: the A/B harness.
//!
//! For each phase (`off`, then `on` by default):
//!
//! 1. check lindesk is stopped (off) or running (on); with `--manage-services`, stop or
//!    start `lindesk-session` in the user manager (never anything needing root);
//! 2. start `probe` in a transient scope in `app.slice` and wait until its read file is
//!    ready;
//! 3. start `load` in a transient scope in `lindesk-batch.slice` (or `background.slice`),
//!    wait until every generator runs, then let it ramp up;
//! 4. on: mark the probe foreground with `org.lindesk.QoS1.SetForegroundProcess(pid)`;
//! 5. SIGUSR1 both: the measurement window starts. Sample system PSI, MemAvailable and
//!    lindesk's level every second until the probe finishes;
//! 6. when the probe is done, SIGUSR2 the load (end of its window; it stops) and collect
//!    both JSON reports.
//!
//! Ctrl-C at any point stops the children (they get the SIGINT too), deletes the scratch
//! directory and restores any service the harness stopped.

use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, ValueEnum};
use serde::de::DeserializeOwned;

use crate::lindesk::{self, Lindesk};
use crate::results::{
    HostInfo, LindeskState, LoadConfig, LoadReport, Phase, PhaseResult, ProbeReport, RESULTS_VERSION, Results,
    RunConfig, TimelinePoint,
};
use crate::safety::{self, DEFAULT_MAX_MEM_FRAC, GIB, MIB, MemInfo, MemSpec, parse_size};
use crate::sys::{self, PsiSource, PsiWindow, ScratchDir};

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// 10 s phases, 512 MiB of writes, 10% memory: for trying things out.
    Quick,
    /// 45 s phases, 4 GiB of writes, 25% memory.
    Default,
    /// The torture profile: 60 s phases, 4 GiB of writes, 40% memory.
    Full,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceKind {
    /// lindesk-batch.slice (what `lindeskctl run --batch` uses).
    Batch,
    /// background.slice.
    Background,
}

impl SliceKind {
    fn unit(self) -> &'static str {
        match self {
            SliceKind::Batch => "lindesk-batch.slice",
            SliceKind::Background => "background.slice",
        }
    }
}

#[derive(Args, Clone, Debug)]
pub struct RunArgs {
    /// Shorthand for --profile quick.
    #[arg(long)]
    pub quick: bool,
    /// Load and duration preset; the flags below override it.
    #[arg(long, value_enum, default_value = "default")]
    pub profile: Profile,
    /// Phases to run, in order.
    #[arg(long, value_delimiter = ',', default_value = "off,on")]
    pub phases: Vec<String>,
    /// Measured time per phase.
    #[arg(long, value_parser = humantime::parse_duration)]
    pub duration: Option<Duration>,
    /// Load ramp-up before measuring.
    #[arg(long, value_parser = humantime::parse_duration)]
    pub ramp: Option<Duration>,
    /// Pause between phases.
    #[arg(long, value_parser = humantime::parse_duration)]
    pub cooldown: Option<Duration>,
    /// CPU workers [profile: nproc].
    #[arg(long)]
    pub cpu: Option<usize>,
    /// Disk the load may fill at once [profile: 4G, quick 512M].
    #[arg(long, value_parser = parse_size)]
    pub write_cap: Option<u64>,
    /// Disable the buffered (and direct) writers.
    #[arg(long)]
    pub no_write: bool,
    /// Disable the O_DIRECT writer.
    #[arg(long)]
    pub no_direct: bool,
    /// Disable the sequential reader.
    #[arg(long)]
    pub no_read: bool,
    /// Memory pressure, size or % of MemAvailable [profile: 25%, quick 10%, full 40%].
    #[arg(long, value_parser = MemSpec::parse)]
    pub mem: Option<MemSpec>,
    /// Disable memory pressure.
    #[arg(long)]
    pub no_mem: bool,
    /// Never use more than this fraction of MemAvailable (hard limit 0.75).
    #[arg(long, default_value_t = DEFAULT_MAX_MEM_FRAC)]
    pub max_mem_frac: f64,
    /// Also run a real build in the background, e.g. "cargo build --release".
    #[arg(long)]
    pub build_cmd: Option<String>,
    /// Slice for the background load.
    #[arg(long, value_enum, default_value = "batch")]
    pub slice: SliceKind,
    /// Scratch directory [default: $XDG_CACHE_HOME/lindesk-bench].
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Where to write bench-results-<ts>.json and .md [default: current directory].
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Label stored with the results.
    #[arg(long)]
    pub label: Option<String>,
    /// Stop/start lindesk-session (user unit) as each phase needs. Never touches lindeskd.
    #[arg(long)]
    pub manage_services: bool,
    /// Run probe and load directly, without systemd-run scopes.
    #[arg(long)]
    pub no_scope: bool,
    /// Don't store the results in the telemetry lake (which needs lindesk-data, from the
    /// optional lindesk-telemetry package; the results JSON is written either way).
    #[arg(long)]
    pub no_lake: bool,
    /// Telemetry lake root [default: $XDG_STATE_HOME/lindesk/telemetry].
    #[arg(long)]
    pub lake: Option<PathBuf>,
    /// On phase: also send BeginInteraction(pid) every second, as if the user were typing.
    #[arg(long)]
    pub interact: bool,
    /// On phase: mark the probe foreground once instead of every second.
    #[arg(long)]
    pub no_reassert: bool,
}

struct Plan {
    profile: &'static str,
    phases: Vec<Phase>,
    duration: Duration,
    ramp: Duration,
    cooldown: Duration,
    load: LoadConfig,
    mem: Option<MemSpec>,
}

fn plan(a: &RunArgs) -> Result<Plan> {
    let profile = if a.quick { Profile::Quick } else { a.profile };
    let n = sys::nproc();
    let (name, duration, ramp, cooldown, cap, mem, direct) = match profile {
        Profile::Quick => ("quick", 10, 2, 3, 512 * MIB, 10.0, false),
        Profile::Default => ("default", 45, 5, 5, 4 * GIB, 25.0, true),
        Profile::Full => ("full", 60, 5, 5, 4 * GIB, 40.0, true),
    };
    let mut phases = Vec::new();
    for p in &a.phases {
        let ph = Phase::parse(p).ok_or_else(|| anyhow!("unknown phase '{p}' (use off, on)"))?;
        phases.push(ph);
    }
    if phases.is_empty() {
        bail!("no phases");
    }
    let mem = if a.no_mem { None } else { Some(a.mem.unwrap_or(MemSpec::Percent(mem))) };
    let duration = a.duration.unwrap_or(Duration::from_secs(duration));
    if duration > Duration::from_secs(600) {
        bail!("--duration is limited to 10m per phase");
    }
    Ok(Plan {
        profile: name,
        phases,
        duration,
        ramp: a.ramp.unwrap_or(Duration::from_secs(ramp)),
        cooldown: a.cooldown.unwrap_or(Duration::from_secs(cooldown)),
        load: LoadConfig {
            cpu: a.cpu.unwrap_or(n),
            write: !a.no_write,
            direct: direct && !a.no_direct && !a.no_write,
            read: !a.no_read,
            mem: mem.map(|m| match m {
                MemSpec::Bytes(b) => safety::fmt_bytes(b),
                MemSpec::Percent(p) => format!("{p}%"),
            }),
            build_cmd: a.build_cmd.clone(),
            write_cap: a.write_cap.unwrap_or(cap),
            fsync_every: 64 * MIB,
            max_mem_frac: a.max_mem_frac.clamp(0.0, safety::HARD_MAX_MEM_FRAC),
        },
        mem,
    })
}

pub fn run(args: RunArgs) -> Result<i32> {
    let plan = plan(&args)?;
    let base = sys::scratch_base(args.dir.as_deref());
    let out_dir = args.out.clone().unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    // Preview of the caps, before anything starts.
    let mi = MemInfo::read()?;
    let mem_plan = plan.mem.map(|m| safety::plan_mem(m, &mi, plan.load.max_mem_frac));
    let existing = base.ancestors().find(|p| p.exists()).unwrap_or(Path::new("/"));
    let disk_plan = safety::plan_disk(plan.load.write_cap, safety::free_space(existing).unwrap_or(0));
    eprintln!(
        "lindesk-bench: profile {}, phases {}, {}s measured per phase (+{}s ramp)",
        plan.profile,
        plan.phases.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(","),
        plan.duration.as_secs(),
        plan.ramp.as_secs()
    );
    eprintln!(
        "lindesk-bench: load: {} CPU workers, writes {} (cap {}), reads {}, memory {}{}",
        plan.load.cpu,
        if plan.load.write { if plan.load.direct { "buffered+direct" } else { "buffered" } } else { "off" },
        safety::fmt_bytes(disk_plan.effective),
        if plan.load.read { "on" } else { "off" },
        mem_plan.map_or("off".into(), |m| format!(
            "{} of {} available",
            safety::fmt_bytes(m.effective),
            safety::fmt_bytes(m.available_at_start)
        )),
        plan.load.build_cmd.as_ref().map_or(String::new(), |c| format!(", build `{c}`")),
    );
    eprintln!("lindesk-bench: scratch in {}; Ctrl-C stops all load and deletes it", base.display());

    let ld = Lindesk::connect()?;
    let initial = ld.detect();
    check_feasible(&plan.phases, &initial, args.manage_services)?;
    let mut services = ServiceGuard { initially_active: initial.session_unit_active, changed: false };
    // systemd-run brings up lindesk-batch.slice if it isn't there; put it back down after.
    let _slice = SliceGuard {
        slice: args.slice.unit(),
        stop: args.slice == SliceKind::Batch && !args.no_scope && !user_unit_active(args.slice.unit()),
    };

    let host = host_info(&base, &mi);
    let run_id = ulid::Ulid::new().to_string();
    let short = run_id[run_id.len() - 8..].to_ascii_lowercase();
    let run_dir = ScratchDir::create(&base, &format!("run-{short}"))?;
    let started = chrono::Utc::now();

    let ctx = Ctx {
        scopes: !args.no_scope,
        slice: args.slice.unit(),
        short: short.clone(),
        plan: &plan,
        reassert: !args.no_reassert,
        interact: args.interact,
    };
    let mut phases = Vec::new();
    for (i, &phase) in plan.phases.iter().enumerate() {
        if i > 0 && !sys::sleep_unless_stopped(plan.cooldown) {
            break;
        }
        let state = ensure_phase_state(&ld, phase, args.manage_services, &mut services)?;
        eprintln!(
            "lindesk-bench: phase {} (lindesk {})",
            phase.as_str(),
            if state.running() { "running" } else { "stopped" }
        );
        match run_phase(&ctx, phase, &ld, state, run_dir.path()) {
            Ok(p) => phases.push(p),
            Err(e) if sys::stop_requested() => {
                eprintln!("lindesk-bench: interrupted ({e:#})");
                break;
            }
            Err(e) => return Err(e),
        }
    }
    drop(run_dir);
    drop(services);
    if sys::stop_requested() {
        eprintln!("lindesk-bench: interrupted; load stopped and scratch files removed, no results written");
        return Ok(130);
    }

    let results = Results {
        version: RESULTS_VERSION,
        tool: format!("lindesk-bench {}", env!("CARGO_PKG_VERSION")),
        run_id,
        label: args
            .label
            .clone()
            .unwrap_or_else(|| format!("{}-{}", plan.profile, host.hostname.as_deref().unwrap_or("host"))),
        started: started.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        config: RunConfig {
            profile: plan.profile.into(),
            phases: plan.phases.clone(),
            duration_s: plan.duration.as_secs_f64(),
            ramp_s: plan.ramp.as_secs_f64(),
            cooldown_s: plan.cooldown.as_secs_f64(),
            slice: args.slice.unit().into(),
            scopes: !args.no_scope,
            manage_services: args.manage_services,
            reassert_foreground: !args.no_reassert,
            interact: args.interact,
            load: plan.load.clone(),
            scratch_dir: base.display().to_string(),
        },
        host,
        phases,
    };

    let stem = format!("bench-results-{}", started.format("%Y%m%dT%H%M%SZ"));
    let json_path = out_dir.join(format!("{stem}.json"));
    std::fs::write(&json_path, serde_json::to_string_pretty(&results)?)
        .with_context(|| format!("writing {}", json_path.display()))?;
    let md = crate::report::render_results(&results);
    let md_path = out_dir.join(format!("{stem}.md"));
    std::fs::write(&md_path, &md).with_context(|| format!("writing {}", md_path.display()))?;
    println!("{md}");
    eprintln!("lindesk-bench: results: {}", json_path.display());
    eprintln!("lindesk-bench: table:   {}", md_path.display());
    if !args.no_lake {
        match crate::lake::store(&json_path, args.lake.as_deref()) {
            Ok(msg) => eprintln!("lindesk-bench: {msg}"),
            Err(e) => eprintln!("lindesk-bench: not stored in the telemetry lake: {e:#}"),
        }
    }
    Ok(0)
}

fn check_feasible(phases: &[Phase], s: &LindeskState, manage: bool) -> Result<()> {
    let system_side = s.broker_bus || s.system_unit_active || s.processes.iter().any(|p| p.starts_with("lindeskd"));
    if phases.contains(&Phase::Off) && s.running() && (!manage || system_side) {
        bail!(
            "the off phase needs lindesk stopped, but it is running ({}). Stop it with:\n{}",
            describe(s),
            lindesk::stop_instructions(s)
        );
    }
    if phases.contains(&Phase::On) && !s.running() && !manage {
        bail!(
            "the on phase needs lindesk running, but it isn't. Start it with:\n  systemctl --user start lindesk-session\n\
             (and `sudo systemctl start lindeskd` for system-wide control), rerun with --manage-services,\n\
             or run only the baseline with --phases off"
        );
    }
    Ok(())
}

fn describe(s: &LindeskState) -> String {
    let mut v = Vec::new();
    if s.session_bus {
        v.push("org.lindesk.QoS1 on the session bus".to_string());
    }
    if s.broker_bus {
        v.push("org.lindesk.Broker1 on the system bus".into());
    }
    if s.session_unit_active {
        v.push(lindesk::SESSION_UNIT.into());
    }
    if s.system_unit_active {
        v.push(lindesk::SYSTEM_UNIT.into());
    }
    v.extend(s.processes.iter().cloned());
    v.join(", ")
}

/// Puts `lindesk-session` back the way it was.
struct ServiceGuard {
    initially_active: bool,
    changed: bool,
}

impl Drop for ServiceGuard {
    fn drop(&mut self) {
        if self.changed {
            let verb = if self.initially_active { "start" } else { "stop" };
            eprintln!("lindesk-bench: restoring lindesk-session ({verb})");
            if let Err(e) = lindesk::user_service(verb) {
                eprintln!("lindesk-bench: {e:#}");
            }
        }
    }
}

fn user_unit_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", unit])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Stops a slice the bench activated, but only if nothing runs in it any more.
struct SliceGuard {
    slice: &'static str,
    stop: bool,
}

impl Drop for SliceGuard {
    fn drop(&mut self) {
        if !self.stop || !user_unit_active(self.slice) {
            return;
        }
        let cg = Command::new("systemctl")
            .args(["--user", "show", "-p", "ControlGroup", "--value", self.slice])
            .stderr(Stdio::null())
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| s.starts_with('/'));
        let empty = cg.is_some_and(|cg| {
            std::fs::read_to_string(format!("/sys/fs/cgroup{cg}/cgroup.events"))
                .is_ok_and(|e| e.lines().any(|l| l.trim() == "populated 0"))
        });
        if empty {
            let _ = Command::new("systemctl")
                .args(["--user", "stop", self.slice])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn ensure_phase_state(ld: &Lindesk, phase: Phase, manage: bool, g: &mut ServiceGuard) -> Result<LindeskState> {
    let s = ld.detect();
    match phase {
        Phase::Off if s.running() => {
            if !manage {
                bail!(
                    "lindesk is running ({}); stop it for the off phase:\n{}",
                    describe(&s),
                    lindesk::stop_instructions(&s)
                );
            }
            lindesk::user_service("stop")?;
            g.changed = true;
            wait_for(ld, |s| !s.running(), "lindesk to stop")
        }
        Phase::On if !s.running() => {
            if !manage {
                bail!("lindesk is not running; start it for the on phase: systemctl --user start lindesk-session");
            }
            lindesk::user_service("start")?;
            g.changed = true;
            wait_for(ld, |s| s.session_bus, "lindesk-session to own org.lindesk.QoS1")
        }
        _ => Ok(s),
    }
}

fn wait_for(ld: &Lindesk, ok: impl Fn(&LindeskState) -> bool, what: &str) -> Result<LindeskState> {
    let t0 = Instant::now();
    loop {
        let s = ld.detect();
        if ok(&s) {
            return Ok(s);
        }
        if t0.elapsed() > Duration::from_secs(15) {
            bail!("timed out waiting for {what} ({})", describe(&s));
        }
        if !sys::sleep_unless_stopped(Duration::from_millis(250)) {
            bail!("interrupted");
        }
    }
}

struct Ctx<'a> {
    scopes: bool,
    slice: &'static str,
    short: String,
    plan: &'a Plan,
    reassert: bool,
    interact: bool,
}

/// A probe or load child with its stdout read line by line.
struct Spawned {
    what: &'static str,
    child: Child,
    lines: Receiver<String>,
    seen: Vec<String>,
}

impl Spawned {
    fn start(ctx: &Ctx<'_>, what: &'static str, unit: &str, slice: &str, args: Vec<String>) -> Result<Spawned> {
        let exe = std::env::current_exe().context("finding own executable")?;
        let mut cmd = if ctx.scopes {
            let mut c = Command::new("systemd-run");
            c.args(["--user", "--scope", "--collect", "--quiet"])
                .arg(format!("--unit={unit}"))
                .arg(format!("--slice={slice}"))
                .arg(format!("--description=lindesk-bench {what}"))
                .arg("--")
                .arg(&exe);
            c
        } else {
            Command::new(&exe)
        };
        cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        // SAFETY: prctl only, async-signal-safe.
        unsafe { cmd.pre_exec(sys::parent_death_signal) };
        let mut child = cmd.spawn().with_context(|| {
            if ctx.scopes {
                "starting systemd-run (rerun with --no-scope to skip scopes)".to_string()
            } else {
                format!("starting {what}")
            }
        })?;
        let out = child.stdout.take().expect("piped stdout");
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Spawned { what, child, lines: rx, seen: Vec::new() })
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Waits for a `{"event": <event>}` line.
    fn wait_event(&mut self, event: &str, timeout: Duration) -> Result<()> {
        let t0 = Instant::now();
        loop {
            if sys::stop_requested() {
                bail!("interrupted while waiting for {} to get ready", self.what);
            }
            match self.lines.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    let ev = serde_json::from_str::<serde_json::Value>(&line).ok();
                    let hit = ev.as_ref().and_then(|v| v.get("event")).and_then(|e| e.as_str()) == Some(event);
                    self.seen.push(line);
                    if hit {
                        return Ok(());
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    let st = self.child.wait().ok();
                    bail!("{} exited before it was ready ({})", self.what, st.map_or("?".into(), |s| s.to_string()));
                }
            }
            if t0.elapsed() > timeout {
                bail!("{} not ready after {}s", self.what, timeout.as_secs());
            }
        }
    }

    fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// Waits for exit (SIGKILL after `timeout`) and returns the last JSON line of `kind`.
    fn finish<T: DeserializeOwned>(&mut self, kind: &str, timeout: Duration) -> Result<T> {
        let t0 = Instant::now();
        while !self.exited() {
            if t0.elapsed() > timeout {
                eprintln!("lindesk-bench: {} did not exit in {}s, killing it", self.what, timeout.as_secs());
                let _ = self.child.kill();
                let _ = self.child.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // The reader thread finishes at EOF.
        while let Ok(line) = self.lines.recv_timeout(Duration::from_secs(2)) {
            self.seen.push(line);
        }
        let line = self
            .seen
            .iter()
            .rev()
            .find(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .is_ok_and(|v| v.get("kind").and_then(|k| k.as_str()) == Some(kind))
            })
            .ok_or_else(|| anyhow!("{} produced no {kind} report", self.what))?;
        serde_json::from_str(line).with_context(|| format!("parsing the {} report", self.what))
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        if !self.exited() {
            sys::kill(self.pid(), libc::SIGINT);
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(10) {
                if self.exited() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn run_phase(ctx: &Ctx<'_>, phase: Phase, ld: &Lindesk, state: LindeskState, run_dir: &Path) -> Result<PhaseResult> {
    let plan = ctx.plan;
    let unit = |kind: &str| format!("lindesk-bench-{kind}-{}-{}", ctx.short, phase.as_str());
    let dir = run_dir.display().to_string();

    let mut probe = Spawned::start(
        ctx,
        "probe",
        &unit("probe"),
        "app.slice",
        vec![
            "probe".into(),
            "--wait-start".into(),
            "--compact".into(),
            "--dir".into(),
            dir.clone(),
            "--duration".into(),
            format!("{}ms", plan.duration.as_millis()),
        ],
    )?;
    probe.wait_event("ready", Duration::from_secs(120))?;

    let l = &plan.load;
    let load_secs = plan.ramp.as_secs() + plan.duration.as_secs() + 120;
    let mut largs: Vec<String> = vec![
        "load".into(),
        "--announce-ready".into(),
        "--compact".into(),
        "--dir".into(),
        dir,
        "--duration".into(),
        format!("{load_secs}s"),
        "--cpu".into(),
        l.cpu.to_string(),
        "--cap".into(),
        l.write_cap.to_string(),
        "--fsync-every".into(),
        l.fsync_every.to_string(),
        "--max-mem-frac".into(),
        l.max_mem_frac.to_string(),
    ];
    for (on, flag) in [(l.write, "--write"), (l.direct, "--direct"), (l.read, "--read")] {
        if on {
            largs.push(flag.into());
        }
    }
    if let Some(m) = &plan.mem {
        largs.push("--mem".into());
        largs.push(match m {
            MemSpec::Bytes(b) => b.to_string(),
            MemSpec::Percent(p) => format!("{p}%"),
        });
    }
    if let Some(c) = &l.build_cmd {
        largs.push("--cmd".into());
        largs.push(c.clone());
    }
    let mut load = Spawned::start(ctx, "load", &unit("load"), ctx.slice, largs)?;
    load.wait_event("ready", Duration::from_secs(180))?;
    if !sys::sleep_unless_stopped(plan.ramp) {
        bail!("interrupted during ramp-up");
    }

    let on = phase == Phase::On;
    let mut foreground_marked = false;
    if on {
        match ld.set_foreground(probe.pid()) {
            Ok(()) => foreground_marked = true,
            Err(e) => eprintln!("lindesk-bench: could not mark the probe as foreground: {e:#}"),
        }
    }

    // Start of the measurement window.
    sys::kill(load.pid(), libc::SIGUSR1);
    sys::kill(probe.pid(), libc::SIGUSR1);
    let t0 = Instant::now();
    let started = chrono::Utc::now();

    let mut timeline = Vec::new();
    let mut prev = PsiSource::System.snapshot();
    let deadline = plan.duration + Duration::from_secs(30);
    let mut next_tick = Duration::from_secs(1);
    while !probe.exited() {
        if sys::stop_requested() {
            bail!("interrupted");
        }
        if t0.elapsed() > deadline {
            break;
        }
        if t0.elapsed() >= next_tick {
            next_tick += Duration::from_secs(1);
            let now = PsiSource::System.snapshot();
            let level = if on { ld.level() } else { None };
            if on && ctx.reassert {
                let _ = ld.set_foreground(probe.pid());
            }
            if on && ctx.interact {
                let _ = ld.begin_interaction(probe.pid());
            }
            timeline.push(TimelinePoint {
                t_s: (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
                psi: PsiWindow::between(&prev, &now),
                mem_available: MemInfo::read().map_or(0, |m| m.available),
                level,
            });
            prev = now;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // End of the window for the load too, before anything slow happens.
    sys::kill(load.pid(), libc::SIGUSR2);
    let probe_report: ProbeReport = probe.finish("probe-summary", Duration::from_secs(10))?;
    let load_report: LoadReport = load.finish("load-report", Duration::from_secs(30))?;
    if !load_report.cleaned_up {
        eprintln!("lindesk-bench: warning: the load reported leftover files: {:?}", load_report.errors);
    }
    let mut levels = std::collections::BTreeMap::new();
    for p in &timeline {
        if let Some(l) = &p.level {
            *levels.entry(l.clone()).or_insert(0) += 1;
        }
    }
    let mut result = PhaseResult {
        phase,
        started: started.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        started_ms: started.timestamp_millis(),
        lindesk: state,
        foreground_marked,
        probe: probe_report,
        load: load_report,
        timeline,
        levels,
        metrics: Vec::new(),
    };
    result.metrics = crate::lake::phase_metrics(&result);
    Ok(result)
}

fn host_info(scratch: &Path, mi: &MemInfo) -> HostInfo {
    let p = lindesk_sys::probe(&lindesk_sys::Roots::host(), &lindesk_sys::Env::from_process());
    let (level, notes) = p.support();
    let read = |path: &str| std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let cpu_model = std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|s| {
        s.lines().find_map(|l| {
            l.strip_prefix("model name").and_then(|r| r.split_once(':')).map(|(_, v)| v.trim().to_string())
        })
    });
    let name = |v: serde_json::Result<serde_json::Value>| {
        v.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
    };
    let desktop =
        format!("{}/{}", name(serde_json::to_value(p.desktop.kind)), name(serde_json::to_value(p.desktop.session)));
    let scratch = std::fs::canonicalize(scratch).unwrap_or_else(|_| scratch.to_path_buf());
    let scratch_fs =
        p.mounts.iter().filter(|m| scratch.starts_with(&m.mount_point)).max_by_key(|m| m.mount_point.len()).map(|m| {
            let devs: Vec<String> = m
                .devices
                .iter()
                .map(|d| format!("{} ({})", d.name, d.scheduler.as_deref().unwrap_or("no scheduler")))
                .collect();
            format!("{} on {} [{}]", m.fs_type, m.mount_point, devs.join(", "))
        });
    HostInfo {
        hostname: read("/proc/sys/kernel/hostname"),
        kernel: p.kernel.clone(),
        cpu_model,
        nproc: sys::nproc(),
        mem_total: mi.total,
        mem_available: mi.available,
        support_level: level.as_str().into(),
        support_notes: notes,
        backend: p.backend().as_str().into(),
        desktop: Some(desktop),
        scratch_fs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(flatten)]
        run: RunArgs,
    }

    fn parse(args: &[&str]) -> RunArgs {
        T::parse_from(std::iter::once("t").chain(args.iter().copied())).run
    }

    #[test]
    fn profiles_and_overrides() {
        let p = plan(&parse(&["--quick", "--phases", "off"])).unwrap();
        assert_eq!(p.profile, "quick");
        assert_eq!(p.phases, vec![Phase::Off]);
        assert_eq!(p.duration, Duration::from_secs(10));
        assert_eq!(p.load.write_cap, 512 * MIB);
        assert!(!p.load.direct);
        assert_eq!(p.mem, Some(MemSpec::Percent(10.0)));

        let p = plan(&parse(&["--profile", "full", "--no-mem", "--cpu", "3", "--max-mem-frac", "0.9"])).unwrap();
        assert_eq!(p.phases, vec![Phase::Off, Phase::On]);
        assert_eq!(p.duration, Duration::from_secs(60));
        assert_eq!(p.load.write_cap, 4 * GIB);
        assert!(p.load.direct && p.load.write && p.load.read);
        assert_eq!(p.load.cpu, 3);
        assert_eq!(p.mem, None);
        assert_eq!(p.load.max_mem_frac, safety::HARD_MAX_MEM_FRAC);

        assert!(plan(&parse(&["--phases", "off,maybe"])).is_err());
        assert!(plan(&parse(&["--duration", "1h"])).is_err());
    }

    #[test]
    fn feasibility() {
        let off = LindeskState::default();
        let session = LindeskState { session_bus: true, session_unit_active: true, ..Default::default() };
        let broker = LindeskState { broker_bus: true, ..Default::default() };
        assert!(check_feasible(&[Phase::Off], &off, false).is_ok());
        assert!(check_feasible(&[Phase::Off, Phase::On], &off, false).is_err());
        assert!(check_feasible(&[Phase::Off, Phase::On], &off, true).is_ok());
        assert!(check_feasible(&[Phase::Off], &session, false).is_err());
        assert!(check_feasible(&[Phase::Off], &session, true).is_ok());
        // lindeskd is never stopped by the bench.
        let e = check_feasible(&[Phase::Off], &broker, true).unwrap_err().to_string();
        assert!(e.contains("sudo systemctl stop lindeskd"));
        assert!(check_feasible(&[Phase::On], &broker, false).is_ok());
    }
}
