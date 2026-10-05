//! `lindeskctl on | off | pause | resume`.
//!
//! * `off`: stop and disable the daemons (they restore everything on stop; a stale applied
//!   state is restored explicitly), put conflicting daemons back (revert the changesets that
//!   disabled them, then the install-time snapshot), and record `lindesk_enabled: false` in
//!   `/var/lib/lindesk/install-state.json` so upgrades and package scripts keep it off.
//!   Config, profiles, change history and telemetry stay.
//! * `on`: the reverse, with the same conflict prompt as the installer.
//! * `pause [--for D]` / `resume`: observe-only without stopping anything (`SetPaused`).
//!
//! systemd is driven with `systemctl` (`$LINDESK_SYSTEMCTL` overrides it, for tests).

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use lindesk_changes::install_state;
use serde_json::{Value, json};

use crate::bus::{self, Daemon};
use crate::cli::{OffArgs, OnArgs, SetupOrigin};
use crate::cmd::setup::{self, Choice, Env};
use crate::paths;
use crate::ui::{self, Exit, Ui, out};

const SESSION_UNITS: &[&str] = &["lindesk-session.service", "lindesk-mcp.socket"];

fn systemctl_program() -> PathBuf {
    lindesk_changes::Systemctl::from_env().program
}

/// Run `systemctl ARGS`; `Err` with its stderr on failure.
fn systemctl(args: &[&str]) -> Result<()> {
    let o = Command::new(systemctl_program())
        .args(args)
        .output()
        .map_err(|e| anyhow!("running systemctl {}: {e}", args.join(" ")))?;
    if o.status.success() {
        Ok(())
    } else {
        Err(anyhow!("systemctl {} failed: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim()))
    }
}

/// Logged-in users (a running user manager under /run/user), as (name, uid).
fn logged_in_users() -> Vec<(String, u32)> {
    let mut v = Vec::new();
    for e in std::fs::read_dir("/run/user").into_iter().flatten().flatten() {
        let Some(uid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        if uid < 1000 {
            continue;
        }
        let name = Command::new("id")
            .args(["-nu", &uid.to_string()])
            .output()
            .ok()
            .and_then(|o| o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string()));
        if let Some(n) = name.filter(|n| !n.is_empty()) {
            v.push((n, uid));
        }
    }
    v
}

/// An applied-state file that still lists settings (a daemon died without restoring).
fn stale_state(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|s| !matches!(s.trim(), "" | "[]"))
}

fn runtime_state_file() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("lindesk/state.json"))
}

/// Record on/off in the install-time state (root only; the file is root's).
fn record(env: &Env, enabled: bool, report: &mut Vec<String>) {
    if !paths::is_root() {
        return;
    }
    match install_state::set_enabled(
        &env.state_path(),
        enabled,
        paths::uid(),
        env!("CARGO_PKG_VERSION"),
        &**env.services(),
        lindesk_core::clock::now_ms(),
    ) {
        Ok(_) => report.push(format!(
            "recorded lindesk_enabled: {enabled} in {} ({})",
            env.state_path().display(),
            if enabled { "upgrades start it again" } else { "upgrades and package scripts keep it off" }
        )),
        Err(e) => report.push(format!("could not record the state in {}: {e}", env.state_path().display())),
    }
}

pub fn off(ui: &Ui, a: &OffArgs) -> Result<()> {
    let root = paths::is_root();
    let mut done: Vec<String> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    fn note(r: Result<()>, ok: String, done: &mut Vec<String>, problems: &mut Vec<String>) {
        match r {
            Ok(()) => done.push(ok),
            Err(e) => problems.push(e.to_string()),
        }
    }

    // Session agents first (they relay to the broker), then the broker.
    if root {
        let users = if a.all_users { logged_in_users() } else { Vec::new() };
        for (name, _) in &users {
            let m = format!("--machine={name}@");
            let mut args = vec!["--user", m.as_str(), "mask", "--now"];
            args.extend(SESSION_UNITS);
            note(systemctl(&args), format!("stopped and masked lindesk-session for {name}"), &mut done, &mut problems);
        }
        let mut args = vec!["--global", "disable"];
        args.extend(SESSION_UNITS);
        note(systemctl(&args), "lindesk-session no longer starts at login".into(), &mut done, &mut problems);
        if users.is_empty() {
            done.push("running session agents stop at logout (now: lindeskctl off --all-users)".into());
        }
        note(
            systemctl(&["disable", "--now", "lindeskd.service"]),
            "stopped and disabled lindeskd (it restored everything on stop)".into(),
            &mut done,
            &mut problems,
        );
    } else {
        let mut args = vec!["--user", "mask", "--now"];
        args.extend(SESSION_UNITS);
        note(
            systemctl(&args),
            "stopped and masked your lindesk-session (it restored your units)".into(),
            &mut done,
            &mut problems,
        );
        if bus::running(Daemon::Broker).is_ok() {
            // polkit may ask; without it the system part needs sudo.
            match systemctl(&["disable", "--now", "lindeskd.service"]) {
                Ok(()) => done.push("stopped and disabled lindeskd".into()),
                Err(_) => problems.push("lindeskd is still running: sudo lindeskctl off".into()),
            }
        }
    }

    // Both daemons restore on stop (ExecStopPost=--restore); finish a stale state.
    let checks: Vec<(PathBuf, &str)> = if root {
        vec![(PathBuf::from("/run/lindesk/state.json"), "lindeskd")]
    } else {
        runtime_state_file().map(|p| vec![(p, "lindesk-session")]).unwrap_or_default()
    };
    for (path, bin) in checks {
        if stale_state(&path) {
            let st = Command::new(bin).arg("--restore").status();
            match st {
                Ok(s) if s.success() => done.push(format!("restored the leftovers in {}", path.display())),
                _ => problems.push(format!("{bin} --restore failed; leftovers in {}", path.display())),
            }
        }
    }

    // Conflicting daemons back to how they were.
    let env = Env::new(None, SetupOrigin::Cli)?;
    if root && !a.keep_conflicts {
        if let Err(e) = setup::restore_conflicts_cmd(ui, &env, true) {
            problems.push(format!("restoring conflicting daemons: {e}"));
        }
        let choice = if a.yes { Choice::Restore } else { Choice::Ask };
        if let Err(e) = setup::reconcile_conflicts(ui, &env, choice) {
            problems.push(format!("reconciling conflicting daemons: {e}"));
        }
    } else if !root && !a.keep_conflicts {
        done.push("conflicting daemons: unchanged (restoring them needs root: sudo lindeskctl off)".into());
    }
    record(&env, false, &mut done);

    if ui.json {
        return ui.print_json(&json!({"state": "off", "done": done, "problems": problems}));
    }
    out(&format!("{} lindesk is off: the machine is back to its default behaviour.", ui.ok_mark()));
    for d in &done {
        out(&format!("  {}", ui.dim(d)));
    }
    for p in &problems {
        out(&format!("  {}", ui.yellow(p)));
    }
    out(&ui.dim("  Config, profiles, change history and telemetry are kept. Turn it back on: lindeskctl on"));
    if problems.is_empty() { Ok(()) } else { Err(Exit(1).into()) }
}

pub fn on(ui: &Ui, a: &OnArgs) -> Result<()> {
    let root = paths::is_root();
    let mut done: Vec<String> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let mut step = |r: Result<()>, ok: &str| match r {
        Ok(()) => done.push(ok.to_string()),
        Err(e) => problems.push(e.to_string()),
    };
    if root {
        step(systemctl(&["enable", "--now", "lindeskd.service"]), "enabled and started lindeskd");
        let mut args = vec!["--global", "enable"];
        args.push(SESSION_UNITS[0]);
        step(systemctl(&args), "lindesk-session starts at every login");
        if a.all_users {
            for (name, _) in logged_in_users() {
                let m = format!("--machine={name}@");
                let r = systemctl(&["--user", m.as_str(), "unmask", SESSION_UNITS[0], SESSION_UNITS[1]])
                    .and_then(|()| systemctl(&["--user", m.as_str(), "start", SESSION_UNITS[0]]));
                step(r, &format!("started lindesk-session for {name}"));
            }
        }
    } else {
        let r = systemctl(&["--user", "unmask", SESSION_UNITS[0], SESSION_UNITS[1]])
            .and_then(|()| systemctl(&["--user", "enable", "--now", SESSION_UNITS[0]]));
        step(r, "enabled and started your lindesk-session");
        if bus::running(Daemon::Broker).is_err() && systemctl(&["enable", "--now", "lindeskd.service"]).is_err() {
            problems.push("lindeskd isn't running (system units unmanaged): sudo lindeskctl on".into());
        }
    }
    let env = Env::new(None, SetupOrigin::Cli)?;
    if root {
        let running: Vec<String> =
            setup::installed(&**env.services()).into_iter().filter(|u| u.1 || u.2).map(|u| u.0).collect();
        if !running.is_empty() {
            let disable = if a.disable_conflicts {
                true
            } else if a.keep_conflicts {
                false
            } else {
                ui.stdin_tty
                    && ui::confirm(&format!(
                        "{} also re-prioritise the foreground and will fight lindesk. Disable (revertible)?",
                        running.join(", ")
                    ))
            };
            if disable {
                if let Err(e) = setup::disable_conflicts(ui, &env, true) {
                    problems.push(format!("disabling conflicting daemons: {e}"));
                }
            } else {
                done.push(format!(
                    "kept {} running (they will fight lindesk): sudo lindeskctl setup conflicts --disable",
                    running.join(", ")
                ));
            }
        }
    }
    record(&env, true, &mut done);

    // Wait until the daemons answer.
    let want: Vec<Daemon> = if root { vec![Daemon::Broker] } else { vec![Daemon::Session] };
    let deadline = Instant::now() + Duration::from_secs(a.wait);
    let mut answering = Vec::new();
    for d in want {
        loop {
            if bus::status_json(d).is_ok() {
                answering.push(d.name());
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    if ui.json {
        return ui.print_json(&json!({"state": "on", "done": done, "answering": answering, "problems": problems}));
    }
    for d in &done {
        out(&format!("  {}", ui.dim(d)));
    }
    for p in &problems {
        out(&format!("  {}", ui.yellow(p)));
    }
    if answering.is_empty() {
        out(&ui.yellow("lindesk was enabled but doesn't answer yet; check: lindeskctl doctor"));
        return Err(Exit(1).into());
    }
    out(&format!("{} lindesk is on", ui.ok_mark()));
    crate::cmd::status::bare(ui)?;
    if problems.is_empty() { Ok(()) } else { Err(Exit(1).into()) }
}

fn pause_text(v: &Value) -> String {
    match v["until_ms"].as_i64() {
        Some(u) => format!("resumes in {}", ui::duration_ms((u - lindesk_core::clock::now_ms()).max(0))),
        None => "until lindeskctl resume".into(),
    }
}

/// Pause/resume every daemon that answers (the broker may need polkit).
fn set_all(ui: &Ui, paused: bool, seconds: u32) -> Result<()> {
    let mut results = Vec::new();
    let mut ok = 0;
    for d in [Daemon::Session, Daemon::Broker] {
        if bus::running(d).is_err() {
            continue;
        }
        match bus::set_paused(d, paused, seconds) {
            Ok(j) => {
                ok += 1;
                let v: Value = serde_json::from_str(&j).unwrap_or_default();
                results.push(json!({"daemon": d.name(), "pause": v}));
            }
            Err(e) => {
                let hint = if d == Daemon::Broker {
                    format!(
                        " (system units are still managed: sudo lindeskctl {})",
                        if paused { "pause" } else { "resume" }
                    )
                } else {
                    String::new()
                };
                results.push(json!({"daemon": d.name(), "error": format!("{e}{hint}")}));
            }
        }
    }
    if ui.json {
        return ui.print_json(&results);
    }
    if results.is_empty() {
        return Err(Daemon::Session.not_running());
    }
    for r in &results {
        match (r.get("pause"), r.get("error")) {
            (Some(p), _) if paused => out(&format!(
                "{} {} paused ({}): every setting it applied is reverted; it keeps observing",
                ui.yellow("⏸"),
                r["daemon"].as_str().unwrap_or("?"),
                pause_text(p)
            )),
            (Some(_), _) => out(&format!("{} {} resumed", ui.ok_mark(), r["daemon"].as_str().unwrap_or("?"))),
            (_, Some(e)) => {
                out(&ui.yellow(format!("{}: {}", r["daemon"].as_str().unwrap_or("?"), e.as_str().unwrap_or(""))))
            }
            _ => {}
        }
    }
    if ok == 0 { Err(Exit(1).into()) } else { Ok(()) }
}

pub fn pause(ui: &Ui, dur: Option<Duration>) -> Result<()> {
    let secs = dur.map_or(0, |d| u32::try_from(d.as_secs().max(1)).unwrap_or(u32::MAX));
    set_all(ui, true, secs)
}

pub fn resume(ui: &Ui) -> Result<()> {
    set_all(ui, false, 0)
}
