//! Bounded, volatile observations and explicitly reported one-hop dependencies.
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use lindesk_host::StepReport;
use lindesk_plugin_api::{Metric, Sample};
use serde_json::{Value, json};

const HISTORY: usize = 240;
const OBSERVATIONS: usize = 1024;
const METRICS: &[&str] =
    &["desktop.frame_ms", "desktop.input_ms", "desktop.runqueue_ms", "desktop.audio_xruns", "desktop.timer_late_ms"];

#[derive(Default)]
pub struct Recorder {
    context: VecDeque<Value>,
    observations: VecDeque<(i64, String, f64)>,
    dropped: u64,
}
impl Recorder {
    pub fn metric(&mut self, m: &Metric) {
        if !METRICS.contains(&m.name.as_str()) || !m.value.is_finite() || m.value < 0.0 {
            return;
        }
        if self.observations.len() == OBSERVATIONS {
            self.observations.pop_front();
            self.dropped += 1;
        }
        // Labels are intentionally excluded: no unbounded cardinality or app-supplied text.
        self.observations.push_back((m.ts, m.name.clone(), m.value));
    }
    pub fn step(&mut self, r: &StepReport, pressure: &[Sample]) {
        if self.context.len() == HISTORY {
            self.context.pop_front();
        }
        self.context.push_back(json!({"ts": r.now, "level": r.level,
            "timings_us": r.timings, "actions": r.actions.len(), "pressure": pressure}));
    }
    pub fn report(&self) -> Value {
        let mut values: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
        for (_, name, value) in &self.observations {
            values.entry(name).or_default().push(*value);
        }
        let mut summary = serde_json::Map::new();
        for (name, mut v) in values {
            v.sort_by(f64::total_cmp);
            let percentile = |p: usize| v[(v.len() * p).div_ceil(100).saturating_sub(1)];
            summary.insert(
                name.into(),
                json!({"count": v.len(), "p50": percentile(50),
                "p95": percentile(95), "p99": percentile(99), "max": v.last(), "sum": v.iter().sum::<f64>()}),
            );
        }
        json!({"schema_version": 1, "context_capacity": HISTORY, "observation_capacity": OBSERVATIONS,
            "dropped_observations": self.dropped, "summary": summary,
            "observations": self.observations, "context": self.context,
            "note": "Application metrics require ReportMetric instrumentation; timer jitter and engine timing are not input-to-display latency."})
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    pid: i32,
    uid: u32,
    start: u64,
}
impl Identity {
    fn read(pid: i32) -> Option<Self> {
        let uid = super::proc_uid("/proc".as_ref(), pid)?;
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let start = stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()?;
        Some(Self { pid, uid, start })
    }
    fn alive(&self) -> bool {
        Self::read(self.pid).as_ref() == Some(self)
    }
}

#[derive(Clone, Debug)]
pub struct Lease {
    client: Identity,
    server: Identity,
}
impl Lease {
    pub fn capture(client: i32, server: i32, uid: u32, ttl: u32) -> Result<Self, String> {
        if ttl > 30_000 || client <= 0 || server <= 0 || client == server {
            return Err("need distinct positive pids and ttl_ms in 0..30000".into());
        }
        let client = Identity::read(client).ok_or("client process unavailable")?;
        let server = Identity::read(server).ok_or("server process unavailable")?;
        if client.uid != uid || server.uid != uid {
            return Err("both processes must belong to the caller's uid".into());
        }
        Ok(Self { client, server })
    }
}

#[derive(Default)]
pub struct Dependencies {
    leases: Vec<(Lease, Instant)>,
}
impl Dependencies {
    pub fn update(&mut self, lease: Lease, ttl: u32, now: Instant) -> Result<(), String> {
        if ttl > 30_000 {
            return Err("ttl_ms exceeds 30000".into());
        }
        self.leases.retain(|(l, expires)| {
            *expires > now && !(l.client.pid == lease.client.pid && l.server.pid == lease.server.pid)
        });
        if ttl == 0 {
            return Ok(());
        }
        if self.leases.len() >= 256
            || self.leases.iter().filter(|(l, _)| l.client.uid == lease.client.uid).count() >= 64
        {
            return Err("dependency lease limit reached (64 per uid, 256 total)".into());
        }
        self.leases.push((lease, now + Duration::from_millis(ttl.into())));
        Ok(())
    }
    pub fn live(&mut self, now: Instant) -> Vec<(i32, i32)> {
        self.leases.retain(|(l, expires)| *expires > now && l.client.alive() && l.server.alive());
        self.leases.iter().map(|(l, _)| (l.client.pid, l.server.pid)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn lease(n: i32) -> Lease {
        Lease { client: Identity { pid: 1, uid: 1000, start: 1 }, server: Identity { pid: n, uid: 1000, start: 1 } }
    }
    #[test]
    fn leases_expire_cancel_and_bound_memory() {
        let now = Instant::now();
        let mut d = Dependencies::default();
        for n in 2..66 {
            d.update(lease(n), 100, now).unwrap();
        }
        assert!(d.update(lease(66), 100, now).is_err());
        d.update(lease(2), 0, now).unwrap();
        assert_eq!(d.leases.len(), 63);
        d.update(lease(66), 100, now + Duration::from_millis(101)).unwrap();
        assert_eq!(d.leases.len(), 1);
        assert!(d.live(now + Duration::from_secs(1)).is_empty());
        assert!(Lease::capture(1, 1, 0, 100).is_err());
        let me = std::process::id() as i32;
        assert!(Identity::read(me).unwrap().alive());
        let mut old = Identity::read(me).unwrap();
        old.start += 1;
        assert!(!old.alive());
        assert!(Lease::capture(me, 1, u32::MAX, 100).is_err());
    }
    #[test]
    fn metrics_are_bounded_finite_and_percentiles_use_nearest_rank() {
        let mut r = Recorder::default();
        for n in 1..=1100 {
            r.metric(&Metric {
                ts: n,
                name: "desktop.frame_ms".into(),
                value: n as f64,
                labels: Default::default(),
                plugin: "test".into(),
            });
        }
        r.metric(&Metric {
            ts: 1101,
            name: "desktop.frame_ms".into(),
            value: f64::NAN,
            labels: Default::default(),
            plugin: "test".into(),
        });
        let v = r.report();
        assert_eq!(v["dropped_observations"], 76);
        assert_eq!(v["summary"]["desktop.frame_ms"]["count"], 1024);
        assert_eq!(v["summary"]["desktop.frame_ms"]["p99"], 1090.0);
    }
}
