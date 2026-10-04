//! `lindesk-session`: the lindesk per-user agent.
//!
//! Serves `org.lindesk.QoS1` on the session bus, runs the built-in foreground adapters,
//! applies settings to this user's units through the user systemd manager, hosts user
//! plugins and writes the user's telemetry lake. With a `lindeskd` broker it applies what the
//! broker decides (connected mode); without one it runs the whole pipeline for the user's own
//! units (standalone mode). See docs/ARCHITECTURE.md.

// Shared with lindeskd (each binary uses a subset).
#[allow(dead_code)]
#[path = "../../lindeskd/src/common/mod.rs"]
mod common;
mod dbus;
mod desktop;
mod gate;
mod link;
mod mode;
mod session;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use lindesk_builtins::actuators::systemd::SystemdClient;
use lindesk_builtins::actuators::{self, Bus};
use lindesk_builtins::adapters::{self, SessionEnv};
use lindesk_builtins::classifier::RulesClassifier;
use lindesk_builtins::env::{Mode as EnvMode, SysEnvironment};
use lindesk_builtins::policy::{RelayFeed, RelayPolicy, ThresholdPolicy, relay};
use lindesk_builtins::sensors::{CgroupStatSensor, PsiSensor};
use lindesk_builtins::sinks::ParquetSink;
use lindesk_core::config::Config;
use lindesk_host::{Engine, EngineOptions};
use lindesk_ipc::{QOS_NAME, QOS_PATH};
use lindesk_plugin_api::{Adapter, Event, PluginError, Privilege};
use lindesk_sys::requirements::Component;
use lindesk_sys::{Env, Roots};
use serde_json::json;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::common::Signals;
use crate::common::calibration::Calibrator;
use crate::common::defaults::ensure_rules;
use crate::common::engine_thread::{self, EngineHandle, Notification};
use crate::common::observe::Observed;
use crate::common::profiles::Profiles;
use crate::common::restore::{arm_timeout, restore_state};
use crate::common::setup::{self, registered};
use crate::common::startup::{HardwareWatch, check_requirements, notice, requirement_notices};
use crate::gate::{Gate, GatedPolicy, GatedSensor};
use crate::link::Forward;
use crate::mode::Mode;
use crate::session::{SessionExt, SessionHooks};

/// How long shutdown waits for the engine to restore everything.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(20);

/// lindesk session agent: per-user foreground tracking and QoS.
#[derive(Parser, Debug, Clone, PartialEq)]
#[command(name = "lindesk-session", version, about, long_about = None)]
pub struct Cli {
    /// Configuration file [default: $XDG_CONFIG_HOME/lindesk/lindesk.toml].
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Runtime state directory [default: $XDG_RUNTIME_DIR/lindesk].
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
    /// Telemetry lake root (overrides `[telemetry] dir`).
    #[arg(long, value_name = "DIR")]
    pub telemetry_dir: Option<PathBuf>,
    /// Observe only: compute and record actions but change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Ignore the lindeskd broker and run the whole pipeline for this user.
    #[arg(long)]
    pub standalone: bool,
    /// Undo everything recorded in the state directory, then exit.
    #[arg(long)]
    pub restore: bool,
    /// Don't start the built-in foreground adapters (X11, COSMIC).
    #[arg(long)]
    pub no_adapters: bool,
    /// Log filter (`info`, `debug`, …). Default: `RUST_LOG`, else `info`.
    #[arg(long, value_name = "FILTER")]
    pub log_level: Option<String>,
}

/// `$VAR` if set and absolute, else `$HOME/<fallback>`.
fn xdg(var: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback)))
}

impl Cli {
    pub fn config_path(&self) -> PathBuf {
        self.config.clone().unwrap_or_else(|| {
            xdg("XDG_CONFIG_HOME", ".config").unwrap_or_else(|| PathBuf::from(".config")).join("lindesk/lindesk.toml")
        })
    }

    pub fn state_dir(&self, uid: u32) -> PathBuf {
        self.state_dir.clone().unwrap_or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")))
                .join("lindesk")
        })
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    common::logging::init("lindesk-session", cli.log_level.as_deref());
    let uid = rustix::process::getuid().as_raw();
    if cli.restore {
        return restore(&cli, uid);
    }
    match run(cli, uid) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn restore(cli: &Cli, uid: u32) -> ExitCode {
    arm_timeout("lindesk-session --restore");
    let probe = lindesk_sys::probe(&Roots::host(), &Env::from_process());
    let acts = actuators::for_backend(probe.backend(), Bus::User);
    let state = cli.state_dir(uid).join("state.json");
    match restore_state(&state, "lindesk-session", Privilege::User, Some(uid), acts) {
        Ok(o) if o.entries == 0 => {
            tracing::info!("no applied settings to restore");
            ExitCode::SUCCESS
        }
        Ok(o) => {
            tracing::info!("restored {}/{} settings", o.restored, o.entries);
            if o.is_clean() { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Err(e) => {
            tracing::error!("restoring {}: {e:#}", state.display());
            ExitCode::FAILURE
        }
    }
}

/// Unprivileged PSI triggers need windows that are multiples of 2 s.
fn default_psi_config(config: &mut Config) {
    let pc = config.plugins.entry(lindesk_builtins::sensors::PSI.to_string()).or_default();
    if pc.config.is_null() {
        pc.config = json!({ "window": "2s", "stall": "200ms" });
    }
}

fn run(cli: Cli, uid: u32) -> anyhow::Result<()> {
    let config_path = cli.config_path();
    let state_dir = cli.state_dir(uid);
    std::fs::create_dir_all(&state_dir).with_context(|| format!("creating {}", state_dir.display()))?;
    let prepare = {
        let (dry, tel, state) = (cli.dry_run, cli.telemetry_dir.clone(), state_dir.clone());
        move |c: &mut Config| {
            setup::prepare_config(c, dry, tel.as_deref(), Privilege::User);
            default_psi_config(c);
            ensure_rules(c, &state, Privilege::User);
        }
    };
    let mut config = Config::load(&config_path)?;
    prepare(&mut config);
    let rules_source = ensure_rules(&mut config, &state_dir, Privilege::User);

    let roots = Roots::host();
    let env_vars = Env::from_process();
    let probe = lindesk_sys::probe(&roots, &env_vars);
    // The session bus is optional: without it the agent still protects the foreground it
    // can see (adapters) and reports `degraded`.
    let requirements = match check_requirements(Component::Session, &roots, &env_vars, &probe, false) {
        Ok(w) => w,
        Err(code) => std::process::exit(code),
    };
    let mut caps = probe.host_caps();
    let calibration = calibration_path();
    if let Some(map) = calibration.as_deref().and_then(|p| lindesk_sys::calibrate::load(p).ok()) {
        lindesk_sys::calibrate::attach(&mut caps.hardware, map);
    }
    let mut profiles = Profiles::load(
        config.clone(),
        &lindesk_core::profile::default_dirs(xdg("XDG_CONFIG_HOME", ".config").as_deref()),
    );
    let config = profiles.initial(&caps.hardware, lindesk_core::clock::now_ms());
    if config.general.dry_run {
        tracing::info!("lindesk-session {} for uid {uid}: dry run, nothing will be changed", env!("CARGO_PKG_VERSION"));
    }

    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    // Signal handlers before the engine exists: a SIGTERM during start-up still ends in an
    // orderly shutdown.
    let signals = rt.block_on(async { Signals::install() })?;
    let (sys_conn, initial_state) =
        if cli.standalone { (None, None) } else { rt.block_on(link::connect_and_register()) };
    let initial_mode = if initial_state.is_some() { Mode::Connected } else { Mode::Standalone };

    // Engine (built outside the runtime: nothing here may block on zbus inside tokio).
    let env = Arc::new(
        SysEnvironment::new(roots.clone(), EnvMode::User(uid)).with_systemd(SystemdClient::new(Bus::User, uid)),
    );
    let state_path = state_dir.join("state.json");
    let mut opts = EngineOptions::new("lindesk-session", Privilege::User);
    opts.uid = Some(uid);
    opts.caps = caps.clone();
    opts.state_path = Some(state_path.clone());
    let mut engine = Engine::new(config.clone(), Box::new(env.clone()), opts)?;
    let feed = RelayFeed::new();
    let threshold_gate = Gate::new(false);
    let psi_gate = Gate::new(false);
    let mut hooks = SessionHooks::new(uid, feed.clone(), threshold_gate.clone(), psi_gate.clone());
    hooks.forced_standalone = cli.standalone;
    hooks.config_path = config_path;
    hooks.prepare = Box::new(prepare);
    let profile_name = profiles.active().to_string();
    hooks.profiles = Some(profiles);
    hooks.hardware = Some(HardwareWatch::new(roots.clone()));
    hooks.requirements = requirements.clone();
    hooks.rules_source = rules_source.as_str();
    hooks.degraded = degraded_reasons(&probe, uid);
    hooks.calibrator = calibration.map(|p| Calibrator::new(p, roots.clone(), lindesk_core::clock::now_ms(), true));
    hooks.set_mode(initial_mode, initial_state.as_deref(), config.general.dry_run);
    {
        let tick = config.general.tick;
        let reg = engine.registry_mut();
        registered("psi", reg.register_sensor(Box::new(GatedSensor::new(PsiSensor::new(), psi_gate)), tick));
        registered("cgroup-stat", reg.register_sensor(Box::new(CgroupStatSensor::new()), config.telemetry.interval));
        registered("rules", reg.register_classifier(Box::new(RulesClassifier::default()), 0));
        let threshold = Observed::new(ThresholdPolicy::default(), hooks.metrics.clone());
        registered("threshold", reg.register_policy(Box::new(GatedPolicy::new(threshold, threshold_gate)), 0));
        registered(relay::NAME, reg.register_policy(Box::new(RelayPolicy::new(feed)), relay::PRIORITY));
        let acts = actuators::for_backend(probe.backend(), Bus::User);
        let n = acts.len() as i32;
        for (i, a) in acts.into_iter().enumerate() {
            let name = a.info().name;
            registered(&name, reg.register_actuator(a, 100 + n - i as i32));
        }
        if config.telemetry.enabled {
            registered(setup::PARQUET, reg.register_sink(Box::new(ParquetSink::new())));
        }
    }
    setup::load_externals(&mut engine, Privilege::User);
    match engine.load_state(&state_path) {
        Ok(0) => {}
        Ok(n) => tracing::info!("recovered {n} applied settings from {}", state_path.display()),
        Err(e) => tracing::warn!("ignoring applied state: {e}"),
    }

    requirement_notices(&mut engine, &requirements);

    let (handle, inbox) = engine_thread::channel()?;
    let (fwd_tx, fwd_rx) = unbounded_channel::<Forward>();
    if !cli.no_adapters {
        hooks.adapters = start_adapters(&engine, &config, &handle, fwd_tx.clone());
    }
    let adapter_names: Vec<String> = hooks.adapters.iter().map(|a| a.info().name).collect();
    if adapter_names.is_empty()
        && !matches!(
            desktop::desktop_of(std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref()),
            desktop::Desktop::Gnome | desktop::Desktop::Kde
        )
    {
        hooks
            .degraded
            .push("no foreground adapter for this session: focus arrives only through org.lindesk.QoS1".into());
    }
    let mode_name = hooks.mode_name();
    let thread = engine_thread::spawn(engine, hooks, inbox)?;
    let (notes, join) = thread.split();
    tracing::info!(
        "lindesk-session ready: level {}, profile {}, {}, adapters [{}], mode {}{}, rules {}",
        probe.support_level(),
        profile_name,
        lindesk_sys::hardware::summary_line(&probe.hardware),
        adapter_names.join(", "),
        mode_name,
        if config.general.dry_run { " (dry run)" } else { "" },
        rules_source.as_str(),
    );
    desktop_integration(&config, &handle);

    let served =
        rt.block_on(serve(uid, handle.clone(), notes, sys_conn, initial_mode, cli.standalone, fwd_tx, fwd_rx, signals));
    handle.shutdown();
    if join.join().is_err() {
        tracing::error!("the engine thread panicked");
    }
    rt.shutdown_timeout(Duration::from_secs(1));
    served
}

/// Where a standalone agent stores its quick calibration.
fn calibration_path() -> Option<PathBuf> {
    xdg("XDG_STATE_HOME", ".local/state").map(|d| d.join("lindesk/calibration.json"))
}

/// Why this agent runs with less than the full feature set.
fn degraded_reasons(probe: &lindesk_sys::Probe, uid: u32) -> Vec<String> {
    let mut v = Vec::new();
    if !probe.psi.global {
        v.push("no PSI: no pressure-driven levels, static weights only".to_string());
    }
    if probe.backend() != lindesk_sys::Backend::Systemd {
        v.push(format!("no systemd: using the {} backend", probe.backend()));
    } else if !probe.user_managers.iter().any(|m| m.uid == uid) {
        v.push("no systemd user manager for this user: user units cannot be changed".to_string());
    }
    v
}

/// First-run desktop integration on a helper thread (it runs external tools).
fn desktop_integration(config: &Config, handle: &EngineHandle<SessionExt>) {
    if !config.plugin(desktop::NAME).enabled {
        return;
    }
    if config.general.dry_run {
        tracing::info!("dry run: first-run desktop integration skipped");
        return;
    }
    let Some(state) = desktop::first_run_path() else { return };
    let h = handle.clone();
    let current = std::env::var("XDG_CURRENT_DESKTOP").ok();
    let _ = std::thread::Builder::new().name("lindesk-first-run".into()).spawn(move || {
        if let Some(action) = desktop::first_run(current.as_deref(), &state) {
            tracing::info!("desktop integration: {action}");
            let _ = h.push_event(notice(
                "setup.desktop_integration",
                "lindesk-session",
                action.clone(),
                json!({ "action": action, "desktop": current }),
            ));
        }
    });
}

/// Detect, create, init and start the built-in adapters. Their events go into the engine
/// (waking it) and to the broker link.
fn start_adapters(
    engine: &Engine,
    config: &Config,
    handle: &EngineHandle<SessionExt>,
    forward: tokio::sync::mpsc::UnboundedSender<Forward>,
) -> Vec<Box<dyn Adapter>> {
    let session = SessionEnv::from_process();
    let names = adapters::detect(&session);
    if names.is_empty() {
        tracing::info!(
            "no built-in adapter for this session ({}); shells push focus over org.lindesk.QoS1",
            session.current_desktop.as_deref().unwrap_or("unknown desktop")
        );
    }
    let mut running = Vec::new();
    for name in names {
        let pc = config.plugin(name);
        if !pc.enabled {
            tracing::info!(plugin = name, "adapter disabled by [plugins.{name}]");
            continue;
        }
        let Some(mut a) = adapters::create(name) else {
            tracing::info!(plugin = name, "adapter not compiled into this build");
            continue;
        };
        let ctx = engine.registry().host().init_context(pc.config);
        let host = engine.host_sender();
        let h = handle.clone();
        let fwd = forward.clone();
        let sink: lindesk_plugin_api::EventSink = Arc::new(move |e: Event| {
            if let Some(f) = Forward::from_event(&e) {
                let _ = fwd.send(f);
            }
            host.event(e);
            h.wake();
        });
        let started = a.init(&ctx).and_then(|()| a.start(sink));
        match started {
            Ok(()) => {
                tracing::info!(plugin = name, "adapter started");
                running.push(a);
            }
            Err(PluginError::Unsupported(why)) => tracing::info!(plugin = name, "adapter not usable here: {why}"),
            Err(e) => tracing::warn!(plugin = name, "adapter failed to start: {e}"),
        }
    }
    running
}

#[allow(clippy::too_many_arguments)]
async fn serve(
    uid: u32,
    handle: EngineHandle<SessionExt>,
    mut notes: UnboundedReceiver<Notification>,
    sys_conn: Option<zbus::Connection>,
    initial_mode: Mode,
    standalone: bool,
    fwd_tx: tokio::sync::mpsc::UnboundedSender<Forward>,
    fwd_rx: UnboundedReceiver<Forward>,
    signals: Signals,
) -> anyhow::Result<()> {
    let Signals { mut term, mut int, mut hup } = signals;
    let iface = dbus::QoS { engine: handle.clone(), uid, forward: fwd_tx, coalesce: Mutex::new(Default::default()) };
    let built = match zbus::connection::Builder::session() {
        Ok(b) => b.serve_at(QOS_PATH, iface)?.build().await,
        Err(e) => Err(e),
    };
    let conn = match built {
        Ok(conn) => {
            // Type=dbus: the name is the readiness signal, so it comes last.
            conn.request_name(QOS_NAME)
                .await
                .with_context(|| format!("owning {QOS_NAME} (is another agent running?)"))?;
            tracing::info!("serving {QOS_NAME} on the session bus");
            Some(conn)
        }
        Err(e) => {
            let _ = handle.ext(SessionExt::Degraded(format!(
                "no session bus ({e}): org.lindesk.QoS1 is unavailable, shells can't report focus"
            )));
            None
        }
    };

    tokio::spawn(link::run(sys_conn, uid, standalone, initial_mode, handle.clone(), fwd_rx));

    let mut stopping = false;
    loop {
        tokio::select! {
            _ = term.recv(), if !stopping => { tracing::info!("SIGTERM: stopping"); stopping = true; handle.shutdown(); }
            _ = int.recv(), if !stopping => { tracing::info!("SIGINT: stopping"); stopping = true; handle.shutdown(); }
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
            n = notes.recv() => match n {
                Some(Notification::LevelChanged(level)) => {
                    if let Some(c) = &conn { dbus::emit_level(c, level.as_str()).await; }
                }
                Some(Notification::Events(json)) => {
                    if let Some(c) = &conn { dbus::emit_events(c, &json).await; }
                }
                Some(Notification::UserState { .. }) => {}
                Some(Notification::Stopped) | None => {
                    if !stopping { tracing::error!("the engine stopped unexpectedly"); }
                    break;
                }
            },
        }
        if stopping {
            let drain = async {
                while let Some(n) = notes.recv().await {
                    if n == Notification::Stopped {
                        return;
                    }
                }
            };
            if tokio::time::timeout(SHUTDOWN_TIMEOUT, drain).await.is_err() {
                tracing::error!("the engine did not stop within {SHUTDOWN_TIMEOUT:?}");
            }
            break;
        }
    }
    if let Some(c) = conn {
        let _ = c.release_name(QOS_NAME).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parsing() {
        let c = Cli::try_parse_from(["lindesk-session"]).unwrap();
        assert!(!c.standalone && !c.dry_run && !c.restore && !c.no_adapters);
        assert_eq!(c.config, None);
        let c = Cli::try_parse_from([
            "lindesk-session",
            "--standalone",
            "--dry-run",
            "--no-adapters",
            "--state-dir",
            "/tmp/s",
            "--telemetry-dir",
            "/tmp/t",
            "--config",
            "/tmp/c.toml",
            "--log-level",
            "debug",
        ])
        .unwrap();
        assert!(c.standalone && c.dry_run && c.no_adapters);
        assert_eq!(c.state_dir(1000), PathBuf::from("/tmp/s"));
        assert_eq!(c.config_path(), PathBuf::from("/tmp/c.toml"));
        assert!(Cli::try_parse_from(["lindesk-session", "--restore"]).unwrap().restore);
        assert!(Cli::try_parse_from(["lindesk-session", "--nope"]).is_err());
    }

    #[test]
    fn default_paths_follow_xdg() {
        let c = Cli::try_parse_from(["lindesk-session"]).unwrap();
        assert!(c.config_path().ends_with("lindesk/lindesk.toml"));
        assert!(c.state_dir(1000).ends_with("lindesk"));
    }

    #[test]
    fn psi_defaults_are_unprivileged_friendly() {
        let mut c = Config::default();
        default_psi_config(&mut c);
        let cfg = lindesk_builtins::sensors::PsiConfig::from_json(&c.plugin("psi").config).unwrap();
        assert_eq!(cfg.window, Duration::from_secs(2));
        let mut c = Config::from_toml("[plugins.psi]\nconfig = { triggers = false }\n").unwrap();
        default_psi_config(&mut c);
        assert_eq!(c.plugin("psi").config, json!({ "triggers": false }), "explicit config kept");
    }
}
