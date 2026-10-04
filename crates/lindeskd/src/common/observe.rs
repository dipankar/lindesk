//! [`Observed`]: a transparent policy wrapper that remembers the metrics of the latest
//! evaluation, so the daemon can show them (e.g. whether `threshold` has an interaction
//! epoch running) without reaching into the policy.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use lindesk_plugin_api::{InitContext, Plugin, PluginInfo, PluginResult, Policy, PolicyOutput, Snapshot};

/// Metric name → value from a policy's latest evaluation. Cloneable; clones share.
#[derive(Clone, Debug, Default)]
pub struct PolicyMetrics(Arc<Mutex<BTreeMap<String, f64>>>);

impl PolicyMetrics {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, f64>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get(&self, name: &str) -> Option<f64> {
        self.lock().get(name).copied()
    }

    pub fn clear(&self) {
        self.lock().clear();
    }

    /// `threshold`'s interaction-epoch flag (`threshold.epoch`), when it has evaluated.
    pub fn epoch_active(&self) -> Option<bool> {
        self.get("threshold.epoch").map(|v| v >= 0.5)
    }
}

/// See the [module docs](self).
pub struct Observed<P> {
    inner: P,
    metrics: PolicyMetrics,
}

impl<P> Observed<P> {
    pub fn new(inner: P, metrics: PolicyMetrics) -> Self {
        Observed { inner, metrics }
    }
}

impl<P: Policy> Plugin for Observed<P> {
    fn info(&self) -> PluginInfo {
        self.inner.info()
    }
    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
        self.metrics.clear();
        self.inner.init(ctx)
    }
    fn shutdown(&mut self) {
        self.inner.shutdown()
    }
}

impl<P: Policy> Policy for Observed<P> {
    fn evaluate(&mut self, snapshot: &Snapshot) -> PluginResult<PolicyOutput> {
        let out = self.inner.evaluate(snapshot)?;
        let mut m = self.metrics.lock();
        m.clear();
        m.extend(out.metrics.iter().map(|x| (x.name.clone(), x.value)));
        Ok(out)
    }
}
