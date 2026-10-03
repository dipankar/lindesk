/// Lowers the I/O weight of background and batch work while system I/O pressure is high.
struct {{Struct}} {
    /// `[config] io_threshold`: PSI io some avg10 (percent) that triggers throttling.
    threshold: f32,
}

impl Default for {{Struct}} {
    fn default() -> Self {
        {{Struct}} { threshold: 40.0 }
    }
}

impl Plugin for {{Struct}} {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: "{{name}}".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Policy],
            description: env!("CARGO_PKG_DESCRIPTION").into(),
        }
    }

    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
        if let Some(t) = ctx.config.get("io_threshold").and_then(|v| v.as_f64()) {
            self.threshold = t as f32;
        }
        Ok(())
    }
}

impl Policy for {{Struct}} {
    /// Return the *complete* set of intents that should be in force now; anything you stop
    /// returning is reverted, and every value passes through lindesk's safety caps.
    fn evaluate(&mut self, s: &Snapshot) -> PluginResult<PolicyOutput> {
        let io = s.system_pressure(Resource::Io).map_or(0.0, |p| p.some_avg10);
        if io < self.threshold {
            return Ok(PolicyOutput { level: Some(Level::Green), ..Default::default() });
        }
        let intents = s
            .units
            .iter()
            .filter(|u| u.class.yields() && !u.foreground)
            .map(|u| Intent {
                target: u.target.clone(),
                setting: Setting::IoWeight { value: 10 },
                priority: 0, // 0 = the priority from plugin.toml
                reason: format!("io pressure {io:.0}%"),
            })
            .collect();
        Ok(PolicyOutput { intents, level: Some(Level::Yellow), metrics: vec![] })
    }
}

lindesk_plugin::main!({{Struct}}, policy);

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_plugin::testing::{Harness, snapshot};

    fn busy(io: f32) -> Snapshot {
        let mut s = snapshot(0);
        s.pressure.push(Sample {
            ts: 0,
            scope: Scope::System,
            resource: Resource::Io,
            some_avg10: io,
            full_avg10: None,
            some_delta_us: 0,
            full_delta_us: None,
            interval_ms: 1000,
        });
        for (name, class) in [("app-cargo-1.scope", QosClass::Batch), ("app-firefox-2.scope", QosClass::Normal)] {
            s.units.push(UnitState {
                target: Target::user_unit(1000, name),
                class,
                foreground: false,
                pressure: vec![],
                usage: None,
                applied: vec![],
                top_comm: None,
            });
        }
        s
    }

    #[test]
    fn quiet_system_is_left_alone() {
        let mut h = Harness::new(Server::new({{Struct}}::default()).policy());
        h.initialize().unwrap();
        let out = h.evaluate(&busy(1.0)).unwrap();
        assert_eq!(out.level, Some(Level::Green));
        assert!(out.intents.is_empty());
    }

    #[test]
    fn io_pressure_throttles_batch_only() {
        let mut h = Harness::new(Server::new({{Struct}}::default()).policy());
        h.initialize().unwrap();
        let out = h.evaluate(&busy(80.0)).unwrap();
        assert_eq!(out.level, Some(Level::Yellow));
        assert_eq!(out.intents.len(), 1);
        assert_eq!(out.intents[0].target, Target::user_unit(1000, "app-cargo-1.scope"));
    }
}
