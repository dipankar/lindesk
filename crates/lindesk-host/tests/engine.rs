//! Engine end-to-end tests: fake environment, fake sensor, in-process policies and
//! classifiers, a recording actuator and recording sinks. No root, no real cgroups.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use lindesk_core::config::Config;
use lindesk_host::{ActionOutcome, ClassSource, Engine, EngineOptions, MemoryEnvironment};
use lindesk_plugin_api::*;

const CARGO: &str = "/user.slice/user-1000.slice/user@1000.service/app.slice/app-cargo-77.scope";
const FIREFOX: &str = "/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-org.mozilla.firefox-12.scope";
const STEAM: &str = "/user.slice/user-1000.slice/user@1000.service/app.slice/app-steam-5.scope";

fn cargo() -> Target {
    Target::user_unit(1000, "app-cargo-77.scope")
}
fn firefox() -> Target {
    Target::user_unit(1000, "app-gnome-org.mozilla.firefox-12.scope")
}

struct World {
    env: MemoryEnvironment,
    sensor: FakeSensor,
    act: RecordingActuator,
}

fn world() -> World {
    let env = MemoryEnvironment::new();
    env.spawn(77, "rustc", CARGO);
    env.spawn(12, "firefox", FIREFOX);
    env.watch_cgroup(CARGO);
    env.watch_cgroup(FIREFOX);
    env.set_mem_total(Some(16 << 30));
    World { env, sensor: FakeSensor::default(), act: RecordingActuator::default() }
}

fn engine_with(w: &World, config: Config, opts: EngineOptions) -> Engine {
    let mut e = Engine::new(config, Box::new(w.env.clone()), opts).unwrap();
    let r = e.registry_mut();
    r.register_sensor(Box::new(w.sensor.clone()), Duration::from_millis(1)).unwrap();
    r.register_actuator(Box::new(w.act.clone()), 0).unwrap();
    e
}

fn engine(w: &World) -> Engine {
    engine_with(w, Config::default(), EngineOptions::new("test", Privilege::System))
}

/// Throttle batch units while io pressure is above 50; `weight` is shared so tests can
/// change what the policy wants.
fn throttle_policy(weight: Arc<Mutex<u64>>) -> FnPolicy {
    FnPolicy::new("throttle", move |s| {
        let io = s.system_pressure(Resource::Io).map_or(0.0, |p| p.some_avg10);
        if io < 50.0 {
            return Ok(PolicyOutput { level: Some(Level::Green), ..Default::default() });
        }
        let w = *lock(&weight);
        Ok(PolicyOutput {
            intents: s
                .units
                .iter()
                .filter(|u| u.class == QosClass::Batch)
                .map(|u| intent(&u.target, Setting::CpuWeight { value: w }, "io pressure"))
                .collect(),
            level: Some(Level::Yellow),
            metrics: vec![Metric {
                ts: s.now,
                name: "io".into(),
                value: io as f64,
                labels: Default::default(),
                plugin: String::new(),
            }],
        })
    })
    .0
}

fn rustc_is_batch() -> FnClassifier {
    FnClassifier::new("rules", |p| Ok(if p.comm == "rustc" { class(QosClass::Batch, 1.0) } else { None })).0
}

#[test]
fn active_audio_and_its_ancestors_reject_throttles_until_playback_stops() {
    use lindesk_core::attention::Attention;
    let w = world();
    let parent = Target::user_unit(1000, "app.slice");
    w.env.watch_cgroup("/user.slice/user-1000.slice/user@1000.service/app.slice");
    let mut e = engine(&w);
    let pt = parent.clone();
    let (policy, _) = FnPolicy::new("throttle-all", move |_| {
        Ok(PolicyOutput {
            intents: [firefox(), cargo(), pt.clone()]
                .iter()
                .map(|t| intent(t, Setting::CpuWeight { value: 20 }, "competing policy"))
                .collect(),
            ..Default::default()
        })
    });
    e.registry_mut().register_policy(Box::new(policy), 0).unwrap();
    e.push_event(Event::Audible { ts: 1000, pids: vec![12], source: "pipewire".into() });
    let out = e.step(1000);
    assert!(out.actions.iter().all(|a| a.target == cargo()));
    assert_eq!(e.status().protected.iter().find(|t| t.target == firefox()).unwrap().attention, Some(Attention::Media));
    assert!(e.explain(&parent).notes.iter().any(|n| n.kind == "rejected"));

    e.push_event(Event::Audible { ts: 2000, pids: vec![], source: "pipewire".into() });
    let out = e.step(2000);
    assert!(out.actions.iter().any(|a| a.target == firefox() && a.outcome == ActionOutcome::Applied));
    assert!(e.status().protected.iter().all(|t| t.target != firefox()));
    assert!(out.actions.iter().any(|a| a.target == parent));

    e.push_event(Event::Audible { ts: 3000, pids: vec![12], source: "pipewire".into() });
    let out = e.step(3000);
    assert_eq!(out.actions.iter().filter(|a| a.outcome == ActionOutcome::Reverted).count(), 2);
    // A dead adapter must not leave a permanent media exception behind.
    let out = e.step(3000 + lindesk_host::PRESENCE_TTL_MS + 1);
    assert!(out.actions.iter().any(|a| a.target == firefox() && a.outcome == ActionOutcome::Applied));
    assert!(e.status().protected.iter().all(|t| t.target != firefox()));
}

#[test]
fn memory_protection_reaches_ancestors_and_restores_admin_baselines() {
    let w = world();
    let ancestor = Target::system_unit("user.slice");
    lock(&w.act.log).values.insert((ancestor.clone(), Knob::MemoryLow), Setting::MemoryLow { bytes: 256 << 20 });
    let mut e = engine(&w);
    let (policy, _) = FnPolicy::new("memory", |_| {
        Ok(PolicyOutput {
            intents: vec![intent(&firefox(), Setting::MemoryLow { bytes: 64 << 20 }, "foreground")],
            ..Default::default()
        })
    });
    e.registry_mut().register_policy(Box::new(policy), 0).unwrap();
    let r = e.step(1000);
    assert_eq!(r.actions.len(), 5);
    assert!(r.actions.iter().all(|a| a.outcome == ActionOutcome::Applied));
    for t in [
        Target::system_unit("user-1000.slice"),
        Target::system_unit("user@1000.service"),
        Target::user_unit(1000, "app.slice"),
        firefox(),
    ] {
        assert_eq!(lock(&w.act.log).values[&(t, Knob::MemoryLow)], Setting::MemoryLow { bytes: 64 << 20 });
    }
    assert_eq!(lock(&w.act.log).values[&(ancestor.clone(), Knob::MemoryLow)], Setting::MemoryLow { bytes: 256 << 20 });
    assert!(e.step(2000).actions.is_empty(), "unchanged budgets must not churn");
    let restored = e.set_paused(true);
    assert_eq!(restored.len(), 5);
    assert!(restored.iter().all(|a| a.outcome == ActionOutcome::Reverted));
    assert!(e.applied().is_empty());
    for ((target, knob), setting) in &lock(&w.act.log).values {
        assert_eq!(*knob, Knob::MemoryLow);
        assert_eq!(*setting, Setting::MemoryLow { bytes: if *target == ancestor { 256 << 20 } else { 0 } });
    }
}

#[test]
fn apply_change_revert() {
    let w = world();
    let mut e = engine(&w);
    let weight = Arc::new(Mutex::new(20));
    e.registry_mut().register_classifier(Box::new(rustc_is_batch()), 0).unwrap();
    e.registry_mut().register_policy(Box::new(throttle_policy(weight.clone())), 0).unwrap();

    let r = e.step(1000);
    assert!(r.actions.is_empty());
    assert_eq!(r.level, Level::Green);
    assert_eq!(e.class_of(&cargo()), Some(QosClass::Batch));
    assert!(r.events.iter().any(|ev| matches!(ev, Event::ClassSet { class: QosClass::Batch, .. })));

    w.sensor.set_pressure(Resource::Io, 80.0);
    let r = e.step(2000);
    assert_eq!(r.level, Level::Yellow);
    assert_eq!(r.actions.len(), 1, "{r}");
    assert_eq!(r.actions[0].outcome, ActionOutcome::Applied);
    assert_eq!(r.actions[0].setting, Some(Setting::CpuWeight { value: 20 }));
    assert_eq!(r.actions[0].original, Some(Setting::CpuWeight { value: 100 }));
    assert!(r.events.iter().any(|ev| matches!(ev, Event::LevelChanged { to: Level::Yellow, .. })));
    assert!(r.events.iter().any(|ev| matches!(ev, Event::ActionApplied { .. })));

    // Same desire: idempotent.
    assert!(e.step(3000).actions.is_empty());

    // Change.
    *lock(&weight) = 10;
    let r = e.step(4000);
    assert_eq!(r.actions.len(), 1);
    assert_eq!(r.actions[0].setting, Some(Setting::CpuWeight { value: 10 }));

    // Pressure gone: revert to the original.
    w.sensor.set_pressure(Resource::Io, 5.0);
    let r = e.step(5000);
    assert_eq!(r.actions.len(), 1);
    assert_eq!(r.actions[0].outcome, ActionOutcome::Reverted);
    assert!(e.applied().is_empty());
    let calls = lock(&w.act.log).calls.clone();
    assert_eq!(
        calls,
        vec![
            format!("read cpu.weight {}", cargo()),
            format!("apply cpu.weight=20 {}", cargo()),
            format!("apply cpu.weight=10 {}", cargo()),
            format!("reset cpu.weight {} to cpu.weight=100", cargo()),
        ]
    );
}

#[test]
fn pause_reverts_everything_and_resume_reapplies() {
    let w = world();
    let mut e = engine(&w);
    e.registry_mut().register_classifier(Box::new(rustc_is_batch()), 0).unwrap();
    e.registry_mut().register_policy(Box::new(throttle_policy(Arc::new(Mutex::new(20)))), 0).unwrap();
    w.sensor.set_pressure(Resource::Io, 80.0);
    assert_eq!(e.step(1000).actions[0].outcome, ActionOutcome::Applied);

    let restored = e.set_paused(true);
    assert!(e.paused() && e.dry_run());
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].outcome, ActionOutcome::Reverted);
    // Still sensing and deciding, applying nothing.
    let r = e.step(2000);
    assert_eq!(r.level, Level::Yellow);
    assert!(r.actions.iter().all(|a| a.outcome == ActionOutcome::DryRun), "{r}");
    assert!(r.events.iter().any(|ev| matches!(ev, Event::ActionApplied { provenance, .. }
        if provenance.reason.starts_with("paused: "))));
    assert!(e.set_paused(true).is_empty(), "idempotent");

    assert!(e.set_paused(false).is_empty());
    assert!(!e.paused() && !e.dry_run());
    let r = e.step(3000);
    assert_eq!(r.actions.len(), 1, "{r}");
    assert_eq!(r.actions[0].outcome, ActionOutcome::Applied);
    let calls = lock(&w.act.log).calls.clone();
    assert_eq!(
        calls,
        vec![
            format!("read cpu.weight {}", cargo()),
            format!("apply cpu.weight=20 {}", cargo()),
            format!("reset cpu.weight {} to cpu.weight=100", cargo()),
            format!("read cpu.weight {}", cargo()),
            format!("apply cpu.weight=20 {}", cargo()),
        ]
    );
}

#[test]
fn dry_run_records_but_never_applies() {
    let w = world();
    let mut config = Config::default();
    config.general.dry_run = true;
    let mut e = engine_with(&w, config, EngineOptions::new("test", Privilege::System));
    e.registry_mut().register_classifier(Box::new(rustc_is_batch()), 0).unwrap();
    e.registry_mut().register_policy(Box::new(throttle_policy(Arc::new(Mutex::new(20)))), 0).unwrap();
    w.sensor.set_pressure(Resource::Io, 80.0);
    let r = e.step(1000);
    assert_eq!(r.actions.len(), 1);
    assert_eq!(r.actions[0].outcome, ActionOutcome::DryRun);
    assert!(r.events.iter().any(|ev| matches!(ev, Event::ActionApplied { provenance, .. }
        if provenance.reason.starts_with("dry run"))));
    assert!(e.step(2000).actions.is_empty(), "dry-run state still de-duplicates");
    assert!(lock(&w.act.log).calls.is_empty(), "actuator untouched");

    let x = e.explain(&cargo());
    assert!(x.dry_run);
    assert_eq!(x.settings[0].value, Setting::CpuWeight { value: 20 });
    assert_eq!(x.settings[0].provenance.reason, "io pressure");
    assert!(x.to_string().contains("(dry run, not applied)"), "{x}");
    assert!(e.status().dry_run);

    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state.json");
    e.save_state(&state).unwrap();
    assert_eq!(std::fs::read_to_string(&state).unwrap(), "[]", "nothing real to restore");

    w.sensor.set_pressure(Resource::Io, 1.0);
    let r = e.step(3000);
    assert_eq!(r.actions[0].outcome, ActionOutcome::DryRun);
    assert!(lock(&w.act.log).calls.is_empty());
}

#[test]
fn crash_state_is_recovered_and_restored() {
    let w = world();
    // Like the daemons, keep the state file on tmpfs when possible (on a busy ext4 disk
    // every atomic replace forces a flush).
    let tmp = std::env::var_os("XDG_RUNTIME_DIR")
        .and_then(|d| tempfile::tempdir_in(d).ok())
        .unwrap_or_else(|| tempfile::tempdir().unwrap());
    let state = tmp.path().join("run/state.json");
    let mut opts = EngineOptions::new("test", Privilege::System);
    opts.state_path = Some(state.clone());
    {
        let mut e = engine_with(&w, Config::default(), opts.clone());
        e.registry_mut().register_classifier(Box::new(rustc_is_batch()), 0).unwrap();
        e.registry_mut().register_policy(Box::new(throttle_policy(Arc::new(Mutex::new(20)))), 0).unwrap();
        w.sensor.set_pressure(Resource::Io, 80.0);
        assert_eq!(e.step(1000).actions.len(), 1);
        // "Crash": the engine goes away without restoring.
    }
    let saved = std::fs::read_to_string(&state).unwrap();
    assert!(saved.contains("app-cargo-77.scope"), "{saved}");

    // A fresh daemon whose policy no longer wants the throttle.
    let act2 = RecordingActuator::default();
    let mut e = Engine::new(Config::default(), Box::new(w.env.clone()), opts.clone()).unwrap();
    e.registry_mut().register_actuator(Box::new(act2.clone()), 0).unwrap();
    assert_eq!(e.load_state(&state).unwrap(), 1);
    let x = e.explain(&cargo());
    assert_eq!(x.settings[0].original, Some(Setting::CpuWeight { value: 100 }), "original survives the crash");
    let r = e.step(2000);
    assert_eq!(r.actions.len(), 1);
    assert_eq!(lock(&act2.log).calls, vec![format!("reset cpu.weight {} to cpu.weight=100", cargo())]);
    assert_eq!(std::fs::read_to_string(&state).unwrap().trim(), "[]");

    // `lindeskd --restore`: load + restore_all.
    std::fs::write(&state, &saved).unwrap();
    let act3 = RecordingActuator::default();
    let mut e = Engine::new(Config::default(), Box::new(w.env.clone()), opts).unwrap();
    e.registry_mut().register_actuator(Box::new(act3.clone()), 0).unwrap();
    e.load_state(&state).unwrap();
    let restored = e.restore_all();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].outcome, ActionOutcome::Reverted);
    assert_eq!(lock(&act3.log).calls.len(), 1);
    assert!(e.applied().is_empty());
}

#[test]
fn classification_precedence() {
    let w = world();
    w.env.spawn(5, "steam", STEAM);
    w.env.watch_cgroup(STEAM);
    let config = Config::from_toml(
        r#"
        [[override]]
        unit = "app-cargo-*.scope"
        class = "background"
        "#,
    )
    .unwrap();
    let mut e = engine_with(&w, config.clone(), EngineOptions::new("test", Privilege::System));
    // High priority but unsure about rustc; certain about firefox.
    let (unsure, _) = FnClassifier::new("unsure", |p| {
        Ok(match p.comm.as_str() {
            "rustc" => class(QosClass::Interactive, 0.3),
            "firefox" => class(QosClass::Interactive, 0.9),
            _ => None,
        })
    });
    let (fallback, _) = FnClassifier::new("fallback", |_| Ok(class(QosClass::Batch, 1.0)));
    let (steam_only, steam_calls) = FnClassifier::new("steam-only", |_| Ok(class(QosClass::Critical, 1.0)));
    e.registry_mut().register_classifier(Box::new(unsure), 90).unwrap();
    e.registry_mut().register_classifier(Box::new(fallback), 10).unwrap();
    let section = ClassifierSection {
        priority: 50,
        min_confidence: 0.5,
        prefilter: Prefilter { comm: vec!["steam*".into()], ..Default::default() },
    };
    e.registry_mut().register_classifier_with(Box::new(steam_only), &section).unwrap();
    e.step(1000);

    // Override beats classifiers.
    let x = e.explain(&cargo());
    assert_eq!(x.class.as_ref().unwrap().class, QosClass::Background);
    assert_eq!(x.class.as_ref().unwrap().source, ClassSource::Override { index: 0 });
    // Highest-priority confident classifier wins.
    let x = e.explain(&firefox());
    assert_eq!(x.class.as_ref().unwrap().class, QosClass::Interactive);
    assert!(matches!(&x.class.as_ref().unwrap().source, ClassSource::Classifier { plugin, .. } if plugin == "unsure"));
    // Prefilter: only steam reached the steam-only classifier.
    assert_eq!(*lock(&steam_calls), vec!["steam".to_string()]);
    assert_eq!(e.class_of(&Target::user_unit(1000, "app-steam-5.scope")), Some(QosClass::Critical));

    // Explicit beats override; clearing goes back.
    e.set_class(cargo(), QosClass::Interactive, "dbus");
    e.step(2000);
    let x = e.explain(&cargo());
    assert_eq!(x.class.as_ref().unwrap().source, ClassSource::Explicit { source: "dbus".into() });
    e.clear_class(&cargo());
    e.step(3000);
    assert_eq!(e.class_of(&cargo()), Some(QosClass::Background));

    // Without the override, the unsure (0.3 < 0.5) verdict is skipped for the fallback.
    e.reload_config(Config::default()).unwrap();
    e.step(4000);
    let x = e.explain(&cargo());
    assert_eq!(x.class.as_ref().unwrap().class, QosClass::Batch);
    assert!(
        matches!(&x.class.as_ref().unwrap().source, ClassSource::Classifier { plugin, .. } if plugin == "fallback")
    );
    assert!(x.to_string().contains("classifier 'fallback'"), "{x}");

    // Nobody has an opinion → normal.
    let env2 = MemoryEnvironment::new();
    env2.spawn(1, "bash", "/system.slice/foo.service");
    env2.watch_cgroup("/system.slice/foo.service");
    let mut e2 = Engine::new(Config::default(), Box::new(env2), EngineOptions::new("t", Privilege::System)).unwrap();
    e2.step(0);
    let x = e2.explain(&Target::system_unit("foo.service"));
    assert_eq!(
        x.class.unwrap(),
        lindesk_host::ClassDecision { class: QosClass::Normal, source: ClassSource::Default, reason: String::new() }
    );
}

#[test]
fn foreground_resolution() {
    let w = world();
    let mut e = engine(&w);
    let (policy, seen) = FnPolicy::new("watcher", |_| Ok(PolicyOutput::default()));
    e.registry_mut().register_policy(Box::new(policy), 0).unwrap();

    e.push_event(Event::Focus { ts: 1, pid: Some(12), app_id: None, target: None, source: "test".into() });
    let r = e.step(1000);
    assert_eq!(r.foreground, Some(firefox()));
    let snap = lock(&seen).last().unwrap().clone();
    assert_eq!(snap.foreground, Some(firefox()));
    let fg = snap.units.iter().find(|u| u.foreground).unwrap();
    assert_eq!(fg.target, firefox());
    assert_eq!(fg.class, QosClass::Normal, "class stays as classified");
    assert_eq!(fg.top_comm.as_deref(), Some("firefox"));
    match &snap.events[0] {
        Event::Focus { target, .. } => assert_eq!(target.as_ref(), Some(&firefox()), "host filled the target"),
        other => panic!("{other:?}"),
    }

    // By app id, from another thread through the host sender (like an adapter).
    let tx = e.host_sender();
    std::thread::spawn(move || {
        tx.event(Event::Focus { ts: 2, pid: None, app_id: Some("cargo".into()), target: None, source: "x".into() })
    })
    .join()
    .unwrap();
    e.step(2000);
    assert_eq!(e.foreground(), Some(&cargo()));
    assert_eq!(e.status().foreground.unwrap().target, cargo());

    // Interactions get targets too; a focus on an unknown pid clears the foreground.
    e.push_event(Event::Interaction { ts: 3, pid: Some(12), target: None, end: false, source: "t".into() });
    e.push_event(Event::Focus { ts: 3, pid: Some(9999), app_id: None, target: None, source: "t".into() });
    e.step(3000);
    let snap = lock(&seen).last().unwrap().clone();
    assert!(
        matches!(&snap.events[..], [Event::Interaction { target: Some(t), .. }, Event::Focus { target: None, .. }] if t == &firefox())
    );
    assert_eq!(e.foreground(), None);
}

#[test]
fn sinks_get_their_tables() {
    let w = world();
    let mut config = Config::default();
    config.telemetry.interval = Duration::from_millis(2000);
    let mut e = engine_with(&w, config, EngineOptions::new("test", Privilege::System));
    e.registry_mut().register_classifier(Box::new(rustc_is_batch()), 0).unwrap();
    e.registry_mut().register_policy(Box::new(throttle_policy(Arc::new(Mutex::new(20)))), 0).unwrap();
    let events_only = RecordingSink::new("events-only", &["events"]);
    let everything = RecordingSink::new("everything", &[]);
    e.registry_mut().register_sink(Box::new(events_only.clone())).unwrap();
    e.registry_mut().register_sink(Box::new(everything.clone())).unwrap();
    w.sensor.set_pressure(Resource::Io, 80.0);
    w.sensor.set_usage(&cargo(), Usage { cpu_usage_us: 1000, interval_ms: 1000, ..Default::default() });
    e.push_event(Event::Focus { ts: 1, pid: Some(12), app_id: None, target: None, source: "t".into() });
    e.step(1000);

    assert_eq!(events_only.tables_seen(), ["events"].into());
    assert_eq!(everything.tables_seen(), ["events", "metrics", "pressure", "units"].into());
    let recs = lock(&everything.records).clone();
    let kinds: Vec<&str> =
        recs.iter().filter_map(|r| if let Record::Event(e) = r { Some(e.kind()) } else { None }).collect();
    for k in ["focus", "class_set", "level", "action_applied"] {
        assert!(kinds.contains(&k), "missing {k} in {kinds:?}");
    }
    let unit_row = recs
        .iter()
        .find_map(|r| match r {
            Record::Unit { target, applied, usage, .. } if target == &cargo() => Some((applied.clone(), usage.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(unit_row.0, vec![Setting::CpuWeight { value: 20 }], "unit rows carry this step's applied settings");
    assert_eq!(unit_row.1.cpu_usage_us, 1000);

    // Units are recorded once per telemetry interval.
    let units = |s: &RecordingSink| lock(&s.records).iter().filter(|r| r.table() == "units").count();
    let n = units(&everything);
    e.step(2000);
    assert_eq!(units(&everything), n);
    e.step(3000);
    assert_eq!(units(&everything), 2 * n);

    // Telemetry off: sinks get nothing.
    let before = lock(&everything.records).len();
    let mut c = e.config().clone();
    c.telemetry.enabled = false;
    e.reload_config(c).unwrap();
    e.step(4000);
    assert_eq!(lock(&everything.records).len(), before);

    e.shutdown();
    assert_eq!(*lock(&everything.flushed), 1);
}

#[test]
fn safety_clamp_surfaces_in_explain() {
    let w = world();
    let mut e = engine(&w);
    let (greedy, _) = FnPolicy::new("greedy", |s| {
        Ok(PolicyOutput {
            intents: s
                .units
                .iter()
                .filter(|u| u.foreground)
                .map(|u| intent(&u.target, Setting::UclampMin { percent: 100.0 }, "max boost"))
                .collect(),
            ..Default::default()
        })
    });
    e.registry_mut().register_policy(Box::new(greedy), 0).unwrap();
    e.push_event(Event::Focus { ts: 1, pid: Some(12), app_id: None, target: None, source: "t".into() });
    let r = e.step(1000);
    assert_eq!(r.actions[0].setting, Some(Setting::UclampMin { percent: 50.0 }));
    assert!(r.events.iter().any(|ev| matches!(ev, Event::PluginError { plugin, message, .. }
        if plugin == "greedy" && message.contains("clamped"))));
    assert!(r.notes.iter().any(|n| n.contains("[clamped] greedy")));

    let x = e.explain(&firefox());
    let s = &x.settings[0];
    assert_eq!(s.value, Setting::UclampMin { percent: 50.0 });
    assert_eq!(s.provenance.plugin, "greedy");
    assert_eq!(s.provenance.clamped_from.as_deref(), Some("cpu.uclamp.min=100.00"));
    assert_eq!(s.original, Some(Setting::UclampMin { percent: 0.0 }));
    assert!(x.notes.iter().any(|n| n.kind == "clamped" && n.plugin == "greedy"));
    let text = x.to_string();
    assert!(text.contains("clamped from cpu.uclamp.min=100.00"), "{text}");
    assert!(text.contains("foreground: yes"), "{text}");

    // The clamp keeps happening, but the PluginError event isn't repeated every step.
    let r = e.step(2000);
    assert!(!r.events.iter().any(|ev| matches!(ev, Event::PluginError { .. })));
}

#[test]
fn a_slice_holding_the_app_in_use_is_never_throttled() {
    // app.slice holds both firefox and the build. A policy that throttles everything not in
    // front would throttle the slice, and with it the app in use.
    const APP_SLICE: &str = "/user.slice/user-1000.slice/user@1000.service/app.slice";
    let w = world();
    w.env.watch_cgroup(APP_SLICE);
    let app_slice = Target::user_unit(1000, "app.slice");
    let mut e = engine(&w);
    let (limit_all, _) = FnPolicy::new("throttle-all", |s| {
        Ok(PolicyOutput {
            intents: s
                .units
                .iter()
                .filter(|u| !u.foreground)
                .map(|u| intent(&u.target, Setting::CpuWeight { value: 20 }, "yield"))
                .collect(),
            ..Default::default()
        })
    });
    e.registry_mut().register_policy(Box::new(limit_all), 0).unwrap();
    e.step(1000);
    // Nothing in front: the slice may be limited like anything else.
    assert_eq!(e.explain(&app_slice).settings.len(), 1);

    e.push_event(Event::Focus { ts: 2000, pid: Some(12), app_id: None, target: None, source: "test".into() });
    e.step(2000);
    assert_eq!(e.foreground(), Some(&firefox()));
    let x = e.explain(&app_slice);
    assert!(x.settings.is_empty(), "the limit on the slice holding firefox is taken off: {:?}", x.settings);
    assert!(x.notes.iter().any(|n| n.kind == "rejected"));
    // A sibling unit is still limited.
    assert_eq!(e.explain(&cargo()).settings.len(), 1);
}

#[test]
fn critical_targets_cannot_be_throttled_and_admin_overrides_win() {
    let w = world();
    let config = Config::from_toml(
        r#"
        [[override]]
        comm = "firefox"
        class = "critical"
        [[override]]
        unit = "app-cargo-*"
        settings = [{ knob = "cpu_weight", value = 5 }]
        "#,
    )
    .unwrap();
    let mut e = engine_with(&w, config, EngineOptions::new("test", Privilege::System));
    let (evil, _) = FnPolicy::new("evil", |s| {
        Ok(PolicyOutput {
            intents: s
                .units
                .iter()
                .map(|u| intent(&u.target, Setting::CpuWeight { value: 50 }, "throttle all"))
                .collect(),
            ..Default::default()
        })
    });
    e.registry_mut().register_policy(Box::new(evil), 100).unwrap();
    e.step(1000);
    // Critical: rejected; nothing applied.
    assert!(e.explain(&firefox()).settings.is_empty());
    assert!(e.explain(&firefox()).notes.iter().any(|n| n.kind == "rejected"));
    // Admin wins over the plugin.
    let x = e.explain(&cargo());
    assert_eq!(x.settings[0].value, Setting::CpuWeight { value: 5 });
    assert_eq!(x.settings[0].provenance.plugin, "admin");
    assert!(x.notes.iter().any(|n| n.kind == "overruled" && n.plugin == "evil"));
}

#[test]
fn failing_policy_is_skipped_and_reported() {
    let w = world();
    let mut e = engine(&w);
    let (broken, _) = FnPolicy::new("broken", |_| Err(PluginError::Failed("division by zero".into())));
    let (good, _) = FnPolicy::new("good", |s| {
        Ok(PolicyOutput {
            intents: s.units.iter().map(|u| intent(&u.target, Setting::IoWeight { value: 200 }, "boost")).collect(),
            ..Default::default()
        })
    });
    e.registry_mut().register_policy(Box::new(broken), 50).unwrap();
    e.registry_mut().register_policy(Box::new(good), 0).unwrap();
    let r = e.step(1000);
    assert_eq!(r.actions.len(), 2);
    assert!(r.events.iter().any(|ev| matches!(ev, Event::PluginError { plugin, message, .. }
        if plugin == "broken" && message.contains("division by zero"))));
    let h = e.status().plugins.into_iter().find(|p| p.name == "broken").unwrap();
    assert_eq!((h.state.as_str(), h.stats.calls, h.stats.errors), ("running", 1, 1));
    assert_eq!(e.global_notes().len(), 1);
}

#[test]
fn actuator_errors_remain_recoverable_and_retry() {
    let w = world();
    let mut e = engine(&w);
    e.registry_mut().register_classifier(Box::new(rustc_is_batch()), 0).unwrap();
    e.registry_mut().register_policy(Box::new(throttle_policy(Arc::new(Mutex::new(20)))), 0).unwrap();
    w.sensor.set_pressure(Resource::Io, 80.0);
    lock(&w.act.log).fail_apply = Some("unit is gone".into());
    let r = e.step(1000);
    assert!(matches!(&r.actions[0].outcome, ActionOutcome::Failed { error } if error.contains("unit is gone")));
    assert!(e.applied().get(&cargo(), &Knob::CpuWeight).unwrap().pending);
    assert!(e.explain(&cargo()).settings.is_empty());
    assert!(e.explain(&cargo()).notes.iter().any(|n| n.kind == "actuator_error"));
    // Healed: retried and applied.
    lock(&w.act.log).fail_apply = None;
    let r = e.step(2000);
    assert_eq!(r.actions[0].outcome, ActionOutcome::Applied);
    assert_eq!(e.applied().len(), 1);
}

#[test]
fn no_actuator_is_reported() {
    let w = world();
    let mut e =
        Engine::new(Config::default(), Box::new(w.env.clone()), EngineOptions::new("t", Privilege::System)).unwrap();
    let (p, _) = FnPolicy::new("p", |s| {
        Ok(PolicyOutput {
            intents: s.units.iter().map(|u| intent(&u.target, Setting::Nice { value: 5 }, "nice")).collect(),
            ..Default::default()
        })
    });
    e.registry_mut().register_policy(Box::new(p), 0).unwrap();
    let r = e.step(1000);
    assert!(r.actions.iter().all(|a| a.outcome == ActionOutcome::NoActuator));
    assert!(e.applied().is_empty());
}

#[test]
fn user_hosts_only_touch_their_own_units() {
    let w = world();
    w.env.spawn(300, "packagekitd", "/system.slice/packagekit.service");
    w.env.watch_cgroup("/system.slice/packagekit.service");
    let mut opts = EngineOptions::new("lindesk-session", Privilege::User);
    opts.uid = Some(1000);
    let mut e = engine_with(&w, Config::default(), opts);
    let (p, _) = FnPolicy::new("everything", |s| {
        Ok(PolicyOutput {
            intents: s.units.iter().map(|u| intent(&u.target, Setting::CpuWeight { value: 50 }, "x")).collect(),
            ..Default::default()
        })
    });
    e.registry_mut().register_policy(Box::new(p), 0).unwrap();
    e.step(1000);
    let pk = Target::system_unit("packagekit.service");
    assert!(e.explain(&pk).settings.is_empty());
    assert!(e.explain(&pk).notes.iter().any(|n| n.kind == "rejected" && n.message.contains("outside this user")));
    assert_eq!(e.explain(&cargo()).settings.len(), 1);
}

#[test]
fn status_reports_yield_against_the_pre_throttle_baseline() {
    let w = world();
    let mut e = engine(&w);
    e.registry_mut().register_classifier(Box::new(rustc_is_batch()), 0).unwrap();
    e.registry_mut().register_policy(Box::new(throttle_policy(Arc::new(Mutex::new(20)))), 0).unwrap();
    let io = |mbps: u64, cpu_ms: u64| Usage {
        cpu_usage_us: cpu_ms * 1000,
        io_wbytes: mbps << 20,
        interval_ms: 1000,
        mem_current: 1 << 30,
        ..Default::default()
    };
    w.sensor.set_usage(&cargo(), io(100, 800));
    let mut now = 0;
    for _ in 0..10 {
        now += 1000;
        e.step(now);
    }
    w.sensor.set_pressure(Resource::Io, 80.0);
    now += 1000;
    e.step(now);
    w.sensor.set_usage(&cargo(), io(69, 400));
    for _ in 0..20 {
        now += 1000;
        e.step(now);
    }
    let st = e.status();
    assert_eq!(st.level, Level::Yellow);
    assert_eq!(st.yielding.len(), 1);
    let y = &st.yielding[0];
    assert_eq!(y.summary.comm.as_deref(), Some("rustc"));
    assert!((y.io_delta_pct.unwrap() + 31.0).abs() < 0.5, "{y:?}");
    assert!((y.cpu_delta_pct.unwrap() + 50.0).abs() < 0.5, "{y:?}");
    assert_eq!(y.to_string(), "rustc -50% CPU -31% IO");
    let text = st.to_string();
    assert!(text.contains("yielding:   rustc -50% CPU -31% IO"), "{text}");
    assert!(text.contains("level YELLOW"), "{text}");
    let json = serde_json::to_value(&st).unwrap();
    assert_eq!(json["yielding"][0]["comm"], "rustc");
}

#[test]
fn forget_target_drops_state_without_restoring() {
    let w = world();
    let mut e = engine(&w);
    e.registry_mut().register_classifier(Box::new(rustc_is_batch()), 0).unwrap();
    e.registry_mut().register_policy(Box::new(throttle_policy(Arc::new(Mutex::new(20)))), 0).unwrap();
    w.sensor.set_pressure(Resource::Io, 80.0);
    e.step(1000);
    assert_eq!(e.applied().len(), 1);
    w.env.unwatch(&cargo());
    w.env.remove_proc(77);
    e.forget_target(&cargo());
    assert!(e.applied().is_empty());
    let calls = lock(&w.act.log).calls.len();
    e.step(2000);
    assert_eq!(lock(&w.act.log).calls.len(), calls, "nothing to reset for a vanished unit");
    assert!(!e.explain(&cargo()).known);
}

#[test]
fn sensors_see_watched_targets_and_respect_intervals() {
    let w = world();
    let mut e =
        Engine::new(Config::default(), Box::new(w.env.clone()), EngineOptions::new("t", Privilege::System)).unwrap();
    e.registry_mut().register_sensor(Box::new(w.sensor.clone()), Duration::from_millis(1000)).unwrap();
    e.step(0);
    e.step(500);
    e.step(1000);
    let polls = lock(&w.sensor.polls).clone();
    assert_eq!(polls.len(), 2, "polled at 0 and 1000, not 500");
    let mut targets: Vec<_> = polls[0].targets.iter().map(|t| t.cgroup.clone().unwrap()).collect();
    targets.sort();
    assert_eq!(targets, vec![CARGO.to_string(), FIREFOX.to_string()]);
}

#[test]
fn disabled_builtins_are_listed_but_not_run() {
    let w = world();
    let config = Config::from_toml("[plugins.throttle]\nenabled = false\n[plugins.rules]\npriority = 99\n").unwrap();
    let mut e = engine_with(&w, config, EngineOptions::new("t", Privilege::System));
    e.registry_mut().register_policy(Box::new(throttle_policy(Arc::new(Mutex::new(1)))), 0).unwrap();
    e.registry_mut().register_classifier(Box::new(rustc_is_batch()), 0).unwrap();
    w.sensor.set_pressure(Resource::Io, 99.0);
    assert!(e.step(1000).actions.is_empty());
    let h = e.registry().plugin_health("throttle").unwrap();
    assert_eq!(h.state, "disabled");
    assert!(h.detail.unwrap().contains("[plugins.throttle]"));
    assert!(e.registry_mut().register_classifier(Box::new(rustc_is_batch()), 0).is_err(), "duplicate");
}

/// Acts on system units only; everything else is `Unsupported` (like `systemd` on the
/// system bus, or the broker's `relay` for non-user units).
#[derive(Clone, Default)]
struct SystemUnitsOnly(Arc<Mutex<Vec<String>>>);

impl Plugin for SystemUnitsOnly {
    fn info(&self) -> PluginInfo {
        info("system-only", PluginKind::Actuator)
    }
}

impl Actuator for SystemUnitsOnly {
    fn caps(&self) -> ActuatorCaps {
        ActuatorCaps { knobs: vec![Knob::CpuWeight], targets: vec!["unit".into()] }
    }
    fn read(&mut self, t: &Target, k: &Knob) -> PluginResult<Option<Setting>> {
        lock(&self.0).push(format!("read {k} {t}"));
        if t.user().is_some() {
            return Err(PluginError::Unsupported(format!("{t} is not a system unit")));
        }
        Ok(None)
    }
    fn apply(&mut self, t: &Target, s: &Setting) -> PluginResult<()> {
        lock(&self.0).push(format!("apply {s} {t}"));
        if t.user().is_some() {
            return Err(PluginError::Unsupported(format!("{t} is not a system unit")));
        }
        Ok(())
    }
    fn reset(&mut self, t: &Target, k: &Knob, _o: Option<&Setting>) -> PluginResult<()> {
        lock(&self.0).push(format!("reset {k} {t}"));
        if t.user().is_some() {
            return Err(PluginError::Unsupported(format!("{t} is not a system unit")));
        }
        Ok(())
    }
}

#[test]
fn unsupported_target_falls_back_to_the_next_actuator() {
    let w = world();
    let mut e = engine(&w);
    let picky = SystemUnitsOnly::default();
    // Higher priority than the recorder (0): asked first.
    e.registry_mut().register_actuator(Box::new(picky.clone()), 10).unwrap();
    e.registry_mut().register_classifier(Box::new(rustc_is_batch()), 0).unwrap();
    e.registry_mut().register_policy(Box::new(throttle_policy(Arc::new(Mutex::new(20)))), 0).unwrap();

    w.sensor.set_pressure(Resource::Io, 80.0);
    let r = e.step(1000);
    let a = r.actions.iter().find(|a| a.target == cargo()).expect("cargo throttled");
    assert_eq!(a.outcome, ActionOutcome::Applied);
    assert_eq!(a.actuator.as_deref(), Some("recorder"), "the user unit went to the next capable actuator");
    assert_eq!(*lock(&picky.0), vec![format!("read cpu.weight {}", cargo())], "asked once, said Unsupported");
    assert!(lock(&w.act.log).calls.iter().any(|c| c.starts_with("apply cpu.weight=20")));
    assert!(e.explain(&cargo()).notes.iter().all(|n| n.kind != "no_actuator"));
    let stats = e.registry().health().into_iter().find(|h| h.name == "system-only").unwrap();
    assert_eq!(stats.stats.errors, 0, "Unsupported is an answer, not an error");

    // The revert takes the same route.
    w.sensor.set_pressure(Resource::Io, 0.0);
    let r = e.step(2000);
    let a = r.actions.iter().find(|a| a.target == cargo()).expect("cargo restored");
    assert_eq!((a.outcome.clone(), a.actuator.as_deref()), (ActionOutcome::Reverted, Some("recorder")));
    assert!(e.applied().is_empty());
}

#[test]
fn every_actuator_unsupported_is_no_actuator() {
    let w = world();
    let mut e =
        Engine::new(Config::default(), Box::new(w.env.clone()), EngineOptions::new("t", Privilege::System)).unwrap();
    e.registry_mut().register_sensor(Box::new(w.sensor.clone()), Duration::from_millis(1)).unwrap();
    e.registry_mut().register_actuator(Box::new(SystemUnitsOnly::default()), 0).unwrap();
    e.registry_mut().register_classifier(Box::new(rustc_is_batch()), 0).unwrap();
    e.registry_mut().register_policy(Box::new(throttle_policy(Arc::new(Mutex::new(20)))), 0).unwrap();
    w.sensor.set_pressure(Resource::Io, 80.0);
    let r = e.step(1000);
    let a = r.actions.iter().find(|a| a.target == cargo()).unwrap();
    assert_eq!(a.outcome, ActionOutcome::NoActuator);
    let note = e.explain(&cargo()).notes.into_iter().find(|n| n.kind == "no_actuator").unwrap();
    assert!(note.message.contains("is not a system unit"), "{}", note.message);
}

/// Simulates death after the kernel write by inspecting the journal inside apply itself.
struct JournalActuator {
    path: std::path::PathBuf,
    read_fails: bool,
    recorder: RecordingActuator,
}
impl Plugin for JournalActuator {
    fn info(&self) -> PluginInfo {
        info("journal-test", PluginKind::Actuator)
    }
}
impl Actuator for JournalActuator {
    fn caps(&self) -> ActuatorCaps {
        self.recorder.caps()
    }
    fn read(&mut self, t: &Target, k: &Knob) -> PluginResult<Option<Setting>> {
        if self.read_fails { Err(PluginError::Failed("cannot read original".into())) } else { self.recorder.read(t, k) }
    }
    fn apply(&mut self, t: &Target, s: &Setting) -> PluginResult<()> {
        let saved =
            lindesk_core::reconcile::AppliedState::from_json(&std::fs::read_to_string(&self.path).unwrap()).unwrap();
        let entry = saved.get(t, &s.knob()).expect("original must be on disk before mutation");
        assert!(entry.pending);
        assert_eq!(entry.original, Some(Setting::CpuWeight { value: 100 }));
        self.recorder.apply(t, s)?;
        Err(PluginError::Failed("simulated loss of reply after kernel write".into()))
    }
    fn reset(&mut self, t: &Target, k: &Knob, o: Option<&Setting>) -> PluginResult<()> {
        self.recorder.reset(t, k, o)
    }
}

#[test]
fn write_ahead_journal_recovers_a_write_without_a_successful_reply() {
    let w = world();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let mut opts = EngineOptions::new("test", Privilege::System);
    opts.state_path = Some(path.clone());
    let mut e = Engine::new(Config::default(), Box::new(w.env.clone()), opts.clone()).unwrap();
    e.registry_mut()
        .register_actuator(
            Box::new(JournalActuator { path: path.clone(), read_fails: false, recorder: w.act.clone() }),
            0,
        )
        .unwrap();
    let (policy, _) = FnPolicy::new("test-policy", |_| {
        Ok(PolicyOutput {
            intents: vec![intent(&cargo(), Setting::CpuWeight { value: 20 }, "test")],
            ..Default::default()
        })
    });
    e.registry_mut().register_policy(Box::new(policy), 0).unwrap();
    e.step(1000);
    assert_eq!(lock(&w.act.log).values[&(cargo(), Knob::CpuWeight)], Setting::CpuWeight { value: 20 });
    drop(e);
    let mut recovered = Engine::new(Config::default(), Box::new(w.env.clone()), opts).unwrap();
    recovered.registry_mut().register_actuator(Box::new(w.act.clone()), 0).unwrap();
    assert_eq!(recovered.load_state(&path).unwrap(), 1);
    recovered.restore_all();
    assert_eq!(lock(&w.act.log).values[&(cargo(), Knob::CpuWeight)], Setting::CpuWeight { value: 100 });
}

#[test]
fn unreadable_original_or_unwritable_journal_prevents_mutation() {
    for read_fails in [false, true] {
        let w = world();
        let dir = tempfile::tempdir().unwrap();
        let block = dir.path().join("file-not-directory");
        std::fs::write(&block, "blocked").unwrap();
        let path = if read_fails { dir.path().join("state.json") } else { block.join("state.json") };
        let mut opts = EngineOptions::new("test", Privilege::System);
        opts.state_path = Some(path.clone());
        let mut e = Engine::new(Config::default(), Box::new(w.env), opts).unwrap();
        e.registry_mut()
            .register_actuator(Box::new(JournalActuator { path, read_fails, recorder: w.act.clone() }), 0)
            .unwrap();
        let (policy, _) = FnPolicy::new("test-policy", |_| {
            Ok(PolicyOutput {
                intents: vec![intent(&cargo(), Setting::CpuWeight { value: 20 }, "test")],
                ..Default::default()
            })
        });
        e.registry_mut().register_policy(Box::new(policy), 0).unwrap();
        let r = e.step(1000);
        assert!(matches!(r.actions[0].outcome, ActionOutcome::Failed { .. }));
        assert!(lock(&w.act.log).values.is_empty());
        assert!(e.applied().is_empty());
    }
}

#[test]
fn dependencies_protect_only_one_hop_while_the_client_is_interactive() {
    use lindesk_core::attention::Attention;
    let w = world();
    w.env.spawn(5, "steam", STEAM);
    w.env.watch_cgroup(STEAM);
    let steam = Target::user_unit(1000, "app-steam-5.scope");
    let mut e = engine(&w);
    let st = steam.clone();
    let (policy, _) = FnPolicy::new("throttle-all", move |_| {
        Ok(PolicyOutput {
            intents: [cargo(), st.clone()]
                .iter()
                .map(|t| intent(t, Setting::CpuWeight { value: 20 }, "test"))
                .collect(),
            ..Default::default()
        })
    });
    e.registry_mut().register_policy(Box::new(policy), 0).unwrap();
    e.set_dependencies(&[(12, 77), (77, 5)]);
    e.push_event(Event::Focus { ts: 100, pid: Some(12), app_id: None, target: None, source: "test".into() });
    let report = e.step(100);
    assert!(!report.actions.iter().any(|a| a.target == cargo() && a.outcome == ActionOutcome::Applied));
    assert!(report.actions.iter().any(|a| a.target == steam && a.outcome == ActionOutcome::Applied));
    assert_eq!(
        e.status().protected.iter().find(|t| t.target == cargo()).unwrap().attention,
        Some(Attention::Dependency)
    );
    e.set_dependencies(&[]);
    let report = e.step(200);
    assert!(report.actions.iter().any(|a| a.target == cargo() && a.outcome == ActionOutcome::Applied));
    e.set_dependencies(&[(12, 77)]);
    e.push_event(Event::Focus { ts: 300, pid: Some(5), app_id: None, target: None, source: "test".into() });
    e.step(300);
    assert!(e.status().protected.iter().all(|t| t.target != cargo()));
    assert!(e.applied().for_target(&cargo()).any(|s| s.current == Setting::CpuWeight { value: 20 }));
}
