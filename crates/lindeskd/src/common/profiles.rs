//! Profile selection for a daemon: `[general] profile = "auto"` re-evaluated every step
//! (power source, hardware, foreground app) with the selector's minimum dwell; a pinned
//! profile (`[general] profile = "<name>"`) skips auto-selection. A switch recomputes
//! `base ⊕ profile chain` and reloads the engine (built-in plugins whose config changed are
//! re-initialised), emitting `Notice { name: "profile.changed" }` into the engine's events
//! (so it reaches `Events`, the lake and `lindeskctl events`).

use std::path::PathBuf;

use lindesk_core::cgroup_path::app_id_from_unit;
use lindesk_core::config::Config;
use lindesk_core::profile::{DEFAULT_PROFILE, ProfileContext, ProfileSelector, ProfileSet, ProfileSwitch};
use lindesk_host::Engine;
use lindesk_plugin_api::{Hardware, Target, TimestampMs};
use serde_json::{Value, json};

use super::engine_thread::ProfilesView;

pub struct Profiles {
    set: ProfileSet,
    selector: ProfileSelector,
    /// The config as loaded and prepared, before any profile overlay.
    base: Config,
    /// Where the profiles come from (reloaded with the config).
    dirs: Vec<PathBuf>,
    /// Every profile whose `[match]` holds for the latest context, best first.
    matched: Vec<String>,
    last_ctx: Option<ProfileContext>,
    /// Auto-selection paused: a connected session agent follows the broker's profile.
    pub frozen: bool,
}

fn load_set(dirs: &[PathBuf]) -> ProfileSet {
    let (set, errors) = ProfileSet::load(dirs);
    for e in errors {
        tracing::warn!("profiles: {e}");
    }
    set
}

impl Profiles {
    /// Load the built-ins plus `dirs` (`lindesk_core::profile::default_dirs`: vendor, `/etc`,
    /// and for a user agent the user's).
    pub fn load(base: Config, dirs: &[PathBuf]) -> Self {
        Profiles {
            set: load_set(dirs),
            selector: ProfileSelector::default(),
            base,
            dirs: dirs.to_vec(),
            matched: Vec::new(),
            last_ctx: None,
            frozen: false,
        }
    }

    fn ctx(hw: &Hardware, fg: Option<&Target>) -> ProfileContext {
        let app = fg.and_then(|t| match t {
            Target::Unit { name, .. } => app_id_from_unit(name),
            _ => None,
        });
        ProfileContext::from_hardware(hw, app.as_deref())
    }

    fn engine_ctx(engine: &Engine) -> ProfileContext {
        Self::ctx(&engine.options().caps.hardware, engine.foreground())
    }

    fn observe(&mut self, ctx: &ProfileContext) {
        if self.last_ctx.as_ref() != Some(ctx) {
            self.matched = self.set.select(ctx).matched;
            self.last_ctx = Some(ctx.clone());
        }
    }

    fn effective(&self, name: &str) -> Config {
        match self.set.effective_config(&self.base, name) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("profile {name}: {e}; running without a profile overlay");
                self.base.clone()
            }
        }
    }

    /// The effective config to start the engine with (call once, before `Engine::new`).
    pub fn initial(&mut self, hw: &Hardware, now: TimestampMs) -> Config {
        let ctx = Self::ctx(hw, None);
        self.observe(&ctx);
        let general = self.base.general.profile.clone();
        let switch = self.selector.update(&self.set, &general, &ctx, now);
        let name = switch.map(|s| s.to).unwrap_or_else(|| self.active().to_string());
        self.effective(&name)
    }

    /// Apply `switch` to the engine and announce it. Returns false when the engine refused
    /// the new config.
    fn switch(&self, engine: &mut Engine, switch: &ProfileSwitch, now: TimestampMs) -> bool {
        let cfg = match self.set.effective_config(&self.base, &switch.to) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("profile {}: {e}", switch.to);
                return false;
            }
        };
        match engine.reload_config(cfg) {
            Ok(()) => {
                tracing::info!(
                    "profile {} -> {} ({})",
                    switch.from.as_deref().unwrap_or("none"),
                    switch.to,
                    switch.reason
                );
                engine.push_event(switch.notice(now));
                true
            }
            Err(e) => {
                tracing::warn!("profile {}: {e}", switch.to);
                false
            }
        }
    }

    /// Re-evaluate (every step); on a switch reload the engine. Returns the switch.
    pub fn tick(&mut self, engine: &mut Engine, now: TimestampMs) -> Option<ProfileSwitch> {
        if self.frozen {
            return None;
        }
        let ctx = Self::engine_ctx(engine);
        self.observe(&ctx);
        let general = self.base.general.profile.clone();
        let switch = self.selector.update(&self.set, &general, &ctx, now)?;
        self.switch(engine, &switch, now).then_some(switch)
    }

    /// A new base config (reload, or a versioned change to the config file): re-read the
    /// profile files, re-run the selection right away (a pin or unpin switches immediately)
    /// and apply `base ⊕ active profile`.
    pub fn rebase(&mut self, engine: &mut Engine, base: Config, now: TimestampMs) -> Result<(), String> {
        self.base = base;
        self.set = load_set(&self.dirs);
        self.last_ctx = None;
        let ctx = Self::engine_ctx(engine);
        self.observe(&ctx);
        if !self.frozen {
            let general = self.base.general.profile.clone();
            if let Some(switch) = self.selector.update(&self.set, &general, &ctx, now) {
                // `switch` reloads the engine with the new profile.
                if self.switch(engine, &switch, now) {
                    return Ok(());
                }
            }
        }
        let cfg = self.effective(self.active());
        engine.reload_config(cfg).map_err(|e| e.to_string())
    }

    pub fn active(&self) -> &str {
        self.selector.active().unwrap_or(DEFAULT_PROFILE)
    }

    pub fn names(&self) -> Vec<String> {
        self.set.names()
    }

    /// `ListProfiles` rows; `active` overrides which one is marked active (a connected
    /// session shows the broker's).
    pub fn view(&self, engine: &Engine, active: Option<&str>) -> ProfilesView {
        let ctx = Self::engine_ctx(engine);
        let list = self.set.list(&ctx, Some(active.unwrap_or(self.active())));
        ProfilesView { names: self.names(), list: serde_json::to_value(list).unwrap_or(Value::Array(Vec::new())) }
    }

    /// Status `daemon.profile`: `{active, setting, pinned, reason, matched}`.
    pub fn status(&self) -> Value {
        json!({
            "active": self.active(),
            "setting": self.base.general.profile,
            "pinned": self.selector.is_pinned(),
            "reason": self.selector.reason(),
            "matched": self.matched,
        })
    }
}

#[cfg(test)]
mod tests {
    use lindesk_host::{EngineOptions, MemoryEnvironment};
    use lindesk_plugin_api::{Event, Privilege};

    use super::*;

    fn engine(config: Config) -> Engine {
        Engine::new(config, Box::new(MemoryEnvironment::new()), EngineOptions::new("t", Privilege::User)).unwrap()
    }

    fn notices(engine: &mut Engine, now: TimestampMs) -> Vec<Value> {
        engine
            .step(now)
            .events
            .into_iter()
            .filter_map(|e| match e {
                Event::Notice { name, data, .. } if name == "profile.changed" => Some(data),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn pinning_by_rebase_switches_now_and_reloads() {
        let base = Config::default();
        let mut p = Profiles::load(base.clone(), &[]);
        let cfg = p.initial(&Hardware::default(), 0);
        let mut e = engine(cfg);
        let auto = p.active().to_string();
        assert_eq!(p.status()["pinned"], false);
        assert!(p.status()["matched"].is_array());

        // Pinned with `general.profile` (what a SetProfile changeset writes).
        let mut pinned = base.clone();
        pinned.general.profile = "battery".into();
        p.rebase(&mut e, pinned, 1).unwrap();
        assert_eq!(p.active(), "battery");
        let st = p.status();
        assert_eq!((st["active"].as_str(), st["pinned"].as_bool()), (Some("battery"), Some(true)));
        let n = notices(&mut e, 2);
        assert_eq!(n.len(), 1, "one profile.changed notice");
        assert_eq!((n[0]["from"].as_str(), n[0]["to"].as_str()), (Some(auto.as_str()), Some("battery")));
        // A pinned profile ignores auto-selection.
        assert!(p.tick(&mut e, 60_000).is_none());

        // Back to auto.
        p.rebase(&mut e, base, 3).unwrap();
        assert_eq!(p.active(), auto);
        assert_eq!(p.status()["pinned"], false);
        let v = p.view(&e, None);
        assert!(v.names.iter().any(|n| n == "battery"));
        let active: Vec<_> =
            v.list.as_array().unwrap().iter().filter(|r| r["active"] == true).map(|r| r["name"].clone()).collect();
        assert_eq!(active, vec![Value::from(auto)]);
    }

    #[test]
    fn frozen_profiles_do_not_switch() {
        let mut p = Profiles::load(Config::default(), &[]);
        let mut e = engine(p.initial(&Hardware::default(), 0));
        p.frozen = true;
        let mut pinned = Config::default();
        pinned.general.profile = "latency".into();
        p.rebase(&mut e, pinned, 1).unwrap();
        assert_ne!(p.active(), "latency", "a frozen selector keeps its profile");
        assert!(p.tick(&mut e, 60_000).is_none());
    }
}
