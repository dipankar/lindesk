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

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use lindesk_builtins::actuators::systemd::SystemdClient;
use lindesk_builtins::actuators::{self, Bus, RelayActuator, RelayTable};
use lindesk_builtins::classifier::RulesClassifier;
use lindesk_builtins::env::{Mode, SysEnvironment, UserWatch};
use lindesk_builtins::policy::ThresholdPolicy;
use lindesk_builtins::sensors::{CgroupStatSensor, PsiSensor};
use lindesk_builtins::sinks::ParquetSink;
use lindesk_core::config::Config;
use lindesk_host::{Engine, EngineOptions};
use lindesk_ipc::{BROKER_NAME, BROKER_PATH};
use lindesk_plugin_api::{API_VERSION, Actuator, HostCaps, InitContext, Privilege};
use lindesk_sys::blockdev::{self, BlockDev, DevId};
use lindesk_sys::requirements::Component;
use lindesk_sys::{Backend, Env, Probe, Roots};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::broker::{BrokerHooks, HostInfo};
use crate::common::Signals;
use crate::common::calibration::Calibrator;
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
#[command(name = "lindeskd", version, about, long_about = None)]
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

fn init_context(caps: &HostCaps, config: serde_json::Value) -> InitContext {
    InitContext {
        api_version: API_VERSION,
        host: "lindeskd".into(),
        privilege: Privilege::System,
        uid: None,
        caps: caps.clone(),
        config,
    }
}

/// The broker's actuators, highest priority first: the relay (user units), wrapping the
/// backend's unit actuator (system units) as its fallback, then the rest of the backend set.
fn build_actuators(backend: Backend, config: &Config, caps: &HostCaps) -> (Vec<Box<dyn Actuator>>, RelayTable) {
    let mut relay = RelayActuator::new();
    let table = relay.table();
    let mut rest = Vec::new();
    for mut a in actuators::for_backend(backend, Bus::System) {
        let name = a.info().name;
        if relay.fallback_name().is_none() && a.caps().targets.iter().any(|t| t == "unit") {
            let pc = config.plugin(&name);
            if !pc.enabled {
                tracing::info!(plugin = %name, "disabled by [plugins.{name}]");
                continue;
            }
            match a.init(&init_context(caps, pc.config)) {
                Ok(()) => relay = relay.with_fallback(a),
                Err(e) => tracing::warn!(plugin = %name, "init failed: {e}"),
            }
        } else {
            rest.push(a);
        }
    }
    let mut all: Vec<Box<dyn Actuator>> = vec![Box::new(relay)];
    all.extend(rest);
    (all, table)
}

fn restore(cli: &Cli) -> ExitCode {
    arm_timeout("lindeskd --restore");
    let roots = Roots::host();
    let probe = lindesk_sys::probe(&roots, &Env::from_process());
    let config = Config::load(&cli.config).unwrap_or_default();
    let (acts, _) = build_actuators(probe.backend(), &config, &probe.host_caps());
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
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// Devices `[io] devices` names, else the disks behind `/` and `/home`.
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
    if config.telemetry.enabled {
        let dir = config.plugin(setup::PARQUET).config["dir"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(TELEMETRY_DIR));
        if let Err(e) = ensure_dir(&dir, 0o750, Some("lindesk")) {
            tracing::warn!("telemetry directory {}: {e}", dir.display());
        }
    }

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
        let v = ioc.enable(config.io.iocost, &devices);
        if !v.is_empty() {
            probe = lindesk_sys::probe(&roots, &Env::from_process());
        }
        v
    };
    let mut caps = probe.host_caps();
    if let Ok(map) = lindesk_sys::calibrate::load(Path::new(CALIBRATION)) {
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
    let relay = register_builtins(&mut engine, &config, probe.backend(), &caps, &metrics);
    setup::load_externals(&mut engine, Privilege::System);
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
    hooks.prepare = Box::new(prepare);
    let profile_name = profiles.active().to_string();
    hooks.profiles = Some(profiles);
    hooks.hardware = Some(HardwareWatch::new(roots.clone()));
    hooks.requirements = requirements;
    hooks.rules_source = rules_source.as_str();
    hooks.degraded = degraded_reasons(&probe, cli.no_dbus);
    hooks.calibrator = (!dry_run)
        .then(|| Calibrator::new(PathBuf::from(CALIBRATION), roots.clone(), lindesk_core::clock::now_ms(), false));
    let summary = format!(
        "lindeskd ready: level {}, profile {profile_name}, {}, mode broker{}, rules {}",
        probe.support_level(),
        lindesk_sys::hardware::summary_line(&probe.hardware),
        if dry_run { " (dry run)" } else { "" },
        rules_source.as_str()
    );
    let (handle, inbox) = engine_thread::channel()?;
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
    let served = rt.block_on(serve(&cli, handle.clone(), notes, insecure, signals));
    handle.shutdown();
    if join.join().is_err() {
        tracing::error!("the engine thread panicked");
    }
    if !dry_run {
        let (restored, failed) = ioc.restore();
        if restored + failed > 0 {
            tracing::info!("iocost restored on {restored} device(s), {failed} failed");
        }
    }
    rt.shutdown_timeout(Duration::from_secs(1));
    served
}

/// Default system telemetry lake.
const TELEMETRY_DIR: &str = "/var/lib/lindesk/telemetry";
/// System calibration result.
const CALIBRATION: &str = "/var/lib/lindesk/calibration.json";

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

fn register_builtins(
    engine: &mut Engine,
    config: &Config,
    backend: Backend,
    caps: &HostCaps,
    metrics: &PolicyMetrics,
) -> RelayTable {
    let tick = config.general.tick;
    let reg = engine.registry_mut();
    registered("psi", reg.register_sensor(Box::new(PsiSensor::new()), tick));
    registered("cgroup-stat", reg.register_sensor(Box::new(CgroupStatSensor::new()), config.telemetry.interval));
    registered("rules", reg.register_classifier(Box::new(RulesClassifier::default()), 0));
    registered(
        "threshold",
        reg.register_policy(Box::new(Observed::new(ThresholdPolicy::default(), metrics.clone())), 0),
    );
    let (acts, table) = build_actuators(backend, config, caps);
    let n = acts.len() as i32;
    for (i, a) in acts.into_iter().enumerate() {
        let name = a.info().name;
        registered(&name, reg.register_actuator(a, 100 + n - i as i32));
    }
    if config.telemetry.enabled {
        registered(setup::PARQUET, reg.register_sink(Box::new(ParquetSink::new())));
    }
    table
}

/// D-Bus, signals, watchdog and heartbeat until SIGTERM/SIGINT or the engine stops.
async fn serve(
    cli: &Cli,
    handle: EngineHandle<broker::BrokerExt>,
    mut notes: UnboundedReceiver<Notification>,
    insecure: bool,
    signals: Signals,
) -> anyhow::Result<()> {
    let Signals { mut term, mut int, mut hup } = signals;
    let sessions = dbus::Sessions::default();
    let conn = if cli.no_dbus {
        None
    } else {
        let iface = dbus::Broker { engine: handle.clone(), sessions: sessions.clone(), insecure };
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
            n = notes.recv() => match n {
                Some(Notification::UserState { uid, json }) => {
                    if let Some(c) = &conn { dbus::send_user_state(c, &sessions, uid, &json).await; }
                }
                Some(Notification::Events(json)) => {
                    if let Some(c) = &conn { dbus::send_events(c, &json).await; }
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
            tracing::warn!("engine loop unresponsive for {stale} ms; not petting the watchdog");
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
    fn actuators_relay_first_with_unit_fallback() {
        let (acts, _) = build_actuators(Backend::Systemd, &Config::default(), &HostCaps::default());
        let names: Vec<String> = acts.iter().map(|a| a.info().name).collect();
        assert_eq!(names, ["relay", "proc"]);
        assert!(acts[0].info().description.contains("systemd"), "systemd is the relay's fallback");
        let (acts, _) = build_actuators(Backend::CgroupV2Raw, &Config::default(), &HostCaps::default());
        let names: Vec<String> = acts.iter().map(|a| a.info().name).collect();
        assert_eq!(names, ["relay", "cgroup-v2-raw", "proc"]);
        let off = Config::from_toml("[plugins.systemd]\nenabled = false\n").unwrap();
        let (acts, _) = build_actuators(Backend::Systemd, &off, &HostCaps::default());
        assert!(!acts[0].info().description.contains("systemd"));
    }
}
