//! Profile selection for a daemon: `[general] profile = "auto"` re-evaluated every step
//! (power source, hardware, foreground app) with the selector's minimum dwell; a switch
//! recomputes `base ⊕ profile chain` and reloads the engine (built-in plugins whose config
//! changed are re-initialised), emitting `Notice { name: "profile.changed" }`.

use std::path::PathBuf;

use lindesk_core::cgroup_path::app_id_from_unit;
use lindesk_core::config::Config;
use lindesk_core::profile::{ProfileContext, ProfileSelector, ProfileSet, ProfileSwitch};
use lindesk_host::Engine;
use lindesk_plugin_api::{Hardware, Target, TimestampMs};
use serde_json::{Value, json};

pub struct Profiles {
    set: ProfileSet,
    selector: ProfileSelector,
    /// The config as loaded and prepared, before any profile overlay.
    base: Config,
}

impl Profiles {
    /// Load the built-ins plus `dirs` (vendor, `/etc`, user).
    pub fn load(base: Config, dirs: &[PathBuf]) -> Self {
        let (set, errors) = ProfileSet::load(dirs);
        for e in errors {
            tracing::warn!("profiles: {e}");
        }
        Profiles { set, selector: ProfileSelector::default(), base }
    }

    fn ctx(hw: &Hardware, fg: Option<&Target>) -> ProfileContext {
        let app = fg.and_then(|t| match t {
            Target::Unit { name, .. } => app_id_from_unit(name),
            _ => None,
        });
        ProfileContext::from_hardware(hw, app.as_deref())
    }

    /// The effective config to start the engine with (call once, before `Engine::new`).
    pub fn initial(&mut self, hw: &Hardware, now: TimestampMs) -> Config {
        let ctx = Self::ctx(hw, None);
        let general = self.base.general.profile.clone();
        let switch = self.selector.update(&self.set, &general, &ctx, now);
        let name = switch.map(|s| s.to).or_else(|| self.selector.active().map(str::to_string));
        match name.map(|n| self.set.effective_config(&self.base, &n)) {
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                tracing::warn!("profile: {e}; running without a profile overlay");
                self.base.clone()
            }
            None => self.base.clone(),
        }
    }

    /// Re-evaluate; on a switch reload the engine. Returns the switch.
    pub fn tick(&mut self, engine: &mut Engine, now: TimestampMs) -> Option<ProfileSwitch> {
        let ctx = Self::ctx(&engine.options().caps.hardware, engine.foreground());
        let general = self.base.general.profile.clone();
        let switch = self.selector.update(&self.set, &general, &ctx, now)?;
        match self.set.effective_config(&self.base, &switch.to) {
            Ok(cfg) => match engine.reload_config(cfg) {
                Ok(()) => {
                    tracing::info!(
                        "profile {} -> {} ({})",
                        switch.from.as_deref().unwrap_or("none"),
                        switch.to,
                        switch.reason
                    );
                    engine.push_event(switch.notice(now));
                    Some(switch)
                }
                Err(e) => {
                    tracing::warn!("profile {}: {e}", switch.to);
                    None
                }
            },
            Err(e) => {
                tracing::warn!("profile {}: {e}", switch.to);
                None
            }
        }
    }

    /// A new base config (reload): apply it with the active profile.
    pub fn rebase(&mut self, engine: &mut Engine, base: Config) -> Result<(), String> {
        self.base = base;
        let name = self.selector.active().unwrap_or(lindesk_core::profile::DEFAULT_PROFILE).to_string();
        let cfg = self.set.effective_config(&self.base, &name).unwrap_or_else(|_| self.base.clone());
        engine.reload_config(cfg).map_err(|e| e.to_string())
    }

    pub fn active(&self) -> &str {
        self.selector.active().unwrap_or(lindesk_core::profile::DEFAULT_PROFILE)
    }

    /// Status `daemon.profile`.
    pub fn status(&self) -> Value {
        json!({
            "active": self.active(),
            "setting": self.base.general.profile,
            "pinned": self.selector.is_pinned(),
            "reason": self.selector.reason(),
        })
    }
}
