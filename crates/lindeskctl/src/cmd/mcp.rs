//! `lindeskctl mcp [serve|config|status]`: the MCP server for local AI agents.
//!
//! `serve` and `config` run the `lindesk-mcp` binary; `status` reports how an agent can
//! reach it (stdio needs nothing; the optional user socket unit serves HTTP).

use std::ffi::OsString;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Result, anyhow};
use lindesk_sys::doctor::{Finding, Severity};
use serde::Serialize;

use crate::bus::{self, Daemon};
use crate::cli::McpCmd;
use crate::paths;
use crate::ui::{Ui, out};

pub const ENABLE_SOCKET: &str = "systemctl --user enable --now lindesk-mcp.socket";
pub const ADD_CLAUDE: &str = "claude mcp add lindesk -- lindesk-mcp";

pub fn run(ui: &Ui, cmd: Option<&McpCmd>) -> Result<()> {
    match cmd {
        None | Some(McpCmd::Status) => status(ui),
        Some(McpCmd::Serve { args }) => exec_mcp(args.clone()),
        Some(McpCmd::Config { client, listen }) => {
            let mut args: Vec<OsString> = vec!["config".into(), "--client".into(), client.as_str().into()];
            if let Some(l) = listen {
                args.push("--listen".into());
                args.push(l.into());
            }
            exec_mcp(args)
        }
    }
}

fn binary() -> Option<PathBuf> {
    paths::which("lindesk-mcp").or_else(|| {
        // Next to this lindeskctl (a source checkout's target/ directory).
        let me = std::env::current_exe().ok()?;
        let p = me.parent()?.join("lindesk-mcp");
        p.is_file().then_some(p)
    })
}

fn exec_mcp(args: Vec<OsString>) -> Result<()> {
    let bin = binary().ok_or_else(|| {
        anyhow!(
            "lindesk-mcp isn't installed (it ships with the lindesk package; from a checkout: \
             cargo build --release -p lindesk-mcp, then put it on PATH)"
        )
    })?;
    let err = Command::new(&bin).args(args).exec();
    Err(anyhow!("running {}: {err}", bin.display()))
}

/// `systemctl --user <verb> <unit>` → its one-word answer (`active`, `enabled`, …).
fn user_unit(verb: &str, unit: &str) -> Option<String> {
    paths::which("systemctl")?;
    let o = Command::new("systemctl").args(["--user", verb, unit]).output().ok()?;
    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_absolute())
}

/// Everything `mcp status` (and doctor) report.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct McpStatus {
    pub binary: Option<PathBuf>,
    /// stdio works whenever the binary exists; clients spawn it.
    pub stdio_ready: bool,
    pub socket_unit: Option<String>,
    pub socket_enabled: Option<String>,
    pub service_unit: Option<String>,
    pub socket_path: Option<PathBuf>,
    pub socket_listening: bool,
    pub session_daemon: bool,
    pub broker: bool,
}

pub fn probe() -> McpStatus {
    let binary = binary();
    let socket_path = runtime_dir().map(|d| d.join("lindesk/mcp.sock"));
    let socket_listening =
        socket_path.as_ref().and_then(|p| std::fs::symlink_metadata(p).ok()).is_some_and(|m| m.file_type().is_socket());
    McpStatus {
        stdio_ready: binary.is_some(),
        binary,
        socket_unit: user_unit("is-active", "lindesk-mcp.socket"),
        socket_enabled: user_unit("is-enabled", "lindesk-mcp.socket"),
        service_unit: user_unit("is-active", "lindesk-mcp.service"),
        socket_path,
        socket_listening,
        session_daemon: matches!(bus::reach(Daemon::Session), bus::Reach::Running),
        broker: matches!(bus::reach(Daemon::Broker), bus::Reach::Running),
    }
}

/// The doctor line.
pub fn finding(st: &McpStatus) -> Finding {
    let check = "MCP server".to_string();
    match (&st.binary, st.socket_unit.as_deref() == Some("active") || st.socket_listening) {
        (None, _) => Finding {
            severity: Severity::Info,
            check,
            status: "lindesk-mcp not installed".into(),
            remedy: Some("it ships with the lindesk package (cargo build --release -p lindesk-mcp)".into()),
        },
        (Some(_), true) => Finding {
            severity: Severity::Ok,
            check,
            status: format!(
                "stdio ready; socket listening at {}",
                st.socket_path.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
            ),
            remedy: None,
        },
        (Some(_), false) => Finding {
            severity: Severity::Ok,
            check,
            status: format!("stdio ready ({ADD_CLAUDE}); user socket off (optional: {ENABLE_SOCKET})"),
            remedy: None,
        },
    }
}

pub fn render(ui: &Ui, st: &McpStatus) -> String {
    let row = |k: &str, v: String| format!("{}  {v}\n", ui.bold(format!("{k:<12}")));
    let mut s = String::new();
    match &st.binary {
        Some(b) => s.push_str(&row("lindesk-mcp", b.display().to_string())),
        None => {
            s.push_str(&row("lindesk-mcp", ui.red("not installed")));
            s.push_str(&format!(
                "\n{}\n",
                ui.cyan("install the lindesk package, or from a checkout: cargo build --release -p lindesk-mcp")
            ));
            return s;
        }
    }
    s.push_str(&row(
        "stdio",
        format!("{} {}", ui.green("ready"), ui.dim("(clients start it on demand; no service needed)")),
    ));
    let active = st.socket_unit.as_deref() == Some("active");
    let sock = match (active, st.socket_listening) {
        (true, _) => ui.green(format!("active ({})", st.socket_enabled.clone().unwrap_or_else(|| "enabled?".into()))),
        (false, true) => ui.green("listening (started by hand)"),
        (false, false) if st.socket_enabled.as_deref() == Some("not-found") => {
            ui.dim("not installed (the lindesk package installs lindesk-mcp.socket; optional)")
        }
        (false, false) => ui.dim(format!(
            "{} ({})",
            st.socket_unit.clone().unwrap_or_else(|| "unknown".into()),
            st.socket_enabled.clone().unwrap_or_else(|| "not installed".into())
        )),
    };
    s.push_str(&row("user socket", sock));
    if let Some(p) = &st.socket_path {
        s.push_str(&row("endpoint", format!("unix:{}  {}", p.display(), ui.dim("(HTTP /mcp, mode 0600)"))));
    }
    let dot = |up: bool| if up { ui.green("running") } else { ui.yellow("not running") };
    s.push_str(&row(
        "daemons",
        format!(
            "lindesk-session {} · lindeskd {}  {}",
            dot(st.session_daemon),
            dot(st.broker),
            ui.dim("(changes go through them; read-only tools work without)")
        ),
    ));
    s.push_str(&format!("\n{}\n", ui.bold("Connect an agent")));
    s.push_str(&format!("  {}   {}\n", ui.cyan(ADD_CLAUDE), ui.dim("# Claude Code (stdio)")));
    s.push_str(&format!(
        "  {}   {}\n",
        ui.cyan("lindeskctl mcp config --client claude-desktop"),
        ui.dim("# or cursor, generic")
    ));
    if !active && !st.socket_listening {
        s.push_str(&format!("  {}   {}\n", ui.cyan(ENABLE_SOCKET), ui.dim("# optional: a running HTTP endpoint")));
    }
    s.push_str(&ui.dim(
        "\nAgents can assess and tune safely: every change is guardrailed, versioned and auto-reverts unless confirmed.",
    ));
    s
}

fn status(ui: &Ui) -> Result<()> {
    let st = probe();
    if ui.json {
        return ui.print_json(&st);
    }
    out(&render(ui, &st));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st() -> McpStatus {
        McpStatus {
            binary: Some("/usr/bin/lindesk-mcp".into()),
            stdio_ready: true,
            socket_unit: Some("inactive".into()),
            socket_enabled: Some("disabled".into()),
            service_unit: Some("inactive".into()),
            socket_path: Some("/run/user/1000/lindesk/mcp.sock".into()),
            socket_listening: false,
            session_daemon: true,
            broker: false,
        }
    }

    #[test]
    fn renders_status_and_hints() {
        let s = render(&Ui::plain(), &st());
        assert!(s.contains("lindesk-mcp   /usr/bin/lindesk-mcp"), "{s}");
        assert!(s.contains("user socket   inactive (disabled)"), "{s}");
        assert!(s.contains(ENABLE_SOCKET) && s.contains(ADD_CLAUDE), "{s}");
        assert!(s.contains("lindesk-session running · lindeskd not running"), "{s}");
        let f = finding(&st());
        assert_eq!(f.severity, Severity::Ok);
        assert!(f.status.contains("user socket off"), "{}", f.status);
        let on = McpStatus { socket_unit: Some("active".into()), socket_enabled: Some("enabled".into()), ..st() };
        assert!(!render(&Ui::plain(), &on).contains(ENABLE_SOCKET));
        let none = McpStatus { binary: None, stdio_ready: false, ..st() };
        assert!(render(&Ui::plain(), &none).contains("not installed"));
        assert_eq!(finding(&none).severity, Severity::Info);
    }
}
