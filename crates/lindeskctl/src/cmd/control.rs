//! `explain`, `run`, `set` and `restore`: steering lindesk.

use std::ffi::OsStr;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use lindesk_host::Explanation;
use lindesk_plugin_api::{IoPrioClass, QosClass};

use crate::bus::{self, Daemon};
use crate::cli::RunArgs;
use crate::paths;
use crate::procs::{self, Subject};
use crate::ui::{Exit, Ui, out};

const PROC: &str = "/proc";

// ----------------------------------------------------------------------------- explain

pub fn explain(ui: &Ui, what: &str) -> Result<()> {
    let arg = match procs::subject(what) {
        Subject::Pid(p) => p.to_string(),
        Subject::Target(t) => t,
        Subject::Name(n) => {
            let pid = procs::resolve_name(Path::new(PROC), &n)?;
            ui.info(ui.dim(format!("{n} → pid {pid}")));
            pid.to_string()
        }
    };
    let order: &[Daemon] = if ui.system { &[Daemon::Broker] } else { &[Daemon::Session, Daemon::Broker] };
    let mut first_err = None;
    for &d in order {
        match bus::explain_json(d, &arg) {
            Ok(raw) => return print_explanation(ui, d, &raw),
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    Err(first_err.unwrap_or_else(|| Daemon::Session.not_running()))
}

fn print_explanation(ui: &Ui, d: Daemon, raw: &str) -> Result<()> {
    let v: serde_json::Value =
        serde_json::from_str(raw).with_context(|| format!("{} returned invalid JSON from Explain", d.name()))?;
    if ui.json {
        return ui.print_json(&v);
    }
    match serde_json::from_value::<Explanation>(v.clone()) {
        Ok(e) => {
            let text = e.to_string();
            let mut lines = text.lines();
            let mut s = String::new();
            if let Some(head) = lines.next() {
                s.push_str(&ui.bold(head));
                s.push('\n');
            }
            for l in lines {
                s.push_str(l);
                s.push('\n');
            }
            if !ui.quiet {
                s.push_str(&ui.dim(format!("(from {})", d.name())));
            }
            out(&s);
        }
        Err(_) => {
            ui.warn(format!("{}'s explanation has an unknown format; showing it raw", d.name()));
            ui.print_json(&v)?;
        }
    }
    Ok(())
}

// --------------------------------------------------------------------------------- set

pub fn set(ui: &Ui, what: &str, class: QosClass) -> Result<()> {
    let pid = procs::resolve_pid(Path::new(PROC), what)?;
    let d = Daemon::for_scope(ui.system);
    bus::set_class(d, pid, class.as_str())?;
    if ui.json {
        return ui.print_json(&serde_json::json!({"pid": pid, "class": class, "daemon": d.name()}));
    }
    let comm = lindesk_sys::procfs::comm(Path::new(PROC), pid).unwrap_or_default();
    ui.info(format!("{} {comm} (pid {pid}) is now {}", ui.ok_mark(), ui.bold(class)));
    ui.info(ui.dim(format!("check it: lindeskctl explain {pid}")));
    Ok(())
}

// --------------------------------------------------------------------------------- run

static CHILD: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward(sig: libc::c_int) {
    let pid = CHILD.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: kill is async-signal-safe.
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

/// The slice a class runs in.
pub fn slice_for(class: Option<QosClass>) -> &'static str {
    match class {
        Some(QosClass::Batch) => "lindesk-batch.slice",
        Some(QosClass::Background) => "background.slice",
        _ => "app.slice",
    }
}

/// A unit-name-safe version of `s`.
pub fn sanitize(s: &str) -> String {
    let mut out: String =
        s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
    out.truncate(48);
    if out.is_empty() { "cmd".into() } else { out }
}

fn user_manager_available() -> bool {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| format!("/run/user/{}", paths::uid()).into());
    paths::which("systemd-run").is_some() && runtime.join("systemd/private").exists()
}

/// The full systemd-run command line for a scope.
pub fn systemd_run_args(
    unit: &str,
    slice: &str,
    description: &str,
    command: &[std::ffi::OsString],
) -> Vec<std::ffi::OsString> {
    let mut v: Vec<std::ffi::OsString> = ["--user", "--scope", "--collect", "--quiet"].iter().map(Into::into).collect();
    v.push(format!("--slice={slice}").into());
    v.push(format!("--unit={unit}").into());
    v.push(format!("--description={description}").into());
    v.push("--".into());
    v.extend(command.iter().cloned());
    v
}

fn in_unit(pid: i32, unit: &str) -> bool {
    lindesk_sys::procfs::cgroup(Path::new(PROC), pid).ok().flatten().is_some_and(|c| c.contains(unit))
}

pub fn run(ui: &Ui, args: &RunArgs) -> Result<()> {
    let class = args.class();
    let program = args.command.first().context("no command given")?;
    let base = args
        .name
        .clone()
        .unwrap_or_else(|| Path::new(program).file_name().unwrap_or(OsStr::new("cmd")).to_string_lossy().into_owned());
    let nonce =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let unit = format!("lindesk-run-{}-{}{:x}.scope", sanitize(&base), std::process::id(), nonce & 0xffff);
    let slice = slice_for(class);
    let systemd = user_manager_available();

    let mut cmd;
    if systemd {
        let desc = format!("lindeskctl run{}: {base}", class.map(|c| format!(" --class {c}")).unwrap_or_default());
        cmd = Command::new("systemd-run");
        cmd.args(systemd_run_args(&unit, slice, &desc, &args.command));
    } else {
        cmd = Command::new(program);
        cmd.args(&args.command[1..]);
        let prio = match class {
            Some(QosClass::Batch) => Some((19i8, IoPrioClass::Idle, 7u8)),
            Some(QosClass::Background) => Some((10, IoPrioClass::BestEffort, 7)),
            _ => None,
        };
        if let Some((nice, io, level)) = prio {
            ui.info(
                ui.dim(format!("no systemd user manager: running with nice {nice} and {io:?} I/O priority instead")),
            );
            // SAFETY: the closure only makes two raw syscalls, which is fork-safe.
            unsafe {
                cmd.pre_exec(move || {
                    let _ = lindesk_sys::syscalls::set_nice(0, nice);
                    let _ = lindesk_sys::syscalls::ioprio_set(0, io, level);
                    Ok(())
                });
            }
        }
    }
    if let Some(us) = args.slice_us {
        if std::fs::read_to_string("/sys/kernel/sched_ext/state").is_ok_and(|s| s.trim() == "enabled") {
            bail!("--slice-us requires the fair scheduler; sched_ext is active");
        }
        // SAFETY: this changes only the child's own scheduler attributes using syscalls.
        unsafe {
            cmd.pre_exec(move || lindesk_sys::syscalls::set_child_slice_us(us));
        }
    }
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "{}: command not found",
                if systemd { "systemd-run".into() } else { program.to_string_lossy() }
            )
        } else {
            anyhow::anyhow!("starting {}: {e}", program.to_string_lossy())
        }
    })?;
    let pid = child.id() as i32;
    CHILD.store(pid, Ordering::SeqCst);
    // The terminal sends Ctrl-C/Ctrl-\ to the whole foreground group (the child gets it
    // directly), so ignore them here and report the child's fate; forward the rest.
    // SAFETY: setting dispositions; the handler only calls kill().
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
        for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGUSR1, libc::SIGUSR2, libc::SIGCONT, libc::SIGWINCH] {
            libc::signal(sig, forward as *const () as libc::sighandler_t);
        }
    }

    if let Some(class) = class {
        if systemd {
            // systemd-run moves itself into the scope and then execs the command.
            let deadline = Instant::now() + Duration::from_secs(2);
            while !in_unit(pid, &unit) && Instant::now() < deadline {
                if let Ok(Some(_)) = child.try_wait() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let d = Daemon::for_scope(ui.system);
        if let Err(e) = bus::set_class(d, pid, class.as_str()) {
            let why = e.to_string().lines().next().unwrap_or_default().to_string();
            if systemd {
                ui.warn(format!("couldn't tell {} the class ({why}); the {slice} rules still classify it", d.name()));
            } else {
                ui.warn(format!("couldn't tell {} the class ({why})", d.name()));
            }
        }
    }

    let status = child.wait().context("waiting for the command")?;
    CHILD.store(0, Ordering::SeqCst);
    if let Some(code) = status.code() {
        if code != 0 {
            return Err(Exit(code.clamp(0, 255) as u8).into());
        }
        return Ok(());
    }
    if let Some(sig) = status.signal() {
        // Die the same way, so shells see an interrupted command.
        // SAFETY: restoring the default disposition and raising a signal.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
        return Err(Exit((128 + sig).clamp(0, 255) as u8).into());
    }
    Ok(())
}

// ----------------------------------------------------------------------------- restore

fn systemctl(system: bool, args: &[&str]) -> std::io::Result<std::process::Output> {
    let mut c = Command::new("systemctl");
    if !system {
        c.arg("--user");
    }
    c.args(args).output()
}

pub fn restore(ui: &Ui, stop: bool) -> Result<()> {
    let d = Daemon::for_scope(ui.system);
    if ui.system {
        paths::require_root("restoring system settings")?;
    }
    let running = matches!(bus::reach(d), bus::Reach::Running);
    let unit = format!("{}.service", d.name());
    if running {
        if !stop {
            bail!(
                "{} is running and would re-apply its settings on the next tick.\n  Stop it (its unit restores everything on stop): lindeskctl restore{} --stop\n  Or keep it running in observe-only mode: set [general] dry_run = true and reload.",
                d.name(),
                if ui.system { " --system" } else { "" }
            );
        }
        ui.info(format!("stopping {unit} (ExecStopPost restores every setting)…"));
        let o = systemctl(ui.system, &["stop", &unit]).context("running systemctl")?;
        if !o.status.success() {
            bail!("systemctl stop {unit} failed: {}", String::from_utf8_lossy(&o.stderr).trim());
        }
    }
    let Some(bin) = paths::which(d.name()) else {
        bail!(
            "{} is not on PATH, so there's nothing to restore with. Settings are runtime-only: a reboot (or `systemctl{} daemon-reexec` for unit properties) clears them.",
            d.name(),
            if ui.system { "" } else { " --user" }
        );
    };
    let o =
        Command::new(&bin).arg("--restore").output().with_context(|| format!("running {} --restore", bin.display()))?;
    let stdout = String::from_utf8_lossy(&o.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
    if !o.status.success() {
        bail!("{} --restore failed ({}): {}", d.name(), o.status, if stderr.is_empty() { &stdout } else { &stderr });
    }
    if ui.json {
        return ui.print_json(&serde_json::json!({"daemon": d.name(), "stopped": running, "output": stdout}));
    }
    if !stdout.is_empty() {
        out(&stdout);
    }
    ui.info(format!(
        "{} restored everything {} had changed{}.",
        ui.ok_mark(),
        d.name(),
        if running {
            format!(" and stopped it; start it again with: {}", d.start_hint().replace("enable --now", "start"))
        } else {
            String::new()
        }
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_and_names() {
        assert_eq!(slice_for(Some(QosClass::Batch)), "lindesk-batch.slice");
        assert_eq!(slice_for(Some(QosClass::Background)), "background.slice");
        assert_eq!(slice_for(Some(QosClass::Interactive)), "app.slice");
        assert_eq!(slice_for(None), "app.slice");
        assert_eq!(sanitize("make -j32/x"), "make_-j32_x");
        assert_eq!(sanitize(""), "cmd");
    }

    #[test]
    fn systemd_run_command_line() {
        let a =
            systemd_run_args("lindesk-run-make-1.scope", "lindesk-batch.slice", "d", &["make".into(), "-j32".into()]);
        let a: Vec<String> = a.iter().map(|s| s.to_string_lossy().into_owned()).collect();
        assert_eq!(
            a,
            [
                "--user",
                "--scope",
                "--collect",
                "--quiet",
                "--slice=lindesk-batch.slice",
                "--unit=lindesk-run-make-1.scope",
                "--description=d",
                "--",
                "make",
                "-j32"
            ]
        );
    }
}
