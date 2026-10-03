/// Reports focus changes from the desktop.
#[derive(Default)]
struct {{Struct}};

impl Plugin for {{Struct}} {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: "{{name}}".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Adapter],
            description: env!("CARGO_PKG_DESCRIPTION").into(),
        }
    }
}

impl Adapter for {{Struct}} {
    /// Start watching the desktop. Return quickly: do the watching on a thread and call
    /// `sink` for each event.
    fn start(&mut self, sink: EventSink) -> PluginResult<()> {
        std::thread::spawn(move || {
            // A real adapter subscribes to D-Bus, Wayland or X11 here, e.g.
            //   sink(Event::Focus { ts, pid: Some(pid), app_id: None, target: None, source: "{{name}}".into() });
            let _ = sink;
        });
        Ok(())
    }
}

lindesk_plugin::main!({{Struct}}, adapter);

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_plugin::testing::Harness;

    #[test]
    fn starts() {
        let mut h = Harness::new(Server::new({{Struct}}).adapter());
        h.initialize().unwrap();
        h.start_adapter().unwrap();
    }
}
