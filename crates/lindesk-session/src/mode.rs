//! Connected / standalone mode switching, as a pure state machine.
//!
//! * **Connected**: a broker (`lindeskd`) accepted `RegisterSession`. The broker is the brain;
//!   the agent applies its `SessionState` through the `broker` relay policy. The local
//!   `threshold` policy and `psi` sensor are off.
//! * **Standalone**: no broker, it vanished, registration failed, or `--standalone`. The
//!   agent runs the full pipeline for its own units (`psi`, `threshold`); the relay is
//!   cleared, so everything the broker asked for is reverted unless `threshold` wants it too.
//! * **Starting**: before the first decision; no policy is active.
//!
//! Policies are declarative, so switching only flips which policies produce intents; the
//! engine reverts whatever is no longer requested on the next step. No stale settings.

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Starting,
    Standalone,
    Connected,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Starting => "starting",
            Mode::Standalone => "standalone",
            Mode::Connected => "connected",
        }
    }
}

/// What happened on the system bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// `RegisterSession` succeeded.
    Registered,
    /// `RegisterSession` failed or timed out (or the system bus is unreachable).
    RegisterFailed,
    /// `org.lindesk.Broker1` got an owner.
    BrokerAppeared,
    /// `org.lindesk.Broker1` lost its owner.
    BrokerVanished,
}

/// The new mode and whether to (re-)register now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transition {
    pub mode: Mode,
    pub register: bool,
}

/// Next state. `forced`: `--standalone` (the broker is ignored entirely).
pub fn next(forced: bool, current: Mode, input: Input) -> Transition {
    if forced {
        return Transition { mode: Mode::Standalone, register: false };
    }
    match input {
        Input::Registered => Transition { mode: Mode::Connected, register: false },
        Input::RegisterFailed | Input::BrokerVanished => Transition { mode: Mode::Standalone, register: false },
        // Keep doing what we do until the broker accepts us; a new broker instance needs a
        // fresh registration even when we were connected to its predecessor.
        Input::BrokerAppeared => Transition { mode: current, register: true },
    }
}

/// Which local pipeline parts a mode enables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Effects {
    pub threshold: bool,
    pub psi: bool,
    pub relay: bool,
}

pub fn effects(mode: Mode) -> Effects {
    match mode {
        Mode::Starting => Effects { threshold: false, psi: false, relay: false },
        Mode::Standalone => Effects { threshold: true, psi: true, relay: false },
        Mode::Connected => Effects { threshold: false, psi: false, relay: true },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_standalone_never_registers() {
        for m in [Mode::Starting, Mode::Standalone, Mode::Connected] {
            for i in [Input::Registered, Input::RegisterFailed, Input::BrokerAppeared, Input::BrokerVanished] {
                assert_eq!(next(true, m, i), Transition { mode: Mode::Standalone, register: false });
            }
        }
    }

    #[test]
    fn broker_lifecycle() {
        // Startup without a broker.
        let t = next(false, Mode::Starting, Input::RegisterFailed);
        assert_eq!(t.mode, Mode::Standalone);
        // Broker starts: register, stay standalone until it answers.
        let t = next(false, t.mode, Input::BrokerAppeared);
        assert_eq!(t, Transition { mode: Mode::Standalone, register: true });
        let t = next(false, t.mode, Input::Registered);
        assert_eq!(t.mode, Mode::Connected);
        // Broker restarts: vanish → standalone, appear → register again.
        let t = next(false, t.mode, Input::BrokerVanished);
        assert_eq!(t.mode, Mode::Standalone);
        let t = next(false, Mode::Connected, Input::BrokerAppeared);
        assert_eq!(t, Transition { mode: Mode::Connected, register: true });
        assert_eq!(next(false, Mode::Connected, Input::RegisterFailed).mode, Mode::Standalone);
    }

    #[test]
    fn effects_are_exclusive() {
        assert_eq!(effects(Mode::Starting), Effects { threshold: false, psi: false, relay: false });
        let s = effects(Mode::Standalone);
        let c = effects(Mode::Connected);
        assert!(s.threshold && s.psi && !s.relay);
        assert!(!c.threshold && !c.psi && c.relay);
        assert_eq!(serde_json::to_value(Mode::Connected).unwrap(), "connected");
    }
}
