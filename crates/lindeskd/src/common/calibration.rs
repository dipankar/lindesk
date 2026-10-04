//! First-run calibration scheduling: run the calibration once, the first time the machine
//! is idle (global PSI `some` below 5 % for 2 minutes, at least 3 minutes after start-up, on
//! AC power), never during user activity.
//!
//! [`IdleGate`] decides *when*. [`Calibrator`] runs `lindesk_sys::calibrate::run` on its own
//! thread (it is bounded: ≈15 s, ≈3 s in quick mode, and refuses a busy machine). The run
//! can't be interrupted, so a run during which the user became active or pressure rose is
//! discarded and retried at the next idle period (at most [`MAX_ATTEMPTS`] times). A good
//! result is saved, put into `HostCaps.hardware.latency` (`Engine::set_caps`) and announced
//! with `Notice { name: "calibration.done" }`.

use std::path::PathBuf;

use std::sync::mpsc;

use lindesk_host::{Engine, StepReport};
use lindesk_plugin_api::{Event, LatencyMap, PowerSource, TimestampMs};
use lindesk_sys::Roots;
use lindesk_sys::calibrate::{self, CalibrateOptions};

/// Minimum time after start-up before calibrating.
pub const STARTUP_DELAY_MS: TimestampMs = 3 * 60 * 1000;
/// How long the machine must have been idle.
pub const IDLE_FOR_MS: TimestampMs = 2 * 60 * 1000;
/// Global PSI `some` ratio under which the machine counts as idle.
pub const IDLE_PSI: f32 = 0.05;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Not yet; why.
    Wait(&'static str),
    /// Idle long enough: (keep) calibrating.
    Idle,
}

/// See the [module docs](self).
#[derive(Clone, Debug)]
pub struct IdleGate {
    started: TimestampMs,
    idle_since: Option<TimestampMs>,
}

impl IdleGate {
    pub fn new(started: TimestampMs) -> Self {
        IdleGate { started, idle_since: None }
    }

    /// Feed one observation: the worst global PSI `some` ratio, whether the user is active
    /// (interaction epoch, recent focus/input) and the power source.
    pub fn observe(&mut self, now: TimestampMs, psi_some: f32, user_active: bool, power: PowerSource) -> Decision {
        if power == PowerSource::Battery {
            self.idle_since = None;
            return Decision::Wait("on battery");
        }
        if user_active || psi_some >= IDLE_PSI {
            self.idle_since = None;
            return Decision::Wait(if user_active { "user active" } else { "pressure" });
        }
        let since = *self.idle_since.get_or_insert(now);
        if now - self.started < STARTUP_DELAY_MS {
            return Decision::Wait("just started");
        }
        if now - since < IDLE_FOR_MS {
            return Decision::Wait("not idle long enough");
        }
        Decision::Idle
    }
}

/// Calibration attempts per daemon run.
pub const MAX_ATTEMPTS: u32 = 3;

type Outcome = std::io::Result<LatencyMap>;

/// Owns the first-run calibration of one daemon.
pub struct Calibrator {
    pub path: PathBuf,
    pub gate: IdleGate,
    /// Quick mode (the standalone session agent).
    pub quick: bool,
    roots: Roots,
    running: Option<mpsc::Receiver<Outcome>>,
    disturbed: bool,
    attempts: u32,
    finished: bool,
}

impl std::fmt::Debug for Calibrator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Calibrator").field("path", &self.path).field("quick", &self.quick).finish()
    }
}

impl Calibrator {
    pub fn new(path: PathBuf, roots: Roots, started: TimestampMs, quick: bool) -> Self {
        Calibrator {
            path,
            gate: IdleGate::new(started),
            quick,
            roots,
            running: None,
            disturbed: false,
            attempts: 0,
            finished: false,
        }
    }

    /// True when a calibration result exists already (or we gave up for this run).
    pub fn done(&self) -> bool {
        self.finished || self.path.exists()
    }

    /// Feed one engine step. `epoch`: an interaction epoch is running.
    pub fn tick(&mut self, engine: &mut Engine, report: &StepReport, epoch: Option<bool>) {
        if self.running.is_none() && self.done() {
            return;
        }
        let psi = engine.system_pressure().iter().map(|s| s.some_ratio()).fold(0.0f32, f32::max);
        let active = epoch == Some(true)
            || report.events.iter().any(|e| matches!(e, Event::Focus { .. } | Event::Interaction { .. }));
        let power = engine.options().caps.hardware.power;
        let decision = self.gate.observe(report.now, psi, active, power);
        if let Some(rx) = &self.running {
            if decision != Decision::Idle && !matches!(decision, Decision::Wait("not idle long enough")) {
                self.disturbed = true;
            }
            match rx.try_recv() {
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.finish_attempt(None, engine, report.now),
                Ok(outcome) => self.finish_attempt(Some(outcome), engine, report.now),
            }
            return;
        }
        if decision == Decision::Idle {
            self.start();
        }
    }

    fn start(&mut self) {
        self.attempts += 1;
        self.disturbed = false;
        let (tx, rx) = mpsc::channel();
        let roots = self.roots.clone();
        let opts = CalibrateOptions { quick: self.quick, ..Default::default() };
        tracing::info!(
            "machine idle: calibrating ({} mode, attempt {})",
            if self.quick { "quick" } else { "full" },
            self.attempts
        );
        let spawned = std::thread::Builder::new().name("lindesk-calibrate".into()).spawn(move || {
            let _ = tx.send(calibrate::run(&roots, &opts));
        });
        match spawned {
            Ok(_) => self.running = Some(rx),
            Err(e) => {
                tracing::warn!("cannot start calibration: {e}");
                self.finished = true;
            }
        }
    }

    fn finish_attempt(&mut self, outcome: Option<Outcome>, engine: &mut Engine, now: TimestampMs) {
        self.running = None;
        let source = engine.options().host.clone();
        match outcome {
            Some(Ok(map)) if !self.disturbed => {
                if let Err(e) = calibrate::save(&self.path, &map) {
                    tracing::warn!("saving calibration to {}: {e}", self.path.display());
                }
                let mut caps = engine.options().caps.clone();
                calibrate::attach(&mut caps.hardware, map.clone());
                engine.set_caps(caps);
                engine.push_event(Event::Notice {
                    ts: now,
                    name: "calibration.done".into(),
                    source,
                    message: format!("calibration saved to {}", self.path.display()),
                    data: serde_json::to_value(&map).unwrap_or_default(),
                });
                tracing::info!("calibration done: {}", self.path.display());
                self.finished = true;
                return;
            }
            Some(Ok(_)) => tracing::info!("calibration discarded: the machine was used while it ran"),
            Some(Err(e)) => tracing::info!("calibration not done: {e}"),
            None => tracing::warn!("calibration thread died"),
        }
        // Try again at the next idle period.
        self.gate = IdleGate::new(self.gate.started);
        self.gate.idle_since = None;
        if self.attempts >= MAX_ATTEMPTS {
            tracing::info!(
                "calibration: giving up after {MAX_ATTEMPTS} attempts; `lindeskctl calibrate` runs it by hand"
            );
            self.finished = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: TimestampMs = 60_000;

    #[test]
    fn waits_for_startup_delay_and_sustained_idle() {
        let mut g = IdleGate::new(0);
        assert_eq!(g.observe(0, 0.0, false, PowerSource::Ac), Decision::Wait("just started"));
        assert_eq!(g.observe(2 * MIN, 0.01, false, PowerSource::Ac), Decision::Wait("just started"));
        assert_eq!(g.observe(3 * MIN, 0.01, false, PowerSource::Unknown), Decision::Idle, "idle since t=0");
        // Activity resets the idle window.
        assert_eq!(g.observe(4 * MIN, 0.01, true, PowerSource::Ac), Decision::Wait("user active"));
        assert_eq!(g.observe(5 * MIN, 0.01, false, PowerSource::Ac), Decision::Wait("not idle long enough"));
        assert_eq!(g.observe(7 * MIN, 0.01, false, PowerSource::Ac), Decision::Idle);
        assert_eq!(g.observe(8 * MIN, 0.2, false, PowerSource::Ac), Decision::Wait("pressure"));
        assert_eq!(g.observe(20 * MIN, 0.0, false, PowerSource::Battery), Decision::Wait("on battery"));
        assert_eq!(g.observe(21 * MIN, 0.0, false, PowerSource::Ac), Decision::Wait("not idle long enough"));
        assert_eq!(g.observe(23 * MIN, 0.0, false, PowerSource::Ac), Decision::Idle);
    }
}
