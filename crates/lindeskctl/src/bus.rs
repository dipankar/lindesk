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
    let builder = match d {
        Daemon::Session => zbus::blocking::connection::Builder::session(),
        Daemon::Broker => zbus::blocking::connection::Builder::system(),
    };
    builder.and_then(|b| b.method_timeout(TIMEOUT).build()).map_err(|e| match d {
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
    let conn = connect(d)?;
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
            "org.freedesktop.DBus.Error.AccessDenied" | "org.freedesktop.DBus.Error.AuthFailed" => {
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
