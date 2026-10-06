//! Simulated-time tests for the `threshold` policy.

use super::*;
use lindesk_core::safety::{ClampContext, SafetyCaps, Verdict, is_throttle};
use lindesk_plugin_api::{HostCaps, Scope, Usage};

const TICK: i64 = 250;
const MIB_U: u64 = 1024 * 1024;
const DEV: &str = "8:0";

fn unit(name: &str) -> Target {
    Target::user_unit(1000, name)
}

fn sample(now: i64, scope: Scope, r: Resource, some: f64, full: Option<f64>) -> Sample {
    let us = |v: f64| (v * TICK as f64 * 1000.0).round() as u64;
    Sample {
        ts: now,
        scope,
        resource: r,
        some_avg10: (some * 100.0) as f32,
        full_avg10: full.map(|f| (f * 100.0) as f32),
        some_delta_us: us(some),
        full_delta_us: full.map(us),
        interval_ms: TICK as u32,
    }
}

/// Builds one [`Snapshot`].
struct Snap {
    s: Snapshot,
}

impl Snap {
    fn at(now: i64) -> Self {
        Snap {
            s: Snapshot {
                now,
                level: Default::default(),
                pressure: Vec::new(),
                units: Vec::new(),
                events: Vec::new(),
                foreground: None,
                caps: HostCaps {
                    cgroup_v2: true,
                    systemd: true,
                    psi: true,
                    uclamp: true,
                    io_max: true,
                    devices: vec![DEV.into()],
                    ..Default::default()
                },
            },
        }
    }

    /// System-wide `some` ratio for `r`.
    fn global(mut self, r: Resource, some: f64) -> Self {
        self.s.pressure.retain(|p| p.resource != r);
        let full = (r == Resource::Memory).then_some(0.0);
        self.s.pressure.push(sample(self.s.now, Scope::System, r, some, full));
        self
    }

    /// System-wide memory pressure with an explicit `full` ratio.
    fn mem(mut self, some: f64, full: f64) -> Self {
        self.s.pressure.retain(|p| p.resource != Resource::Memory);
        self.s.pressure.push(sample(self.s.now, Scope::System, Resource::Memory, some, Some(full)));
        self
    }

    fn unit(mut self, name: &str, class: QosClass) -> Self {
        self.s.units.push(UnitState {
            target: unit(name),
            class,
            foreground: false,
            pressure: Vec::new(),
            usage: Some(Usage { mem_current: 1 << 30, interval_ms: TICK as u32, ..Default::default() }),
            applied: Vec::new(),
            top_comm: None,
        });
        self
    }

    fn usage(mut self, name: &str, rbps: u64, wbps: u64, mem: u64) -> Self {
        let u = self.s.units.iter_mut().find(|u| u.target == unit(name)).expect("unit");
        u.usage = Some(Usage {
            io_rbytes: rbps * TICK as u64 / 1000,
            io_wbytes: wbps * TICK as u64 / 1000,
            mem_current: mem,
            interval_ms: TICK as u32,
            ..Default::default()
        });
        self
    }

    /// Mark `name` as the foreground with the given cgroup io `some` stall ratio.
    fn fg(mut self, name: &str, stall: f64) -> Self {
        let now = self.s.now;
        self.s.foreground = Some(unit(name));
        let u = self.s.units.iter_mut().find(|u| u.target == unit(name)).expect("unit");
        u.foreground = true;
        u.pressure = vec![
            sample(now, Scope::Cgroup(format!("/{name}")), Resource::Io, stall, Some(0.0)),
            sample(now, Scope::Cgroup(format!("/{name}")), Resource::Cpu, 0.0, None),
        ];
        self
    }

    fn focus(mut self, ts: i64, name: &str) -> Self {
        self.s.events.push(Event::Focus {
            ts,
            pid: None,
            app_id: None,
            target: Some(unit(name)),
            source: "test".into(),
        });
        self
    }

    fn interaction(mut self, ts: i64, end: bool) -> Self {
        self.s.events.push(Event::Interaction { ts, pid: None, target: None, end, source: "test".into() });
        self
    }

    fn no_io_max(mut self) -> Self {
        self.s.caps.io_max = false;
        self
    }

    fn build(self) -> Snapshot {
        self.s
    }
}

/// Unsmoothed signals make level timings exact in tests.
fn crisp() -> ThresholdConfig {
    ThresholdConfig { ewma_alpha: Some(1.0), ..Default::default() }
}

fn get(out: &PolicyOutput, name: &str, knob: Knob) -> Option<Setting> {
    out.intents.iter().find(|i| i.target == unit(name) && i.setting.knob() == knob).map(|i| i.setting.clone())
}

fn knobs(out: &PolicyOutput, name: &str) -> Vec<Knob> {
    out.intents.iter().filter(|i| i.target == unit(name)).map(|i| i.setting.knob()).collect()
}

fn io_max(out: &PolicyOutput, name: &str) -> Option<IoLimit> {
    match get(out, name, Knob::IoMax(DEV.into())) {
        Some(Setting::IoMax { limit, .. }) => Some(limit),
        _ => None,
    }
}

/// A desktop with one unit of every class; `app` is the foreground.
fn desktop(now: i64, fg_stall: f64) -> Snap {
    Snap::at(now)
        .unit("session.slice", QosClass::Critical)
        .unit("tracker.service", QosClass::Background)
        .unit("build.scope", QosClass::Batch)
        .unit("editor.scope", QosClass::Normal)
        .unit("app.scope", QosClass::Interactive)
        .fg("app.scope", fg_stall)
}

#[test]
fn green_has_only_static_intents_and_mild_foreground_boost() {
    let mut p = ThresholdPolicy::default();
    let out = p.evaluate(&desktop(0, 0.0).build()).unwrap();
    assert_eq!(out.level, Some(Level::Green));

    assert_eq!(get(&out, "session.slice", Knob::CpuWeight), Some(Setting::CpuWeight { value: 200 }));
    assert_eq!(get(&out, "session.slice", Knob::IoWeight), Some(Setting::IoWeight { value: 200 }));
    assert_eq!(get(&out, "session.slice", Knob::MemoryLow), Some(Setting::MemoryLow { bytes: 512 * MIB_U }));
    assert_eq!(knobs(&out, "session.slice").len(), 3);

    assert_eq!(get(&out, "tracker.service", Knob::CpuWeight), Some(Setting::CpuWeight { value: 50 }));
    assert_eq!(get(&out, "tracker.service", Knob::IoWeight), Some(Setting::IoWeight { value: 50 }));
    assert_eq!(knobs(&out, "tracker.service").len(), 2);

    assert_eq!(get(&out, "build.scope", Knob::CpuIdle), Some(Setting::CpuIdle { value: true }));
    assert_eq!(get(&out, "build.scope", Knob::IoWeight), Some(Setting::IoWeight { value: 10 }));
    assert_eq!(knobs(&out, "build.scope").len(), 2);

    assert!(knobs(&out, "editor.scope").is_empty(), "normal units get nothing");

    // Foreground without an epoch: mild boost, no uclamp.
    assert_eq!(get(&out, "app.scope", Knob::CpuWeight), Some(Setting::CpuWeight { value: 200 }));
    assert_eq!(get(&out, "app.scope", Knob::IoWeight), Some(Setting::IoWeight { value: 200 }));
    assert_eq!(get(&out, "app.scope", Knob::MemoryLow), Some(Setting::MemoryLow { bytes: 1 << 30 }));
    assert_eq!(get(&out, "app.scope", Knob::UclampMin), None);
    assert!(out.intents.iter().all(|i| !is_throttle(&i.setting) || i.target != unit("app.scope")));

    // Metrics are present.
    for name in ["threshold.level", "threshold.score.cpu", "threshold.score.io", "threshold.score.memory"] {
        assert!(out.metrics.iter().any(|m| m.name == name), "{name}");
    }
}

#[test]
fn foreground_memory_low_is_capped() {
    let mut p = ThresholdPolicy::default();
    let out = p.evaluate(&desktop(0, 0.0).usage("app.scope", 0, 0, 8 << 30).build()).unwrap();
    assert_eq!(get(&out, "app.scope", Knob::MemoryLow), Some(Setting::MemoryLow { bytes: 2 << 30 }));
}

#[test]
fn epoch_starts_on_focus_and_expires() {
    let mut p = ThresholdPolicy::default();
    let out = p.evaluate(&desktop(1000, 0.0).focus(1000, "app.scope").build()).unwrap();
    assert_eq!(get(&out, "app.scope", Knob::CpuWeight), Some(Setting::CpuWeight { value: 500 }));
    assert_eq!(get(&out, "app.scope", Knob::IoWeight), Some(Setting::IoWeight { value: 500 }));
    assert_eq!(get(&out, "app.scope", Knob::UclampMin), Some(Setting::UclampMin { percent: 30.0 }));
    assert!(out.metrics.iter().any(|m| m.name == "threshold.epoch" && m.value == 1.0));

    // Switching to an app earns the longer switch epoch (3s), not the 500ms input epoch.
    let out = p.evaluate(&desktop(3999, 0.0).build()).unwrap();
    assert_eq!(get(&out, "app.scope", Knob::CpuWeight), Some(Setting::CpuWeight { value: 500 }));

    let out = p.evaluate(&desktop(4000, 0.0).build()).unwrap();
    assert_eq!(get(&out, "app.scope", Knob::CpuWeight), Some(Setting::CpuWeight { value: 200 }));
    assert_eq!(get(&out, "app.scope", Knob::UclampMin), None, "uclamp.min reverts after the epoch");
}

#[test]
fn epoch_is_extended_by_interaction_and_ended_early() {
    let mut p = ThresholdPolicy::default();
    p.evaluate(&desktop(1000, 0.0).focus(1000, "app.scope").build()).unwrap();
    // The switch epoch runs to 4000; input at 3800 extends it by the 500ms input epoch, to 4300.
    let out = p.evaluate(&desktop(3800, 0.0).interaction(3800, false).build()).unwrap();
    assert_eq!(get(&out, "app.scope", Knob::CpuWeight), Some(Setting::CpuWeight { value: 500 }));
    let out = p.evaluate(&desktop(4200, 0.0).build()).unwrap();
    assert_eq!(get(&out, "app.scope", Knob::CpuWeight), Some(Setting::CpuWeight { value: 500 }));
    let out = p.evaluate(&desktop(4300, 0.0).build()).unwrap();
    assert_eq!(get(&out, "app.scope", Knob::CpuWeight), Some(Setting::CpuWeight { value: 200 }));

    // An interaction alone also starts an epoch; end:true cuts it short.
    let out = p.evaluate(&desktop(6000, 0.0).interaction(6000, false).build()).unwrap();
    assert_eq!(get(&out, "app.scope", Knob::CpuWeight), Some(Setting::CpuWeight { value: 500 }));
    let out = p.evaluate(&desktop(6100, 0.0).interaction(6100, true).build()).unwrap();
    assert_eq!(get(&out, "app.scope", Knob::CpuWeight), Some(Setting::CpuWeight { value: 200 }));
    assert_eq!(get(&out, "app.scope", Knob::UclampMin), None);
}

#[test]
fn focus_change_starts_a_new_epoch_for_the_new_target() {
    let mut p = ThresholdPolicy::default();
    p.evaluate(&desktop(1000, 0.0).focus(1000, "app.scope").build()).unwrap();
    // Focus moves to the editor.
    let snap = Snap::at(1200)
        .unit("app.scope", QosClass::Interactive)
        .unit("editor.scope", QosClass::Normal)
        .fg("editor.scope", 0.0)
        .focus(1200, "editor.scope")
        .build();
    let out = p.evaluate(&snap).unwrap();
    assert_eq!(get(&out, "editor.scope", Knob::CpuWeight), Some(Setting::CpuWeight { value: 500 }));
    // The app you just left stays warm: a little above the default share, no epoch boost.
    assert_eq!(get(&out, "app.scope", Knob::CpuWeight), Some(Setting::CpuWeight { value: 150 }));
    assert_eq!(get(&out, "app.scope", Knob::IoWeight), Some(Setting::IoWeight { value: 150 }));
    assert_eq!(get(&out, "app.scope", Knob::UclampMin), None, "the old foreground loses its boost");
    // Interactions for a target that is not the foreground are ignored.
    // (After the editor's 3s switch epoch has run out.)
    let mut snap = Snap::at(4500).unit("app.scope", QosClass::Interactive).unit("editor.scope", QosClass::Normal);
    snap = snap.fg("editor.scope", 0.0);
    snap.s.events.push(Event::Interaction {
        ts: 4500,
        pid: None,
        target: Some(unit("app.scope")),
        end: false,
        source: "test".into(),
    });
    let out = p.evaluate(&snap.build()).unwrap();
    assert_eq!(get(&out, "editor.scope", Knob::CpuWeight), Some(Setting::CpuWeight { value: 200 }));
}

#[test]
fn yellow_adds_background_and_batch_throttles() {
    let mut p = ThresholdPolicy::new(crisp());
    let out = p.evaluate(&desktop(0, 0.0).global(Resource::Io, 0.5).build()).unwrap();
    assert_eq!(out.level, Some(Level::Yellow));
    assert_eq!(get(&out, "tracker.service", Knob::CpuWeight), Some(Setting::CpuWeight { value: 20 }));
    assert_eq!(get(&out, "tracker.service", Knob::IoWeight), Some(Setting::IoWeight { value: 20 }));
    assert_eq!(get(&out, "tracker.service", Knob::UclampMax), Some(Setting::UclampMax { percent: 60.0 }));
    assert_eq!(get(&out, "build.scope", Knob::IoWeight), Some(Setting::IoWeight { value: 1 }));
    assert_eq!(get(&out, "build.scope", Knob::UclampMax), Some(Setting::UclampMax { percent: 40.0 }));
    assert_eq!(get(&out, "build.scope", Knob::CpuIdle), Some(Setting::CpuIdle { value: true }));
    assert!(knobs(&out, "editor.scope").is_empty());
    let reason = &out.intents.iter().find(|i| i.target == unit("tracker.service")).unwrap().reason;
    assert!(reason.starts_with("YELLOW: system io pressure 50% > 20%"), "{reason}");
}

/// Drives a policy through `secs` of 250 ms ticks; `f(now)` gives the snapshot.
fn run(p: &mut ThresholdPolicy, from: i64, to: i64, f: impl Fn(i64) -> Snapshot) -> Vec<(i64, Level, PolicyOutput)> {
    let mut v = Vec::new();
    let mut now = from;
    while now < to {
        let out = p.evaluate(&f(now)).unwrap();
        v.push((now, out.level.unwrap(), out));
        now += TICK;
    }
    v
}

fn transitions(trace: &[(i64, Level, PolicyOutput)]) -> Vec<(i64, Level)> {
    let mut out = Vec::new();
    let mut prev = Level::Green;
    for (t, l, _) in trace {
        if *l != prev {
            out.push((*t, *l));
            prev = *l;
        }
    }
    out
}

#[test]
fn sawtooth_around_enter_threshold_does_not_flap() {
    // Global io alternates 0.14 / 0.28 each tick (mean 0.21, just above the 0.20 enter).
    let mut p = ThresholdPolicy::default();
    let trace = run(&mut p, 0, 120_000, |now| {
        let v = if (now / TICK) % 2 == 0 { 0.14 } else { 0.28 };
        desktop(now, 0.0).global(Resource::Io, v).build()
    });
    let tr = transitions(&trace);
    assert_eq!(tr.len(), 1, "{tr:?}");
    assert_eq!(tr[0].1, Level::Yellow);

    // Foreground stall alternating 0 / 0.12 each tick: YELLOW, but never RED (not sustained
    // for red_after) and never back to GREEN (the exit condition never holds 5 s).
    let mut p = ThresholdPolicy::default();
    let trace = run(&mut p, 0, 60_000, |now| desktop(now, if (now / TICK) % 2 == 0 { 0.0 } else { 0.12 }).build());
    let tr = transitions(&trace);
    assert_eq!(tr, vec![(TICK, Level::Yellow)]);

    // Unsmoothed input with a slow 4 s square wave crossing the enter threshold.
    let mut p = ThresholdPolicy::new(crisp());
    let trace = run(&mut p, 0, 60_000, |now| {
        let v = if (now / 2000) % 2 == 0 { 0.25 } else { 0.15 };
        desktop(now, 0.0).global(Resource::Io, v).build()
    });
    assert_eq!(transitions(&trace).len(), 1, "0.15 is above the exit threshold (0.10): stays YELLOW");
}

#[test]
fn min_dwell_is_honoured() {
    let cfg = ThresholdConfig { recover_after: Duration::from_millis(250), ..crisp() };
    let mut p = ThresholdPolicy::new(cfg);
    // One spike, then calm.
    let trace =
        run(&mut p, 0, 10_000, |now| desktop(now, 0.0).global(Resource::Cpu, if now == 0 { 0.9 } else { 0.0 }).build());
    let tr = transitions(&trace);
    assert_eq!(tr, vec![(0, Level::Yellow), (3000, Level::Green)], "YELLOW min dwell is 3 s");
}

#[test]
fn recovery_is_stepwise_with_recover_after() {
    let mut p = ThresholdPolicy::new(crisp());
    // Foreground stalls at 20% for 3 s, then is healthy.
    let trace = run(&mut p, 0, 30_000, |now| desktop(now, if now < 3000 { 0.2 } else { 0.0 }).build());
    let tr = transitions(&trace);
    // t=0: YELLOW (fg > 5%); t=1000: RED (fg > 10% for red_after = 1 s);
    // calm from 3000: exit holds from 3000, RED → YELLOW at 8000 (recover_after 5 s),
    // then YELLOW → GREEN 5 s later at 13000.
    assert_eq!(tr, vec![(0, Level::Yellow), (1000, Level::Red), (8000, Level::Yellow), (13000, Level::Green)]);
    let red_reason = trace
        .iter()
        .find(|(t, _, _)| *t == 1000)
        .and_then(|(_, _, o)| o.intents.iter().find(|i| matches!(i.setting, Setting::IoMax { .. })))
        .map(|i| i.reason.clone())
        .unwrap();
    assert!(red_reason.starts_with("RED: foreground io stall 20% > 10%; io.max AIMD step 1"), "{red_reason}");
}

#[test]
fn red_dwell_holds_even_if_recover_after_is_short() {
    let cfg = ThresholdConfig { recover_after: Duration::ZERO, ..crisp() };
    let mut p = ThresholdPolicy::new(cfg);
    let trace = run(&mut p, 0, 10_000, |now| desktop(now, if now <= 1000 { 0.2 } else { 0.0 }).build());
    let tr = transitions(&trace);
    // RED at 1000, min dwell 2 s → YELLOW at 3000; YELLOW dwell 3 s → GREEN at 6000.
    assert_eq!(tr, vec![(0, Level::Yellow), (1000, Level::Red), (3000, Level::Yellow), (6000, Level::Green)]);
}

#[test]
fn aimd_halves_while_stalling_respects_floor_and_increases_additively() {
    let mut p = ThresholdPolicy::new(crisp());
    let w0 = 100 * MIB_U;
    let snap = |now: i64, stall: f64| desktop(now, stall).usage("build.scope", 0, w0, 1 << 30).build();
    // Reach RED at t=1000.
    for now in (0..1000).step_by(TICK as usize) {
        let out = p.evaluate(&snap(now, 0.3)).unwrap();
        assert!(io_max(&out, "build.scope").is_none(), "no io.max outside RED");
    }
    let out = p.evaluate(&snap(1000, 0.3)).unwrap();
    assert_eq!(out.level, Some(Level::Red));
    let l = io_max(&out, "build.scope").unwrap();
    assert_eq!(l.wbps, Some(50 * MIB_U), "initial = observed × 0.5");
    assert_eq!(l.rbps, Some(16 * MIB_U), "max(0, floor) × 0.5 clamped back to the floor");
    assert_eq!((l.riops, l.wiops), (None, None));
    let bg = io_max(&out, "tracker.service").unwrap();
    assert_eq!(bg.wbps, Some(16 * MIB_U));
    assert!(io_max(&out, "session.slice").is_none());
    assert!(io_max(&out, "app.scope").is_none());
    assert!(io_max(&out, "editor.scope").is_none());

    // No change within the interval.
    let out = p.evaluate(&snap(1750, 0.3)).unwrap();
    assert_eq!(io_max(&out, "build.scope").unwrap().wbps, Some(50 * MIB_U));
    // Halve every second while stalling, never below the floor.
    let out = p.evaluate(&snap(2000, 0.3)).unwrap();
    assert_eq!(io_max(&out, "build.scope").unwrap().wbps, Some(25 * MIB_U));
    let out = p.evaluate(&snap(3000, 0.3)).unwrap();
    assert_eq!(io_max(&out, "build.scope").unwrap().wbps, Some(16 * MIB_U));
    let out = p.evaluate(&snap(4000, 0.3)).unwrap();
    assert_eq!(io_max(&out, "build.scope").unwrap().wbps, Some(16 * MIB_U));
    let reason = &out.intents.iter().find(|i| matches!(i.setting, Setting::IoMax { .. })).unwrap().reason;
    assert!(reason.contains("AIMD step 4 (decrease"), "{reason}");

    // Still RED (7% is above the 5% red exit) but no longer stalling: +32 MiB/s per second.
    let out = p.evaluate(&snap(5000, 0.07)).unwrap();
    assert_eq!(out.level, Some(Level::Red));
    assert_eq!(io_max(&out, "build.scope").unwrap().wbps, Some(48 * MIB_U));
    let out = p.evaluate(&snap(6000, 0.07)).unwrap();
    assert_eq!(io_max(&out, "build.scope").unwrap().wbps, Some(80 * MIB_U));
    assert_eq!(io_max(&out, "build.scope").unwrap().rbps, Some(80 * MIB_U));

    // Calm: leaves RED after recover_after, and the limits vanish.
    let mut left = None;
    for now in (6250..20_000).step_by(TICK as usize) {
        let out = p.evaluate(&snap(now, 0.0)).unwrap();
        if out.level != Some(Level::Red) {
            assert!(out.intents.iter().all(|i| !matches!(i.setting, Setting::IoMax { .. })));
            left.get_or_insert(now);
        }
    }
    assert_eq!(left, Some(11_250));
    assert!(p.st.aimd.is_empty());
}

#[test]
fn aimd_needs_io_max_support_and_forgets_vanished_targets() {
    let mut p = ThresholdPolicy::new(crisp());
    for now in (0..=1000).step_by(TICK as usize) {
        let out = p.evaluate(&desktop(now, 0.3).no_io_max().build()).unwrap();
        assert!(out.intents.iter().all(|i| !matches!(i.setting, Setting::IoMax { .. })));
    }
    assert_eq!(p.level(), Level::Red);
    p.evaluate(&desktop(1250, 0.3).build()).unwrap();
    assert_eq!(p.st.aimd.len(), 2, "tracker + build");
    // The batch unit exits.
    let snap = Snap::at(1500)
        .unit("tracker.service", QosClass::Background)
        .unit("app.scope", QosClass::Interactive)
        .fg("app.scope", 0.3)
        .build();
    p.evaluate(&snap).unwrap();
    assert_eq!(p.st.aimd.keys().map(|(t, _)| t.clone()).collect::<Vec<_>>(), vec![unit("tracker.service")]);
}

#[test]
fn foreground_is_never_throttled_whatever_its_class() {
    let mut p = ThresholdPolicy::new(crisp());
    let trace = run(&mut p, 0, 5000, |now| {
        Snap::at(now).unit("game.scope", QosClass::Batch).fg("game.scope", 0.5).mem(0.5, 0.5).build()
    });
    assert_eq!(trace.last().unwrap().1, Level::Red);
    for (_, _, out) in &trace {
        for i in &out.intents {
            assert!(!is_throttle(&i.setting), "{i:?}");
        }
    }
}

#[test]
fn memory_high_only_on_batch_and_only_when_sustained() {
    let mut p = ThresholdPolicy::new(crisp());
    let base = |now: i64, full: f64, mem: u64| {
        desktop(now, 0.0)
            .mem(full, full)
            .usage("build.scope", 0, 0, mem)
            .usage("tracker.service", 0, 0, 4 << 30)
            .build()
    };
    let has_mem_high = |o: &PolicyOutput| o.intents.iter().any(|i| matches!(i.setting, Setting::MemoryHigh { .. }));

    // A 1 s burst of memory full pressure: RED, but no memory.high (mem_red_after = 3 s).
    let trace = run(&mut p, 0, 1000, |now| base(now, 0.3, 2 << 30));
    assert!(trace.iter().any(|(_, l, _)| *l == Level::Red));
    assert!(trace.iter().all(|(_, _, o)| !has_mem_high(o)));

    // Sustained: after 3 s above the threshold, memory.high = 90% of the batch usage.
    let mut p = ThresholdPolicy::new(crisp());
    let trace = run(&mut p, 0, 6000, |now| base(now, 0.3, 2 << 30));
    let first = trace.iter().find(|(_, _, o)| has_mem_high(o)).map(|(t, _, _)| *t);
    assert_eq!(first, Some(3000));
    let out = &trace.last().unwrap().2;
    let expected = ((2u64 << 30) as f64 * 0.9) as u64;
    assert_eq!(get(out, "build.scope", Knob::MemoryHigh), Some(Setting::MemoryHigh { bytes: Some(expected) }));
    for name in ["session.slice", "tracker.service", "editor.scope", "app.scope"] {
        assert_eq!(get(out, name, Knob::MemoryHigh), None, "{name}");
    }
    // Latched: usage dropping does not ratchet the limit down.
    let out = p.evaluate(&base(6000, 0.3, 1 << 30)).unwrap();
    assert_eq!(get(&out, "build.scope", Knob::MemoryHigh), Some(Setting::MemoryHigh { bytes: Some(expected) }));
    // Pressure gone: memory.high is dropped immediately (level is still RED).
    let out = p.evaluate(&base(6250, 0.0, 1 << 30)).unwrap();
    assert_eq!(out.level, Some(Level::Red));
    assert!(!has_mem_high(&out));
}

#[test]
fn critical_never_throttled_and_normal_untouched_at_every_level() {
    let mut p = ThresholdPolicy::new(crisp());
    let trace = run(&mut p, 0, 30_000, |now| {
        let hot = now < 8000;
        desktop(now, if hot { 0.4 } else { 0.0 })
            .mem(if hot { 0.5 } else { 0.0 }, if hot { 0.4 } else { 0.0 })
            .global(Resource::Cpu, if hot { 0.9 } else { 0.0 })
            .build()
    });
    let levels: BTreeSet<Level> = trace.iter().map(|(_, l, _)| *l).collect();
    assert_eq!(levels.len(), 3);
    for (_, _, out) in &trace {
        for i in out.intents.iter().filter(|i| i.target == unit("session.slice")) {
            assert!(!is_throttle(&i.setting), "{i:?}");
        }
        assert!(knobs(out, "editor.scope").is_empty());
    }
}

/// Tiny deterministic PRNG (no extra dependencies).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn unit(&mut self) -> f64 {
        (self.next() % 10_001) as f64 / 10_000.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn property_random_pressure_steps_one_level_and_passes_safety_caps() {
    let caps = SafetyCaps::default();
    let ord = |l: Level| level_value(l) as i32;
    for seed in 1..=40u64 {
        let mut rng = Lcg(seed);
        let cfg = if seed % 2 == 0 { ThresholdConfig::default() } else { crisp() };
        let mut p = ThresholdPolicy::new(cfg);
        let mut prev = Level::Green;
        let mut now = 0i64;
        // Regimes that persist for a while make all three levels reachable.
        let mut regime = 0.0;
        for step in 0..2000 {
            if step % 20 == 0 {
                regime = rng.unit();
            }
            let jitter = |r: &mut Lcg| (regime * r.unit() * 1.2).min(1.0);
            let fg = jitter(&mut rng) * 0.4;
            let mut snap = desktop(now, fg)
                .global(Resource::Cpu, jitter(&mut rng))
                .global(Resource::Io, jitter(&mut rng))
                .usage("build.scope", rng.below(400) * MIB_U, rng.below(400) * MIB_U, rng.below(8 << 30))
                .usage("app.scope", 0, 0, rng.below(8 << 30));
            let full = jitter(&mut rng) * 0.5;
            snap = snap.mem((full * 1.5).min(1.0), full);
            match rng.below(10) {
                0 => snap = snap.focus(now, "app.scope"),
                1 => snap = snap.interaction(now, false),
                2 => snap = snap.interaction(now, true),
                _ => {}
            }
            let snap = snap.build();
            let out = p.evaluate(&snap).unwrap();
            let level = out.level.unwrap();
            assert!((ord(level) - ord(prev)).abs() <= 1, "seed {seed} step {step}: {prev} -> {level}");
            prev = level;

            for i in &out.intents {
                let u = snap.units.iter().find(|u| u.target == i.target).expect("intent for a known unit");
                let ctx = ClampContext {
                    class: Some(u.class),
                    mem_current: u.usage.as_ref().map(|u| u.mem_current),
                    mem_total: Some(16 << 30),
                };
                let v = caps.check(&i.setting, &ctx);
                assert!(!matches!(v, Verdict::Rejected(_)), "seed {seed} step {step}: {i:?} -> {v:?}");
                if u.foreground || u.class == QosClass::Critical {
                    assert!(!is_throttle(&i.setting), "{i:?}");
                }
                if matches!(u.class, QosClass::Normal) {
                    panic!("normal unit got {i:?}");
                }
                if let Setting::IoMax { limit, .. } = &i.setting {
                    assert!(limit.rbps.unwrap() >= 16 * MIB_U && limit.wbps.unwrap() >= 16 * MIB_U);
                    assert_eq!(level, Level::Red);
                }
                if matches!(i.setting, Setting::MemoryHigh { .. }) {
                    assert_eq!(u.class, QosClass::Batch);
                }
                assert!(!i.reason.is_empty());
            }
            now += TICK;
        }
    }
}

#[test]
fn deterministic_replay() {
    let snaps: Vec<Snapshot> = (0..200)
        .map(|k| {
            let now = k * TICK;
            desktop(now, if (k / 15) % 3 == 0 { 0.3 } else { 0.01 }).global(Resource::Io, 0.3).build()
        })
        .collect();
    let run_once = || {
        let mut p = ThresholdPolicy::default();
        snaps.iter().map(|s| p.evaluate(s).unwrap()).collect::<Vec<_>>()
    };
    assert_eq!(run_once(), run_once());
}

#[test]
fn config_from_json_and_validation() {
    let mut p = ThresholdPolicy::default();
    let ctx = |config: serde_json::Value| InitContext {
        api_version: lindesk_plugin_api::API_VERSION,
        host: "simulate".into(),
        privilege: lindesk_plugin_api::Privilege::System,
        uid: None,
        caps: HostCaps::default(),
        config,
    };
    p.init(&ctx(serde_json::Value::Null)).unwrap();
    assert_eq!(p.config(), &ThresholdConfig::default());
    p.init(&ctx(serde_json::json!({
        "epoch": "300ms",
        "aimd_floor": "32MiB",
        "enter": { "io": 0.4 },
        "values": { "fg_epoch_weight": 800 }
    })))
    .unwrap();
    assert_eq!(p.config().epoch, Auto::Value(Duration::from_millis(300)));
    assert_eq!(p.config().aimd_floor, Auto::Value(32 * MIB_U));
    assert_eq!(p.config().enter.io, Auto::Value(0.4));
    assert_eq!(p.config().enter.cpu, Auto::Auto);
    assert_eq!(p.config().values.fg_epoch_weight, 800);
    assert!(p.init(&ctx(serde_json::json!({ "exit_ratio": 1.5 }))).is_err());
    assert!(p.init(&ctx(serde_json::json!({ "epoch": "soon" }))).is_err());
    assert!(p.init(&ctx(serde_json::json!({ "typo": 1 }))).is_err());
    assert!(p.init(&ctx(serde_json::json!({ "aimd_floor": 0 }))).is_err());
    assert!(p.init(&ctx(serde_json::json!({ "enter": { "io": 1.5 } }))).is_err());
    assert!(p.init(&ctx(serde_json::json!({ "values": { "fg_epoch_uclamp_min": 150.0 } }))).is_err());
    assert!(p.init(&ctx(serde_json::json!({ "aimd_step": "soon" }))).is_err());
    // "auto" is accepted everywhere it is the default, case-insensitively.
    p.init(&ctx(serde_json::json!({
        "epoch": "auto", "aimd_floor": "Auto", "enter": { "cpu": "auto" },
        "values": { "fg_epoch_uclamp_min": "auto" }
    })))
    .unwrap();
    assert_eq!(p.config(), &ThresholdConfig::default());
    // Serialisation round-trips both forms.
    let cfg = ThresholdConfig { aimd_floor: Auto::Value(8 * MIB_U), ..Default::default() };
    let json = serde_json::to_value(&cfg).unwrap();
    assert_eq!(json["aimd_floor"], "8.0MiB");
    assert_eq!(json["aimd_step"], "auto");
    assert_eq!(json["enter"]["cpu"], "auto");
    assert_eq!(ThresholdConfig::from_json(&json).unwrap(), cfg);
    assert_eq!(p.info().name, "threshold");
}

#[test]
fn packaged_default_config_matches_code_defaults() {
    use lindesk_core::config::Config;
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/config/lindesk.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let cfg = Config::from_toml(&text).unwrap();
    let def = Config::default();
    assert_eq!(cfg.general, def.general);
    assert_eq!(cfg.io, def.io);
    assert_eq!(cfg.safety, def.safety);
    assert_eq!(cfg.telemetry, def.telemetry);
    assert!(cfg.overrides.is_empty(), "the example override is commented out");

    let threshold = ThresholdConfig::from_json(&cfg.plugin("threshold").config).unwrap();
    assert_eq!(threshold, ThresholdConfig::default(), "documented tunables must match the code");

    let rules = cfg.plugin("rules").config;
    assert_eq!(rules["dirs"].as_array().map(Vec::len), Some(3));

    // The commented override example must be valid when uncommented.
    let start = text.find("# BEGIN override example").unwrap();
    let end = text.find("# END override example").unwrap();
    let example: String = text[start..end]
        .lines()
        .skip(1)
        .map(|l| l.strip_prefix("# ").or_else(|| l.strip_prefix('#')).unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n");
    let with_override = Config::from_toml(&example).unwrap();
    assert_eq!(with_override.overrides.len(), 2);
}

// ── hardware-scaled tunables ──────────────────────────────────────────────────────────────

mod hardware {
    use super::*;
    use lindesk_plugin_api::{Hardware, PowerSource, StorageDevice, StorageKind};
    use lindesk_sys::{Env, Roots, fixture, probe};
    use std::path::Path;

    fn caps_of(build: fn(&Path) -> (Roots, Env)) -> HostCaps {
        let d = tempfile::tempdir().unwrap();
        let (roots, env) = build(d.path());
        probe(&roots, &env).host_caps()
    }

    fn resolve(caps: &HostCaps) -> ResolvedThreshold {
        ThresholdConfig::default().resolve(caps)
    }

    fn why<'a>(r: &'a ResolvedThreshold, key: &str) -> &'a Resolution {
        r.why.iter().find(|w| w.key == key).unwrap_or_else(|| panic!("no {key}: {:#?}", r.why))
    }

    #[test]
    fn unknown_hardware_resolves_to_the_previous_constants() {
        let mut caps = Snap::at(0).build().caps;
        let r = resolve(&caps);
        assert_eq!(r.epoch_ms, 500);
        assert_eq!(r.enter, ResourceThresholds { cpu: 0.30, io: 0.20, memory: 0.10 });
        assert_eq!(r.critical_memory_low, 512 * MIB_U);
        assert_eq!(r.fg_memory_low_max, 2048 * MIB_U);
        assert_eq!(r.aimd_for(DEV), (16 * MIB_U, 32 * MIB_U));
        assert_eq!(r.aimd_for("1:2"), (16 * MIB_U, 32 * MIB_U), "unlisted device");
        assert_eq!((r.fg_epoch_uclamp_min, r.yellow_background_uclamp_max), (30.0, 60.0));
        assert!(r.why.iter().all(|w| w.source == Source::Default), "{:#?}", r.why);
        assert_eq!(r, ResolvedThreshold::default().clone_with_aimd(&r));
        // A device of unknown kind keeps the fixed AIMD values too.
        caps.hardware.storage.push(StorageDevice { dev: DEV.into(), name: "sda".into(), ..Default::default() });
        assert_eq!(resolve(&caps).aimd_for(DEV), (16 * MIB_U, 32 * MIB_U));
    }

    trait CloneWithAimd {
        fn clone_with_aimd(&self, other: &ResolvedThreshold) -> ResolvedThreshold;
    }
    impl CloneWithAimd for ResolvedThreshold {
        /// `Default` resolves against no devices; borrow the device rows to compare the rest.
        fn clone_with_aimd(&self, other: &ResolvedThreshold) -> ResolvedThreshold {
            let mut r = self.clone();
            r.aimd = other.aimd.clone();
            r.why = other.why.clone();
            r
        }
    }

    #[test]
    fn dev_host_nvme_desktop() {
        let caps = caps_of(fixture::pop_os);
        let r = resolve(&caps);
        assert_eq!(r.aimd_for("259:0"), (40 * MIB_U, 80 * MIB_U));
        assert_eq!(
            why(&r, "aimd_floor[nvme0n1]").to_string(),
            "aimd_floor[nvme0n1] 40MiB/s (auto: NVMe nominal ~2000MiB/s × 2%)"
        );
        assert_eq!(why(&r, "aimd_step[nvme0n1]").value, "80MiB/s");
        assert_eq!(r.epoch_ms, 500);
        assert_eq!(r.enter, ResourceThresholds { cpu: 0.30, io: 0.20, memory: 0.15 }, "zram");
        assert_eq!((r.critical_memory_low, r.fg_memory_low_max), (512 * MIB_U, 2048 * MIB_U));
        assert!(r.hybrid && !r.on_battery);
        assert!(why(&r, "hybrid").why.contains("P-cores"));
        assert_eq!(r.virtualization, None);
    }

    #[test]
    fn intel_laptop_on_battery() {
        let caps = caps_of(fixture::intel_hybrid_laptop);
        let r = resolve(&caps);
        assert!(r.on_battery && r.hybrid);
        assert_eq!(r.epoch_ms, 300);
        assert_eq!((r.fg_epoch_uclamp_min, r.yellow_background_uclamp_max), (15.0, 40.0));
        assert_eq!(why(&r, "epoch").source, Source::Auto);
        // 31.3 GiB: 4% = 1.25 GiB → capped at 512MiB; 10% = 3.1 GiB → capped at 2GiB.
        assert_eq!((r.critical_memory_low, r.fg_memory_low_max), (512 * MIB_U, 2048 * MIB_U));
        assert_eq!(r.aimd_for("259:0"), (40 * MIB_U, 80 * MIB_U));
    }

    #[test]
    fn arm_board_with_emmc_and_small_ram() {
        let caps = caps_of(fixture::arm_big_little);
        let r = resolve(&caps);
        assert_eq!(r.aimd_for("179:0"), (5 * MIB_U, 10 * MIB_U), "eMMC 250MiB/s × 2% / 4%");
        // 4,000,000 kB: 4% = 156MiB, 10% = 391MiB.
        assert_eq!(r.critical_memory_low, 156 * MIB_U);
        assert_eq!(r.fg_memory_low_max, 391 * MIB_U);
        assert_eq!(r.enter, ResourceThresholds { cpu: 0.30, io: 0.20, memory: 0.10 }, "8 CPUs, no swap");
        assert!(r.hybrid);
        assert_eq!(r.epoch_ms, 500, "no supplies = mains");
    }

    #[test]
    fn hdd_desktop_with_four_cpus() {
        let caps = caps_of(fixture::hdd_desktop);
        let r = resolve(&caps);
        assert_eq!(r.aimd_for("8:0"), (4 * MIB_U, 6 * MIB_U), "HDD: floor clamped to 4MiB/s, step 6MiB/s");
        assert!(why(&r, "aimd_floor[sda]").why.ends_with("× 2%, clamped"), "{}", why(&r, "aimd_floor[sda]"));
        assert_eq!(r.enter.io, 0.15);
        assert!(why(&r, "enter.io").why.contains("sda is a rotational disk"));
        assert_eq!(r.enter.cpu, 0.40);
        assert!(!r.hybrid);
    }

    #[test]
    fn kvm_guest_is_not_scaled_but_reported() {
        let caps = caps_of(fixture::kvm_guest);
        let r = resolve(&caps);
        assert_eq!(r.virtualization.as_deref(), Some("kvm"));
        assert_eq!(r.aimd_for("252:0"), (10 * MIB_U, 20 * MIB_U), "virtio 500MiB/s");
        assert!(why(&r, "virtualization").why.contains("guest only"));

        let mut p = ThresholdPolicy::new(crisp());
        let mut snap = Snap::at(0).build();
        snap.caps = caps;
        let out = p.evaluate(&snap).unwrap();
        let m = out.metrics.iter().find(|m| m.name == "threshold.virtualized").expect("metric");
        assert_eq!((m.value, m.labels["kind"].as_str()), (1.0, "kvm"));
        assert!(out.metrics.iter().any(|m| m.name == "threshold.on_battery" && m.value == 0.0));
    }

    #[test]
    fn explicit_values_win() {
        let caps = caps_of(fixture::intel_hybrid_laptop);
        let cfg = ThresholdConfig::from_json(&serde_json::json!({
            "epoch": "700ms",
            "aimd_floor": "8MiB",
            "critical_memory_low": "1GiB",
            "enter": { "io": 0.33 },
            "values": { "fg_epoch_uclamp_min": 50.0 }
        }))
        .unwrap();
        let r = cfg.resolve(&caps);
        assert_eq!(r.epoch_ms, 700);
        assert_eq!(r.aimd_for("259:0"), (8 * MIB_U, 80 * MIB_U), "explicit floor, auto step");
        assert_eq!(r.critical_memory_low, 1024 * MIB_U);
        assert_eq!(r.enter.io, 0.33);
        assert_eq!(r.fg_epoch_uclamp_min, 50.0, "explicit beats the battery cap");
        assert_eq!(r.yellow_background_uclamp_max, 40.0, "still auto");
        assert_eq!(why(&r, "epoch").source, Source::Explicit);
        assert_eq!(why(&r, "aimd_floor[nvme0n1]").source, Source::Explicit);
        assert_eq!(why(&r, "aimd_step[nvme0n1]").source, Source::Auto);
        assert_eq!(r.aimd_default, (8 * MIB_U, 32 * MIB_U));
    }

    fn laptop(power: PowerSource) -> HostCaps {
        let mut caps = Snap::at(0).build().caps;
        caps.hardware = Hardware {
            cpus: 8,
            cores: 4,
            smt: true,
            mem_total: 16 << 30,
            storage: vec![StorageDevice {
                dev: DEV.into(),
                name: "nvme0n1".into(),
                kind: StorageKind::Nvme,
                scheduler: "none".into(),
                nominal_bps: 2000 * MIB_U,
            }],
            power,
            portable: true,
            ..Default::default()
        };
        caps
    }

    #[test]
    fn battery_vs_ac_behaviour() {
        let run = |power: PowerSource| {
            let mut p = ThresholdPolicy::new(crisp());
            let mk = |now: i64| {
                let mut s = desktop(now, 0.0).build();
                s.caps = laptop(power);
                s
            };
            let mut s = mk(0);
            s.events.push(Event::Focus {
                ts: 0,
                pid: None,
                app_id: None,
                target: Some(unit("app.scope")),
                source: "test".into(),
            });
            let at0 = p.evaluate(&s).unwrap();
            let at400 = p.evaluate(&mk(1500)).unwrap();
            // Push into YELLOW with system CPU pressure.
            let mut y = None;
            for i in 0..4 {
                y = Some(p.evaluate(&desktop_with(mk(4000 + i * TICK), Resource::Cpu, 0.5)).unwrap());
            }
            (at0, at400, y.unwrap())
        };
        let (ac0, ac400, ac_y) = run(PowerSource::Ac);
        let (b0, b400, b_y) = run(PowerSource::Battery);
        assert_eq!(get(&ac0, "app.scope", Knob::UclampMin), Some(Setting::UclampMin { percent: 30.0 }));
        assert_eq!(get(&b0, "app.scope", Knob::UclampMin), Some(Setting::UclampMin { percent: 15.0 }));
        assert!(get(&ac400, "app.scope", Knob::UclampMin).is_some(), "on AC the switch epoch lasts 3s");
        assert_eq!(get(&b400, "app.scope", Knob::UclampMin), None, "on battery it is capped at 1s");
        assert_eq!(ac_y.level, Some(Level::Yellow));
        assert_eq!(b_y.level, Some(Level::Yellow));
        assert_eq!(get(&ac_y, "tracker.service", Knob::UclampMax), Some(Setting::UclampMax { percent: 60.0 }));
        assert_eq!(get(&b_y, "tracker.service", Knob::UclampMax), Some(Setting::UclampMax { percent: 40.0 }));
        assert!(b0.metrics.iter().any(|m| m.name == "threshold.on_battery" && m.value == 1.0));
    }

    fn desktop_with(mut s: Snapshot, r: Resource, some: f64) -> Snapshot {
        let now = s.now;
        s.pressure.retain(|p| p.resource != r);
        s.pressure.push(sample(now, Scope::System, r, some, None));
        s
    }

    #[test]
    fn power_change_is_picked_up_without_reinit() {
        let mut p = ThresholdPolicy::new(crisp());
        let mut s = desktop(0, 0.0).build();
        s.caps = laptop(PowerSource::Ac);
        p.evaluate(&s).unwrap();
        assert_eq!(p.res.epoch_ms, 500);
        s.caps = laptop(PowerSource::Battery);
        s.now = 250;
        p.evaluate(&s).unwrap();
        assert_eq!(p.res.epoch_ms, 300);
        assert_eq!(p.effective_config(&s.caps), p.res);
    }

    #[test]
    fn hdd_aimd_uses_per_device_floor_and_step() {
        let mut caps = Snap::at(0).build().caps;
        caps.devices = vec!["8:0".into(), "259:0".into()];
        caps.hardware.storage = vec![
            StorageDevice {
                dev: "8:0".into(),
                name: "sda".into(),
                kind: StorageKind::Hdd,
                nominal_bps: 150 * MIB_U,
                ..Default::default()
            },
            StorageDevice {
                dev: "259:0".into(),
                name: "nvme0n1".into(),
                kind: StorageKind::Nvme,
                nominal_bps: 2000 * MIB_U,
                ..Default::default()
            },
        ];
        let mut p = ThresholdPolicy::new(crisp());
        let mut last = None;
        for i in 0..12 {
            let mut s = desktop(i * TICK, 0.3).usage("build.scope", 0, 0, 1 << 30).build();
            s.caps = caps.clone();
            last = Some(p.evaluate(&s).unwrap());
        }
        let out = last.unwrap();
        assert_eq!(out.level, Some(Level::Red));
        let lim = |dev: &str| {
            out.intents
                .iter()
                .find_map(|i| match &i.setting {
                    Setting::IoMax { device, limit } if device == dev && i.target == unit("build.scope") => {
                        Some(limit.clone())
                    }
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(lim("8:0").wbps, Some(4 * MIB_U), "HDD floor");
        assert_eq!(lim("259:0").wbps, Some(40 * MIB_U), "NVMe floor");
    }

    #[test]
    fn measured_bandwidth_wins_over_nominal_unless_noisy() {
        use lindesk_plugin_api::{LatencyMap, StorageLatency};
        let mut caps = caps_of(fixture::pop_os);
        let lat = LatencyMap {
            storage: vec![StorageLatency {
                dev: "259:0".into(),
                seq_read_bps: 3000 * MIB_U,
                seq_write_bps: 1000 * MIB_U,
                ..Default::default()
            }],
            ..Default::default()
        };
        caps.hardware.latency = Some(lat.clone());
        let r = resolve(&caps);
        assert_eq!(r.aimd_for("259:0"), (20 * MIB_U, 40 * MIB_U), "lower of read/write: 1000MiB/s");
        assert_eq!(why(&r, "aimd_floor[nvme0n1]").why, "NVMe measured 1000MiB/s × 2%");
        caps.hardware.latency = Some(LatencyMap { noisy: true, ..lat });
        let r = resolve(&caps);
        assert_eq!(r.aimd_for("259:0"), (40 * MIB_U, 80 * MIB_U), "noisy calibration: nominal");
        assert!(why(&r, "aimd_floor[nvme0n1]").why.contains("nominal"));
    }

    #[test]
    fn unknown_hardware_is_never_more_aggressive_than_the_old_defaults() {
        let r = resolve(&HostCaps::default());
        let d = ResolvedThreshold::default();
        assert_eq!(r, d);
        assert!(r.enter.cpu >= 0.30 && r.enter.io >= 0.20 && r.enter.memory >= 0.10);
        assert_eq!(r.aimd_default, (16 * MIB_U, 32 * MIB_U));
        assert_eq!((r.epoch_ms, r.fg_epoch_uclamp_min, r.yellow_background_uclamp_max), (500, 30.0, 60.0));
    }

    #[test]
    #[ignore = "probes the real host"]
    fn real_host_resolution() {
        let caps = probe(&Roots::host(), &Env::from_process()).host_caps();
        let r = ThresholdPolicy::default().effective_config(&caps);
        for w in &r.why {
            eprintln!("{w}");
        }
        eprintln!("{}", serde_json::to_string_pretty(&r).unwrap());
    }
}

#[test]
fn an_app_you_just_left_stays_warm_then_cools_off() {
    let mut p = ThresholdPolicy::default();
    let both = |now: i64| {
        let mut s = Snap::at(now).unit("browser.scope", QosClass::Normal).unit("editor.scope", QosClass::Normal);
        s.s.caps.hardware.mem_total = 64 << 30;
        s
    };
    // The browser has focus and uses 700 MiB.
    let mut s = both(1000).fg("browser.scope", 0.0).focus(1000, "browser.scope").build();
    usage_mem(&mut s, "browser.scope", 700 << 20);
    p.evaluate(&s).unwrap();

    // You switch to the editor: the browser keeps a raised share and its memory protected,
    // rounded up to the 64 MiB step, so coming back doesn't wait for a page-in.
    let mut s = both(10_000).fg("editor.scope", 0.0).focus(10_000, "editor.scope").build();
    usage_mem(&mut s, "browser.scope", 700 << 20);
    let out = p.evaluate(&s).unwrap();
    assert_eq!(get(&out, "browser.scope", Knob::CpuWeight), Some(Setting::CpuWeight { value: 150 }));
    assert_eq!(get(&out, "browser.scope", Knob::IoWeight), Some(Setting::IoWeight { value: 150 }));
    assert_eq!(get(&out, "browser.scope", Knob::MemoryLow), Some(Setting::MemoryLow { bytes: 704 << 20 }));
    assert_eq!(get(&out, "browser.scope", Knob::UclampMin), None);
    let reason = &out.intents.iter().find(|i| i.target == unit("browser.scope")).unwrap().reason;
    assert!(reason.starts_with("warm: in front 9s ago"), "{reason}");

    // A few megabytes of growth don't rewrite memory.low.
    let mut s = both(12_000).fg("editor.scope", 0.0).build();
    usage_mem(&mut s, "browser.scope", 703 << 20);
    let out = p.evaluate(&s).unwrap();
    assert_eq!(get(&out, "browser.scope", Knob::MemoryLow), Some(Setting::MemoryLow { bytes: 704 << 20 }));

    // Half an hour later it has cooled off: nothing is held for it any more.
    let mut s = both(10_000 + 31 * 60_000).fg("editor.scope", 0.0).build();
    usage_mem(&mut s, "browser.scope", 700 << 20);
    let out = p.evaluate(&s).unwrap();
    assert!(knobs(&out, "browser.scope").is_empty(), "{:?}", knobs(&out, "browser.scope"));
}

#[test]
fn warm_respects_class_budget_and_the_off_switch() {
    // Background work that once had focus (a build's terminal tab) still yields.
    let mut p = ThresholdPolicy::default();
    let mk = |now: i64, fg: &str| {
        let mut s = Snap::at(now)
            .unit("tab.scope", QosClass::Background)
            .unit("big.scope", QosClass::Normal)
            .unit("editor.scope", QosClass::Normal);
        s.s.caps.hardware.mem_total = 4 << 30;
        s.fg(fg, 0.0)
    };
    p.evaluate(&mk(1000, "tab.scope").focus(1000, "tab.scope").build()).unwrap();
    p.evaluate(&mk(2000, "big.scope").focus(2000, "big.scope").build()).unwrap();
    let mut s = mk(9000, "editor.scope").focus(9000, "editor.scope").build();
    usage_mem(&mut s, "big.scope", 3 << 30);
    let out = p.evaluate(&s).unwrap();
    assert_eq!(
        get(&out, "tab.scope", Knob::CpuWeight),
        Some(Setting::CpuWeight { value: 50 }),
        "background stays background"
    );
    // 1 GiB (the per-app cap) is exactly the 25%-of-4GiB budget: granted.
    assert_eq!(get(&out, "big.scope", Knob::MemoryLow), Some(Setting::MemoryLow { bytes: 1 << 30 }));

    // warm_for = 0 turns the whole thing off.
    let mut p = ThresholdPolicy::new(ThresholdConfig { warm_for: Duration::ZERO, ..ThresholdConfig::default() });
    p.evaluate(&mk(1000, "big.scope").focus(1000, "big.scope").build()).unwrap();
    let out = p.evaluate(&mk(9000, "editor.scope").focus(9000, "editor.scope").build()).unwrap();
    assert!(knobs(&out, "big.scope").is_empty());
}

/// Give a unit a memory footprint in a built snapshot.
fn usage_mem(s: &mut Snapshot, name: &str, bytes: u64) {
    let u = s.units.iter_mut().find(|u| u.target == unit(name)).expect("unit in the snapshot");
    u.usage = Some(Usage { mem_current: bytes, interval_ms: 1000, ..Default::default() });
}
