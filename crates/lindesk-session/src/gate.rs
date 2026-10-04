//! On/off switches for registered plugins. The `Registry` can't unregister, so plugins
//! that only run in one mode are registered once behind a [`Gate`]. A gated plugin keeps its
//! name and health row; while the gate is closed it produces nothing, and when it opens
//! again it is re-initialised so no stale state (hysteresis, epochs, PSI triggers) carries
//! over.

use std::os::fd::RawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lindesk_plugin_api::{
    InitContext, Plugin, PluginInfo, PluginResult, Policy, PolicyOutput, Sensor, SensorInput, SensorOutput, Snapshot,
};

#[derive(Clone, Debug, Default)]
pub struct Gate(Arc<AtomicBool>);

impl Gate {
    pub fn new(open: bool) -> Self {
        Gate(Arc::new(AtomicBool::new(open)))
    }
    pub fn set(&self, open: bool) {
        self.0.store(open, Ordering::SeqCst);
    }
    pub fn is_open(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// A policy behind a [`Gate`].
pub struct GatedPolicy<P> {
    inner: P,
    gate: Gate,
    /// Initialised and evaluating since the gate last opened.
    active: bool,
    ctx: Option<InitContext>,
}

impl<P> GatedPolicy<P> {
    pub fn new(inner: P, gate: Gate) -> Self {
        GatedPolicy { inner, gate, active: false, ctx: None }
    }
}

impl<P: Policy> Plugin for GatedPolicy<P> {
    fn info(&self) -> PluginInfo {
        self.inner.info()
    }
    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
        self.ctx = Some(ctx.clone());
        self.inner.init(ctx)?;
        self.active = true;
        Ok(())
    }
    fn shutdown(&mut self) {
        self.inner.shutdown()
    }
}

impl<P: Policy> Policy for GatedPolicy<P> {
    fn evaluate(&mut self, snapshot: &Snapshot) -> PluginResult<PolicyOutput> {
        if !self.gate.is_open() {
            self.active = false;
            return Ok(PolicyOutput::default());
        }
        if !self.active {
            if let Some(ctx) = &self.ctx {
                self.inner.init(ctx)?;
            }
            self.active = true;
        }
        self.inner.evaluate(snapshot)
    }
}

/// A sensor behind a [`Gate`]: closed means no samples, no wake fds, triggers disarmed.
pub struct GatedSensor<S> {
    inner: S,
    gate: Gate,
    active: bool,
    ctx: Option<InitContext>,
}

impl<S> GatedSensor<S> {
    pub fn new(inner: S, gate: Gate) -> Self {
        GatedSensor { inner, gate, active: false, ctx: None }
    }
}

impl<S: Sensor> Plugin for GatedSensor<S> {
    fn info(&self) -> PluginInfo {
        self.inner.info()
    }
    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
        self.ctx = Some(ctx.clone());
        self.inner.init(ctx)?;
        self.active = true;
        if !self.gate.is_open() {
            self.inner.shutdown();
            self.active = false;
        }
        Ok(())
    }
    fn shutdown(&mut self) {
        if self.active {
            self.inner.shutdown();
            self.active = false;
        }
    }
}

impl<S: Sensor> Sensor for GatedSensor<S> {
    fn poll(&mut self, input: &SensorInput) -> PluginResult<SensorOutput> {
        if !self.gate.is_open() {
            if self.active {
                self.inner.shutdown();
                self.active = false;
            }
            return Ok(SensorOutput::default());
        }
        if !self.active {
            if let Some(ctx) = &self.ctx {
                self.inner.init(ctx)?;
            }
            self.active = true;
        }
        self.inner.poll(input)
    }

    fn wake_fds(&self) -> Vec<RawFd> {
        if self.gate.is_open() && self.active { self.inner.wake_fds() } else { Vec::new() }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use lindesk_plugin_api::{API_VERSION, HostCaps, Level, PluginKind, Privilege};

    use super::*;

    #[derive(Default, Clone)]
    struct Log(Arc<Mutex<Vec<&'static str>>>);

    struct P(Log, u32);
    impl Plugin for P {
        fn info(&self) -> PluginInfo {
            PluginInfo {
                name: "p".into(),
                version: "0".into(),
                kinds: vec![PluginKind::Policy, PluginKind::Sensor],
                description: String::new(),
            }
        }
        fn init(&mut self, _: &InitContext) -> PluginResult<()> {
            self.1 = 0;
            self.0.0.lock().unwrap().push("init");
            Ok(())
        }
        fn shutdown(&mut self) {
            self.0.0.lock().unwrap().push("shutdown");
        }
    }
    impl Policy for P {
        fn evaluate(&mut self, _: &Snapshot) -> PluginResult<PolicyOutput> {
            self.1 += 1;
            Ok(PolicyOutput { level: Some(if self.1 > 1 { Level::Red } else { Level::Yellow }), ..Default::default() })
        }
    }
    impl Sensor for P {
        fn poll(&mut self, _: &SensorInput) -> PluginResult<SensorOutput> {
            self.0.0.lock().unwrap().push("poll");
            Ok(SensorOutput::default())
        }
        fn wake_fds(&self) -> Vec<RawFd> {
            vec![7]
        }
    }

    fn ctx() -> InitContext {
        InitContext {
            api_version: API_VERSION,
            host: "t".into(),
            privilege: Privilege::User,
            uid: Some(1),
            caps: HostCaps::default(),
            config: serde_json::Value::Null,
        }
    }

    fn snap() -> Snapshot {
        Snapshot {
            now: 0,
            level: Level::Green,
            pressure: vec![],
            units: vec![],
            events: vec![],
            foreground: None,
            caps: HostCaps::default(),
        }
    }

    #[test]
    fn policy_is_silent_when_closed_and_fresh_when_reopened() {
        let log = Log::default();
        let gate = Gate::new(true);
        let mut p = GatedPolicy::new(P(log.clone(), 0), gate.clone());
        p.init(&ctx()).unwrap();
        assert_eq!(p.evaluate(&snap()).unwrap().level, Some(Level::Yellow));
        assert_eq!(p.evaluate(&snap()).unwrap().level, Some(Level::Red), "state carries while open");
        gate.set(false);
        assert_eq!(p.evaluate(&snap()).unwrap(), PolicyOutput::default());
        gate.set(true);
        assert_eq!(p.evaluate(&snap()).unwrap().level, Some(Level::Yellow), "re-initialised");
        assert_eq!(*log.0.lock().unwrap(), vec!["init", "init"]);
    }

    #[test]
    fn sensor_disarms_when_closed() {
        let log = Log::default();
        let gate = Gate::new(false);
        let mut s = GatedSensor::new(P(log.clone(), 0), gate.clone());
        s.init(&ctx()).unwrap();
        assert!(s.wake_fds().is_empty());
        let input = SensorInput { now: 0, targets: vec![], foreground: None };
        assert_eq!(s.poll(&input).unwrap(), SensorOutput::default());
        gate.set(true);
        s.poll(&input).unwrap();
        assert_eq!(s.wake_fds(), vec![7]);
        gate.set(false);
        s.poll(&input).unwrap();
        assert!(s.wake_fds().is_empty());
        assert_eq!(*log.0.lock().unwrap(), vec!["init", "shutdown", "init", "poll", "shutdown"]);
    }
}
