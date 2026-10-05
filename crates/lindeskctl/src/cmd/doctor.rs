//! `lindeskctl doctor`: the kernel/desktop probe plus reachability of lindesk's own pieces.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use lindesk_sys::doctor::{Finding, Severity, findings, render_json, render_text};
use lindesk_sys::probe::DesktopKind;
use lindesk_sys::requirements::{self, CheckOptions, Component, Requirement, RequirementsReport, Status as ReqStatus};
use lindesk_sys::{Env, Probe, Roots};
use serde_json::json;

use crate::bus::{self, Daemon, Reach};
use crate::paths;
use crate::ui::{Exit, Ui, bytes};

const WIDTH: usize = 25;
const GNOME_UUID: &str = "lindesk@lindesk.org";

fn finding(severity: Severity, check: &str, status: impl Into<String>, remedy: Option<String>) -> Finding {
    Finding { severity, check: check.into(), status: status.into(), remedy }
}

fn daemon_finding(d: Daemon) -> Finding {
    let check = d.name();
    match bus::reach(d) {
        Reach::Running => finding(Severity::Ok, check, format!("running ({})", d.bus_name()), None),
        Reach::NotRunning => {
            let installed = paths::which(d.name()).is_some();
            let remedy = if installed {
                Some(d.start_hint().to_string())
            } else {
                Some(
                    "install lindesk: sudo apt install ./lindesk_*.deb (or dnf/AUR), or ./packaging/install.sh"
                        .to_string(),
                )
            };
            finding(Severity::Warn, check, if installed { "not running" } else { "not installed" }, remedy)
        }
        Reach::NoBus(e) => {
            let sev = if d == Daemon::Session { Severity::Info } else { Severity::Warn };
            finding(sev, check, format!("unknown: no bus ({e})"), None)
        }
    }
}

/// The GNOME extension or KWin script; COSMIC and X11 adapters are built in.
fn adapter_finding(p: &Probe) -> Finding {
    match p.desktop.kind {
        DesktopKind::Gnome => gnome_extension(),
        DesktopKind::Kde => kwin_script(),
        DesktopKind::Cosmic => finding(Severity::Ok, "Desktop adapter", "COSMIC (built into lindesk-session)", None),
        _ if p.desktop.session == lindesk_sys::probe::SessionType::X11 => {
            finding(Severity::Ok, "Desktop adapter", "X11 (built into lindesk-session)", None)
        }
        _ => finding(
            Severity::Info,
            "Desktop adapter",
            "none for this desktop",
            Some(
                "foreground tracking needs GNOME, KDE, COSMIC or X11; apps can still call SetForegroundProcess".into(),
            ),
        ),
    }
}

fn data_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(h) = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute()) {
        v.push(h);
    } else if let Some(h) = std::env::var_os("HOME") {
        v.push(PathBuf::from(h).join(".local/share"));
    }
    let sys = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    v.extend(sys.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    v
}

fn gnome_extension() -> Finding {
    const CHECK: &str = "GNOME extension";
    let enable = format!("gnome-extensions enable {GNOME_UUID}");
    if paths::which("gnome-extensions").is_some()
        && let Ok(out) = Command::new("gnome-extensions").args(["info", GNOME_UUID]).output()
    {
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        if out.status.success() {
            let state = text
                .lines()
                .find_map(|l| l.trim().strip_prefix("State:"))
                .map(|s| s.trim().to_ascii_uppercase())
                .unwrap_or_default();
            let enabled = text.lines().find_map(|l| l.trim().strip_prefix("Enabled:")).map(|s| s.trim() == "Yes");
            return match (state.as_str(), enabled) {
                ("ACTIVE" | "ENABLED", _) | (_, Some(true)) if state != "ERROR" => {
                    finding(Severity::Ok, CHECK, format!("{GNOME_UUID} enabled"), None)
                }
                ("ERROR", _) => finding(
                    Severity::Warn,
                    CHECK,
                    "installed, failed to load",
                    Some(format!("check: journalctl --user -b | grep -i {GNOME_UUID}")),
                ),
                _ => finding(
                    Severity::Warn,
                    CHECK,
                    format!("installed, {}", if state.is_empty() { "disabled".into() } else { state.to_lowercase() }),
                    Some(enable),
                ),
            };
        }
    }
    let installed = data_dirs().iter().any(|d| d.join("gnome-shell/extensions").join(GNOME_UUID).is_dir());
    if installed {
        finding(Severity::Warn, CHECK, "installed, state unknown", Some(format!("{enable} (then log out and in)")))
    } else {
        finding(
            Severity::Warn,
            CHECK,
            "not installed",
            Some(
                "foreground tracking on GNOME needs it: install the lindesk package or adapters/gnome-shell-extension"
                    .into(),
            ),
        )
    }
}

fn kwin_script() -> Finding {
    const CHECK: &str = "KWin script";
    let installed = data_dirs().iter().any(|d| d.join("kwin/scripts/lindesk").is_dir());
    if !installed {
        return finding(
            Severity::Warn,
            CHECK,
            "not installed",
            Some("foreground tracking on KDE needs it: install the lindesk package or adapters/kwin-script".into()),
        );
    }
    let enabled = paths::which("kreadconfig6")
        .or_else(|| paths::which("kreadconfig5"))
        .and_then(|k| {
            Command::new(k).args(["--file", "kwinrc", "--group", "Plugins", "--key", "lindeskEnabled"]).output().ok()
        })
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true");
    match enabled {
        Some(true) => finding(Severity::Ok, CHECK, "lindesk enabled", None),
        _ => finding(
            Severity::Warn,
            CHECK,
            "installed, not enabled",
            Some("System Settings → Window Management → KWin Scripts → lindesk".into()),
        ),
    }
}

fn telemetry_finding(system: bool) -> (Finding, serde_json::Value) {
    let config = paths::load_config(&paths::config_path(system)).ok();
    let root = paths::lake_root(None, system, config.as_ref());
    let enabled = config.as_ref().is_none_or(|c| c.telemetry.enabled);
    let installed = lindesk_sys::telemetry::installed();
    let check = if system { "Telemetry (system)" } else { "Telemetry" };
    let size = paths::dir_size(&root);
    let max = config.as_ref().map(|c| c.telemetry.retention.max_size).unwrap_or(2 << 30);
    let info = json!({
        "installed": installed, "dir": root, "bytes": size, "enabled": enabled, "max_size": max, "exists": root.is_dir()
    });
    let f = if !installed {
        let left = if root.is_dir() && size > 0 {
            format!("; {} left in {}", bytes(size), tilde(&root))
        } else {
            String::new()
        };
        finding(
            Severity::Info,
            check,
            format!("not installed (optional){left}"),
            Some(
                "to record history: sudo apt install lindesk-telemetry (or ./packaging/install.sh --with-telemetry)"
                    .into(),
            ),
        )
    } else if !enabled {
        finding(Severity::Info, check, "disabled (telemetry.enabled = false)", None)
    } else if !root.is_dir() {
        finding(Severity::Info, check, format!("installed, no data yet ({})", tilde(&root)), None)
    } else if max > 0 && size > max + max / 10 {
        finding(
            Severity::Warn,
            check,
            format!("installed, {} in {} (over max_size {})", bytes(size), tilde(&root), bytes(max)),
            Some("lindesk-compact.timer prunes it; run now: lindeskctl data prune".into()),
        )
    } else {
        finding(Severity::Ok, check, format!("installed, {} in {}", bytes(size), tilde(&root)), None)
    };
    (f, info)
}

fn config_finding(system: bool) -> Finding {
    let path = paths::config_path(system);
    let check = if system { "Config (system)" } else { "Config" };
    if !path.exists() {
        return finding(Severity::Ok, check, format!("{} not present; defaults apply", tilde(&path)), None);
    }
    match lindesk_core::config::Config::load(&path) {
        Ok(_) => finding(Severity::Ok, check, format!("{} valid", tilde(&path)), None),
        Err(e) => finding(
            Severity::Error,
            check,
            format!("{} invalid", tilde(&path)),
            Some(format!(
                "{}; details: lindeskctl config check{}",
                first_line(&e.to_string()),
                if system { " --system" } else { "" }
            )),
        ),
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().to_string()
}

/// `/home/me/x` → `~/x`.
pub fn tilde(p: &Path) -> String {
    if let Some(h) = std::env::var_os("HOME").map(PathBuf::from)
        && let Ok(rest) = p.strip_prefix(&h)
    {
        return format!("~/{}", rest.display());
    }
    p.display().to_string()
}

/// The lindesk-specific checks.
pub fn service_findings(p: &Probe) -> (Vec<Finding>, serde_json::Value) {
    let mut v = vec![daemon_finding(Daemon::Broker), daemon_finding(Daemon::Session), adapter_finding(p)];
    v.push(config_finding(false));
    if paths::is_root() || std::path::Path::new(paths::SYSTEM_CONFIG).exists() {
        v.push(config_finding(true));
    }
    let (user, user_info) = telemetry_finding(false);
    v.push(user);
    let mut tele = json!({"user": user_info});
    let sys_root = paths::lake_root(None, true, None);
    if lindesk_sys::telemetry::installed() && std::fs::read_dir(&sys_root).is_ok() {
        let (f, info) = telemetry_finding(true);
        v.push(f);
        tele["system"] = info;
    }
    (v, tele)
}

fn render(ui: &Ui, fs: &[Finding]) -> String {
    let mut s = String::new();
    for f in fs {
        let mut status = f.status.clone();
        if let Some(r) = &f.remedy {
            status.push_str(" → ");
            status.push_str(r);
        }
        status = match f.severity {
            Severity::Ok => status,
            Severity::Info => ui.cyan(status),
            Severity::Warn => ui.yellow(status),
            Severity::Error => ui.red(status),
        };
        let check = if f.check.chars().count() >= WIDTH { format!("{} ", f.check) } else { f.check.clone() };
        s.push_str(&format!("{check:<WIDTH$}{status}\n"));
    }
    s
}

// --------------------------------------------------------------------------- control plane

/// Profiles, pending changes, calibration and the MCP endpoint.
pub fn control_plane_findings(system: bool) -> (Vec<Finding>, serde_json::Value) {
    use crate::cmd::{changes, hw, mcp, profile};
    let mut v = Vec::new();
    let now = lindesk_core::clock::now_ms();
    // Profile: the daemon's choice, else what it would pick.
    let fetched = crate::cmd::status::fetch(system).ok();
    let st = fetched.as_ref().and_then(|f| profile::ProfileState::from_status(&f.raw));
    let prof = match &st {
        Some(p) => {
            let why = if p.reason.is_empty() { String::new() } else { format!(" ({})", p.reason) };
            finding(Severity::Ok, "Profile", format!("{}{why}", p.short()), None)
        }
        None => {
            let lv = profile::local_view(system);
            let a = lv.active.clone().unwrap_or_default();
            let why = lv.reason.clone().map(|r| format!(" ({r})")).unwrap_or_default();
            finding(
                Severity::Info,
                "Profile",
                format!("would be {a}{why}; no daemon applies it"),
                Some("see all: lindeskctl profile list".into()),
            )
        }
    };
    v.push(prof);
    // Changes waiting for confirmation.
    let sources = fetched
        .as_ref()
        .map(|f| crate::cmd::status::pending_sources(f.source))
        .unwrap_or_else(|| if system { vec![Daemon::Broker] } else { vec![Daemon::Session, Daemon::Broker] });
    let pending = changes::pending(&sources);
    v.push(match pending.first() {
        None => finding(Severity::Ok, "Pending changes", "none (nothing will auto-revert)", None),
        Some(p) => finding(
            Severity::Info,
            "Pending changes",
            format!(
                "{} unconfirmed: {}{}",
                pending.len(),
                p.countdown(now),
                if pending.len() > 1 { ", …" } else { "" }
            ),
            Some(format!("keep: {} · list: lindeskctl changes list", p.confirm_hint())),
        ),
    });
    // Calibration.
    let cal = hw::saved_calibration(system);
    v.push(match &cal {
        None => finding(
            Severity::Info,
            "Calibration",
            "not measured: nominal latencies are used",
            Some("lindeskctl calibrate (≈15 s, when idle)".into()),
        ),
        Some((m, p)) => {
            let age_days = (now - m.measured_at) / 86_400_000;
            let mut status = format!("measured {} ({})", changes::ago(m.measured_at, now), tilde(p));
            if m.noisy {
                status.push_str(", NOISY");
            }
            if m.noisy || age_days > 90 {
                finding(Severity::Info, "Calibration", status, Some("re-run when idle: lindeskctl calibrate".into()))
            } else {
                finding(Severity::Ok, "Calibration", status, None)
            }
        }
    });
    let mst = mcp::probe();
    v.push(mcp::finding(&mst));
    let info = json!({
        "profile": st,
        "pending_changes": pending,
        "calibration": hw::calibration_summary(),
        "mcp": mst,
    });
    (v, info)
}

// --------------------------------------------------------------------------- requirements

/// The user a root installer acts for (`SUDO_UID`), else our own uid.
fn session_uid() -> u32 {
    std::env::var("SUDO_UID").ok().and_then(|u| u.parse().ok()).unwrap_or_else(paths::uid)
}

/// Requirement reports for lindeskd, lindesk-session and lindeskctl. lindeskd is checked as
/// root (that's how it runs); lindesk-session as the (sudo) user, without insisting on a
/// session bus when we are root (installers have none).
pub fn requirement_reports(probe: &Probe) -> Vec<RequirementsReport> {
    let roots = Roots::host();
    let env = Env::from_process();
    let root = paths::is_root();
    vec![
        requirements::check_with(&roots, &env, Component::Broker, probe, &CheckOptions::default().euid(0)),
        requirements::check_with(
            &roots,
            &env,
            Component::Session,
            probe,
            &CheckOptions::default().euid(session_uid()).dbus_required(!root),
        ),
        requirements::check(&roots, &env, Component::Cli, probe),
    ]
}

/// Distinct warnings and failures over several reports (shared checks count once).
fn distinct(reports: &[RequirementsReport], status: ReqStatus) -> Vec<&Requirement> {
    let mut seen = std::collections::BTreeSet::new();
    reports
        .iter()
        .flat_map(|r| r.items.iter())
        .filter(|i| i.status == status && seen.insert((i.id, i.detail.clone())))
        .collect()
}

/// `✓ all requirements met`, `⚠ requirements met with 2 warnings`, `✗ 1 requirement failed`.
pub fn requirements_summary(ui: &Ui, reports: &[RequirementsReport]) -> String {
    summary_line(ui, reports, true)
}

fn summary_line(ui: &Ui, reports: &[RequirementsReport], hint: bool) -> String {
    let f = distinct(reports, ReqStatus::Fail).len();
    let w = distinct(reports, ReqStatus::Warn).len();
    let n = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    let more = if hint { " (lindeskctl doctor --preflight)" } else { "" };
    match (f, w) {
        (0, 0) => ui.green("✓ all requirements met"),
        (0, w) => ui.yellow(format!("⚠ requirements met with {}{more}", n(w, "warning"))),
        (f, w) => ui.red(format!("✗ {} failed, {}{more}", n(f, "requirement"), n(w, "warning"))),
    }
}

fn req_line(ui: &Ui, r: &Requirement, title_w: usize, who: &str) -> String {
    let (mark, tag) = match r.status {
        ReqStatus::Pass => (ui.green("✓"), ui.green("PASS")),
        ReqStatus::Warn => (ui.yellow("⚠"), ui.yellow("WARN")),
        ReqStatus::Fail => (ui.red("✗"), ui.paint("1;31", "FAIL")),
    };
    let mut s = format!("  {mark} {tag}  {:<title_w$}  {}{}\n", r.title, r.detail, ui.dim(who));
    if let Some(rem) = &r.remedy
        && r.status != ReqStatus::Pass
    {
        s.push_str(&format!("  {:<w$}{}\n", "", ui.cyan(format!("→ {rem}")), w = 10 + title_w));
    }
    s
}

/// The checklist. `quiet` prints each distinct warning or failure once (with the programs
/// it affects) and nothing at all when everything passes.
pub fn render_preflight(ui: &Ui, reports: &[RequirementsReport], hardware: &str, quiet: bool) -> String {
    let title_w =
        reports.iter().flat_map(|r| r.items.iter()).map(|i| i.title.chars().count()).max().unwrap_or(10).min(28);
    let mut s = String::new();
    if quiet {
        let mut seen: Vec<(&Requirement, Vec<&str>)> = Vec::new();
        for rep in reports {
            for i in rep.items.iter().filter(|i| i.status != ReqStatus::Pass) {
                match seen.iter_mut().find(|(r, _)| r.id == i.id && r.detail == i.detail) {
                    Some((_, who)) => who.push(rep.component.program()),
                    None => seen.push((i, vec![rep.component.program()])),
                }
            }
        }
        if seen.is_empty() {
            return s;
        }
        seen.sort_by_key(|(r, _)| std::cmp::Reverse(r.status));
        for (r, who) in &seen {
            s.push_str(&req_line(ui, r, title_w, &format!("  ({})", who.join(", "))));
        }
        s.push_str(&summary_line(ui, reports, false));
        s.push('\n');
        return s;
    }
    s.push_str(&format!("{}  {}\n", ui.bold("lindesk preflight"), ui.dim(hardware)));
    for rep in reports {
        s.push_str(&format!("\n{}\n", ui.bold(rep.component.program())));
        for i in &rep.items {
            s.push_str(&req_line(ui, i, title_w, ""));
        }
    }
    s.push_str(&format!("\n{}\n", summary_line(ui, reports, false)));
    s
}

pub fn preflight(ui: &Ui) -> Result<()> {
    let probe = lindesk_sys::probe(&Roots::host(), &Env::from_process());
    let reports = requirement_reports(&probe);
    let ok = reports.iter().all(RequirementsReport::ok);
    if ui.json {
        let comps: Vec<serde_json::Value> =
            reports.iter().map(|r| serde_json::from_str(&r.render_json()).unwrap_or_default()).collect();
        ui.print_json(&json!({
            "ok": ok,
            "hardware": probe.hardware,
            "summary": lindesk_sys::hardware::summary_line(&probe.hardware),
            "components": comps,
        }))?;
    } else {
        let text = render_preflight(ui, &reports, &crate::ui::hardware_compact(&probe.hardware), ui.quiet);
        if !text.is_empty() {
            crate::ui::out(text.trim_end_matches('\n'));
        }
    }
    if ok { Ok(()) } else { Err(Exit(1).into()) }
}

pub fn run(ui: &Ui) -> Result<()> {
    let probe = lindesk_sys::probe(&Roots::host(), &Env::from_process());
    let reports = requirement_reports(&probe);
    let (services, telemetry) = service_findings(&probe);
    let (control, control_info) = control_plane_findings(ui.system);
    let needs_action = findings(&probe).iter().chain(services.iter()).any(|f| f.severity >= Severity::Warn);
    if ui.json {
        let mut v: serde_json::Value = serde_json::from_str(&render_json(&probe))?;
        v["services"] = serde_json::to_value(&services)?;
        v["control_plane"] = json!({"findings": control, "details": control_info});
        v["telemetry"] = telemetry;
        v["needs_action"] = json!(needs_action);
        v["requirements"] = serde_json::to_value(&reports)?;
        ui.print_json(&v)?;
    } else {
        let mut s = String::new();
        s.push_str(&format!("{}\n", requirements_summary(ui, &reports)));
        s.push_str(&format!("{}\n\n", ui.dim(crate::ui::hardware_compact(&probe.hardware))));
        s.push_str(&ui.bold("System\n"));
        s.push_str(&render_text(&probe, ui.color));
        s.push('\n');
        s.push_str(&ui.bold("lindesk services\n"));
        s.push_str(&render(ui, &services));
        s.push('\n');
        s.push_str(&ui.bold("Control plane\n"));
        s.push_str(&render(ui, &control));
        s.push('\n');
        s.push_str(&verdict(ui, &probe, &reports, &services));
        crate::ui::out(&s);
    }
    if needs_action { Err(Exit(1).into()) } else { Ok(()) }
}

/// `profile auto→balanced` from a status envelope, when the daemon reports one.
fn profile_of(raw: &serde_json::Value) -> Option<String> {
    if let Some(st) = raw.pointer("/daemon/profile").and_then(|_| crate::cmd::profile::ProfileState::from_status(raw)) {
        return Some(st.short());
    }
    let p = raw.get("profile")?;
    if let Some(s) = p.as_str() {
        return Some(s.to_string());
    }
    let get = |k: &str| p.get(k).and_then(serde_json::Value::as_str).map(str::to_string);
    let active = get("active").or_else(|| get("name"))?;
    Some(match get("mode").or_else(|| get("selected")) {
        Some(m) if m != active => format!("{m}→{active}"),
        _ => active,
    })
}

/// The last line of `doctor`: either "it works" or the single most important next step.
pub fn verdict(ui: &Ui, probe: &Probe, reports: &[RequirementsReport], services: &[Finding]) -> String {
    if let Some(f) = reports.iter().flat_map(|r| r.failures()).next() {
        let fix = f.remedy.clone().map(|r| format!(" → {r}")).unwrap_or_default();
        return ui.paint("1;31", format!("✗ lindesk can't run here: {} ({}){fix}", f.title, f.detail));
    }
    for name in ["lindesk-session", "lindeskd"] {
        if let Some(f) = services.iter().find(|f| f.check == name && f.severity >= Severity::Warn) {
            let step = f.remedy.clone().unwrap_or_else(|| format!("start {name}"));
            return ui.yellow(format!("→ next step: {step}  ({name} is {}, so nothing is protected yet)", f.status));
        }
    }
    if let Some(f) = services.iter().find(|f| f.severity >= Severity::Warn && f.remedy.is_some()) {
        return ui.yellow(format!(
            "→ next step: {}  ({}: {})",
            f.remedy.clone().unwrap_or_default(),
            f.check,
            f.status
        ));
    }
    let (level, why) = probe.support();
    let profile = status_profile().map(|p| format!(", profile {p}")).unwrap_or_default();
    let mut s = ui.green(format!("✓ lindesk is protecting your desktop ({level}{profile})"));
    if let Some(w) = why.first() {
        s.push_str(&ui.dim(format!("  to do better: {w}")));
    }
    s
}

fn status_profile() -> Option<String> {
    let f = crate::cmd::status::fetch(false).ok()?;
    profile_of(&f.raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_remedies_and_pads() {
        let ui = Ui::plain();
        let s = render(
            &ui,
            &[
                finding(Severity::Ok, "lindeskd", "running", None),
                finding(Severity::Warn, "lindesk-session", "not running", Some("systemctl --user start x".into())),
            ],
        );
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], format!("{:<25}running", "lindeskd"));
        assert!(lines[1].ends_with("not running → systemctl --user start x"));
    }

    #[test]
    fn preflight_checklist_and_quiet() {
        let rep = |c, items: Vec<(&'static str, ReqStatus)>| RequirementsReport {
            component: c,
            items: items
                .into_iter()
                .map(|(id, status)| Requirement {
                    id,
                    title: id.to_string(),
                    status,
                    detail: format!("{id} detail"),
                    remedy: (status != ReqStatus::Pass).then(|| format!("fix {id}")),
                })
                .collect(),
        };
        let reports = vec![
            rep(Component::Broker, vec![("kernel", ReqStatus::Pass), ("psi", ReqStatus::Warn)]),
            rep(Component::Session, vec![("kernel", ReqStatus::Pass), ("psi", ReqStatus::Warn)]),
        ];
        let ui = Ui::plain();
        let full = render_preflight(&ui, &reports, "8×CPU", false);
        assert!(full.contains("lindesk preflight  8×CPU"), "{full}");
        assert!(full.contains("✓ PASS  kernel"), "{full}");
        assert!(full.contains("⚠ WARN  psi") && full.contains("→ fix psi"), "{full}");
        assert!(!full.contains("doctor --preflight)"), "{full}");
        assert!(full.contains("⚠ requirements met with 1 warning"), "shared checks count once: {full}");
        let quiet = render_preflight(&ui, &reports, "8×CPU", true);
        assert!(!quiet.contains("PASS") && quiet.contains("WARN"), "{quiet}");
        assert_eq!(quiet.matches("WARN").count(), 1, "shared warnings print once: {quiet}");
        assert!(quiet.contains("(lindeskd, lindesk-session)"), "{quiet}");
        let all_ok = vec![rep(Component::Cli, vec![("linux", ReqStatus::Pass)])];
        assert_eq!(render_preflight(&ui, &all_ok, "", true), "");
        assert!(render_preflight(&ui, &all_ok, "", false).contains("✓ all requirements met"));
        let failing = vec![rep(Component::Broker, vec![("root", ReqStatus::Fail)])];
        assert!(
            requirements_summary(&ui, &failing)
                .contains("✗ 1 requirement failed, 0 warnings (lindeskctl doctor --preflight)")
        );
    }

    #[test]
    fn profiles_in_verdict() {
        assert_eq!(profile_of(&json!({"profile": "gaming"})).as_deref(), Some("gaming"));
        assert_eq!(
            profile_of(&json!({"profile": {"mode": "auto", "active": "balanced"}})).as_deref(),
            Some("auto→balanced")
        );
        assert_eq!(profile_of(&json!({})), None);
        assert_eq!(
            profile_of(&json!({"daemon": {"profile": {"active": "battery", "setting": "auto", "pinned": false, "reason": "battery power"}}}))
                .as_deref(),
            Some("auto→battery")
        );
    }

    #[test]
    fn tilde_shortens_home() {
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            assert_eq!(tilde(&Path::new(&home).join("x/y")), "~/x/y");
        }
        assert_eq!(tilde(Path::new("/var/lib/x")), "/var/lib/x");
    }
}
