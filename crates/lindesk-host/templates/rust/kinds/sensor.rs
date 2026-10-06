use std::collections::BTreeMap;

/// Reports system PSI (cpu, io, memory) from /proc/pressure.
#[derive(Default)]
struct {{Struct}} {
    /// Previous (total stall µs, timestamp) per resource, for deltas.
    last: BTreeMap<Resource, (u64, TimestampMs)>,
}

impl Plugin for {{Struct}} {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: "{{name}}".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Sensor],
            description: env!("CARGO_PKG_DESCRIPTION").into(),
        }
    }
}

/// `(avg10, total)` from the "some" line of /proc/pressure/<resource>.
fn read_psi(resource: Resource) -> Option<(f32, u64)> {
    let text = std::fs::read_to_string(format!("/proc/pressure/{resource}")).ok()?;
    let line = text.lines().next()?;
    let field = |k: &str| line.split_whitespace().find_map(|kv| kv.strip_prefix(k));
    Some((field("avg10=")?.parse().ok()?, field("total=")?.parse().ok()?))
}

impl Sensor for {{Struct}} {
    /// Called every `[sensor] interval_ms`.
    fn poll(&mut self, input: &SensorInput) -> PluginResult<SensorOutput> {
        let mut out = SensorOutput::default();
        for resource in Resource::ALL {
            let Some((avg10, total)) = read_psi(resource) else { continue };
            let prev = self.last.insert(resource, (total, input.now));
            let (delta, interval) =
                prev.map_or((0, 0), |(t, ts)| (total.saturating_sub(t), (input.now - ts).max(0) as u32));
            out.samples.push(Sample {
                ts: input.now,
                scope: Scope::System,
                resource,
                some_avg10: avg10,
                full_avg10: None,
                some_delta_us: delta,
                full_delta_us: None,
                interval_ms: interval,
            });
        }
        Ok(out)
    }
}

lindesk_plugin::main!({{Struct}}, sensor);

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_plugin::testing::Harness;

    #[test]
    fn polls_return_samples_with_intervals() {
        let mut h = Harness::new(Server::new({{Struct}}::default()).sensor());
        h.initialize().unwrap();
        h.poll(&SensorInput { now: 1000, targets: vec![], foreground: None, foreground_family: Vec::new() }).unwrap();
        let out = h.poll(&SensorInput { now: 2000, targets: vec![], foreground: None, foreground_family: Vec::new() }).unwrap();
        // Empty on kernels without PSI.
        assert!(out.samples.iter().all(|s| s.interval_ms == 1000));
    }
}
