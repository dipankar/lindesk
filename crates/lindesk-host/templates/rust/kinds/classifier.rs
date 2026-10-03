/// Classifies build tools as batch work.
#[derive(Default)]
struct {{Struct}} {
    /// Extra command names from `[config] batch = [...]`.
    extra: Vec<String>,
}

impl Plugin for {{Struct}} {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: "{{name}}".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Classifier],
            description: env!("CARGO_PKG_DESCRIPTION").into(),
        }
    }

    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
        if let Some(list) = ctx.config.get("batch").and_then(|v| v.as_array()) {
            self.extra = list.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        Ok(())
    }
}

impl Classifier for {{Struct}} {
    /// `None` means "no opinion": the next classifier decides.
    fn classify(&mut self, p: &ProcInfo) -> PluginResult<Option<Classification>> {
        const BATCH: &[&str] = &["rustc", "cc1", "cc1plus", "ld", "make", "ninja"];
        let batch = BATCH.contains(&p.comm.as_str()) || self.extra.contains(&p.comm);
        Ok(batch.then(|| Classification {
            class: QosClass::Batch,
            confidence: 0.9,
            reason: format!("{} is a build tool", p.comm),
        }))
    }
}

lindesk_plugin::main!({{Struct}}, classifier);

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_plugin::serde_json::json;
    use lindesk_plugin::testing::{Harness, proc_info, test_context};

    fn harness() -> Harness<{{Struct}}> {
        Harness::new(Server::new({{Struct}}::default()).classifier())
    }

    #[test]
    fn compilers_are_batch() {
        let mut h = harness();
        h.initialize().unwrap();
        assert_eq!(h.classify(&proc_info("rustc")).unwrap().unwrap().class, QosClass::Batch);
        assert_eq!(h.classify(&proc_info("firefox")).unwrap(), None);
    }

    #[test]
    fn config_adds_commands() {
        let mut h = harness();
        h.initialize_with(test_context(json!({"batch": ["blender"]}))).unwrap();
        assert!(h.classify(&proc_info("blender")).unwrap().is_some());
    }
}
