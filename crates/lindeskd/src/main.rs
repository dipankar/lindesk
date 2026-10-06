//! `lindeskd`: the lindesk root broker.
//!
//! Probes the host, enables iocost where it helps, runs the plugin pipeline for system units
//! (and relays decisions about user units to each user's `lindesk-session`), and serves
//! `org.lindesk.Broker1` on the system bus. See docs/ARCHITECTURE.md.
//!
//! Threads: the engine runs on its own thread ([`common::engine_thread`]); the D-Bus server,
//! signal handling, the systemd watchdog and the `UserState` heartbeat run on a small tokio
//! runtime.

mod broker;
// Shared with lindesk-session (each binary uses a subset).
#[allow(dead_code)]
mod common;
mod dbus;
mod iocost;
mod pagein;
mod threadqos;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use lindesk_builtins::actuators::systemd::SystemdClient;
use lindesk_builtins::actuators::{self, Bus, RelayActuator, RelayTable};
use lindesk_builtins::classifier::{RulesClassifier, WorkloadClassifier};
use lindesk_builtins::env::{Mode, SysEnvironment, UserWatch};
use lindesk_builtins::policy::ThresholdPolicy;
use lindesk_builtins::sensors::{CgroupStatSensor, PsiSensor};
use lindesk_changes::Scope as ChangeScope;
use lindesk_core::config::Config;
use lindesk_host::{Engine, EngineOptions};
use lindesk_ipc::{BROKER_NAME, BROKER_PATH};
use lindesk_plugin_api::{Actuator, Privilege};
use lindesk_sys::blockdev::{self, BlockDev, DevId};
use lindesk_sys::requirements::Component;
use lindesk_sys::{Backend, Env, Probe, Roots};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::broker::{BrokerHooks, HostInfo};
use crate::common::Signals;
use crate::common::calibration::Calibrator;
use crate::common::control::Control;
use crate::common::defaults::{ensure_dir, ensure_rules};
use crate::common::engine_thread::{self, EngineHandle, Notification};
use crate::common::observe::{Observed, PolicyMetrics};
use crate::common::profiles::Profiles;
use crate::common::restore::{arm_timeout, restore_state};
use crate::common::setup::{self, registered};
use crate::common::startup::{HardwareWatch, check_requirements, requirement_notices};
use crate::iocost::IoCostManager;

/// How often every registered user's state is re-sent, changed or not.
const HEARTBEAT: Duration = Duration::from_secs(30);
/// How long shutdown waits for the engine to restore everything.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(20);

/// lindesk root broker: system-wide QoS governor for desktop responsiveness.
#[derive(Parser, Debug, Clone, PartialEq)]
#[command(name = "lindeskd", version = lindesk_version::LONG, about, long_about = None)]
pub struct Cli {
    /// Configuration file.
    #[arg(long, value_name = "FILE", default_value = "/etc/lindesk/lindesk.toml")]
    pub config: PathBuf,
    /// Runtime state directory (applied state, saved iocost settings).
    #[arg(long, value_name = "DIR", default_value = "/run/lindesk")]
    pub state_dir: PathBuf,
    /// Telemetry lake root (overrides `[telemetry] dir`).
    #[arg(long, value_name = "DIR")]
    pub telemetry_dir: Option<PathBuf>,
    /// Observe only: compute and record actions but change nothing (iocost included).
    #[arg(long)]
    pub dry_run: bool,
    /// Log filter (`info`, `debug`, `info,lindesk_host=debug`, …). Default: `RUST_LOG`, else `info`.
    #[arg(long, value_name = "FILTER")]
    pub log_level: Option<String>,
    /// Undo everything recorded in the state directory (settings and iocost), then exit.
    #[arg(long)]
    pub restore: bool,
    /// Don't connect to the system bus (tests, hosts without D-Bus).
    #[arg(long)]
    pub no_dbus: bool,
}

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let cli = Cli::parse();
    common::logging::init("lindeskd", cli.log_level.as_deref());
    if cli.restore {
        return restore(&cli);
    }
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

/// The broker's actuators, highest priority first: the relay (user units; every other
/// target is `Unsupported` there, so the engine moves on), then the backend's set (normally
/// `systemd` on the system bus for system units, then `proc`).
fn build_actuators(backend: Backend) -> (Vec<Box<dyn Actuator>>, RelayTable) {
    let relay = RelayActuator::new();
    let table = relay.table();
    let mut all: Vec<Box<dyn Actuator>> = vec![Box::new(relay)];
    all.extend(actuators::for_backend(backend, Bus::System));
    // Timer slack is per thread and needs CAP_SYS_NICE: done here for every user's units.
    all.push(Box::new(actuators::TimerSlackActuator::default()));
    (all, table)
}

/// Persistent state (versioned changes, calibration): `/var/lib/lindesk` as root, else the
/// state directory (unprivileged test runs).
fn lib_dir(state_dir: &std::path::Path) -> PathBuf {
    if rustix::process::geteuid().is_root() { PathBuf::from(LIB_DIR) } else { state_dir.to_path_buf() }
}

fn restore(cli: &Cli) -> ExitCode {
    arm_timeout("lindeskd --restore");
    let roots = Roots::host();
    let probe = lindesk_sys::probe(&roots, &Env::from_process());
    let (acts, _) = build_actuators(probe.backend());
    let state = cli.state_dir.join("state.json");
    let mut ok = true;
    match restore_state(&state, "lindeskd", Privilege::System, None, acts) {
        Ok(o) if o.entries == 0 => tracing::info!("no applied settings to restore"),
        Ok(o) => {
            tracing::info!("restored {}/{} settings", o.restored, o.entries);
            ok &= o.is_clean();
        }
        Err(e) => {
            tracing::error!("restoring {}: {e:#}", state.display());
            ok = false;
        }
    }
    let (restored, failed) = IoCostManager::new(&roots.cgroup, &cli.state_dir).restore();
    if restored > 0 {
        tracing::info!("restored iocost on {restored} device(s)");
    }
    ok &= failed == 0;
    let threads = threadqos::restore(&cli.state_dir, &roots);
    if threads > 0 {
        tracing::info!("put {threads} thread priorities back");
    }
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// Devices `[io] devices` names, else the disks behind `/` and `/home`.
/// Per-thread priorities in the app in use (`[plugins.thread-qos]`), unless disabled,
/// misconfigured or a dry run.
fn start_threadqos(
    engine: &Engine,
    config: &Config,
    roots: &Roots,
    state_dir: &std::path::Path,
    dry_run: bool,
) -> Option<threadqos::ThreadQos> {
    let pc = config.plugin(threadqos::NAME);
    if dry_run || !pc.enabled {
        return None;
    }
    let cfg = match threadqos::ThreadQosConfig::from_json(&pc.config, config.safety.nice_min) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("[plugins.{}] config: {e}; per-thread priorities are off", threadqos::NAME);
            return None;
        }
    };
    let host = engine.options().host.clone();
    match threadqos::ThreadQos::spawn(cfg, roots.clone(), state_dir, host, engine.host_sender()) {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!("per-thread priorities: cannot start the worker thread: {e}");
            None
        }
    }
}

/// Page-in on focus (`[plugins.pagein]`), unless disabled, misconfigured or a dry run.
fn start_pagein(engine: &Engine, config: &Config, roots: &Roots, dry_run: bool) -> Option<pagein::PageIn> {
    let pc = config.plugin(pagein::NAME);
    if dry_run || !pc.enabled {
        return None;
    }
    let cfg = match pagein::PageInConfig::from_json(&pc.config) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("[plugins.{}] config: {e}; page-in on focus is off", pagein::NAME);
            return None;
        }
    };
    let host = engine.options().host.clone();
    match pagein::PageIn::spawn(cfg, roots.clone(), host, engine.host_sender()) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!("page-in on focus: cannot start the worker thread: {e}");
            None
        }
    }
}

fn managed_devices(config: &Config, probe: &Probe, roots: &Roots) -> Vec<BlockDev> {
    if config.io.devices.is_empty() {
        return probe.devices().into_iter().cloned().collect();
    }
    config
        .io
        .devices
        .iter()
        .filter_map(|d| match d.parse::<DevId>().and_then(|id| blockdev::block_dev(&roots.sys, id)) {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::warn!("[io] devices: {d}: {e}");
                None
            }
        })
        .collect()
}

fn run(cli: Cli) -> anyhow::Result<()> {
    std::fs::create_dir_all(&cli.state_dir).with_context(|| format!("creating {}", cli.state_dir.display()))?;
    // The engine's command queue exists before the engine: the change store reports
    // recovered changesets into it.
    let (handle, inbox) = engine_thread::channel()?;
    let (change_tx, change_rx) = tokio::sync::mpsc::unbounded_channel();
    let lib = lib_dir(&cli.state_dir);
    let mut control_error = None;
    let control =
        match Control::open(&lib.join("changes"), ChangeScope::System, cli.config.clone(), handle.clone(), change_tx) {
            Ok(c) => {
                // Before the config is read: a rolled-back changeset may have restored it.
                c.recover();
                Some(Arc::new(c))
            }
            Err(e) => {
                tracing::error!("versioned changes unavailable: {e}");
                control_error = Some(format!("versioned changes unavailable: {e}"));
                None
            }
        };
    if rustix::process::geteuid().is_root() {
        // The pre-lindesk state of the conflicting daemons, however lindesk got installed
        // (install.sh and the packages normally wrote it already; never overwritten).
        let path = lib.join("install-state.json");
        let services = lindesk_changes::Systemctl::from_env();
        match lindesk_changes::install_state::ensure(
            &path,
            "daemon",
            env!("CARGO_PKG_VERSION"),
            &services,
            lindesk_core::clock::now_ms(),
        ) {
            Ok(true) => tracing::info!("recorded the install-time state in {}", path.display()),
            Ok(false) => {}
            Err(e) => tracing::warn!("install-time state {}: {e}", path.display()),
        }
    }
    let prepare = {
        let (dry, tel, state) = (cli.dry_run, cli.telemetry_dir.clone(), cli.state_dir.clone());
        move |c: &mut Config| {
            setup::prepare_config(c, dry, tel.as_deref(), Privilege::System);
            ensure_rules(c, &state, Privilege::System);
        }
    };
    let mut config = Config::load(&cli.config)?;
    prepare(&mut config);
    let dry_run = config.general.dry_run;
    // The runtime and signal handlers come first: a SIGTERM during start-up must still end in
    // an orderly shutdown (the engine may already have applied settings).
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let signals = rt.block_on(async { Signals::install() })?;
    let rules_source = ensure_rules(&mut config, &cli.state_dir, Privilege::System);

    let roots = Roots::host();
    let env_vars = Env::from_process();
    let mut probe = lindesk_sys::probe(&roots, &env_vars);
    let requirements = match check_requirements(Component::Broker, &roots, &env_vars, &probe, !cli.no_dbus) {
        Ok(w) => w,
        Err(code) => std::process::exit(code),
    };
    if dry_run {
        tracing::info!("lindeskd {}: dry run, nothing will be changed", env!("CARGO_PKG_VERSION"));
    }
    let conflicts: Vec<String> = probe.conflicts.iter().filter(|c| c.running).map(|c| c.name.clone()).collect();
    for c in &conflicts {
        tracing::warn!("{c} is running and also tunes priorities/cgroups; the two will fight. Stop one of them.");
    }

    // iocost.
    let ioc = IoCostManager::new(&roots.cgroup, &cli.state_dir);
    let devices = managed_devices(&config, &probe, &roots);
    let iocost_enabled = if dry_run {
        // Leftovers of a crashed real run are put back; nothing new is enabled.
        ioc.restore();
        for d in &devices {
            if let iocost::Decision::Enable = iocost::decide(config.io.iocost, d, probe.iocost_enabled(d.dev)) {
                tracing::info!("dry run: would enable iocost on {} ({})", d.name, d.dev);
            }
        }
        Vec::new()
    } else {
        let v = ioc.enable(config.io.iocost, config.io.iocost_latency_targets, &devices);
        if !v.is_empty() {
            probe = lindesk_sys::probe(&roots, &Env::from_process());
        }
        v
    };
    let mut caps = probe.host_caps();
    let calibration = if rustix::process::geteuid().is_root() {
        lindesk_sys::calibrate::default_path(true)
    } else {
        lib.join("calibration.json")
    };
    if let Ok(map) = lindesk_sys::calibrate::load(&calibration) {
        tracing::info!("using the calibration in {}", calibration.display());
        lindesk_sys::calibrate::attach(&mut caps.hardware, map);
    }
    let mut profiles = Profiles::load(config.clone(), &lindesk_core::profile::default_dirs(None));
    let config = profiles.initial(&caps.hardware, lindesk_core::clock::now_ms());

    // Engine.
    let users = UserWatch::new();
    let env = Arc::new(
        SysEnvironment::new(roots.clone(), Mode::System)
            .with_systemd(SystemdClient::new(Bus::System, 0))
            .with_users(users.clone()),
    );
    let state_path = cli.state_dir.join("state.json");
    let mut opts = EngineOptions::new("lindeskd", Privilege::System);
    opts.caps = caps.clone();
    opts.state_path = Some(state_path.clone());
    let mut engine = Engine::new(config.clone(), Box::new(env.clone()), opts)?;
    let metrics = PolicyMetrics::default();
    let relay = register_builtins(&mut engine, &config, probe.backend(), &metrics);
    setup::load_externals(&mut engine, Privilege::System);
    if setup::telemetry_loaded(&engine) {
        // The plugin (lindesk-telemetry) creates the lake itself; group `lindesk` lets its
        // members query the system lake.
        let dir = config.plugin(setup::TELEMETRY).config["dir"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(TELEMETRY_DIR));
        if let Err(e) = ensure_dir(&dir, 0o750, Some("lindesk")) {
            tracing::warn!("telemetry directory {}: {e}", dir.display());
        }
    } else if config.telemetry.enabled {
        tracing::info!("{}", setup::TELEMETRY_NOT_INSTALLED);
    }
    match engine.load_state(&state_path) {
        Ok(0) => {}
        Ok(n) => tracing::info!("recovered {n} applied settings from {}", state_path.display()),
        Err(e) => tracing::warn!("ignoring applied state: {e}"),
    }
    requirement_notices(&mut engine, &requirements);
    for (name, e) in engine.start() {
        tracing::warn!(plugin = %name, "adapter not started: {e}");
    }

    let mut hooks = BrokerHooks::new(env, users, relay, metrics);
    hooks.info = HostInfo {
        support_level: probe.support_level().to_string(),
        backend: probe.backend().to_string(),
        iocost_enabled,
        conflicts,
    };
    hooks.config_path = cli.config.clone();
    hooks.pause_file = Some(lib.join("paused.json"));
    hooks.prepare = Box::new(prepare);
    let profile_name = profiles.active().to_string();
    hooks.profiles = Some(profiles);
    hooks.hardware = Some(HardwareWatch::new(roots.clone()));
    hooks.requirements = requirements;
    hooks.requirements_watch = Some(crate::common::startup::RequirementsWatch::new(
        Component::Broker,
        roots.clone(),
        Env::from_process(),
        !cli.no_dbus,
    ));
    hooks.rules_source = rules_source.as_str();
    hooks.degraded = degraded_reasons(&probe, cli.no_dbus);
    hooks.degraded.extend(control_error);
    let calibrator = Calibrator::new(calibration.clone(), roots.clone(), lindesk_core::clock::now_ms(), false);
    let calibration = dbus::CalibrationSetup { roots: roots.clone(), path: calibration, busy: calibrator.busy() };
    hooks.calibrator = (!dry_run).then_some(calibrator);
    hooks.pagein = start_pagein(&engine, &config, &roots, dry_run);
    hooks.threadqos = start_threadqos(&engine, &config, &roots, &cli.state_dir, dry_run);
    let summary = format!(
        "lindeskd ready: level {}, profile {profile_name}, {}, mode broker{}, rules {}",
        probe.support_level(),
        lindesk_sys::hardware::summary_line(&probe.hardware),
        if dry_run { " (dry run)" } else { "" },
        rules_source.as_str()
    );
    let thread = engine_thread::spawn(engine, hooks, inbox)?;
    let (notes, join) = thread.split();
    tracing::info!("{summary}");

    let insecure = dbus::insecure_no_polkit(
        std::env::var(dbus::INSECURE_ENV).ok().as_deref(),
        rustix::process::geteuid().is_root(),
    );
    if insecure {
        tracing::warn!("{}=1: polkit checks are DISABLED (test mode)", dbus::INSECURE_ENV);
    }
    let served = rt.block_on(serve(&cli, handle.clone(), notes, insecure, signals, control, calibration, change_rx));
    handle.shutdown();
    let engine = engine_thread::joined(join);
    if !dry_run {
        let (restored, failed) = ioc.restore();
        if restored + failed > 0 {
            tracing::info!("iocost restored on {restored} device(s), {failed} failed");
        }
    }
    rt.shutdown_timeout(Duration::from_secs(1));
    // A panicked engine is an error: exit non-zero so Restart=on-failure brings us back.
    engine?;
    served
}

/// Default system telemetry lake.
const TELEMETRY_DIR: &str = "/var/lib/lindesk/telemetry";
/// Persistent broker state (changes, calibration) when running as root.
const LIB_DIR: &str = "/var/lib/lindesk";

/// Why the broker runs with less than the full feature set.
fn degraded_reasons(probe: &Probe, no_dbus: bool) -> Vec<String> {
    let mut v = Vec::new();
    if !probe.psi.global {
        v.push("no PSI: no pressure-driven levels, static weights only".to_string());
    }
    if probe.backend() != Backend::Systemd {
        v.push(format!("no systemd: using the {} backend", probe.backend()));
    }
    if no_dbus {
        v.push("--no-dbus: no org.lindesk.Broker1, no session agents".to_string());
    }
    if !rustix::process::geteuid().is_root() {
        v.push("not running as root: system units can't be changed".to_string());
    }
    v
}

fn register_builtins(engine: &mut Engine, config: &Config, backend: Backend, metrics: &PolicyMetrics) -> RelayTable {
    let tick = config.general.tick;
    let reg = engine.registry_mut();
    registered("psi", reg.register_sensor(Box::new(PsiSensor::new()), tick));
    registered("cgroup-stat", reg.register_sensor(Box::new(CgroupStatSensor::new()), config.telemetry.interval));
    registered("rules", reg.register_classifier(Box::new(RulesClassifier::default()), 0));
    // After the rules: speaks only for units they had no opinion on (builds in terminal tabs).
    registered("workload", reg.register_classifier(Box::new(WorkloadClassifier::default()), -10));
    registered(
        "threshold",
        reg.register_policy(Box::new(Observed::new(ThresholdPolicy::default(), metrics.clone())), 0),
    );
    let (acts, table) = build_actuators(backend);
    let n = acts.len() as i32;
    for (i, a) in acts.into_iter().enumerate() {
        let name = a.info().name;
        registered(&name, reg.register_actuator(a, 100 + n - i as i32));
    }
    table
}

/// D-Bus, signals, watchdog, heartbeat and the changes' dead-man timer until SIGTERM/SIGINT
/// or the engine stops.
#[allow(clippy::too_many_arguments)]
async fn serve(
    cli: &Cli,
    handle: EngineHandle<broker::BrokerExt>,
    mut notes: UnboundedReceiver<Notification>,
    insecure: bool,
    signals: Signals,
    control: Option<Arc<Control<broker::BrokerExt>>>,
    calibration: dbus::CalibrationSetup,
    mut changes: UnboundedReceiver<String>,
) -> anyhow::Result<()> {
    let Signals { mut term, mut int, mut hup } = signals;
    let sessions = dbus::Sessions::default();
    if let Some(c) = &control {
        tokio::spawn(c.clone().run_expiry());
    }
    let conn = if cli.no_dbus {
        None
    } else {
        let iface = dbus::Broker { engine: handle.clone(), sessions: sessions.clone(), insecure, control, calibration };
        let conn = zbus::connection::Builder::system()?
            .serve_at(BROKER_PATH, iface)?
            .name(BROKER_NAME)?
            .build()
            .await
            .context("serving org.lindesk.Broker1 on the system bus")?;
        tracing::info!("serving {BROKER_NAME} on the system bus");
        Some(conn)
    };
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Ready, sd_notify::NotifyState::Status("running")]);

    let mut usec = 0u64;
    if sd_notify::watchdog_enabled(false, &mut usec) && usec > 0 {
        let h = handle.clone();
        tokio::spawn(watchdog(h, Duration::from_micros(usec)));
    }

    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.tick().await;
    let mut stopping = false;
    loop {
        tokio::select! {
            _ = term.recv(), if !stopping => { stopping = true; begin_stop(&handle, "SIGTERM"); }
            _ = int.recv(), if !stopping => { stopping = true; begin_stop(&handle, "SIGINT"); }
            _ = hup.recv() => {
                let h = handle.clone();
                tokio::spawn(async move {
                    match h.reload().await {
                        Ok(Ok(s)) => tracing::info!("SIGHUP: {s}"),
                        Ok(Err(e)) => tracing::warn!("SIGHUP reload failed: {e}"),
                        Err(e) => tracing::warn!("SIGHUP: {e}"),
                    }
                });
            }
            _ = heartbeat.tick() => {
                if let Some(c) = &conn {
                    for (uid, json) in handle.published().users {
                        dbus::send_user_state(c, &sessions, uid, &json).await;
                    }
                }
            }
            Some(json) = changes.recv() => {
                if let Some(c) = &conn { dbus::send_change_event(c, &json).await; }
            }
            n = notes.recv() => match n {
                Some(Notification::UserState { uid, json }) => {
                    if let Some(c) = &conn { dbus::send_user_state(c, &sessions, uid, &json).await; }
                }
                Some(Notification::Events { json, audience }) => {
                    if let Some(c) = &conn { dbus::send_events(c, &sessions, &json, audience).await; }
                }
                Some(Notification::LevelChanged(level)) => {
                    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Status(&format!("level {level}"))]);
                }
                Some(Notification::Stopped) | None => {
                    if !stopping { tracing::error!("the engine stopped unexpectedly"); }
                    break;
                }
            },
        }
        if stopping {
            // Keep serving while the engine restores, but don't wait forever.
            match tokio::time::timeout(SHUTDOWN_TIMEOUT, drain_until_stopped(&mut notes)).await {
                Ok(()) => {}
                Err(_) => tracing::error!("the engine did not stop within {SHUTDOWN_TIMEOUT:?}"),
            }
            break;
        }
    }
    if let Some(c) = conn {
        let _ = c.release_name(BROKER_NAME).await;
    }
    Ok(())
}

fn begin_stop(handle: &EngineHandle<broker::BrokerExt>, why: &str) {
    tracing::info!("{why}: stopping");
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]);
    handle.shutdown();
}

async fn drain_until_stopped(notes: &mut UnboundedReceiver<Notification>) {
    while let Some(n) = notes.recv().await {
        if n == Notification::Stopped {
            return;
        }
    }
}

/// Pet the systemd watchdog at half its period, but only while the engine loop is alive.
async fn watchdog(handle: EngineHandle<broker::BrokerExt>, period: Duration) {
    let mut t = tokio::time::interval(period / 2);
    loop {
        t.tick().await;
        let stale = lindesk_core::clock::now_ms() - handle.last_alive();
        if stale < (period / 2).as_millis() as i64 {
            let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]);
        } else {
            tracing::warn!(
                "engine loop unresponsive for {stale} ms while {}; not petting the watchdog",
                handle.doing()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_defaults() {
        let c = Cli::try_parse_from(["lindeskd"]).unwrap();
        assert_eq!(c.config, PathBuf::from("/etc/lindesk/lindesk.toml"));
        assert_eq!(c.state_dir, PathBuf::from("/run/lindesk"));
        assert!(!c.dry_run && !c.restore && !c.no_dbus);
        assert_eq!((c.telemetry_dir, c.log_level), (None, None));
    }

    #[test]
    fn cli_flags() {
        let c = Cli::try_parse_from([
            "lindeskd",
            "--config",
            "/tmp/c.toml",
            "--state-dir",
            "/tmp/s",
            "--telemetry-dir",
            "/tmp/t",
            "--dry-run",
            "--log-level",
            "debug",
            "--no-dbus",
        ])
        .unwrap();
        assert_eq!(c.config, PathBuf::from("/tmp/c.toml"));
        assert_eq!(c.telemetry_dir, Some(PathBuf::from("/tmp/t")));
        assert!(c.dry_run && c.no_dbus);
        assert_eq!(c.log_level.as_deref(), Some("debug"));
        assert!(Cli::try_parse_from(["lindeskd", "--restore"]).unwrap().restore);
        assert!(Cli::try_parse_from(["lindeskd", "--bogus"]).is_err());
    }

    #[test]
    fn actuators_relay_first_then_the_backend() {
        let (acts, _) = build_actuators(Backend::Systemd);
        let names: Vec<String> = acts.iter().map(|a| a.info().name).collect();
        assert_eq!(names, ["relay", "systemd", "proc", "timer-slack"]);
        let (acts, _) = build_actuators(Backend::CgroupV2Raw);
        let names: Vec<String> = acts.iter().map(|a| a.info().name).collect();
        assert_eq!(names, ["relay", "cgroup-v2-raw", "proc", "timer-slack"]);
    }
}
