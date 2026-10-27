//! External plugin tests: real processes (small Python and shell plugins written to
//! temporary directories) over the stdio protocol.

mod common;

use std::time::{Duration, Instant};

use common::*;
use lindesk_core::config::Config;
use lindesk_host::discovery::{self, DiscoveryOptions};
use lindesk_host::external::{ExternalOptions, ExternalPlugin, LogSource, ShutdownOutcome, host_queues};
use lindesk_host::{Engine, EngineOptions, Health, MemoryEnvironment, SupervisorConfig};
use lindesk_plugin_api::*;

fn ctx() -> InitContext {
    InitContext {
        api_version: API_VERSION,
        host: "test".into(),
        privilege: Privilege::User,
        uid: Some(1000),
        caps: HostCaps::default(),
        config: serde_json::json!({"greeting": "hi"}),
    }
}

fn fast_supervisor(max_failures: usize) -> SupervisorConfig {
    SupervisorConfig {
        backoff_base: Duration::from_millis(20),
        backoff_max: Duration::from_millis(80),
        max_failures,
        failure_window: Duration::from_secs(60),
    }
}

fn spawn(dir: &std::path::Path, opts: ExternalOptions) -> ExternalPlugin {
    let d = discovery::inspect_dev(dir).unwrap();
    let p = ExternalPlugin::new(d.manifest, d.dir, ctx(), opts);
    p.start().unwrap_or_else(|e| panic!("start: {e}; logs: {:?}", p.recent_logs()));
    p
}

fn proc_info(comm: &str) -> ProcInfo {
    ProcInfo { pid: 1, uid: 1000, comm: comm.into(), cgroup: "/x".into(), ..Default::default() }
}

const MULTI: &str = r#"
@lp.on_init
def init(ctx):
    lp.log("config greeting=%s" % lp.config.get("greeting"))

@lp.classifier
def classify(proc):
    return "batch" if proc["comm"] == "rustc" else None

@lp.policy
def evaluate(snapshot):
    return lp.output([lp.intent(u["target"], cpu_weight=30, reason="py") for u in snapshot["units"]], level="red")
"#;

#[test]
fn python_plugin_serves_several_kinds() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = write_plugin(tmp.path(), "multi", &["classifier", "policy"], &python_sdk_script(MULTI), "");
    let mut p = spawn(&dir, ExternalOptions::default());
    assert_eq!(p.health(), Health::Running);
    let init = p.init_result().unwrap();
    assert_eq!(init.kinds, vec![PluginKind::Classifier, PluginKind::Policy]);

    let c = Classifier::classify(&mut p, &proc_info("rustc")).unwrap().unwrap();
    assert_eq!(c.class, QosClass::Batch);
    assert_eq!(Classifier::classify(&mut p, &proc_info("vim")).unwrap(), None);

    let snap = Snapshot {
        now: 0,
        level: Default::default(),
        pressure: vec![],
        units: vec![UnitState {
            target: Target::user_unit(1000, "a.scope"),
            class: QosClass::Batch,
            foreground: false,
            pressure: vec![],
            usage: None,
            applied: vec![],
            top_comm: None,
            visible: None,
            audible: None,
            serving: false,
        }],
        events: vec![],
        foreground: None,
        caps: HostCaps::default(),
    };
    let out = Policy::evaluate(&mut p, &snap).unwrap();
    assert_eq!(out.level, Some(Level::Red));
    assert_eq!(out.intents[0].setting, Setting::CpuWeight { value: 30 });

    let stats = p.stats();
    assert_eq!((stats.calls, stats.errors, stats.failures), (3, 0, 0));
    assert!(stats.latency_ewma_us > 0.0);
    assert!(eventually(|| p.recent_logs().iter().any(|l| l.message == "config greeting=hi")));
    assert!(matches!(p.shutdown(), ShutdownOutcome::Exited { code: Some(0), .. }));
    assert!(Classifier::classify(&mut p, &proc_info("rustc")).is_err(), "no restarts after shutdown");
}

#[test]
fn environment_is_cleared() {
    let tmp = tempfile::tempdir().unwrap();
    let body = r#"
import os
@lp.classifier
def classify(proc):
    return lp.classification("normal", reason=",".join(sorted(os.environ)) + "|" + os.getcwd())
"#;
    let dir = write_plugin(tmp.path(), "envcheck", &["classifier"], &python_sdk_script(body), "");
    let mut p = spawn(&dir, ExternalOptions::default());
    let reason = Classifier::classify(&mut p, &proc_info("x")).unwrap().unwrap().reason;
    let (vars, cwd) = reason.split_once('|').unwrap();
    for v in vars.split(',') {
        assert!(
            matches!(v, "PATH" | "HOME" | "LANG" | "LINDESK_API_VERSION" | "LINDESK_SDK_PATH" | "LC_CTYPE")
                || v.starts_with("XDG_")
                // user plugins may reach the desktop session
                || matches!(v, "DBUS_SESSION_BUS_ADDRESS" | "WAYLAND_DISPLAY" | "DISPLAY" | "SWAYSOCK" | "HYPRLAND_INSTANCE_SIGNATURE"),
            "unexpected env var {v} leaked into the plugin ({vars})"
        );
    }
    assert!(vars.contains("LINDESK_API_VERSION"));
    assert_eq!(std::fs::canonicalize(cwd).unwrap(), std::fs::canonicalize(&dir).unwrap());
}

#[test]
fn crash_backoff_then_auto_disable() {
    let tmp = tempfile::tempdir().unwrap();
    // Initialises fine, then dies on the first request.
    let body = r#"
import os
@lp.classifier
def classify(proc):
    sys.stderr.write("about to die\n"); sys.stderr.flush()
    os._exit(3)
"#;
    let dir = write_plugin(tmp.path(), "crasher", &["classifier"], &python_sdk_script(body), "");
    let opts = ExternalOptions { supervisor: fast_supervisor(3), ..Default::default() };
    let mut p = spawn(&dir, opts);

    let e = Classifier::classify(&mut p, &proc_info("x")).unwrap_err();
    assert!(matches!(&e, PluginError::Io(m) if m.contains("exit status: 3") && m.contains("about to die")), "{e}");
    assert!(matches!(p.health(), Health::Backoff { .. }));
    // During backoff calls fail fast without spawning.
    let e = Classifier::classify(&mut p, &proc_info("x")).unwrap_err();
    assert!(e.to_string().contains("backing off"), "{e}");

    // After backoff it is restarted, crashes again... until disabled.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !matches!(p.health(), Health::Disabled { .. }) {
        assert!(Instant::now() < deadline, "never disabled: {:?}", p.health());
        let _ = Classifier::classify(&mut p, &proc_info("x"));
        std::thread::sleep(Duration::from_millis(10));
    }
    let stats = p.stats();
    assert_eq!(stats.failures, 3);
    assert_eq!(stats.restarts, 2);
    let e = Classifier::classify(&mut p, &proc_info("x")).unwrap_err();
    assert!(e.to_string().contains("disabled"), "{e}");
    assert!(p.recent_logs().iter().any(|l| l.source == LogSource::Host && l.message.contains("restarting")));
}

#[test]
fn timeouts_count_as_failures() {
    let tmp = tempfile::tempdir().unwrap();
    let body = r#"
import time
@lp.classifier
def classify(proc):
    if proc["comm"] == "slow":
        time.sleep(1)
    return None
"#;
    let dir =
        write_plugin(tmp.path(), "slow", &["classifier"], &python_sdk_script(body), "[timeouts]\ncall_ms = 150\n");
    let opts = ExternalOptions { supervisor: fast_supervisor(5), ..Default::default() };
    let mut p = spawn(&dir, opts);
    let start = Instant::now();
    assert_eq!(Classifier::classify(&mut p, &proc_info("slow")), Err(PluginError::Timeout));
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(150) && took < Duration::from_millis(900), "{took:?}");
    let s = p.stats();
    assert_eq!((s.timeouts, s.failures), (1, 1));
    assert!(matches!(p.health(), Health::Backoff { .. }));
    // The late reply is dropped; once backoff ends the same process answers again.
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(Classifier::classify(&mut p, &proc_info("fast")), Ok(None));
    assert_eq!(p.health(), Health::Running);
    assert_eq!(p.stats().restarts, 0, "a timeout doesn't restart a live process");
}

#[test]
fn malformed_output_is_a_protocol_failure() {
    let tmp = tempfile::tempdir().unwrap();
    // Raw protocol in shell: prints junk before each answer.
    let script = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*) echo '{"jsonrpc":"2.0","id":1,"result":{"name":"junk","version":"1","api_version":1,"kinds":["classifier"]}}' ;;
    *'"classifier.classify"'*) echo 'DEBUG: classifying!'; id=$(echo "$line" | sed 's/.*"id":\([0-9]*\).*/\1/'); echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
    *'"shutdown"'*) exit 0 ;;
  esac
done
"#;
    let dir = write_plugin(tmp.path(), "junk", &["classifier"], script, "");
    let opts = ExternalOptions { supervisor: fast_supervisor(5), ..Default::default() };
    let mut p = spawn(&dir, opts);
    assert_eq!(Classifier::classify(&mut p, &proc_info("x")), Ok(None));
    assert!(eventually(|| p.malformed_lines() == 1));
    assert_eq!(p.stats().failures, 1);
    assert!(
        p.recent_logs()
            .iter()
            .any(|l| l.message.contains("malformed output") && l.message.contains("DEBUG: classifying!"))
    );

    // A result that doesn't match the schema is a protocol error too.
    let script = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(echo "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) echo '{"jsonrpc":"2.0","id":1,"result":{"name":"bad","version":"1","api_version":1,"kinds":["classifier"]}}' ;;
    *'"shutdown"'*) exit 0 ;;
    *) echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"class\":\"urgent\"}}" ;;
  esac
done
"#;
    let dir = write_plugin(tmp.path(), "bad", &["classifier"], script, "");
    // A generous timeout: this is about the malformed answer, not about a loaded machine.
    let mut p =
        spawn(&dir, ExternalOptions { supervisor: fast_supervisor(5), timeout_scale: 40.0, ..Default::default() });
    assert!(
        matches!(Classifier::classify(&mut p, &proc_info("x")), Err(PluginError::Protocol(m)) if m.contains("urgent"))
    );
    assert_eq!(p.stats().failures, 1);
}

#[test]
fn notifications_are_routed() {
    let tmp = tempfile::tempdir().unwrap();
    let body = r#"
@lp.classifier
def classify(proc):
    lp.log("warned", "warn")
    lp.metric("frames", 3, app="x")
    lp.emit(lp.focus(pid=12, source="py"))
    lp.samples([lp.sample("io", 42.0)])
    print("to stderr")
    return None
"#;
    let dir = write_plugin(tmp.path(), "chatty", &["classifier"], &python_sdk_script(body), "");
    let (tx, rx) = host_queues();
    let mut p = spawn(&dir, ExternalOptions { host: tx, ..Default::default() });
    Classifier::classify(&mut p, &proc_info("x")).unwrap();
    // Notifications precede the reply on the same pipe; allow for scheduling under load.
    let wait = std::time::Duration::from_secs(5);
    let m = rx.metrics.recv_timeout(wait).unwrap();
    assert_eq!((m.name.as_str(), m.value, m.plugin.as_str()), ("frames", 3.0, "chatty"));
    assert_eq!(m.labels["app"], "x");
    assert!(matches!(rx.events.recv_timeout(wait).unwrap(), Event::Focus { pid: Some(12), .. }));
    assert_eq!(rx.samples.recv_timeout(wait).unwrap().some_avg10, 42.0);
    let logs = p.recent_logs();
    assert!(logs.iter().any(|l| l.source == LogSource::Plugin && l.message == "warned"));
    assert!(eventually(|| p.recent_logs().iter().any(|l| l.source == LogSource::Stderr && l.message == "to stderr")));
    assert_eq!(p.malformed_lines(), 0);
}

#[test]
fn shutdown_escalates_to_kill() {
    let tmp = tempfile::tempdir().unwrap();
    // Answers initialize, then ignores shutdown, EOF and SIGTERM.
    let script = r#"#!/bin/bash
trap '' TERM
read -r line
echo '{"jsonrpc":"2.0","id":1,"result":{"name":"stubborn","version":"1","api_version":1,"kinds":["sink"]}}'
while true; do sleep 0.05; done
"#;
    let dir = write_plugin(tmp.path(), "stubborn", &["sink"], script, "");
    let p = spawn(&dir, ExternalOptions::default());
    let start = Instant::now();
    assert_eq!(p.shutdown(), ShutdownOutcome::Killed);
    assert!(start.elapsed() >= Duration::from_secs(2));
    assert!(p.pid().is_none());
}

/// A sink that stops reading its stdin must never stall the caller: `sink.record` is a
/// notification queued to a writer thread, and batches beyond the queue are dropped.
#[test]
fn a_sink_that_stops_reading_never_blocks_record() {
    let tmp = tempfile::tempdir().unwrap();
    // Answers initialize, then sleeps without reading stdin again.
    let script = r#"#!/bin/bash
read -r line
echo '{"jsonrpc":"2.0","id":1,"result":{"name":"wedged","version":"1","api_version":1,"kinds":["sink"]}}'
exec sleep 600
"#;
    let dir = write_plugin(tmp.path(), "wedged", &["sink"], script, "");
    let mut p = spawn(&dir, ExternalOptions::default());
    // ~20 KiB per batch: the 64 KiB pipe fills after a few, then the queue.
    let batch: Vec<Record> = (0..200)
        .map(|i| {
            Record::Metric(Metric {
                ts: i,
                name: "padding_padding_padding_padding".into(),
                value: i as f64,
                labels: Default::default(),
                plugin: "test".into(),
            })
        })
        .collect();
    let mut took: Vec<Duration> = (0..500)
        .map(|_| {
            let t = Instant::now();
            Sink::record(&mut p, &batch).unwrap();
            t.elapsed()
        })
        .collect();
    took.sort();
    // The claim is "never waits for the plugin" (which would be seconds), not a latency SLO:
    // leave room for a loaded test machine.
    assert!(took[474] < Duration::from_millis(50), "p95 record() {:?}", took[474]);
    assert!(took[499] < Duration::from_millis(200), "max record() {:?}", took[499]);
    assert!(p.stats().dropped > 100, "batches beyond the queue are dropped: {:?}", p.stats());
    assert_eq!(p.health(), Health::Running, "dropping is not a failure");
    // A request can't get through either: it fails like a timeout instead of hanging.
    let t = Instant::now();
    assert!(matches!(Sink::flush(&mut p), Err(PluginError::Timeout)));
    assert!(t.elapsed() < Duration::from_millis(500));
    let t = Instant::now();
    p.shutdown();
    assert!(t.elapsed() < Duration::from_secs(5), "shutdown is bounded");
}

#[test]
fn handshake_must_report_declared_kinds() {
    let tmp = tempfile::tempdir().unwrap();
    let body = "@lp.classifier\ndef c(p):\n    return None\n";
    let dir = write_plugin(tmp.path(), "liar", &["classifier", "policy"], &python_sdk_script(body), "");
    let d = discovery::inspect_dev(&dir).unwrap();
    let p = ExternalPlugin::from_discovered(&d, ctx(), ExternalOptions::default());
    let e = p.start().unwrap_err();
    assert!(matches!(&e, PluginError::Protocol(m) if m.contains("[policy]")), "{e}");
    assert!(matches!(p.health(), Health::Backoff { .. }));

    // Missing executable: a clear error, also supervised.
    let mut m = d.manifest.clone();
    m.plugin.exec = "nope".into();
    let p = ExternalPlugin::new(m, &dir, ctx(), ExternalOptions::default());
    assert!(matches!(p.start(), Err(PluginError::Io(m)) if m.contains("spawning")));
}

#[test]
fn engine_runs_external_policy_classifier_and_adapter() {
    let tmp = tempfile::tempdir().unwrap();
    let plugins = tmp.path().join("plugins");
    write_plugin(
        &plugins,
        "py-rules",
        &["classifier", "policy"],
        &python_sdk_script(MULTI),
        "[classifier]\npriority = 60\nprefilter = { comm = [\"rustc\"] }\n",
    );
    let adapter = r#"
@lp.adapter
def watch(emit):
    emit(lp.focus(pid=12, source="py-adapter"))
"#;
    write_plugin(&plugins, "py-focus", &["adapter"], &python_sdk_script(adapter), "");
    write_plugin(&plugins, "off", &["policy"], &python_sdk_script(MULTI), "");
    let config =
        Config::from_toml("[plugins.off]\nenabled = false\n[plugins.py-rules]\nconfig = { greeting = \"hello\" }\n")
            .unwrap();
    let found = discovery::discover(&DiscoveryOptions {
        host: Privilege::User,
        dirs: vec![plugins.clone()],
        config: &config,
        ownership: &|_: &std::path::Path| Ok(()),
    });
    assert_eq!(found.plugins.len(), 3, "{:?}", found.rejected);

    let env = MemoryEnvironment::new();
    let cargo = "/user.slice/user-1000.slice/user@1000.service/app.slice/app-cargo-77.scope";
    let ff = "/user.slice/user-1000.slice/user@1000.service/app.slice/app-firefox-12.scope";
    env.spawn(77, "rustc", cargo);
    env.spawn(12, "firefox", ff);
    env.watch_cgroup(cargo);
    env.watch_cgroup(ff);
    let mut opts = EngineOptions::new("lindesk-session", Privilege::User);
    opts.uid = Some(1000);
    let mut e = Engine::new(config, Box::new(env), opts).unwrap();
    let act = RecordingActuator::default();
    e.registry_mut().register_actuator(Box::new(act.clone()), 0).unwrap();
    for p in &found.plugins {
        e.registry_mut().load_external(p).unwrap();
    }
    assert!(e.start().is_empty());
    let rules = e.registry().external("py-rules").unwrap().clone();
    assert!(eventually(|| rules.recent_logs().iter().any(|l| l.message == "config greeting=hello")));

    let mut report = e.step(1000);
    let mut now = 1000;
    while report.foreground.is_none() && now < 6000 {
        std::thread::sleep(Duration::from_millis(20));
        now += 100;
        report = e.step(now);
    }
    let cargo_t = Target::user_unit(1000, "app-cargo-77.scope");
    let ff_t = Target::user_unit(1000, "app-firefox-12.scope");
    assert_eq!(report.foreground, Some(ff_t.clone()), "the external adapter's focus event arrived");
    assert_eq!(e.class_of(&cargo_t), Some(QosClass::Batch));
    assert_eq!(e.class_of(&ff_t), Some(QosClass::Normal), "prefilter kept firefox away from py-rules");
    assert_eq!(e.level(), Level::Red);
    let x = e.explain(&cargo_t);
    assert_eq!(x.settings[0].value, Setting::CpuWeight { value: 30 });
    assert_eq!(x.settings[0].provenance.plugin, "py-rules");
    assert_eq!(x.settings[0].provenance.priority, 10, "intent priority 0 → plugin priority");

    let health = e.status().plugins;
    let row = |n: &str| health.iter().find(|h| h.name == n).unwrap().clone();
    assert_eq!(row("py-rules").state, "running");
    assert!(row("py-rules").pid.is_some());
    assert_eq!(row("off").state, "disabled");
    assert_eq!(row("recorder").state, "running");

    // A plugin's broad throttle cannot override the host's foreground protection.
    assert!(e.explain(&ff_t).settings.is_empty());
    assert!(e.explain(&ff_t).notes.iter().any(|n| n.kind == "rejected"));
    let restored = e.shutdown();
    assert_eq!(restored.len(), 1);
    assert!(rules.pid().is_none());
}

#[test]
fn engine_skips_a_crashing_external_policy() {
    let tmp = tempfile::tempdir().unwrap();
    let body = "import os\n@lp.policy\ndef evaluate(s):\n    os._exit(1)\n";
    let dir = write_plugin(tmp.path(), "dies", &["policy"], &python_sdk_script(body), "");
    let d = discovery::inspect_dev(&dir).unwrap();
    let env = MemoryEnvironment::new();
    let mut opts = EngineOptions::new("t", Privilege::User);
    opts.supervisor = fast_supervisor(2);
    let mut e = Engine::new(Config::default(), Box::new(env), opts).unwrap();
    let (good, _) = FnPolicy::new("good", |_| Ok(PolicyOutput { level: Some(Level::Yellow), ..Default::default() }));
    e.registry_mut().register_policy(Box::new(good), 0).unwrap();
    e.registry_mut().load_external(&d).unwrap();
    let r = e.step(1000);
    assert_eq!(r.level, Level::Yellow, "the built-in still ran");
    assert!(r.events.iter().any(|ev| matches!(ev, Event::PluginError { plugin, .. } if plugin == "dies")));
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut now = 1000;
    while e.registry().plugin_health("dies").unwrap().state != "disabled" {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
        now += 100;
        e.step(now);
    }
    let h = e.registry().plugin_health("dies").unwrap();
    assert!(h.detail.unwrap().contains("2 failures"));
}

/// `a` relative to `base` (both absolute).
fn relative_to(a: &std::path::Path, base: &std::path::Path) -> std::path::PathBuf {
    let a: Vec<_> = a.components().collect();
    let b: Vec<_> = base.components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut out = std::path::PathBuf::new();
    for _ in common..b.len() {
        out.push("..");
    }
    for c in &a[common..] {
        out.push(c.as_os_str());
    }
    out
}

/// A plugin given by a relative path (`lindeskctl plugin test steam-games`) runs inside its
/// directory: its relative `exec` must be resolved exactly once (it used to become
/// `steam-games/steam-games/steam_games.py`).
#[test]
fn relative_plugin_path_resolves_exec_once() {
    let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let body = "@lp.classifier\ndef classify(proc):\n    return None\n";
    let dir = write_plugin(base.path(), "rel-plugin", &["classifier"], &python_sdk_script(body), "");
    let real = std::fs::canonicalize(&dir).unwrap();
    let rel = relative_to(&real, &std::env::current_dir().unwrap());
    assert!(rel.is_relative() && rel.join("plugin.toml").is_file(), "{}", rel.display());

    for path in [rel.clone(), rel.join("plugin.toml")] {
        let d = discovery::inspect_dev(&path).unwrap();
        assert_eq!(d.dir, real, "{}", path.display());
        assert_eq!(d.manifest_path, real.join("plugin.toml"));
        assert_eq!(d.exec, real.join("plugin"));
    }

    // Conformance (`lindeskctl plugin test <relative dir>`) starts the plugin.
    let report = lindesk_host::run_conformance(&rel);
    let init = report.checks.iter().find(|c| c.name.starts_with("initialize handshake")).expect("handshake check");
    assert!(init.ok, "{:?}; logs: {:?}", init.detail, report.logs);

    // So does an ExternalPlugin built from a relative directory and a relative `exec`.
    let d = discovery::inspect_dev(&rel).unwrap();
    let mut p = ExternalPlugin::new(d.manifest, &rel, ctx(), ExternalOptions::default());
    assert!(p.dir().is_absolute());
    p.start().unwrap_or_else(|e| panic!("start: {e}; logs: {:?}", p.recent_logs()));
    assert_eq!(Classifier::classify(&mut p, &proc_info("x")).unwrap(), None);
    p.shutdown();
}
