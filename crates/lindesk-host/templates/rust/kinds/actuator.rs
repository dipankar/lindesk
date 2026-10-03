use std::collections::BTreeMap;

/// A skeleton actuator that only remembers values; replace `apply`/`reset` with real writes.
/// Actuators run as root in lindeskd (privilege = "system").
#[derive(Default)]
struct {{Struct}} {
    values: BTreeMap<(Target, Knob), Setting>,
}

impl Plugin for {{Struct}} {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: "{{name}}".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Actuator],
            description: env!("CARGO_PKG_DESCRIPTION").into(),
        }
    }
}

impl Actuator for {{Struct}} {
    fn caps(&self) -> ActuatorCaps {
        ActuatorCaps { knobs: vec![Knob::CpuWeight, Knob::IoWeight], targets: vec!["unit".into()] }
    }

    /// The current value, recorded by the host before the first change.
    fn read(&mut self, target: &Target, knob: &Knob) -> PluginResult<Option<Setting>> {
        Ok(self.values.get(&(target.clone(), knob.clone())).cloned())
    }

    fn apply(&mut self, target: &Target, setting: &Setting) -> PluginResult<()> {
        eprintln!("apply {setting} on {target}");
        self.values.insert((target.clone(), setting.knob()), setting.clone());
        Ok(())
    }

    /// Put `original` back, or the default when it is `None`.
    fn reset(&mut self, target: &Target, knob: &Knob, original: Option<&Setting>) -> PluginResult<()> {
        eprintln!("reset {knob} on {target}");
        match original {
            Some(s) => self.values.insert((target.clone(), knob.clone()), s.clone()),
            None => self.values.remove(&(target.clone(), knob.clone())),
        };
        Ok(())
    }
}

lindesk_plugin::main!({{Struct}}, actuator);

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_plugin::serde_json::json;
    use lindesk_plugin::testing::Harness;

    #[test]
    fn apply_then_read() {
        let mut h = Harness::new(Server::new({{Struct}}::default()).actuator());
        let init = h.initialize().unwrap();
        assert!(init.actuator.unwrap().supports_knob(&Knob::CpuWeight));
        let t = Target::system_unit("example.service");
        let s = Setting::CpuWeight { value: 50 };
        let () = h.request("actuator.apply", json!({"target": t, "setting": s})).unwrap();
        let v: Option<Setting> = h.request("actuator.read", json!({"target": t, "knob": Knob::CpuWeight})).unwrap();
        assert_eq!(v, Some(s));
    }
}
