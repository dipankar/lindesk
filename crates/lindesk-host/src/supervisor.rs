//! Plugin health: a small state machine plus call statistics.
//!
//! ```text
//! Starting ──ok──► Running ──failure──► Backoff{until} ──retry ok──► Running
//!     │                                      │
//!     └──────────── N failures in window ────┴──► Disabled{reason}
//! ```
//!
//! Timeouts, protocol errors and crashes are *failures*: they move a restartable
//! (external) plugin into exponential backoff (1 s, 2 s, 4 s … capped at 60 s) and, after
//! [`SupervisorConfig::max_failures`] within [`SupervisorConfig::failure_window`], disable it.
//! Plain RPC errors (`Failed`, `Unsupported`) only count as errors. Built-in plugins are
//! not restartable: they keep running and only accumulate statistics.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lindesk_core::clock::now_ms;
use lindesk_plugin_api::{PluginError, TimestampMs};
use serde::{Deserialize, Serialize};

/// Restart and auto-disable tuning.
#[derive(Clone, Debug, PartialEq)]
pub struct SupervisorConfig {
    /// First backoff delay; doubles per consecutive failure.
    pub backoff_base: Duration,
    /// Backoff cap.
    pub backoff_max: Duration,
    /// Failures within `failure_window` that disable the plugin.
    pub max_failures: usize,
    pub failure_window: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        SupervisorConfig {
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            max_failures: 5,
            failure_window: Duration::from_secs(300),
        }
    }
}

/// Lifecycle state of one plugin.
#[derive(Clone, Debug, PartialEq)]
pub enum Health {
    /// Process spawned / handshake in progress.
    Starting,
    Running,
    /// Not called until `until`; external plugins are restarted then if they died.
    Backoff {
        until: Instant,
    },
    /// Permanently off until reloaded.
    Disabled {
        reason: String,
    },
}

impl Health {
    /// `starting`, `running`, `backoff` or `disabled`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Health::Starting => "starting",
            Health::Running => "running",
            Health::Backoff { .. } => "backoff",
            Health::Disabled { .. } => "disabled",
        }
    }
}

/// How a call went wrong, for statistics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    Timeout,
    Protocol,
    Crash,
}

/// Per-plugin call statistics (serialisable for `status` and `plugin info`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PluginStats {
    pub calls: u64,
    /// Calls that returned an error (including failures).
    pub errors: u64,
    pub timeouts: u64,
    /// Timeouts, protocol errors and crashes.
    pub failures: u64,
    pub restarts: u64,
    /// Exponentially weighted moving average of call latency (α = 0.2), µs.
    pub latency_ewma_us: f64,
    /// p99 over the last 256 calls, µs.
    pub latency_p99_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_at: Option<TimestampMs>,
}

const LATENCY_WINDOW: usize = 256;

/// Health state machine plus statistics for one plugin.
#[derive(Debug)]
pub struct Supervisor {
    cfg: SupervisorConfig,
    restartable: bool,
    health: Health,
    failures: VecDeque<Instant>,
    consecutive: u32,
    stats: PluginStats,
    latencies: VecDeque<u32>,
}

/// A supervisor shared between a plugin handle, its reader thread and the registry.
pub type SharedSupervisor = Arc<Mutex<Supervisor>>;

/// Lock a shared supervisor, ignoring poisoning (the state stays meaningful).
pub fn lock(s: &SharedSupervisor) -> MutexGuard<'_, Supervisor> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

/// True for errors that indicate a broken plugin rather than a refused request.
pub fn is_failure(e: &PluginError) -> Option<FailureKind> {
    match e {
        PluginError::Timeout => Some(FailureKind::Timeout),
        PluginError::Protocol(_) => Some(FailureKind::Protocol),
        PluginError::Io(_) => Some(FailureKind::Crash),
        PluginError::Failed(_) | PluginError::Unsupported(_) | PluginError::Unavailable(_) | PluginError::Gone(_) => {
            None
        }
    }
}

impl Supervisor {
    /// `restartable` = external plugin (backoff/disable apply); built-ins only collect stats.
    pub fn new(cfg: SupervisorConfig, restartable: bool) -> Self {
        Supervisor {
            cfg,
            restartable,
            health: if restartable { Health::Starting } else { Health::Running },
            failures: VecDeque::new(),
            consecutive: 0,
            stats: PluginStats::default(),
            latencies: VecDeque::with_capacity(LATENCY_WINDOW),
        }
    }

    /// A new shared supervisor.
    pub fn shared(cfg: SupervisorConfig, restartable: bool) -> SharedSupervisor {
        Arc::new(Mutex::new(Supervisor::new(cfg, restartable)))
    }

    pub fn health(&self) -> &Health {
        &self.health
    }

    pub fn config(&self) -> &SupervisorConfig {
        &self.cfg
    }

    /// May the plugin be called now? (Backoff that has expired counts as available; the
    /// caller is expected to restart the process if it died.)
    pub fn is_available(&self, now: Instant) -> bool {
        match &self.health {
            Health::Starting | Health::Running => true,
            Health::Backoff { until } => now >= *until,
            Health::Disabled { .. } => false,
        }
    }

    /// Time left in backoff, if any.
    pub fn backoff_remaining(&self, now: Instant) -> Option<Duration> {
        match &self.health {
            Health::Backoff { until } if *until > now => Some(*until - now),
            _ => None,
        }
    }

    /// The process is (re)starting.
    pub fn on_starting(&mut self, restart: bool) {
        if matches!(self.health, Health::Disabled { .. }) {
            return;
        }
        if restart {
            self.stats.restarts += 1;
        }
        self.health = Health::Starting;
    }

    /// Handshake completed.
    pub fn on_started(&mut self) {
        if !matches!(self.health, Health::Disabled { .. }) {
            self.health = Health::Running;
        }
    }

    /// A call completed (successfully or with an RPC error) in `latency`.
    pub fn on_call(&mut self, latency: Duration, result: Result<(), &PluginError>) {
        self.stats.calls += 1;
        let us = latency.as_micros().min(u32::MAX as u128) as u32;
        self.stats.latency_ewma_us =
            if self.stats.calls == 1 { us as f64 } else { 0.8 * self.stats.latency_ewma_us + 0.2 * us as f64 };
        if self.latencies.len() == LATENCY_WINDOW {
            self.latencies.pop_front();
        }
        self.latencies.push_back(us);
        match result {
            Ok(()) => {
                self.consecutive = 0;
                if matches!(self.health, Health::Starting | Health::Backoff { .. }) && self.restartable {
                    self.health = Health::Running;
                }
            }
            Err(e) => {
                self.note_error(&e.to_string());
                if let Some(kind) = is_failure(e) {
                    self.on_failure(kind, &e.to_string(), Instant::now());
                }
            }
        }
    }

    fn note_error(&mut self, msg: &str) {
        self.stats.errors += 1;
        self.stats.last_error = Some(msg.to_string());
        self.stats.last_error_at = Some(now_ms());
    }

    /// Record a failure that didn't come back through [`Supervisor::on_call`] (crash noticed
    /// by the reader thread, malformed output, failed handshake).
    pub fn on_failure(&mut self, kind: FailureKind, msg: &str, now: Instant) {
        self.stats.failures += 1;
        if kind == FailureKind::Timeout {
            self.stats.timeouts += 1;
        }
        self.stats.last_error = Some(msg.to_string());
        self.stats.last_error_at = Some(now_ms());
        if !self.restartable || matches!(self.health, Health::Disabled { .. }) {
            return;
        }
        self.failures.push_back(now);
        while self.failures.front().is_some_and(|t| now.duration_since(*t) > self.cfg.failure_window) {
            self.failures.pop_front();
        }
        if self.failures.len() >= self.cfg.max_failures {
            self.health = Health::Disabled {
                reason: format!(
                    "{} failures within {}s (last: {msg})",
                    self.failures.len(),
                    self.cfg.failure_window.as_secs()
                ),
            };
            return;
        }
        let exp = self.consecutive.min(16);
        self.consecutive += 1;
        let delay = self.cfg.backoff_base.saturating_mul(1u32 << exp).min(self.cfg.backoff_max);
        self.health = Health::Backoff { until: now + delay };
    }

    /// Disable permanently (config, privilege violation, operator request).
    pub fn disable(&mut self, reason: impl Into<String>) {
        self.health = Health::Disabled { reason: reason.into() };
    }

    /// Re-enable a disabled plugin (e.g. `lindeskctl plugin enable`).
    pub fn reset(&mut self) {
        self.failures.clear();
        self.consecutive = 0;
        self.health = if self.restartable { Health::Starting } else { Health::Running };
    }

    /// Statistics snapshot including the current p99.
    pub fn stats(&self) -> PluginStats {
        let mut s = self.stats.clone();
        if !self.latencies.is_empty() {
            let mut v: Vec<u32> = self.latencies.iter().copied().collect();
            v.sort_unstable();
            let idx = ((v.len() as f64 * 0.99).ceil() as usize).clamp(1, v.len()) - 1;
            s.latency_p99_us = v[idx] as u64;
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SupervisorConfig {
        SupervisorConfig {
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(4),
            max_failures: 5,
            failure_window: Duration::from_secs(300),
        }
    }

    #[test]
    fn exponential_backoff_then_disable() {
        let mut s = Supervisor::new(cfg(), true);
        s.on_started();
        let t0 = Instant::now();
        let mut delays = vec![];
        for i in 0..4 {
            s.on_failure(FailureKind::Crash, "died", t0 + Duration::from_secs(i));
            match s.health() {
                Health::Backoff { until } => delays.push((*until - (t0 + Duration::from_secs(i))).as_secs()),
                h => panic!("{h:?}"),
            }
        }
        assert_eq!(delays, vec![1, 2, 4, 4], "doubles, capped");
        assert!(!s.is_available(t0 + Duration::from_secs(4)));
        assert!(s.is_available(t0 + Duration::from_secs(10)));
        s.on_failure(FailureKind::Timeout, "slow", t0 + Duration::from_secs(10));
        assert!(matches!(s.health(), Health::Disabled { reason } if reason.contains("5 failures")));
        assert_eq!(s.stats().timeouts, 1);
        assert_eq!(s.stats().failures, 5);
    }

    #[test]
    fn failures_outside_window_are_forgotten_and_success_resets_backoff() {
        let mut s = Supervisor::new(SupervisorConfig { max_failures: 2, ..cfg() }, true);
        let t0 = Instant::now();
        s.on_failure(FailureKind::Crash, "x", t0);
        s.on_call(Duration::from_millis(1), Ok(()));
        assert_eq!(s.health(), &Health::Running);
        s.on_failure(FailureKind::Crash, "x", t0 + Duration::from_secs(301));
        match s.health() {
            Health::Backoff { until } => assert_eq!(*until - (t0 + Duration::from_secs(301)), Duration::from_secs(1)),
            h => panic!("{h:?}"),
        }
    }

    #[test]
    fn builtins_only_collect_stats() {
        let mut s = Supervisor::new(cfg(), false);
        for _ in 0..10 {
            s.on_call(Duration::from_micros(100), Err(&PluginError::Timeout));
        }
        s.on_call(Duration::from_micros(5000), Err(&PluginError::Failed("no".into())));
        assert_eq!(s.health(), &Health::Running);
        let st = s.stats();
        assert_eq!((st.calls, st.errors, st.timeouts, st.failures), (11, 11, 10, 10));
        assert_eq!(st.latency_p99_us, 5000);
        assert_eq!(st.last_error.as_deref(), Some("no"));
    }
}
