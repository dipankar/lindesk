//! The session agent's part of the engine loop: mode switching, the broker relay and the
//! built-in adapters.

use lindesk_builtins::policy::{RelayFeed, RelayedSetting};
use lindesk_core::clock::now_ms;
use lindesk_core::config::Config;
use lindesk_host::{Engine, StepReport};
use lindesk_ipc::SessionState;
use lindesk_plugin_api::{Adapter, Level, Privilege, Target, TimestampMs};
use serde_json::{Value, json};
use std::path::PathBuf;

use crate::common::calibration::Calibrator;
use crate::common::engine_thread::{Hooks, LoopCx, ProfilesView};
use crate::common::observe::PolicyMetrics;
use crate::common::profiles::Profiles;
use crate::common::setup;
use crate::common::startup::HardwareWatch;
use crate::gate::Gate;
use crate::mode::{self, Mode};

/// Session-specific engine commands.
#[derive(Debug)]
pub enum SessionExt {
    /// Switch mode; `state` is the `SessionState` JSON returned by `RegisterSession`.
    SetMode { mode: Mode, state: Option<String> },
    /// A `UserState` signal for this user.
    BrokerState(String),
    /// Something optional is unavailable (shown in status `daemon.degraded`).
    Degraded(String),
}

/// What the broker last told us.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BrokerView {
    pub level: Level,
    pub desired: usize,
    pub dry_run: bool,
    pub updated: TimestampMs,
    /// The broker's status `daemon.profile` (the profile in effect while connected).
    pub profile: Value,
}

/// Turn a `SessionState` JSON into relay settings for `uid`. Settings for other users are
/// dropped. Returns (level, settings, broker dry-run).
pub fn parse_state(uid: u32, json: &str) -> Result<(Level, Vec<RelayedSetting>, bool), String> {
    let v: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    parse_state_value(uid, v)
}

fn parse_state_value(uid: u32, v: Value) -> Result<(Level, Vec<RelayedSetting>, bool), String> {
    let dry = v.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
    let st: SessionState = serde_json::from_value(v).map_err(|e| e.to_string())?;
    if st.uid != uid {
        return Err(format!("state for uid {} (we are {uid})", st.uid));
    }
    let settings = st
        .desired
        .into_iter()
        .filter(|d| d.target.user() == Some(uid))
        .map(|d| RelayedSetting { target: d.target, setting: d.setting, provenance: d.provenance })
        .collect();
    Ok((st.level, settings, dry))
}

pub struct SessionHooks {
    pub uid: u32,
    pub feed: RelayFeed,
    pub threshold: Gate,
    pub psi: Gate,
    pub metrics: PolicyMetrics,
    pub forced_standalone: bool,
    pub adapters: Vec<Box<dyn Adapter>>,
    pub config_path: PathBuf,
    /// Prepares a freshly loaded config like at start-up (CLI overrides, defaults).
    pub prepare: Box<dyn Fn(&mut Config) + Send>,
    /// Profile selection; `None` = no profile overlay.
    pub profiles: Option<Profiles>,
    pub hardware: Option<HardwareWatch>,
    pub requirements: Vec<Value>,
    /// Why the agent runs with less than everything (status `daemon.degraded`).
    pub degraded: Vec<String>,
    /// Where the vendor rules come from (`system`, `embedded`, `configured`).
    pub rules_source: &'static str,
    /// First-run quick calibration (standalone mode only).
    /// Where a pause is persisted (`lindeskctl pause`).
    pub pause_file: Option<PathBuf>,
    /// Re-checks the requirement warnings (maintenance tick, after a versioned change).
    pub requirements_watch: Option<crate::common::startup::RequirementsWatch>,
    /// A focus event arrived (from a built-in adapter or over D-Bus from the GNOME
    /// extension / KWin script): foreground tracking works.
    pub focus_seen: bool,
    pub calibrator: Option<Calibrator>,
    mode: Mode,
    broker: Option<BrokerView>,
    warned_dry: bool,
}

impl SessionHooks {
    /// `active`, or why there is no focus information yet: on GNOME/KDE focus comes from
    /// the shell extension / KWin script, which a running Wayland shell only loads after a
    /// login.
    pub fn foreground_tracking(&self) -> String {
        let focus = |a: &dyn Adapter| lindesk_builtins::adapters::reports_focus(&a.info().name);
        if self.focus_seen || self.adapters.iter().any(|a| focus(a.as_ref()) && a.connected()) {
            return "active".into();
        }
        match crate::desktop::desktop_of(std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref()) {
            crate::desktop::Desktop::Gnome => "waiting for GNOME extension (log out and in once)".into(),
            crate::desktop::Desktop::Kde => {
                "waiting for the KWin script (enable it in System Settings > KWin Scripts, or log out and in)".into()
            }
            // A built-in adapter (Hyprland, COSMIC, X11) that has not reached its compositor.
            crate::desktop::Desktop::Other if self.adapters.iter().any(|a| focus(a.as_ref())) => {
                "waiting for the compositor".into()
            }
            crate::desktop::Desktop::Other => "no foreground adapter for this desktop".into(),
        }
    }

    pub fn new(uid: u32, feed: RelayFeed, threshold: Gate, psi: Gate) -> Self {
        SessionHooks {
            uid,
            feed,
            threshold,
            psi,
            metrics: PolicyMetrics::default(),
            forced_standalone: false,
            adapters: Vec::new(),
            config_path: PathBuf::new(),
            prepare: Box::new(|_| {}),
            profiles: None,
            hardware: None,
            requirements: Vec::new(),
            degraded: Vec::new(),
            rules_source: "system",
            pause_file: None,
            requirements_watch: None,
            focus_seen: false,
            calibrator: None,
            mode: Mode::Starting,
            broker: None,
            warned_dry: false,
        }
    }

    pub fn mode_name(&self) -> &'static str {
        self.mode.as_str()
    }

    /// Switch mode now (also used before the engine thread starts).
    pub fn set_mode(&mut self, mode: Mode, state: Option<&str>, engine_dry_run: bool) {
        let e = mode::effects(mode);
        if mode != self.mode {
            tracing::info!("mode: {} -> {}", self.mode.as_str(), mode.as_str());
        }
        self.mode = mode;
        self.threshold.set(e.threshold);
        self.psi.set(e.psi);
        if !e.threshold {
            self.metrics.clear();
        }
        // Connected, the broker's profile is the one in effect; ours resumes when standalone.
        if let Some(p) = self.profiles.as_mut() {
            p.frozen = mode == Mode::Connected;
        }
        if e.relay {
            match state {
                Some(s) => self.apply_state(s, engine_dry_run),
                None if !self.feed.is_active() => self.feed.set(Level::Green, Vec::new()),
                None => {}
            }
        } else {
            self.feed.clear();
            self.broker = None;
        }
    }

    /// Feed a `SessionState` into the relay (connected mode only).
    pub fn apply_state(&mut self, json: &str, engine_dry_run: bool) {
        if self.mode != Mode::Connected {
            return;
        }
        let profile = serde_json::from_str::<Value>(json)
            .ok()
            .and_then(|mut v| v.get_mut("profile").map(Value::take))
            .unwrap_or(Value::Null);
        match parse_state(self.uid, json) {
            Ok((level, settings, broker_dry)) => {
                let n = settings.len();
                // An observing broker must not make a real agent change anything.
                let settings = if broker_dry && !engine_dry_run {
                    if !self.warned_dry {
                        tracing::warn!("the broker runs in dry-run mode; not applying its settings");
                        self.warned_dry = true;
                    }
                    Vec::new()
                } else {
                    settings
                };
                self.feed.set(level, settings);
                self.broker = Some(BrokerView { level, desired: n, dry_run: broker_dry, updated: now_ms(), profile });
            }
            Err(e) => tracing::warn!("ignoring broker state: {e}"),
        }
    }

    /// The broker's active profile while connected.
    fn broker_profile(&self) -> Option<&Value> {
        let b = self.broker.as_ref().filter(|_| self.mode == Mode::Connected)?;
        (!b.profile.is_null()).then_some(&b.profile)
    }

    /// Status `daemon.profile`: ours, or the broker's while connected (`"source": "broker"`).
    fn profile_status(&self) -> Value {
        let own = self.profiles.as_ref().map_or(Value::Null, Profiles::status);
        match self.broker_profile() {
            Some(b) => {
                let mut v = b.clone();
                if let Some(o) = v.as_object_mut() {
                    o.insert("source".into(), json!("broker"));
                    o.insert("local".into(), own);
                }
                v
            }
            None => {
                let mut v = own;
                if let Some(o) = v.as_object_mut() {
                    o.insert("source".into(), json!("local"));
                }
                v
            }
        }
    }

    fn epoch_from_feed(&self) -> Option<bool> {
        let (_, settings) = self.feed.get()?;
        Some(settings.iter().any(|s| s.provenance.reason.contains("interaction epoch")))
    }
}

impl Hooks for SessionHooks {
    type Ext = SessionExt;

    fn status_extra(&self, _engine: &Engine) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        m.insert("mode".into(), json!(self.mode.as_str()));
        m.insert("forced_standalone".into(), json!(self.forced_standalone));
        m.insert("degraded".into(), json!(self.degraded));
        m.insert("rules".into(), json!(self.rules_source));
        m.insert("profile".into(), self.profile_status());
        m.insert(
            "broker".into(),
            match &self.broker {
                Some(b) => json!({
                    "connected": self.mode == Mode::Connected,
                    "level": b.level,
                    "desired": b.desired,
                    "dry_run": b.dry_run,
                    "updated": b.updated,
                }),
                None => json!({ "connected": false }),
            },
        );
        let adapters: Vec<Value> =
            self.adapters.iter().map(|a| json!({ "name": a.info().name, "connected": a.connected() })).collect();
        m.insert("adapters".into(), Value::Array(adapters));
        m.insert("foreground_tracking".into(), json!(self.foreground_tracking()));
        m
    }

    fn epoch(&self, _engine: &Engine) -> (Option<bool>, &'static str) {
        match self.mode {
            Mode::Connected => (self.epoch_from_feed(), "broker"),
            Mode::Standalone => (self.metrics.epoch_active(), "threshold"),
            Mode::Starting => (None, "none"),
        }
    }

    fn profiles(&self, engine: &Engine) -> ProfilesView {
        let broker_active = self.broker_profile().and_then(|p| p["active"].as_str());
        self.profiles
            .as_ref()
            .map(|p| p.view(engine, broker_active))
            .unwrap_or_else(|| ProfilesView { names: Vec::new(), list: json!([]) })
    }

    fn unit_candidates(&self, _engine: &Engine, name: &str) -> Vec<Target> {
        vec![Target::user_unit(self.uid, name)]
    }

    fn ext(&mut self, engine: &mut Engine, ext: SessionExt, cx: &mut LoopCx<'_>) {
        match ext {
            SessionExt::SetMode { mode, state } => self.set_mode(mode, state.as_deref(), engine.dry_run()),
            SessionExt::BrokerState(json) => self.apply_state(&json, engine.dry_run()),
            SessionExt::Degraded(why) => {
                tracing::warn!("degraded: {why}");
                self.degraded.push(why);
            }
        }
        cx.want_step = true;
    }

    fn after_step(&mut self, engine: &mut Engine, report: &StepReport, _cx: &mut LoopCx<'_>) {
        if crate::common::startup::has_change_notice(&report.events)
            && let Some(w) = self.requirements_watch.as_mut()
        {
            self.requirements = w.refresh();
        }
        if !self.focus_seen && report.events.iter().any(|e| matches!(e, lindesk_plugin_api::Event::Focus { .. })) {
            self.focus_seen = true;
        }
        if let Some(p) = self.profiles.as_mut() {
            p.tick(engine, report.now);
        }
        if self.mode == Mode::Standalone && !engine.dry_run() {
            let epoch = self.metrics.epoch_active();
            if let Some(c) = self.calibrator.as_mut() {
                c.tick(engine, report, epoch);
            }
        }
    }

    fn pause_file(&self) -> Option<PathBuf> {
        self.pause_file.clone()
    }

    fn requirements(&self) -> Value {
        Value::Array(self.requirements.clone())
    }

    fn maintenance(&mut self, engine: &mut Engine) {
        if let Some(h) = &self.hardware {
            h.refresh(engine);
        }
        if let Some(w) = self.requirements_watch.as_mut() {
            self.requirements = w.refresh();
        }
    }

    fn requirements_checked(&self) -> Option<TimestampMs> {
        self.requirements_watch.as_ref().map(|w| w.checked_ms)
    }

    fn reload(&mut self, engine: &mut Engine) -> Result<String, String> {
        let profiles = &mut self.profiles;
        let mut apply = |engine: &mut Engine, cfg: Config| match profiles.as_mut() {
            Some(p) => p.rebase(engine, cfg, now_ms()),
            None => engine.reload_config(cfg).map_err(|e| e.to_string()),
        };
        setup::reload(engine, &self.config_path, Privilege::User, &*self.prepare, &mut apply)
    }

    fn shutdown(&mut self, _engine: &mut Engine) {
        for a in &mut self.adapters {
            a.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use lindesk_ipc::DesiredSetting;
    use lindesk_plugin_api::{Provenance, Setting};

    use super::*;

    fn state(uid: u32, dry: bool) -> String {
        let s = SessionState {
            uid,
            level: Level::Yellow,
            desired: vec![
                DesiredSetting {
                    target: Target::user_unit(uid, "app-x.scope"),
                    setting: Setting::CpuWeight { value: 300 },
                    provenance: Provenance {
                        plugin: "threshold".into(),
                        reason: "foreground: interaction epoch boost".into(),
                        ..Default::default()
                    },
                },
                DesiredSetting {
                    target: Target::system_unit("evil.service"),
                    setting: Setting::CpuWeight { value: 1 },
                    provenance: Provenance::default(),
                },
            ],
        };
        let mut v = serde_json::to_value(s).unwrap();
        v["dry_run"] = json!(dry);
        v.to_string()
    }

    fn hooks() -> SessionHooks {
        SessionHooks::new(1000, RelayFeed::new(), Gate::new(false), Gate::new(false))
    }

    #[test]
    fn parse_filters_foreign_targets() {
        let (level, s, dry) = parse_state(1000, &state(1000, false)).unwrap();
        assert_eq!((level, s.len(), dry), (Level::Yellow, 1, false));
        assert!(parse_state(1001, &state(1000, false)).is_err());
        assert!(parse_state(1000, "nope").is_err());
    }

    #[test]
    fn mode_switches_flip_gates_and_feed() {
        let mut h = hooks();
        h.set_mode(Mode::Connected, Some(&state(1000, false)), false);
        assert!(!h.threshold.is_open() && !h.psi.is_open());
        assert_eq!(h.feed.get().unwrap().1.len(), 1);
        assert_eq!(h.epoch_from_feed(), Some(true));
        h.apply_state(&state(1000, false).replace("interaction epoch", "mild"), false);
        assert_eq!(h.epoch_from_feed(), Some(false));

        h.set_mode(Mode::Standalone, None, false);
        assert!(h.threshold.is_open() && h.psi.is_open());
        assert!(!h.feed.is_active(), "relayed settings dropped (the engine reverts them)");
        h.apply_state(&state(1000, false), false);
        assert!(!h.feed.is_active(), "states are ignored while standalone");

        h.set_mode(Mode::Connected, None, false);
        assert_eq!(h.feed.get(), Some((Level::Green, vec![])), "connected without a state yet");
    }

    #[test]
    fn connected_shows_the_brokers_profile() {
        let mut h = hooks();
        h.profiles = Some(Profiles::load(lindesk_core::config::Config::default(), &[]));
        assert_eq!(h.profile_status()["source"], "local");
        let mut v: Value = serde_json::from_str(&state(1000, false)).unwrap();
        v["profile"] = json!({"active": "gaming", "pinned": false, "reason": "steam", "matched": ["gaming"]});
        h.set_mode(Mode::Connected, Some(&v.to_string()), false);
        assert!(h.profiles.as_ref().unwrap().frozen);
        let st = h.profile_status();
        assert_eq!((st["active"].as_str(), st["source"].as_str()), (Some("gaming"), Some("broker")));
        assert!(st["local"].is_object());
        h.set_mode(Mode::Standalone, None, false);
        assert!(!h.profiles.as_ref().unwrap().frozen);
        assert_eq!(h.profile_status()["source"], "local");
    }

    #[test]
    fn dry_run_broker_is_not_applied_by_a_real_agent() {
        let mut h = hooks();
        h.set_mode(Mode::Connected, Some(&state(1000, true)), false);
        assert_eq!(h.feed.get().unwrap().1.len(), 0);
        assert_eq!(h.broker.as_ref().unwrap().desired, 1);
        // A dry-run agent relays (nothing is applied anyway).
        h.set_mode(Mode::Connected, Some(&state(1000, true)), true);
        assert_eq!(h.feed.get().unwrap().1.len(), 1);
    }
}
