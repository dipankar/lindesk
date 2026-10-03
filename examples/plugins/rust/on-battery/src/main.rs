//! on-battery: a lindesk policy *modifier*.
//!
//! The built-in `threshold` policy decides how hard background work yields. This plugin
//! runs next to it and, only while the laptop runs on battery, returns intents for a few
//! knobs at a higher priority, so it wins exactly those knobs:
//!
//! * background units: a lower `cpu.weight`;
//! * batch units: `cpu.uclamp.max` (50% by default), which caps their CPU frequency;
//! * the foreground: a shorter interaction boost (200 ms instead of threshold's 500 ms).
//!
//! Its values are stricter than threshold's at every level, so winning a knob never
//! loosens it. On AC power it returns nothing and threshold is in sole charge.
//!
//! * `cargo test -p on-battery`: unit tests through the JSON protocol (`testing::Harness`)
//!   plus conformance checks of every example plugin
//! * `[config] sysfs_root`: read power supplies from a fixture instead of `/sys`

mod power;

use std::path::PathBuf;

use lindesk_plugin::prelude::*;
use serde::Deserialize;

use crate::power::Power;

/// The `[config]` table from plugin.toml, merged with `[plugins.on-battery] config` from
/// lindesk.toml. Unknown keys are rejected so typos fail loudly at startup.
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
struct Config {
    sysfs_root: PathBuf,
    poll_ms: i64,
    background_cpu_weight: u64,
    background_cpu_weight_pressure: u64,
    batch_uclamp_max: f32,
    batch_uclamp_max_pressure: f32,
    epoch_ms: i64,
    threshold_epoch_ms: i64,
    fg_weight: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            sysfs_root: PathBuf::from("/sys"),
            poll_ms: 5000,
            background_cpu_weight: 25,
            background_cpu_weight_pressure: 10,
            batch_uclamp_max: 50.0,
            batch_uclamp_max_pressure: 30.0,
            epoch_ms: 200,
            threshold_epoch_ms: 500,
            fg_weight: 200,
        }
    }
}

impl Config {
    fn validate(&self) -> Result<(), String> {
        for (k, v) in [
            ("background_cpu_weight", self.background_cpu_weight),
            ("background_cpu_weight_pressure", self.background_cpu_weight_pressure),
            ("fg_weight", self.fg_weight),
        ] {
            if !(1..=10_000).contains(&v) {
                return Err(format!("{k} = {v} is outside 1..=10000"));
            }
        }
        for (k, v) in
            [("batch_uclamp_max", self.batch_uclamp_max), ("batch_uclamp_max_pressure", self.batch_uclamp_max_pressure)]
        {
            if !(0.0..=100.0).contains(&v) {
                return Err(format!("{k} = {v} is outside 0..=100 (percent)"));
            }
        }
        if self.poll_ms < 0 || self.epoch_ms < 0 || self.epoch_ms > self.threshold_epoch_ms {
            return Err("need 0 <= epoch_ms <= threshold_epoch_ms and poll_ms >= 0".into());
        }
        Ok(())
    }
}

#[derive(Default)]
struct OnBattery {
    cfg: Config,
    notifier: Option<Notifier>,
    /// On a user host (`lindesk-session`), the session user: only their units are ours.
    user_host: Option<u32>,
    power: Option<Power>,
    read_at: Option<TimestampMs>,
    /// The pipeline's level, followed through `level_changed` events (on-battery has no
    /// level of its own).
    level: Level,
    /// The newest focus/input event: when the current foreground boost started.
    last_input: Option<(Target, TimestampMs)>,
}

impl Plugin for OnBattery {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: "on-battery".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Policy],
            description: env!("CARGO_PKG_DESCRIPTION").into(),
        }
    }

    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
        let cfg: Config = if ctx.config.is_null() {
            Config::default()
        } else {
            lindesk_plugin::serde_json::from_value(ctx.config.clone())
                .map_err(|e| PluginError::Failed(format!("[config]: {e}")))?
        };
        cfg.validate().map_err(|e| PluginError::Failed(format!("[config]: {e}")))?;
        *self = OnBattery { cfg, notifier: self.notifier.take(), ..Default::default() };
        if ctx.privilege == Privilege::User {
            self.user_host = ctx.uid;
        }
        Ok(())
    }
}

impl OnBattery {
    fn log(&self, msg: String) {
        if let Some(n) = &self.notifier {
            n.info(msg);
        }
    }

    /// Re-read sysfs at most every `poll_ms` (always when time goes backwards, as it can
    /// under `lindeskctl simulate`).
    fn refresh_power(&mut self, now: TimestampMs) -> Power {
        let stale = self.read_at.is_none_or(|t| now < t || now - t >= self.cfg.poll_ms);
        if stale || self.power.is_none() {
            let p = power::read(&self.cfg.sysfs_root);
            if self.power.as_ref() != Some(&p) {
                let verb = if p.on_battery() { "background work now yields harder" } else { "standing by" };
                self.log(format!("power: {} ({verb})", p.describe()));
            }
            self.power = Some(p);
            self.read_at = Some(now);
        }
        self.power.clone().unwrap_or(Power::NoBattery)
    }

    /// Follow the level and the foreground's interaction epochs, like threshold does.
    fn track_events(&mut self, s: &Snapshot, fg: Option<&Target>) {
        let mut events: Vec<&Event> = s.events.iter().collect();
        events.sort_by_key(|e| e.ts());
        for e in events {
            match e {
                Event::LevelChanged { to, .. } => self.level = *to,
                Event::Focus { ts, target, .. } => {
                    if let Some(t) = target.as_ref().or(fg) {
                        self.input(t, *ts);
                    }
                }
                Event::Interaction { ts, target, end, .. } => {
                    let Some(t) = target.as_ref().or(fg) else { continue };
                    if Some(t) != fg {
                        continue;
                    }
                    if *end {
                        self.last_input = None;
                    } else {
                        self.input(t, *ts);
                    }
                }
                _ => {}
            }
        }
    }

    fn input(&mut self, t: &Target, ts: TimestampMs) {
        let ts = match &self.last_input {
            Some((prev, at)) if prev == t => ts.max(*at),
            _ => ts,
        };
        self.last_input = Some((t.clone(), ts));
    }

    /// May we target `t`? A user host only accepts its own user's units.
    fn ours(&self, t: &Target) -> bool {
        match (self.user_host, t) {
            (Some(uid), Target::Unit { .. }) => t.user() == Some(uid),
            _ => true,
        }
    }
}

impl Policy for OnBattery {
    fn evaluate(&mut self, s: &Snapshot) -> PluginResult<PolicyOutput> {
        let fg = s.foreground.clone().or_else(|| s.foreground_unit().map(|u| u.target.clone()));
        self.track_events(s, fg.as_ref());
        let power = self.refresh_power(s.now);

        let metric = |name: &str, value: f64| Metric {
            ts: s.now,
            name: format!("on_battery.{name}"),
            value,
            labels: Default::default(),
            plugin: String::new(),
        };
        let mut metrics = vec![metric("active", if power.on_battery() { 1.0 } else { 0.0 })];
        if let Power::Battery { capacity: Some(c), .. } = &power {
            metrics.push(metric("capacity", f64::from(*c)));
        }
        if !power.on_battery() {
            // Policies return their *complete* intent set: nothing here means the host
            // reverts whatever we asked for while on battery.
            return Ok(PolicyOutput { intents: vec![], level: None, metrics });
        }

        let cfg = &self.cfg;
        let pressure = self.level >= Level::Yellow;
        let why = format!("on battery ({}{})", power.describe(), if pressure { ", under pressure" } else { "" });
        let mut intents = Vec::new();
        let mut want = |target: &Target, setting: Setting, reason: String| {
            // priority 0 = the [policy] priority from plugin.toml
            intents.push(Intent { target: target.clone(), setting, priority: 0, reason });
        };

        for u in s.units.iter().filter(|u| self.ours(&u.target)) {
            if u.foreground || Some(&u.target) == fg.as_ref() {
                continue; // never throttle what the user is looking at
            }
            match u.class {
                QosClass::Background => {
                    let value = if pressure { cfg.background_cpu_weight_pressure } else { cfg.background_cpu_weight };
                    want(&u.target, Setting::CpuWeight { value }, format!("{why}: background yields harder"));
                }
                QosClass::Batch if s.caps.uclamp => {
                    let percent = if pressure { cfg.batch_uclamp_max_pressure } else { cfg.batch_uclamp_max };
                    want(&u.target, Setting::UclampMax { percent }, format!("{why}: batch runs at reduced clocks"));
                }
                _ => {}
            }
        }

        // Shorter boosts: once our epoch is over but threshold's isn't, put the foreground
        // back on threshold's no-input values.
        if let (Some(fg), Some((t, at))) = (&fg, &self.last_input) {
            let since = s.now - at;
            let critical = s.units.iter().any(|u| &u.target == fg && u.class == QosClass::Critical);
            if t == fg && !critical && self.ours(fg) && since >= cfg.epoch_ms && since < cfg.threshold_epoch_ms {
                let r = format!("{why}: interaction boost ends after {}ms", cfg.epoch_ms);
                want(fg, Setting::CpuWeight { value: cfg.fg_weight }, r.clone());
                want(fg, Setting::IoWeight { value: cfg.fg_weight }, r.clone());
                if s.caps.uclamp {
                    want(fg, Setting::UclampMin { percent: 0.0 }, r);
                }
            }
        }

        Ok(PolicyOutput { intents, level: None, metrics })
    }
}

fn main() {
    // The builder (rather than `lindesk_plugin::main!`) lets us keep a Notifier for logs;
    // under `testing::Harness` the same notifier records into the harness.
    lindesk_plugin::run(Server::new(OnBattery::default()).policy().with_notifier(|p, n| p.notifier = Some(n)));
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use lindesk_plugin::serde_json::{Value, json};
    use lindesk_plugin::testing::{Harness, snapshot, test_context};

    use super::power::fixture::{laptop, supply};
    use super::*;

    fn harness(sysfs: &Path, extra: Value) -> Harness<OnBattery> {
        let mut config = json!({ "sysfs_root": sysfs, "poll_ms": 0 });
        if let (Some(c), Some(e)) = (config.as_object_mut(), extra.as_object()) {
            c.extend(e.clone());
        }
        let mut h = Harness::new(Server::new(OnBattery::default()).policy().with_notifier(|p, n| p.notifier = Some(n)));
        h.initialize_with(test_context(config)).unwrap();
        h
    }

    fn unit(name: &str, class: QosClass) -> UnitState {
        UnitState {
            target: Target::user_unit(1000, name),
            class,
            foreground: false,
            pressure: vec![],
            usage: None,
            applied: vec![],
            top_comm: None,
        }
    }

    const FG: &str = "app-gnome-org.gnome.TextEditor-77.scope";

    /// Firefox in front, an indexer in the background, a build in batch.
    fn desk(now: TimestampMs) -> Snapshot {
        let mut s = snapshot(now);
        s.caps.uclamp = true;
        let mut fg = unit(FG, QosClass::Normal);
        fg.foreground = true;
        s.foreground = Some(fg.target.clone());
        s.units = vec![
            fg,
            unit("app-tracker-miner-fs-3-12.scope", QosClass::Background),
            unit("app-cargo-99.scope", QosClass::Batch),
            unit("app-slack-5.scope", QosClass::Normal),
        ];
        s
    }

    fn setting(out: &PolicyOutput, unit: &str, knob: Knob) -> Option<Setting> {
        out.intents
            .iter()
            .find(|i| i.target == Target::user_unit(1000, unit) && i.setting.knob() == knob)
            .map(|i| i.setting.clone())
    }

    #[test]
    fn on_ac_it_asks_for_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        laptop(tmp.path(), true);
        let mut h = harness(tmp.path(), json!({}));
        let out = h.evaluate(&desk(1_000)).unwrap();
        assert!(out.intents.is_empty());
        assert_eq!(out.level, None, "a modifier leaves the level to threshold");
        assert_eq!(out.metrics[0].name, "on_battery.active");
        assert_eq!(out.metrics[0].value, 0.0);
    }

    #[test]
    fn on_battery_background_and_batch_yield_harder() {
        let tmp = tempfile::tempdir().unwrap();
        laptop(tmp.path(), false);
        let mut h = harness(tmp.path(), json!({}));
        let out = h.evaluate(&desk(1_000)).unwrap();
        assert_eq!(
            setting(&out, "app-tracker-miner-fs-3-12.scope", Knob::CpuWeight),
            Some(Setting::CpuWeight { value: 25 })
        );
        assert_eq!(setting(&out, "app-cargo-99.scope", Knob::UclampMax), Some(Setting::UclampMax { percent: 50.0 }));
        assert_eq!(out.intents.len(), 2, "normal and foreground units are left alone: {:?}", out.intents);
        assert!(out.intents.iter().all(|i| i.priority == 0 && i.reason.contains("BAT0 64%")));
        assert!(h.logs().iter().any(|l| l.message.contains("power: BAT0 64%")));
    }

    #[test]
    fn stricter_under_pressure_and_back_when_green() {
        let tmp = tempfile::tempdir().unwrap();
        laptop(tmp.path(), false);
        let mut h = harness(tmp.path(), json!({}));
        let level = |ts, from, to| Event::LevelChanged { ts, from, to, policy: "threshold".into() };

        let mut s = desk(1_000);
        s.events.push(level(900, Level::Green, Level::Yellow));
        let out = h.evaluate(&s).unwrap();
        let bg = "app-tracker-miner-fs-3-12.scope";
        assert_eq!(setting(&out, bg, Knob::CpuWeight), Some(Setting::CpuWeight { value: 10 }));
        assert_eq!(setting(&out, "app-cargo-99.scope", Knob::UclampMax), Some(Setting::UclampMax { percent: 30.0 }));

        // The level sticks until the next level_changed event.
        let out = h.evaluate(&desk(1_250)).unwrap();
        assert_eq!(setting(&out, bg, Knob::CpuWeight), Some(Setting::CpuWeight { value: 10 }));

        let mut s = desk(1_500);
        s.events.push(level(1_400, Level::Yellow, Level::Green));
        let out = h.evaluate(&s).unwrap();
        assert_eq!(setting(&out, bg, Knob::CpuWeight), Some(Setting::CpuWeight { value: 25 }));
    }

    #[test]
    fn foreground_boost_is_shortened() {
        let tmp = tempfile::tempdir().unwrap();
        laptop(tmp.path(), false);
        let mut h = harness(tmp.path(), json!({}));
        let fg = Target::user_unit(1000, FG);

        let mut s = desk(1_100);
        s.events.push(Event::Interaction {
            ts: 1_000,
            pid: Some(77),
            target: Some(fg.clone()),
            end: false,
            source: "test".into(),
        });
        // 100ms after input: threshold's boost stands.
        let out = h.evaluate(&s).unwrap();
        assert_eq!(setting(&out, FG, Knob::CpuWeight), None);

        // 300ms: our 200ms epoch is over, threshold's 500ms one is not → mild values.
        let out = h.evaluate(&desk(1_300)).unwrap();
        assert_eq!(setting(&out, FG, Knob::CpuWeight), Some(Setting::CpuWeight { value: 200 }));
        assert_eq!(setting(&out, FG, Knob::IoWeight), Some(Setting::IoWeight { value: 200 }));
        assert_eq!(setting(&out, FG, Knob::UclampMin), Some(Setting::UclampMin { percent: 0.0 }));

        // 600ms: threshold's epoch is over too; nothing left to shorten.
        let out = h.evaluate(&desk(1_600)).unwrap();
        assert_eq!(setting(&out, FG, Knob::CpuWeight), None);
    }

    #[test]
    fn respects_caps_and_user_boundaries() {
        let tmp = tempfile::tempdir().unwrap();
        laptop(tmp.path(), false);
        let mut h = harness(tmp.path(), json!({}));
        let mut s = desk(1_000);
        s.caps.uclamp = false;
        s.units
            .push(UnitState { target: Target::system_unit("packagekit.service"), ..unit("x", QosClass::Background) });
        s.units.push(unit("app-other-1.scope", QosClass::Background));
        s.units.last_mut().unwrap().target = Target::user_unit(1001, "app-other-1.scope");
        let out = h.evaluate(&s).unwrap();
        assert!(out.intents.iter().all(|i| !matches!(i.setting, Setting::UclampMax { .. })), "no uclamp on this host");
        assert!(
            out.intents.iter().all(|i| i.target.user() == Some(1000)),
            "a user host only targets its own user's units: {:?}",
            out.intents
        );
    }

    #[test]
    fn unplugging_is_noticed_after_poll_ms() {
        let tmp = tempfile::tempdir().unwrap();
        laptop(tmp.path(), true);
        let mut h = harness(tmp.path(), json!({ "poll_ms": 5000 }));
        assert!(h.evaluate(&desk(10_000)).unwrap().intents.is_empty());
        laptop(tmp.path(), false);
        assert!(h.evaluate(&desk(12_000)).unwrap().intents.is_empty(), "cached for poll_ms");
        assert!(!h.evaluate(&desk(15_000)).unwrap().intents.is_empty());
    }

    #[test]
    fn peripheral_batteries_do_not_count() {
        let tmp = tempfile::tempdir().unwrap();
        supply(tmp.path(), "hidpp_battery_0", &[("type", "Battery"), ("status", "Discharging"), ("scope", "Device")]);
        let mut h = harness(tmp.path(), json!({}));
        assert!(h.evaluate(&desk(1_000)).unwrap().intents.is_empty());
    }

    #[test]
    fn bad_config_refuses_to_start() {
        let mut h = Harness::new(Server::new(OnBattery::default()).policy());
        let err = h.initialize_with(test_context(json!({ "batch_uclamp_max": 150.0 }))).unwrap_err();
        assert!(err.to_string().contains("batch_uclamp_max"), "{err}");
        let err = h.initialize_with(test_context(json!({ "background_weight": 5 }))).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }
}
