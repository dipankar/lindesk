//! Talking to lindesk: is it running, mark the probe as foreground, read its level.
//!
//! lindesk counts as running if any of these is true:
//!
//! * `org.lindesk.QoS1` has an owner on the session bus (lindesk-session);
//! * `org.lindesk.Broker1` has an owner on the system bus (lindeskd);
//! * `systemctl --user is-active lindesk-session` or `systemctl is-active lindeskd`;
//! * a `lindesk-session` or `lindeskd` process exists.
//!
//! The off phase requires all of them to be false, so a half-stopped lindesk (a unit
//! restarting, a leftover process) can't skew the baseline.

use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use lindesk_ipc::{BROKER_NAME, BrokerProxy, QOS_NAME, QoSProxy};
use zbus::Connection;

use crate::results::LindeskState;

pub const SESSION_UNIT: &str = "lindesk-session.service";
pub const SYSTEM_UNIT: &str = "lindeskd.service";
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// D-Bus client on a private current-thread runtime, so the rest of the bench stays sync.
pub struct Lindesk {
    rt: tokio::runtime::Runtime,
    session: Option<Connection>,
    system: Option<Connection>,
}

impl Lindesk {
    pub fn connect() -> Result<Lindesk> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let (session, system) = rt.block_on(async {
            let s = tokio::time::timeout(CALL_TIMEOUT, Connection::session()).await.ok().and_then(Result::ok);
            let y = tokio::time::timeout(CALL_TIMEOUT, Connection::system()).await.ok().and_then(Result::ok);
            (s, y)
        });
        Ok(Lindesk { rt, session, system })
    }

    fn has_owner(&self, conn: Option<&Connection>, name: &'static str) -> bool {
        conn.is_some_and(|c| self.rt.block_on(name_owned(c, name)))
    }

    pub fn detect(&self) -> LindeskState {
        LindeskState {
            session_bus: self.has_owner(self.session.as_ref(), QOS_NAME),
            broker_bus: self.has_owner(self.system.as_ref(), BROKER_NAME),
            session_unit_active: unit_active(true, SESSION_UNIT),
            system_unit_active: unit_active(false, SYSTEM_UNIT),
            processes: lindesk_processes(),
            paused: Vec::new(),
        }
    }

    /// Marks `pid` as the foreground app: `SetForegroundProcess` on the session agent, or
    /// `ReportForeground` on the broker when no session agent is around.
    pub fn set_foreground(&self, pid: u32) -> Result<()> {
        let pid = pid as i32;
        self.rt.block_on(async {
            if let Some(c) = &self.session
                && name_owned(c, QOS_NAME).await
            {
                let p = QoSProxy::new(c).await?;
                return timed(p.set_foreground_process(pid)).await;
            }
            if let Some(c) = &self.system {
                let p = BrokerProxy::new(c).await?;
                return timed(p.report_foreground(pid, "")).await;
            }
            Err(anyhow!("no lindesk D-Bus service to talk to"))
        })
    }

    /// `SetPaused` on every lindesk daemon that is up: observe-only, every setting it
    /// applied reverted (`seconds > 0` auto-resumes). Returns which daemons paused; the
    /// broker may refuse (polkit), which leaves system units managed.
    pub fn set_paused(&self, paused: bool, seconds: u32) -> Result<Vec<&'static str>> {
        self.rt.block_on(async {
            let mut done = Vec::new();
            let mut errors = Vec::new();
            if let Some(c) = &self.session
                && name_owned(c, QOS_NAME).await
            {
                match timed(QoSProxy::new(c).await?.set_paused(paused, seconds)).await {
                    Ok(_) => done.push("lindesk-session"),
                    Err(e) => errors.push(format!("lindesk-session: {e}")),
                }
            }
            if let Some(c) = &self.system
                && name_owned(c, BROKER_NAME).await
            {
                match timed(BrokerProxy::new(c).await?.set_paused(paused, seconds)).await {
                    Ok(_) => done.push("lindeskd"),
                    Err(e) => errors.push(format!(
                        "lindeskd: {e} (system units stay managed; run as root or allow org.lindesk.manage)"
                    )),
                }
            }
            if done.is_empty() && !errors.is_empty() {
                return Err(anyhow!("{}", errors.join("; ")));
            }
            for e in errors {
                eprintln!("lindesk-bench: warning: {e}");
            }
            Ok(done)
        })
    }

    /// Starts or extends an interaction epoch for `pid`.
    pub fn begin_interaction(&self, pid: u32) -> Result<()> {
        let pid = pid as i32;
        self.rt.block_on(async {
            let c = self.session.as_ref().ok_or_else(|| anyhow!("no session bus"))?;
            let p = QoSProxy::new(c).await?;
            timed(p.begin_interaction(pid)).await
        })
    }

    /// lindesk's current level (`green`, `yellow`, `red`) from `Status()`.
    pub fn level(&self) -> Option<String> {
        self.rt.block_on(async {
            let json = if let Some(c) = &self.session {
                let p = QoSProxy::new(c).await.ok()?;
                tokio::time::timeout(CALL_TIMEOUT, p.status()).await.ok()?.ok()
            } else {
                None
            };
            let json = match json {
                Some(j) => j,
                None => {
                    let c = self.system.as_ref()?;
                    let p = BrokerProxy::new(c).await.ok()?;
                    tokio::time::timeout(CALL_TIMEOUT, p.get_status()).await.ok()?.ok()?
                }
            };
            level_from_status(&json)
        })
    }
}

async fn name_owned(c: &Connection, name: &'static str) -> bool {
    let fut = async {
        let dbus = zbus::fdo::DBusProxy::new(c).await?;
        let n = zbus::names::BusName::try_from(name).map_err(zbus::Error::from)?;
        dbus.name_has_owner(n).await.map_err(zbus::Error::from)
    };
    matches!(tokio::time::timeout(CALL_TIMEOUT, fut).await, Ok(Ok(true)))
}

async fn timed<T>(f: impl std::future::Future<Output = zbus::Result<T>>) -> Result<T> {
    tokio::time::timeout(CALL_TIMEOUT, f).await.context("D-Bus call timed out")?.map_err(Into::into)
}

/// Finds the level in a `Status()` JSON document: a top-level `level`, else the first
/// `level` string found anywhere.
pub fn level_from_status(json: &str) -> Option<String> {
    fn find(v: &serde_json::Value) -> Option<String> {
        match v {
            serde_json::Value::Object(m) => {
                if let Some(serde_json::Value::String(s)) = m.get("level") {
                    return Some(s.to_ascii_lowercase());
                }
                m.values().find_map(find)
            }
            serde_json::Value::Array(a) => a.iter().find_map(find),
            _ => None,
        }
    }
    find(&serde_json::from_str(json).ok()?)
}

fn unit_active(user: bool, unit: &str) -> bool {
    let mut c = Command::new("systemctl");
    if user {
        c.arg("--user");
    }
    c.args(["is-active", "--quiet", unit]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    c.status().is_ok_and(|s| s.success())
}

fn lindesk_processes() -> Vec<String> {
    let mut out = Vec::new();
    let Ok(pids) = lindesk_sys::procfs::list_pids(std::path::Path::new("/proc")) else { return out };
    for pid in pids {
        if let Ok(comm) = lindesk_sys::procfs::comm(std::path::Path::new("/proc"), pid)
            && matches!(comm.as_str(), "lindeskd" | "lindesk-session")
        {
            out.push(format!("{comm}[{pid}]"));
        }
    }
    out
}

/// `systemctl --user <verb> lindesk-session` (never system units, never sudo).
pub fn user_service(verb: &str) -> Result<()> {
    let st = Command::new("systemctl").args(["--user", verb, SESSION_UNIT]).stdin(Stdio::null()).status()?;
    if !st.success() {
        return Err(anyhow!("systemctl --user {verb} {SESSION_UNIT} failed ({st})"));
    }
    Ok(())
}

/// How to stop lindesk by hand, for the off phase.
pub fn stop_instructions(s: &LindeskState) -> String {
    let mut v = Vec::new();
    if s.session_bus || s.session_unit_active || s.processes.iter().any(|p| p.starts_with("lindesk-session")) {
        v.push("  systemctl --user stop lindesk-session      (or rerun with --manage-services)".to_string());
    }
    if s.broker_bus || s.system_unit_active || s.processes.iter().any(|p| p.starts_with("lindeskd")) {
        v.push("  sudo systemctl stop lindeskd".to_string());
    }
    if v.is_empty() {
        v.push("  (no lindesk service found)".into());
    }
    v.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_is_found_anywhere() {
        assert_eq!(level_from_status(r#"{"level":"Red","uid":1}"#).as_deref(), Some("red"));
        assert_eq!(level_from_status(r#"{"engine":{"state":{"level":"yellow"}}}"#).as_deref(), Some("yellow"));
        assert_eq!(level_from_status(r#"{"x":1}"#), None);
        assert_eq!(level_from_status("not json"), None);
    }

    #[test]
    fn running_means_any_signal() {
        assert!(!LindeskState::default().running());
        assert!(LindeskState { processes: vec!["lindeskd[1]".into()], ..Default::default() }.running());
        let s = LindeskState { session_unit_active: true, broker_bus: true, ..Default::default() };
        let msg = stop_instructions(&s);
        assert!(msg.contains("systemctl --user stop lindesk-session") && msg.contains("sudo systemctl stop lindeskd"));
    }
}
