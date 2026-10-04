//! D-Bus clients for lindesk-session (`org.lindesk.QoS1`) and lindeskd
//! (`org.lindesk.Broker1`), with errors that say what to do.

use std::time::Duration;

use anyhow::{Result, anyhow};
use lindesk_ipc::{BROKER_NAME, BrokerProxyBlocking, QOS_NAME, QoSProxyBlocking};
use zbus::blocking::Connection;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Which daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Daemon {
    /// lindesk-session on the session bus.
    Session,
    /// lindeskd on the system bus.
    Broker,
}

impl Daemon {
    pub fn for_scope(system: bool) -> Daemon {
        if system { Daemon::Broker } else { Daemon::Session }
    }

    pub fn name(self) -> &'static str {
        match self {
            Daemon::Session => "lindesk-session",
            Daemon::Broker => "lindeskd",
        }
    }

    pub fn bus_name(self) -> &'static str {
        match self {
            Daemon::Session => QOS_NAME,
            Daemon::Broker => BROKER_NAME,
        }
    }

    fn bus(self) -> &'static str {
        match self {
            Daemon::Session => "session bus",
            Daemon::Broker => "system bus",
        }
    }

    /// The command that starts it.
    pub fn start_hint(self) -> &'static str {
        match self {
            Daemon::Session => "systemctl --user enable --now lindesk-session",
            Daemon::Broker => "sudo systemctl enable --now lindeskd",
        }
    }

    pub fn not_running(self) -> anyhow::Error {
        anyhow!(
            "{} isn't running on the {}.\n  Start it: {}\n  Diagnose: lindeskctl doctor",
            self.name(),
            self.bus(),
            self.start_hint()
        )
    }
}

/// Is the daemon reachable?
#[derive(Debug)]
pub enum Reach {
    Running,
    NotRunning,
    /// The bus itself is unavailable.
    NoBus(String),
}

/// Connect to the bus a daemon lives on.
pub fn connect(d: Daemon) -> Result<Connection> {
    connect_with(d, TIMEOUT)
}

/// [`connect`] with a method-call timeout (mutating calls may wait for polkit; calibration
/// takes ≈15 s).
pub fn connect_with(d: Daemon, timeout: Duration) -> Result<Connection> {
    let builder = match d {
        Daemon::Session => zbus::blocking::connection::Builder::session(),
        Daemon::Broker => zbus::blocking::connection::Builder::system(),
    };
    builder.and_then(|b| b.method_timeout(timeout).build()).map_err(|e| match d {
        Daemon::Session => anyhow!(
            "can't connect to the D-Bus session bus ({e}).\n  lindesk-session lives on the session bus of a desktop login; \
             over ssh, run in the desktop's session or use --system."
        ),
        Daemon::Broker => anyhow!("can't connect to the D-Bus system bus ({e})"),
    })
}

/// Check whether the daemon owns its name, without activating it.
pub fn reach(d: Daemon) -> Reach {
    let conn = match connect(d) {
        Ok(c) => c,
        Err(e) => return Reach::NoBus(first_line(&e.to_string())),
    };
    match owned(&conn, d) {
        Ok(true) => Reach::Running,
        Ok(false) => Reach::NotRunning,
        Err(e) => Reach::NoBus(e.to_string()),
    }
}

fn owned(conn: &Connection, d: Daemon) -> zbus::Result<bool> {
    let dbus = zbus::blocking::fdo::DBusProxy::new(conn)?;
    let name = zbus::names::BusName::try_from(d.bus_name())?;
    Ok(dbus.name_has_owner(name)?)
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

/// A connection to a running daemon (or an actionable error).
pub fn running(d: Daemon) -> Result<Connection> {
    running_with(d, TIMEOUT)
}

/// [`running`] with a method-call timeout.
pub fn running_with(d: Daemon, timeout: Duration) -> Result<Connection> {
    let conn = connect_with(d, timeout)?;
    match owned(&conn, d) {
        Ok(true) => Ok(conn),
        Ok(false) => Err(d.not_running()),
        Err(e) => Err(anyhow!("asking the {} who owns {}: {e}", d.bus(), d.bus_name())),
    }
}

pub fn session_proxy(conn: &Connection) -> Result<QoSProxyBlocking<'_>> {
    QoSProxyBlocking::new(conn).map_err(|e| anyhow!("creating a proxy for {QOS_NAME}: {e}"))
}

pub fn broker_proxy(conn: &Connection) -> Result<BrokerProxyBlocking<'_>> {
    BrokerProxyBlocking::new(conn).map_err(|e| anyhow!("creating a proxy for {BROKER_NAME}: {e}"))
}

/// Turn a failed method call into an actionable error.
pub fn call_error(d: Daemon, method: &str, e: zbus::Error) -> anyhow::Error {
    if let zbus::Error::MethodError(name, msg, _) = &e {
        let msg = msg.clone().unwrap_or_default();
        return match name.as_str() {
            "org.freedesktop.DBus.Error.ServiceUnknown" | "org.freedesktop.DBus.Error.NameHasNoOwner" => {
                d.not_running()
            }
            "org.freedesktop.DBus.Error.UnknownMethod" | "org.freedesktop.DBus.Error.UnknownInterface" => anyhow!(
                "{} doesn't implement {method} ({msg}). Is it a different lindesk version than lindeskctl {}?",
                d.name(),
                env!("CARGO_PKG_VERSION")
            ),
            "org.freedesktop.DBus.Error.AccessDenied"
            | "org.freedesktop.DBus.Error.AuthFailed"
            | "org.freedesktop.DBus.Error.InteractiveAuthorizationRequired" => {
                anyhow!(
                    "{} refused {method}: not authorized ({msg}). Polkit decides; try again from an active desktop session or with sudo.",
                    d.name()
                )
            }
            "org.freedesktop.DBus.Error.NoReply" | "org.freedesktop.DBus.Error.Timeout" => {
                anyhow!(
                    "{} didn't answer {method} in time ({msg}). Is it stuck? Check: journalctl {}-u {}",
                    d.name(),
                    if d == Daemon::Session { "--user " } else { "" },
                    d.name()
                )
            }
            n if n == "org.freedesktop.DBus.Error.InvalidArgs"
                || n.ends_with(".Rejected")
                || n.ends_with(".LimitsExceeded")
                || msg.starts_with("rejected") =>
            {
                let m = msg.strip_prefix("rejected:").map(str::trim).unwrap_or(&msg);
                anyhow!("{} refused it (guardrails): {m}", d.name())
            }
            _ => anyhow!("{} {method} failed: {msg}", d.name()),
        };
    }
    anyhow!("{} {method} failed: {e}", d.name())
}

/// `Status()` / `GetStatus()` as raw JSON.
pub fn status_json(d: Daemon) -> Result<String> {
    let conn = running(d)?;
    match d {
        Daemon::Session => session_proxy(&conn)?.status().map_err(|e| call_error(d, "Status", e)),
        Daemon::Broker => broker_proxy(&conn)?.get_status().map_err(|e| call_error(d, "GetStatus", e)),
    }
}

/// `Explain(what)` as raw JSON.
pub fn explain_json(d: Daemon, what: &str) -> Result<String> {
    let conn = running(d)?;
    match d {
        Daemon::Session => session_proxy(&conn)?.explain(what).map_err(|e| call_error(d, "Explain", e)),
        Daemon::Broker => broker_proxy(&conn)?.explain(what).map_err(|e| call_error(d, "Explain", e)),
    }
}

/// `SetClass(pid, class)`.
pub fn set_class(d: Daemon, pid: i32, class: &str) -> Result<()> {
    let conn = running(d)?;
    match d {
        Daemon::Session => session_proxy(&conn)?.set_class(pid, class).map_err(|e| call_error(d, "SetClass", e)),
        Daemon::Broker => broker_proxy(&conn)?.set_class(pid, class).map_err(|e| call_error(d, "SetClass", e)),
    }
}

/// Broker `Reload()`.
pub fn broker_reload() -> Result<()> {
    let conn = running(Daemon::Broker)?;
    broker_proxy(&conn)?.reload().map_err(|e| call_error(Daemon::Broker, "Reload", e))
}

// ---------------------------------------------------------------- control plane (CONTROL-PLANE.md §5)

/// Mutating calls may wait for a polkit dialog.
pub const MUTATE_TIMEOUT: Duration = Duration::from_secs(120);
/// Calibration takes ≈15 s plus polkit interaction.
pub const CALIBRATE_TIMEOUT: Duration = Duration::from_secs(240);

/// Call `member` on whichever daemon `d` is, through its proxy.
pub fn call<T>(
    d: Daemon,
    member: &str,
    timeout: Duration,
    session: impl FnOnce(&QoSProxyBlocking<'_>) -> zbus::Result<T>,
    broker: impl FnOnce(&BrokerProxyBlocking<'_>) -> zbus::Result<T>,
) -> Result<T> {
    let conn = running_with(d, timeout)?;
    match d {
        Daemon::Session => session(&session_proxy(&conn)?).map_err(|e| call_error(d, member, e)),
        Daemon::Broker => broker(&broker_proxy(&conn)?).map_err(|e| call_error(d, member, e)),
    }
}

pub fn plan_change(d: Daemon, ops_json: &str) -> Result<String> {
    call(d, "PlanChange", TIMEOUT, |p| p.plan_change(ops_json), |p| p.plan_change(ops_json))
}

pub fn apply_change(d: Daemon, ops_json: &str, description: &str, ttl_secs: u32) -> Result<String> {
    call(
        d,
        "ApplyChange",
        MUTATE_TIMEOUT,
        |p| p.apply_change(ops_json, description, ttl_secs),
        |p| p.apply_change(ops_json, description, ttl_secs),
    )
}

pub fn confirm_change(d: Daemon, id: u64) -> Result<()> {
    call(d, "ConfirmChange", MUTATE_TIMEOUT, |p| p.confirm_change(id), |p| p.confirm_change(id))
}

pub fn revert_change(d: Daemon, id: u64) -> Result<String> {
    call(d, "RevertChange", MUTATE_TIMEOUT, |p| p.revert_change(id), |p| p.revert_change(id))
}

pub fn list_changes(d: Daemon, limit: u32) -> Result<String> {
    call(d, "ListChanges", TIMEOUT, |p| p.list_changes(limit), |p| p.list_changes(limit))
}

pub fn get_change(d: Daemon, id: u64) -> Result<String> {
    call(d, "GetChange", TIMEOUT, |p| p.get_change(id), |p| p.get_change(id))
}

pub fn list_profiles(d: Daemon) -> Result<String> {
    call(d, "ListProfiles", TIMEOUT, |p| p.list_profiles(), |p| p.list_profiles())
}

pub fn set_profile(d: Daemon, name: &str) -> Result<String> {
    call(d, "SetProfile", MUTATE_TIMEOUT, |p| p.set_profile(name), |p| p.set_profile(name))
}

pub fn get_hardware(d: Daemon) -> Result<String> {
    call(d, "GetHardware", TIMEOUT, |p| p.get_hardware(), |p| p.get_hardware())
}

pub fn swap_report(d: Daemon) -> Result<String> {
    call(d, "SwapReport", TIMEOUT, |p| p.swap_report(), |p| p.swap_report())
}

/// Broker calibration: `CalibrateWith(options)`, falling back to `Calibrate()` on brokers
/// that don't have it. Returns the `LatencyMap` JSON.
pub fn calibrate(options_json: &str) -> Result<String> {
    let d = Daemon::Broker;
    let conn = running_with(d, CALIBRATE_TIMEOUT)?;
    let p = broker_proxy(&conn)?;
    match p.calibrate_with(options_json) {
        Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod" => {
            p.calibrate().map_err(|e| call_error(d, "Calibrate", e))
        }
        other => other.map_err(|e| call_error(d, "CalibrateWith", e)),
    }
}

/// Is this error "the daemon isn't running" (as opposed to a refusal or a failure)?
pub fn is_not_running(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("isn't running on the") || s.contains("can't connect to the D-Bus")
}

/// Is this error "the daemon doesn't implement this member" (older daemon)?
pub fn is_unsupported(e: &anyhow::Error) -> bool {
    e.to_string().contains("doesn't implement")
}
