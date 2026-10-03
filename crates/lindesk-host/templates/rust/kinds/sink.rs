/// Logs every health-level change.
#[derive(Default)]
struct {{Struct}} {
    notifier: Option<Notifier>,
}

impl Plugin for {{Struct}} {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: "{{name}}".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Sink],
            description: env!("CARGO_PKG_DESCRIPTION").into(),
        }
    }
}

impl Sink for {{Struct}} {
    /// Tables to receive: "pressure", "units", "events", "metrics" (empty = all).
    fn tables(&self) -> Vec<String> {
        vec!["events".into()]
    }

    fn record(&mut self, records: &[Record]) -> PluginResult<()> {
        for r in records {
            if let (Record::Event(Event::LevelChanged { from, to, policy, .. }), Some(n)) = (r, &self.notifier) {
                n.info(format!("level changed {from} -> {to} (by {policy})"));
            }
        }
        Ok(())
    }
}

fn main() {
    lindesk_plugin::run(Server::new({{Struct}}::default()).sink().with_notifier(|p, n| p.notifier = Some(n)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_plugin::testing::Harness;

    #[test]
    fn logs_level_changes() {
        let mut h =
            Harness::new(Server::new({{Struct}}::default()).sink().with_notifier(|p, n| p.notifier = Some(n)));
        assert_eq!(h.initialize().unwrap().tables, vec!["events".to_string()]);
        h.record(&[Record::Event(Event::LevelChanged {
            ts: 0,
            from: Level::Green,
            to: Level::Red,
            policy: "threshold".into(),
        })]);
        h.flush().unwrap();
        let logs = h.logs();
        assert_eq!(logs.len(), 1);
        assert!(logs[0].message.contains("green -> red"));
    }
}
