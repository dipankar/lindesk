//! `lindeskctl status`: a one-shot, readable picture of what lindesk is doing.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use lindesk_host::{Status, TargetSummary};
use lindesk_plugin_api::{Knob, Resource, Setting, Target};
use lindesk_sys::psi::PsiReading;
use serde_json::json;

use crate::bus::{self, Daemon};
use crate::live;
use crate::ui::{self, Exit, Table, Ui, badge, bar, duration_ms, micros, out, pad, settings_short};

/// A status fetched from one daemon.
pub struct Fetched {
    pub source: Daemon,
    pub raw: serde_json::Value,
    pub status: Option<Status>,
    pub envelope: live::Envelope,
}

/// Session first, then the broker; `--system` asks the broker only. Errors are per daemon.
pub fn fetch(system: bool) -> Result<Fetched, Vec<(Daemon, anyhow::Error)>> {
    let order: &[Daemon] = if system { &[Daemon::Broker] } else { &[Daemon::Session, Daemon::Broker] };
    let mut errors = Vec::new();
    for &d in order {
        match bus::status_json(d) {
            Ok(s) => {
                let raw: serde_json::Value = match serde_json::from_str(&s) {
                    Ok(v) => v,
                    Err(e) => {
                        errors.push((d, anyhow::anyhow!("{} returned invalid JSON from Status: {e}", d.name())));
                        continue;
                    }
                };
                let envelope = live::parse_envelope(&raw);
                let status = envelope.status.clone();
                return Ok(Fetched { source: d, raw, status, envelope });
            }
            Err(e) => errors.push((d, e)),
        }
    }
    Err(errors)
}

/// The friendly "nothing is running" message.
pub fn not_running_message(ui: &Ui, errors: &[(Daemon, anyhow::Error)]) -> String {
    let mut s = format!("{} {}\n", ui.dim("○"), ui.bold("lindesk is off (not running, or not reachable)."));
    for (d, e) in errors {
        let first = e.to_string().lines().next().unwrap_or_default().to_string();
        s.push_str(&format!("  {:<16} {}\n", d.name(), ui.dim(first)));
    }
    let none_installed = errors.iter().all(|(d, _)| crate::paths::which(d.name()).is_none());
    if none_installed && !cfg!(test) {
        s.push_str(&format!(
            "\nlindesk isn't installed here. Install it: {}\n",
            ui.cyan("sudo apt install ./lindesk_*.deb (or dnf/AUR), or ./packaging/install.sh")
        ));
    } else {
        s.push_str("\nStart it:\n");
        for (d, _) in errors {
            s.push_str(&format!("  {:<16} {}\n", d.name(), ui.cyan(d.start_hint())));
        }
    }
    s.push_str(&format!(
        "\nThen run `lindeskctl status` again. What this machine supports: {}\nRaw pressure right now: {}",
        ui.cyan("lindeskctl doctor"),
        ui.cyan("lindeskctl watch")
    ));
    s
}

/// `user` for plugins under the user's data dir, `system` for system dirs, else `external`.
pub fn origin_of_path(p: &Path) -> &'static str {
    if p.starts_with(crate::paths::user_plugin_dir()) {
        "user"
    } else if p.starts_with(crate::paths::SYSTEM_PLUGIN_DIR) || p.starts_with(crate::paths::VENDOR_PLUGIN_DIR) {
        "system"
    } else {
        "external"
    }
}

pub fn state_colored(ui: &Ui, state: &str) -> String {
    match state {
        "running" => ui.green(format!("● {state}")),
        "starting" => ui.cyan(format!("◌ {state}")),
        "backoff" => ui.yellow(format!("◐ {state}")),
        "disabled" | "rejected" => ui.red(format!("○ {state}")),
        _ => ui.dim(format!("○ {state}")),
    }
}

fn delta(ui: &Ui, v: Option<f64>) -> String {
    match v {
        Some(d) if d < -0.5 => ui.yellow(format!("{d:+.0}%")),
        Some(d) => format!("{d:+.0}%"),
        None => ui.dim("·"),
    }
}

fn section(ui: &Ui, title: &str) -> String {
    format!("{}\n", ui.bold(ui.dim(title)))
}

fn psi_section(ui: &Ui, psi: &[(Resource, Option<PsiReading>)]) -> String {
    let mut s = section(ui, "PRESSURE  system, % of time stalled (avg10)");
    for (r, reading) in psi {
        match reading {
            Some(p) => {
                let some = p.some.avg10 as f64;
                let full = p.full.map(|f| f.avg10 as f64);
                s.push_str(&format!("  {:<8}{} {:>5.1}%", r.as_str(), bar(ui, some, 24), some));
                if let Some(f) = full.filter(|f| *f > 0.0 || *r != Resource::Cpu) {
                    s.push_str(&ui.dim(format!("   full {f:>4.1}%")));
                }
                s.push('\n');
            }
            None => s.push_str(&format!("  {:<8}{}\n", r.as_str(), ui.dim("PSI unavailable"))),
        }
    }
    s
}

fn class_str(t: &TargetSummary) -> String {
    t.class.map(|c| c.as_str().to_string()).unwrap_or_else(|| "-".into())
}

/// The human rendering of a status. `psi` is shown when given.
pub fn render(
    ui: &Ui,
    source: Daemon,
    st: &Status,
    env: &live::Envelope,
    psi: Option<&[(Resource, Option<PsiReading>)]>,
    with_plugins: bool,
) -> String {
    let mut s = String::new();
    // One-line summary.
    s.push_str(&format!("{} {} {}\n", badge(ui, st.level), ui.dim("—"), ui.bold(live::summary(st))));
    let mut meta = vec![if env.paused.is_some() { "paused" } else { "on" }.to_string(), source.name().to_string()];
    if let Some(m) = &env.mode {
        meta.push(m.clone());
    }
    if let Some(p) = &env.profile {
        meta.push(format!("profile {}", p.short()));
    }
    if let Some(u) = env.uptime {
        meta.push(format!("up {}", duration_ms(u.as_millis() as i64)));
    }
    if let Some(h) = env.hardware.as_ref().map(ui::hardware_compact).filter(|h| !h.is_empty()) {
        meta.push(h);
    }
    let mut line = ui.dim(meta.join(" · "));
    if st.dry_run {
        line.push_str(&format!("{}{}", ui.dim(" · "), ui.yellow("DRY RUN: nothing is applied")));
    }
    s.push_str(&format!("  {line}\n"));
    let now = if st.now > 0 { st.now } else { lindesk_core::clock::now_ms() };
    if let Some(p) = env.paused_text(now) {
        s.push_str(&format!(
            "  {}  {}\n",
            ui.yellow(p),
            ui.dim("nothing is applied; lindesk only observes (lindeskctl resume)")
        ));
    }
    for p in &env.pending {
        s.push_str(&format!(
            "  {} {}{}  {}\n",
            ui.yellow("↺"),
            ui.yellow(p.countdown(now)),
            if p.description.is_empty() { String::new() } else { format!(" — {}", p.description) },
            ui.dim(format!("keep it: {}", p.confirm_hint()))
        ));
    }
    s.push('\n');
    if let Some(psi) = psi {
        s.push_str(&psi_section(ui, psi));
        s.push('\n');
    }
    s.push_str(&section(ui, "FOREGROUND"));
    match &st.foreground {
        Some(f) => {
            let boosts = settings_short(&f.settings);
            s.push_str(&format!("  {}  {}", pad(&ui.bold(f.label()), 18), ui.dim(&f.target)));
            if env.epoch == Some(true) {
                s.push_str(&ui.paint("1;35", "  ⚡ interaction epoch"));
            }
            if !env.fg_stall.is_empty() {
                let stall: Vec<String> = env
                    .fg_stall
                    .iter()
                    .map(|(r, v)| ui.paint(ui::pressure_code(*v), format!("{} {v:.1}%", r.as_str())))
                    .collect();
                s.push_str(&format!("\n  {:<18}  {} {}", "", ui.dim("own stall"), stall.join("  ")));
            }
            if !boosts.is_empty() {
                s.push_str(&format!("\n  {:<18}  {}", "", ui.green(boosts)));
            }
            s.push('\n');
        }
        None => match &env.tracking {
            Some(t) => s.push_str(&format!("  {}\n", ui.yellow(format!("foreground tracking: {t}")))),
            None => s.push_str(&format!("  {}\n", ui.dim("nothing focused (no adapter event yet)"))),
        },
    }
    s.push('\n');
    let protected: Vec<&TargetSummary> =
        st.protected.iter().filter(|p| Some(&p.target) != st.foreground.as_ref().map(|f| &f.target)).collect();
    s.push_str(&section(ui, &format!("PROTECTED  {}", protected.len())));
    if protected.is_empty() {
        s.push_str(&format!("  {}\n", ui.dim("nothing besides the foreground")));
    }
    for p in &protected {
        s.push_str(&format!(
            "  {}  {:<12} {}{}\n",
            pad(&p.label(), 18),
            class_str(p),
            ui.green(settings_short(&p.settings)),
            p.attention.map(|role| ui.dim(format!("  ({})", role.label()))).unwrap_or_default()
        ));
    }
    s.push('\n');
    s.push_str(&section(ui, &format!("YIELDING  {}", st.yielding.len())));
    if st.yielding.is_empty() {
        s.push_str(&format!("  {}\n", ui.dim("nothing is being throttled")));
    } else {
        let mut t = Table::new(&["  WORKLOAD", "CLASS", "CPU", "IO", "FOR", "THROTTLES"]).right(&[2, 3, 4]);
        for y in &st.yielding {
            t.row(vec![
                format!("  {}", y.summary.label()),
                class_str(&y.summary),
                delta(ui, y.cpu_delta_pct),
                delta(ui, y.io_delta_pct),
                if y.since > 0 && st.now >= y.since { duration_ms(st.now - y.since) } else { "-".into() },
                ui.yellow(settings_short(&y.summary.settings)),
            ]);
        }
        s.push_str(&t.render(ui));
    }
    if with_plugins && !st.plugins.is_empty() {
        s.push('\n');
        let bad = st.plugins.iter().filter(|p| p.state != "running").count();
        s.push_str(&section(
            ui,
            &format!(
                "PLUGINS  {} running{}",
                st.plugins.len() - bad,
                if bad > 0 { format!(", {bad} not") } else { String::new() }
            ),
        ));
        let mut line = String::from("  ");
        for p in &st.plugins {
            let dot = match p.state.as_str() {
                "running" => ui.green("●"),
                "backoff" | "starting" => ui.yellow("◐"),
                _ => ui.red("○"),
            };
            line.push_str(&format!("{dot} {} ", p.name));
            if p.stats.calls > 0 {
                line.push_str(&ui.dim(format!("p99 {} ", micros(p.stats.latency_p99_us))));
            }
            if p.stats.errors > 0 {
                line.push_str(&ui.yellow(format!("{} errors ", p.stats.errors)));
            }
            if let Some(d) = &p.detail {
                line.push_str(&ui.dim(format!("({d}) ")));
            }
            line.push_str("  ");
        }
        s.push_str(line.trim_end());
        s.push('\n');
        if bad > 0 {
            s.push_str(&format!("  {}\n", ui.dim("details: lindeskctl plugin list")));
        }
    }
    if with_plugins && let Some(t) = &env.telemetry {
        s.push_str(&format!("  {}\n", telemetry_line(ui, t)));
    }
    if !env.requirements.is_empty() {
        s.push('\n');
        for r in &env.requirements {
            s.push_str(&format!("{}\n", ui.dim(format!("⚠ {r}"))));
        }
    }
    s
}

/// `telemetry: …` from the daemon's `daemon.telemetry`: not installed (with the install
/// command), disabled, or installed with the lake's size when it's readable.
pub fn telemetry_line(ui: &Ui, t: &serde_json::Value) -> String {
    let label = ui.dim("telemetry:");
    if t["enabled"].as_bool() == Some(false) {
        return format!("{label} {}", ui.dim("disabled ([telemetry] enabled = false)"));
    }
    if t["installed"].as_bool() != Some(true) {
        return format!(
            "{label} {} {}",
            ui.dim("not installed (optional)"),
            ui.dim(
                "→ to record history: sudo apt install lindesk-telemetry (or ./packaging/install.sh --with-telemetry)"
            )
        );
    }
    let mut s = format!("{label} installed");
    if let Some(dir) = t["dir"].as_str().map(Path::new) {
        if std::fs::read_dir(dir).is_ok() {
            s.push_str(&format!(", {} in {}", ui::bytes(crate::paths::dir_size(dir)), crate::cmd::doctor::tilde(dir)));
        } else {
            s.push_str(&format!(", {}", crate::cmd::doctor::tilde(dir)));
        }
    }
    match t["state"].as_str() {
        Some("running") | None => {}
        Some(other) => s.push_str(&format!(" ({})", state_colored(ui, other))),
    }
    s
}

/// Daemons whose pending changes a view of `source` shows: the session agent's and the
/// broker's (system changes affect everyone).
pub fn pending_sources(source: Daemon) -> Vec<Daemon> {
    match source {
        Daemon::Session => vec![Daemon::Session, Daemon::Broker],
        Daemon::Broker => vec![Daemon::Broker],
    }
}

pub fn status(ui: &Ui) -> Result<()> {
    match fetch(ui.system) {
        Ok(f) => {
            let pending = crate::cmd::changes::pending(&pending_sources(f.source));
            if ui.json {
                let mut raw = f.raw.clone();
                if let Some(o) = raw.as_object_mut() {
                    o.insert("pending_changes".into(), serde_json::to_value(&pending)?);
                }
                return ui.print_json(&raw);
            }
            match &f.status {
                Some(st) => {
                    let psi = live::read_psi(Path::new("/proc"));
                    let mut env = f.envelope.clone();
                    env.pending = pending;
                    env.requirements = live::merged_requirements(
                        &env.requirements_raw,
                        env.requirements_checked,
                        lindesk_core::clock::now_ms(),
                        live::local_conflicts,
                    );
                    if env.hardware.is_none() {
                        env.hardware = Some(live::local_hardware());
                    }
                    out(&render(ui, f.source, st, &env, Some(&psi), !ui.quiet));
                    if !ui.quiet
                        && let Some(h) = crate::hints::once()
                    {
                        out(&format!("\n{} {}", ui.cyan("→"), h.text));
                    }
                }
                None => {
                    ui.warn(format!(
                        "{}'s status doesn't match this lindeskctl's format (version mismatch?); showing it raw",
                        f.source.name()
                    ));
                    ui.print_json(&f.raw)?;
                }
            }
            Ok(())
        }
        Err(errors) => {
            // Nothing reports hardware and requirements: compute them here.
            let probe = lindesk_sys::probe(&lindesk_sys::Roots::host(), &lindesk_sys::Env::from_process());
            let reports = crate::cmd::doctor::requirement_reports(&probe);
            if ui.json {
                let errs: BTreeMap<&str, String> = errors.iter().map(|(d, e)| (d.name(), e.to_string())).collect();
                ui.print_json(
                    &json!({"running": false, "errors": errs, "hardware": probe.hardware, "requirements": reports}),
                )?;
            } else {
                let eui = Ui { color: ui.err_color, ..ui.clone() };
                eprintln!("{}", not_running_message(&eui, &errors));
                eprintln!(
                    "\n{}\n{}",
                    eui.dim(ui::hardware_compact(&probe.hardware)),
                    crate::cmd::doctor::requirements_summary(&eui, &reports)
                );
            }
            Err(Exit(1).into())
        }
    }
}

const BARE_HINT: &str = "lindeskctl watch for live view · lindeskctl doctor · lindeskctl --help";

fn psi_inline(ui: &Ui, psi: &[(Resource, Option<PsiReading>)]) -> String {
    psi.iter()
        .map(|(r, p)| match p {
            Some(p) => ui.paint(ui::pressure_code(p.some.avg10 as f64), format!("{} {:.1}%", r.as_str(), p.some.avg10)),
            None => ui.dim(format!("{} -", r.as_str())),
        })
        .collect::<Vec<_>>()
        .join(&ui.dim(" · "))
}

/// `lindeskctl` with no subcommand: a compact status and where to go next. Always exit 0.
pub fn bare(ui: &Ui) -> Result<()> {
    let psi = live::read_psi(Path::new("/proc"));
    let mut s = String::new();
    match fetch(ui.system) {
        Ok(Fetched { source, status: Some(st), envelope, .. }) => {
            s.push_str(&format!("{} {} {}\n", badge(ui, st.level), ui.dim("—"), ui.bold(live::summary(&st))));
            let mut meta =
                vec![if envelope.paused.is_some() { "paused" } else { "on" }.to_string(), source.name().to_string()];
            meta.extend(envelope.mode.clone());
            if let Some(p) = &envelope.profile {
                meta.push(format!("profile {}", p.short()));
            }
            let hw = envelope.hardware.clone().unwrap_or_else(live::local_hardware);
            meta.push(ui::hardware_compact(&hw));
            if st.dry_run {
                meta.push("DRY RUN".into());
            }
            s.push_str(&format!("  {}\n", ui.dim(meta.join(" · "))));
            if let Some(p) = envelope.paused_text(lindesk_core::clock::now_ms()) {
                s.push_str(&format!("  {}\n", ui.yellow(p)));
            }
            if let Some(t) = &envelope.tracking {
                s.push_str(&format!("  {}\n", ui.yellow(format!("foreground tracking: {t}"))));
            }
            s.push_str(&format!("  {} {}\n", ui.dim("pressure"), psi_inline(ui, &psi)));
            if !st.yielding.is_empty() {
                let y: Vec<String> = st.yielding.iter().take(4).map(ToString::to_string).collect();
                let more = if st.yielding.len() > 4 { format!(" +{}", st.yielding.len() - 4) } else { String::new() };
                s.push_str(&format!("  {} {}{more}\n", ui.dim("yielding"), ui.yellow(y.join(" · "))));
            }
            let reqs = live::merged_requirements(
                &envelope.requirements_raw,
                envelope.requirements_checked,
                lindesk_core::clock::now_ms(),
                live::local_conflicts,
            );
            for r in reqs.iter().take(1) {
                s.push_str(&format!("  {}\n", ui.dim(format!("⚠ {r}"))));
            }
            let now = lindesk_core::clock::now_ms();
            for p in crate::cmd::changes::pending(&pending_sources(source)).iter().take(2) {
                s.push_str(&format!(
                    "  {} {}\n",
                    ui.yellow(format!("↺ {}", p.countdown(now))),
                    ui.dim(format!("keep it: {}", p.confirm_hint()))
                ));
            }
        }
        Ok(Fetched { source, .. }) => {
            s.push_str(&format!(
                "{} {} answered in an unknown format (version mismatch?)\n",
                ui.yellow("?"),
                source.name()
            ));
        }
        Err(_) => {
            let (state, next) = if crate::paths::which("lindesk-session").is_some() {
                ("lindesk is off", "— turn it on: lindeskctl on".to_string())
            } else {
                ("lindesk is not installed", "— ./packaging/install.sh or your distro package".to_string())
            };
            s.push_str(&format!("{} {} {}\n", ui.dim("○"), ui.bold(state), ui.dim(next)));
            s.push_str(&format!("  {} {}\n", ui.dim("pressure"), psi_inline(ui, &psi)));
            s.push_str(&format!("  {}\n", ui.dim(ui::hardware_compact(&live::local_hardware()))));
        }
    }
    if let Some(h) = crate::hints::once() {
        s.push_str(&format!("{} {}\n", ui.cyan("→"), h.text));
    }
    s.push_str(&ui.dim(BARE_HINT));
    if ui.json {
        return status(ui).or(Ok(()));
    }
    out(&s);
    Ok(())
}

/// One change seen between two status snapshots.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub ts: i64,
    /// Event kind it corresponds to (`level`, `focus`, `action_applied`, `action_reverted`).
    pub kind: &'static str,
    pub text: String,
}

fn settings_by_target(st: &Status) -> BTreeMap<Target, (String, BTreeMap<Knob, Setting>)> {
    let mut m = BTreeMap::new();
    let all = st.protected.iter().chain(st.yielding.iter().map(|y| &y.summary)).chain(st.foreground.iter());
    for t in all {
        let e = m.entry(t.target.clone()).or_insert_with(|| (t.label(), BTreeMap::new()));
        for s in &t.settings {
            e.1.insert(s.knob(), s.clone());
        }
    }
    m
}

/// What changed from `prev` to `cur`: level, foreground, settings applied or reverted.
pub fn diff(prev: &Status, cur: &Status) -> Vec<Change> {
    let ts = cur.now;
    let mut out = Vec::new();
    if prev.level != cur.level {
        out.push(Change {
            ts,
            kind: "level",
            text: format!("level {} → {}", prev.level.as_str().to_uppercase(), cur.level.as_str().to_uppercase()),
        });
    }
    let fg = |s: &Status| s.foreground.as_ref().map(|f| f.target.clone());
    if fg(prev) != fg(cur) {
        let name = cur.foreground.as_ref().map(TargetSummary::label).unwrap_or_else(|| "none".into());
        out.push(Change { ts, kind: "focus", text: format!("focus → {name}") });
    }
    let a = settings_by_target(prev);
    let b = settings_by_target(cur);
    for (t, (label, knobs)) in &b {
        let before = a.get(t).map(|x| &x.1);
        for (k, s) in knobs {
            if before.and_then(|m| m.get(k)) != Some(s) {
                out.push(Change {
                    ts,
                    kind: "action_applied",
                    text: format!("apply  {} on {label}", ui::setting_short(s)),
                });
            }
        }
    }
    for (t, (label, knobs)) in &a {
        let after = b.get(t).map(|x| &x.1);
        for k in knobs.keys() {
            if after.is_none_or(|m| !m.contains_key(k)) {
                out.push(Change { ts, kind: "action_reverted", text: format!("revert {k} on {label}") });
            }
        }
    }
    out
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use lindesk_host::YieldingTarget;
    use lindesk_plugin_api::{Level, QosClass};

    pub fn ts(name: &str, class: QosClass, settings: Vec<Setting>) -> TargetSummary {
        TargetSummary {
            target: Target::user_unit(1000, name),
            comm: Some(name.split(['-', '.']).next().unwrap_or(name).into()),
            class: Some(class),
            foreground: false,
            attention: None,
            settings,
        }
    }

    pub fn status(level: Level, yielding: Vec<YieldingTarget>) -> Status {
        Status {
            now: 10_000,
            host: "lindesk-session".into(),
            level,
            dry_run: false,
            foreground: Some(ts("firefox.scope", QosClass::Interactive, vec![Setting::CpuWeight { value: 200 }])),
            protected: vec![ts("gnome-shell.scope", QosClass::Critical, vec![])],
            yielding,
            plugins: vec![],
        }
    }

    pub fn y(settings: Vec<Setting>) -> YieldingTarget {
        YieldingTarget {
            summary: ts("rustc.scope", QosClass::Batch, settings),
            since: 4_000,
            cpu_delta_pct: Some(-31.0),
            io_delta_pct: Some(-45.0),
        }
    }

    #[test]
    fn protected_media_reason_is_visible_and_old_status_still_parses() {
        let mut st = status(Level::Green, vec![]);
        let mut media = ts("music.scope", QosClass::Normal, vec![Setting::CpuWeight { value: 200 }]);
        media.attention = Some(lindesk_core::attention::Attention::Media);
        st.protected.push(media.clone());
        let text = render(&Ui::plain(), Daemon::Session, &st, &live::Envelope::default(), None, true);
        assert!(text.contains("playing or recording audio"), "{text}");
        let mut old = serde_json::to_value(&media).unwrap();
        old.as_object_mut().unwrap().remove("attention");
        assert!(serde_json::from_value::<TargetSummary>(old).unwrap().attention.is_none());
    }

    #[test]
    fn renders_summary_sections_and_deltas() {
        let st = status(Level::Yellow, vec![y(vec![Setting::CpuWeight { value: 20 }])]);
        let env = live::Envelope {
            mode: Some("connected".into()),
            epoch: Some(true),
            hardware: Some(lindesk_plugin_api::Hardware { cpus: 32, mem_total: 62 << 30, ..Default::default() }),
            requirements: vec!["PSI: per-cgroup PSI disabled".into()],
            profile: Some(crate::cmd::profile::ProfileState {
                active: "battery".into(),
                setting: Some("auto".into()),
                pinned: false,
                reason: "battery power".into(),
            }),
            pending: vec![crate::cmd::changes::Pending {
                id: 7,
                scope: lindesk_changes::Scope::User,
                ttl_until: st.now + 8 * 60_000 + 30_000,
                description: "shorter epochs".into(),
            }],
            telemetry: Some(json!({"installed": false, "enabled": true})),
            ..Default::default()
        };
        let mut waiting = env.clone();
        waiting.tracking = Some("waiting for GNOME extension (log out and in once)".into());
        let mut nofg = st.clone();
        nofg.foreground = None;
        let w = render(&Ui::plain(), Daemon::Session, &nofg, &waiting, None, true);
        assert!(
            w.contains("FOREGROUND\n  foreground tracking: waiting for GNOME extension (log out and in once)"),
            "{w}"
        );
        let s = render(&Ui::plain(), Daemon::Session, &st, &env, None, true);
        assert!(
            s.contains("  telemetry: not installed (optional) → to record history: sudo apt install lindesk-telemetry"),
            "{s}"
        );
        assert!(s.contains("on · lindesk-session · connected · profile auto→battery · 32×CPU · 62 GiB"), "{s}");
        assert!(s.contains("↺ change #7 reverts in 8m — shorter epochs  keep it: lindeskctl changes confirm 7"), "{s}");
        assert!(s.contains("⚡ interaction epoch"), "{s}");
        assert!(s.contains("⚠ PSI: per-cgroup PSI disabled"), "{s}");
        assert!(s.starts_with("● YELLOW — protecting firefox; 1 workload yielding"), "{s}");
        assert!(s.contains("FOREGROUND\n  firefox"), "{s}");
        assert!(s.contains("cpu.weight 200"), "{s}");
        assert!(s.contains("PROTECTED  1\n  gnome"), "{s}");
        assert!(s.contains("rustc") && s.contains("-31%") && s.contains("-45%") && s.contains("6s"), "{s}");
        assert!(s.contains("cpu.weight 20"), "{s}");
    }

    #[test]
    fn stale_conflict_warnings_are_rechecked_locally() {
        let raw = vec![
            json!({"id": "conflicts", "title": "Conflicting daemons", "status": "warn", "detail": "system76-scheduler running"}),
            json!({"id": "psi", "title": "PSI", "status": "warn", "detail": "per-cgroup PSI disabled"}),
        ];
        let now = 1_000_000;
        // Fresh: the daemon's lines as they are, no local check.
        let fresh = live::merged_requirements(&raw, Some(now - 10_000), now, || panic!("no local check when fresh"));
        assert_eq!(fresh.len(), 2);
        // Stale (or from an older daemon): the conflicts line comes from a local check.
        let stale = live::merged_requirements(&raw, Some(now - 120_000), now, || None);
        assert_eq!(stale.len(), 1, "the conflict is gone now: {stale:?}");
        assert!(stale[0].contains("PSI"));
        let old = live::merged_requirements(&raw, None, now, || Some(raw[0].clone()));
        assert!(old.iter().any(|l| l.contains("system76-scheduler")));
    }

    #[test]
    fn telemetry_lines() {
        let ui = Ui::plain();
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("f.vortex"), vec![0u8; 2048]).unwrap();
        let t = json!({"installed": true, "enabled": true, "state": "running", "dir": d.path()});
        let l = telemetry_line(&ui, &t);
        assert!(l.starts_with("telemetry: installed, 2.0 KiB in "), "{l}");
        let l = telemetry_line(
            &ui,
            &json!({"installed": true, "enabled": true, "state": "backoff", "dir": "/nonexistent"}),
        );
        assert!(l.starts_with("telemetry: installed, /nonexistent (") && l.ends_with("backoff)"), "{l}");
        assert_eq!(
            telemetry_line(&ui, &json!({"enabled": false})),
            "telemetry: disabled ([telemetry] enabled = false)"
        );
    }

    #[test]
    fn diff_reports_level_and_settings() {
        let a = status(Level::Green, vec![]);
        let b = status(Level::Yellow, vec![y(vec![Setting::CpuWeight { value: 20 }])]);
        let c: Vec<String> = diff(&a, &b).into_iter().map(|c| c.text).collect();
        assert_eq!(c, vec!["level GREEN → YELLOW", "apply  cpu.weight 20 on rustc"]);
        let c: Vec<String> = diff(&b, &a).into_iter().map(|c| c.text).collect();
        assert_eq!(c, vec!["level YELLOW → GREEN", "revert cpu.weight on rustc"]);
    }

    #[test]
    fn not_running_message_has_hints() {
        let errs =
            vec![(Daemon::Session, Daemon::Session.not_running()), (Daemon::Broker, Daemon::Broker.not_running())];
        let s = not_running_message(&Ui::plain(), &errs);
        assert!(s.contains("systemctl --user enable --now lindesk-session"));
        assert!(s.contains("sudo systemctl enable --now lindeskd"));
        assert!(s.contains("lindeskctl doctor"));
    }

    #[test]
    fn psi_section_bars() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("pressure")).unwrap();
        std::fs::write(
            d.path().join("pressure/io"),
            "some avg10=50.00 avg60=1.00 avg300=0.00 total=1\nfull avg10=25.00 avg60=0.00 avg300=0.00 total=1\n",
        )
        .unwrap();
        let psi = live::read_psi(d.path());
        let s = psi_section(&Ui::plain(), &psi);
        assert!(s.contains("  io      ████████████░░░░░░░░░░░░  50.0%   full 25.0%"), "{s}");
        assert!(s.contains("cpu     PSI unavailable"), "{s}");
    }
}
