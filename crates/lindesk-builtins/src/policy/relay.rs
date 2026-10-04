//! The `broker` policy: the session agent's view of the root broker's decisions.
//!
//! When `lindesk-session` is connected to `lindeskd`, the broker is the brain: it computes
//! the desired settings for this user's units and sends them as a `SessionState`. The agent
//! feeds the latest set into a [`RelayFeed`]; [`RelayPolicy`] returns it as intents (priority
//! [`PRIORITY`], reason `"broker: <original plugin>: <reason>"`) together with the broker's
//! level. Because policies are declarative, the engine reverts whatever the broker stops
//! asking for, and a cleared feed (broker gone, standalone mode) reverts everything relayed.

use std::sync::{Arc, Mutex, MutexGuard};

use lindesk_plugin_api::{
    InitContext, Intent, Level, Plugin, PluginInfo, PluginKind, PluginResult, Policy, PolicyOutput, Provenance,
    Setting, Snapshot, Target,
};

/// Plugin name.
pub const NAME: &str = "broker";

/// Priority of relayed intents (above built-ins and default external plugins, below admin
/// overrides).
pub const PRIORITY: i32 = 50;

/// One setting the broker wants applied.
#[derive(Clone, Debug, PartialEq)]
pub struct RelayedSetting {
    pub target: Target,
    pub setting: Setting,
    /// Who set it on the broker, and why.
    pub provenance: Provenance,
}

#[derive(Clone, Debug, PartialEq)]
struct Feed {
    level: Level,
    settings: Vec<RelayedSetting>,
}

/// Shared slot holding the broker's latest desired set. Cheap to clone; clones share it.
#[derive(Clone, Debug, Default)]
pub struct RelayFeed(Arc<Mutex<Option<Feed>>>);

impl RelayFeed {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Option<Feed>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Replace the desired set (the broker always sends the complete set).
    pub fn set(&self, level: Level, settings: Vec<RelayedSetting>) {
        *self.lock() = Some(Feed { level, settings });
    }

    /// Stop relaying: the policy returns nothing and reports no level.
    pub fn clear(&self) {
        *self.lock() = None;
    }

    /// True while a desired set is held (connected to a broker).
    pub fn is_active(&self) -> bool {
        self.lock().is_some()
    }

    /// The broker's level and desired set, if active.
    pub fn get(&self) -> Option<(Level, Vec<RelayedSetting>)> {
        self.lock().as_ref().map(|f| (f.level, f.settings.clone()))
    }
}

/// The built-in `broker` policy. See the [module docs](self).
#[derive(Clone, Debug, Default)]
pub struct RelayPolicy {
    feed: RelayFeed,
}

impl RelayPolicy {
    /// A policy reading `feed`.
    pub fn new(feed: RelayFeed) -> Self {
        RelayPolicy { feed }
    }

    /// The feed this policy reads.
    pub fn feed(&self) -> RelayFeed {
        self.feed.clone()
    }
}

/// `"broker: <plugin>: <reason>"` (the reason part omitted when empty).
pub fn relay_reason(p: &Provenance) -> String {
    match (p.plugin.is_empty(), p.reason.is_empty()) {
        (true, true) => "broker".into(),
        (false, true) => format!("broker: {}", p.plugin),
        (true, false) => format!("broker: {}", p.reason),
        (false, false) => format!("broker: {}: {}", p.plugin, p.reason),
    }
}

impl Plugin for RelayPolicy {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Policy],
            description: "Applies the settings the lindeskd broker decided for this user's units".into(),
        }
    }

    fn init(&mut self, _ctx: &InitContext) -> PluginResult<()> {
        Ok(())
    }
}

impl Policy for RelayPolicy {
    fn evaluate(&mut self, _snapshot: &Snapshot) -> PluginResult<PolicyOutput> {
        let Some((level, settings)) = self.feed.get() else { return Ok(PolicyOutput::default()) };
        let intents = settings
            .into_iter()
            .map(|s| Intent {
                reason: relay_reason(&s.provenance),
                target: s.target,
                setting: s.setting,
                priority: PRIORITY,
            })
            .collect();
        Ok(PolicyOutput { intents, level: Some(level), metrics: Vec::new() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_plugin_api::HostCaps;

    fn snap() -> Snapshot {
        Snapshot {
            now: 1,
            level: Level::Green,
            pressure: vec![],
            units: vec![],
            events: vec![],
            foreground: None,
            caps: HostCaps::default(),
        }
    }

    #[test]
    fn relays_the_latest_set_until_cleared() {
        let feed = RelayFeed::new();
        let mut p = RelayPolicy::new(feed.clone());
        assert_eq!(p.info().name, "broker");
        assert_eq!(p.evaluate(&snap()).unwrap(), PolicyOutput::default(), "inactive: nothing, no level");
        assert!(!feed.is_active());

        let t = Target::user_unit(1000, "app-x.scope");
        feed.set(
            Level::Yellow,
            vec![RelayedSetting {
                target: t.clone(),
                setting: Setting::CpuWeight { value: 300 },
                provenance: Provenance {
                    plugin: "threshold".into(),
                    reason: "foreground boost".into(),
                    ..Default::default()
                },
            }],
        );
        let out = p.evaluate(&snap()).unwrap();
        assert_eq!(out.level, Some(Level::Yellow));
        assert_eq!(
            out.intents,
            vec![Intent {
                target: t,
                setting: Setting::CpuWeight { value: 300 },
                priority: PRIORITY,
                reason: "broker: threshold: foreground boost".into()
            }]
        );
        feed.set(Level::Green, vec![]);
        let out = p.evaluate(&snap()).unwrap();
        assert!(out.intents.is_empty());
        assert_eq!(out.level, Some(Level::Green));
        feed.clear();
        assert_eq!(p.evaluate(&snap()).unwrap(), PolicyOutput::default());
    }

    #[test]
    fn reasons() {
        let p = |plugin: &str, reason: &str| Provenance {
            plugin: plugin.into(),
            reason: reason.into(),
            ..Default::default()
        };
        assert_eq!(relay_reason(&p("a", "b")), "broker: a: b");
        assert_eq!(relay_reason(&p("a", "")), "broker: a");
        assert_eq!(relay_reason(&p("", "b")), "broker: b");
        assert_eq!(relay_reason(&p("", "")), "broker");
    }
}
