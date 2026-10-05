//! Start-up requirement checks and runtime hardware refresh, shared by both daemons.

use lindesk_core::clock::now_ms;
use lindesk_host::Engine;
use lindesk_plugin_api::Event;
use lindesk_sys::requirements::{self, CheckOptions, Component, EXIT_REQUIREMENTS};
use lindesk_sys::{Env, Probe, Roots, hardware};
use serde_json::Value;

/// Run the requirements check for `component`. Logs every failure (with its remedy) at
/// error and every warning at warn, then the one-line start banner. `Err(exit code)` when a
/// hard requirement fails; otherwise the warnings as JSON (for the status).
pub fn check_requirements(
    component: Component,
    roots: &Roots,
    env: &Env,
    probe: &Probe,
    dbus_required: bool,
) -> Result<Vec<Value>, i32> {
    let opts = CheckOptions::default().dbus_required(dbus_required);
    let report = requirements::check_with(roots, env, component, probe, &opts);
    let mut failed = false;
    for f in report.failures() {
        failed = true;
        match &f.remedy {
            Some(r) => tracing::error!("requirement not met: {}: {} (fix: {r})", f.title, f.detail),
            None => tracing::error!("requirement not met: {}: {}", f.title, f.detail),
        }
    }
    if failed {
        tracing::error!("{} cannot run on this host; exiting with status {EXIT_REQUIREMENTS}", component.program());
        return Err(EXIT_REQUIREMENTS);
    }
    let mut warnings = Vec::new();
    for w in report.warnings() {
        match &w.remedy {
            Some(r) => tracing::warn!("{}: {} (fix: {r})", w.title, w.detail),
            None => tracing::warn!("{}: {}", w.title, w.detail),
        }
        warnings.push(serde_json::to_value(w).unwrap_or(Value::Null));
    }
    tracing::info!(
        "lindesk starting: level {}, backend {}, {}",
        probe.support_level(),
        probe.backend(),
        hardware::summary_line(&probe.hardware)
    );
    Ok(warnings)
}

/// Re-runs the requirements check while the daemon runs, so status warnings (a
/// conflicting daemon that was since disabled, say) don't go stale: every maintenance tick
/// and right after a versioned change.
pub struct RequirementsWatch {
    component: Component,
    roots: Roots,
    env: Env,
    dbus_required: bool,
    /// When the warnings were last computed (ms since the epoch).
    pub checked_ms: lindesk_plugin_api::TimestampMs,
}

impl RequirementsWatch {
    pub fn new(component: Component, roots: Roots, env: Env, dbus_required: bool) -> Self {
        RequirementsWatch { component, roots, env, dbus_required, checked_ms: now_ms() }
    }

    /// The current warnings as JSON (no logging; hard failures are a start-up matter).
    pub fn refresh(&mut self) -> Vec<Value> {
        let probe = lindesk_sys::probe(&self.roots, &self.env);
        let opts = CheckOptions::default().dbus_required(self.dbus_required);
        let report = requirements::check_with(&self.roots, &self.env, self.component, &probe, &opts);
        self.checked_ms = now_ms();
        report.warnings().map(|w| serde_json::to_value(w).unwrap_or(Value::Null)).collect()
    }
}

/// True when a step's events include a versioned change (`change.*` notice), after which
/// the requirement warnings are re-evaluated.
pub fn has_change_notice(events: &[Event]) -> bool {
    events.iter().any(|e| matches!(e, Event::Notice { name, .. } if name.starts_with("change.")))
}

/// A daemon-level `Event::Notice`.
pub fn notice(name: &str, source: &str, message: String, data: Value) -> Event {
    Event::Notice { ts: now_ms(), name: name.into(), source: source.into(), message, data }
}

/// Queue a `requirements.warning` notice per warning (so `events --follow` shows them).
pub fn requirement_notices(engine: &mut Engine, warnings: &[Value]) {
    let source = engine.options().host.clone();
    for w in warnings {
        let msg = format!("{}: {}", w["title"].as_str().unwrap_or_default(), w["detail"].as_str().unwrap_or_default());
        engine.push_event(notice("requirements.warning", &source, msg, w.clone()));
    }
}

/// Re-detects the power source and updates the engine's [`lindesk_plugin_api::HostCaps`]
/// when it changed (AC ⇄ battery), so policies adapt.
#[derive(Debug, Clone)]
pub struct HardwareWatch {
    roots: Roots,
}

impl HardwareWatch {
    pub fn new(roots: Roots) -> Self {
        HardwareWatch { roots }
    }

    /// Returns true when the caps changed.
    pub fn refresh(&self, engine: &mut Engine) -> bool {
        let now = hardware::detect(&self.roots);
        let mut caps = engine.options().caps.clone();
        if caps.hardware.power == now.power {
            return false;
        }
        tracing::info!("power source: {:?} -> {:?}", caps.hardware.power, now.power);
        engine.push_event(notice(
            "hardware.power",
            &engine.options().host,
            format!("power source changed to {:?}", now.power),
            serde_json::json!({ "from": caps.hardware.power, "to": now.power }),
        ));
        caps.hardware.power = now.power;
        engine.set_caps(caps);
        true
    }
}

#[cfg(test)]
mod watch_tests {
    use super::*;

    #[test]
    fn warnings_follow_a_conflicting_daemon_going_away() {
        let d = tempfile::tempdir().unwrap();
        let (roots, env) = lindesk_sys::fixture::pop_os(d.path());
        let f = lindesk_sys::fixture::Fixture::new(d.path());
        f.process(3000, "system76-schedu");
        let mut w = RequirementsWatch::new(Component::Broker, roots, env, false);
        let conflicts = |v: &[Value]| v.iter().any(|r| r["id"] == "conflicts");
        assert!(conflicts(&w.refresh()), "system76-scheduler running: a warning");
        let first = w.checked_ms;
        f.remove("proc/3000");
        assert!(!conflicts(&w.refresh()), "stopped (e.g. setup conflicts --disable): the warning goes");
        assert!(w.checked_ms >= first);
        let n = |name: &str| Event::Notice {
            ts: 0,
            name: name.into(),
            source: "t".into(),
            message: String::new(),
            data: Value::Null,
        };
        assert!(has_change_notice(&[n("profile.changed"), n("change.applied")]));
        assert!(!has_change_notice(&[n("profile.changed")]));
    }
}
