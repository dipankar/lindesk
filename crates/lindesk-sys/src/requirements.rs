//! Start-up and install-time requirement checks.
//!
//! `lindeskd` and `lindesk-session` run [`check`] before doing anything else and exit with
//! [`EXIT_REQUIREMENTS`] when a hard requirement fails; `lindeskctl doctor --preflight` runs
//! it for both daemons (installers call that). Unlike [`crate::doctor`], which explains how
//! well lindesk can work, this answers "can it run at all, and what will be degraded".
//!
//! Every check has a stable id (for scripts and the JSON output). [`Status::Fail`] is used
//! only where lindesk truly cannot run; everything else degrades to [`Status::Warn`].
//!
//! | id | Components | Fail when |
//! |---|---|---|
//! | `linux` | all | `/proc` missing or `osrelease` unreadable |
//! | `kernel` | all | kernel older than 3.10 (Warn below 4.20: no PSI; below 5.4: no cgroup uclamp) |
//! | `cgroups` | all | never (Warn on v1/hybrid/none) |
//! | `psi` | all | never (Warn when missing or disabled) |
//! | `systemd` | all | never (Warn when not running or older than 244) |
//! | `user_delegation` | session | never (Warn when cpu/io/memory are not delegated) |
//! | `root` | broker | not root, unless `LINDESK_ALLOW_UNPRIVILEGED=1` (then Warn) |
//! | `dbus` | broker, session | the bus is missing and D-Bus is required (Warn for the CLI or with `--no-dbus`) |
//! | `polkit` | broker | never (Warn: only root can change classes) |
//! | `io_control` | all | never |
//! | `writeback` | all | never |
//! | `conflicts` | all | never (Warn while a conflicting daemon runs) |
//! | `desktop` | session | never (Warn without a graphical session) |
//! | `hardware` | all | never (informational) |
//! | `virtualization` | all | never (Warn inside containers) |

use std::fmt::Write as _;

use serde::Serialize;

use crate::hardware::{self, summary_line};
use crate::probe::{CgroupMode, IoLatency, Probe, Writeback};
use crate::util::read_lossy;
use crate::{Env, Roots};

/// Process exit code daemons use when a hard requirement fails (EX_CONFIG).
pub const EXIT_REQUIREMENTS: i32 = 78;

/// Which program the checks are for.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Component {
    /// `lindeskd`, the root broker.
    Broker,
    /// `lindesk-session`, the per-user agent.
    Session,
    /// `lindeskctl`.
    Cli,
}

impl Component {
    /// The program name: `lindeskd`, `lindesk-session` or `lindeskctl`.
    pub fn program(self) -> &'static str {
        match self {
            Component::Broker => "lindeskd",
            Component::Session => "lindesk-session",
            Component::Cli => "lindeskctl",
        }
    }
}

/// Outcome of one check.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Warn,
    Fail,
}

impl Status {
    /// `PASS`, `WARN` or `FAIL`.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        }
    }
}

/// One checked requirement.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Requirement {
    /// Stable identifier, e.g. `kernel`, `psi`, `root`.
    pub id: &'static str,
    /// Short human title, e.g. `Kernel version`.
    pub title: String,
    pub status: Status,
    /// What was found, and for Warn what lindesk does instead.
    pub detail: String,
    /// How to fix it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
}

/// The result of [`check`].
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct RequirementsReport {
    pub component: Component,
    pub items: Vec<Requirement>,
}

impl RequirementsReport {
    /// True when nothing failed (warnings are fine).
    pub fn ok(&self) -> bool {
        self.failures().next().is_none()
    }

    /// The failed requirements.
    pub fn failures(&self) -> impl Iterator<Item = &Requirement> {
        self.items.iter().filter(|r| r.status == Status::Fail)
    }

    /// The requirements that passed with a warning.
    pub fn warnings(&self) -> impl Iterator<Item = &Requirement> {
        self.items.iter().filter(|r| r.status == Status::Warn)
    }

    /// The requirement with `id`, if it was checked.
    pub fn get(&self, id: &str) -> Option<&Requirement> {
        self.items.iter().find(|r| r.id == id)
    }

    /// Aligned report ending with a one-line verdict. `color` adds ANSI colours.
    ///
    /// ```text
    /// lindeskd requirements
    ///   PASS  linux            Linux, /proc mounted
    ///   FAIL  root             running as uid 1000
    ///                          → lindeskd must run as root; run lindesk-session for per-user mode
    /// lindeskd: 1 requirement failed, 2 warnings
    /// ```
    pub fn render_text(&self, color: bool) -> String {
        let paint = |s: Status, text: &str| -> String {
            if !color {
                return text.to_string();
            }
            let c = match s {
                Status::Pass => "32",
                Status::Warn => "33",
                Status::Fail => "1;31",
            };
            format!("\x1b[{c}m{text}\x1b[0m")
        };
        let prog = self.component.program();
        let mut out = format!("{prog} requirements\n");
        for r in &self.items {
            let _ = writeln!(out, "  {}  {:<16} {}", paint(r.status, r.status.as_str()), r.id, r.detail);
            if let Some(rem) = &r.remedy {
                let _ = writeln!(out, "  {:<4}  {:<16} → {rem}", "", "");
            }
        }
        let fails = self.failures().count();
        let warns = self.warnings().count();
        let plural = |n: usize, w: &str| if n == 1 { format!("{n} {w}") } else { format!("{n} {w}s") };
        let verdict = match (fails, warns) {
            (0, 0) => paint(Status::Pass, "all requirements met"),
            (0, w) => paint(Status::Warn, &format!("requirements met with {}", plural(w, "warning"))),
            (f, w) => paint(Status::Fail, &format!("{} failed, {}", plural(f, "requirement"), plural(w, "warning"))),
        };
        let _ = writeln!(out, "{prog}: {verdict}");
        out
    }

    /// `{"component": "broker", "ok": false, "items": [{"id", "title", "status", "detail", "remedy"}]}`.
    pub fn render_json(&self) -> String {
        let v = serde_json::json!({
            "component": self.component,
            "program": self.component.program(),
            "ok": self.ok(),
            "items": self.items,
        });
        serde_json::to_string_pretty(&v).expect("report serialises")
    }
}

/// Inputs to [`check_with`] beyond the probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckOptions {
    /// The daemon needs its bus (default). `false` for `--no-dbus`: a missing bus is a Warn.
    pub dbus_required: bool,
    /// Effective uid to check against; `None` reads the real one (`geteuid`).
    pub euid: Option<u32>,
}

impl Default for CheckOptions {
    fn default() -> Self {
        CheckOptions { dbus_required: true, euid: None }
    }
}

impl CheckOptions {
    /// Builder: whether D-Bus is required.
    pub fn dbus_required(mut self, required: bool) -> Self {
        self.dbus_required = required;
        self
    }

    /// Builder: check as this effective uid (tests, or `doctor` on behalf of a user).
    pub fn euid(mut self, uid: u32) -> Self {
        self.euid = Some(uid);
        self
    }
}

/// Check the requirements of `component` with D-Bus required and the real euid.
pub fn check(roots: &Roots, env: &Env, component: Component, probe: &Probe) -> RequirementsReport {
    check_with(roots, env, component, probe, &CheckOptions::default())
}

/// [`check`] with explicit [`CheckOptions`].
pub fn check_with(
    roots: &Roots,
    env: &Env,
    component: Component,
    probe: &Probe,
    opts: &CheckOptions,
) -> RequirementsReport {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = opts.euid.unwrap_or_else(|| unsafe { libc::geteuid() });
    let cx = Cx { roots, env, component, p: probe, opts, euid };
    let mut items = vec![cx.linux(), cx.kernel(), cx.cgroups(), cx.psi(), cx.systemd()];
    if component == Component::Session {
        items.push(cx.user_delegation());
    }
    if component == Component::Broker {
        items.push(cx.root());
    }
    items.push(cx.dbus());
    if component == Component::Broker {
        items.push(cx.polkit());
    }
    items.extend([cx.io_control(), cx.writeback(), cx.conflicts()]);
    if component == Component::Session {
        items.push(cx.desktop());
    }
    items.extend([cx.hardware(), cx.virtualization()]);
    RequirementsReport { component, items }
}

/// Parse `major.minor` from a kernel release such as `6.8.0-45-generic` or `4.19`.
pub fn kernel_version(release: &str) -> Option<(u32, u32)> {
    let mut it = release.split(|c: char| !c.is_ascii_digit());
    let major = it.next()?.parse().ok()?;
    let minor = it.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

fn req(id: &'static str, title: &str, status: Status, detail: impl Into<String>, remedy: Option<&str>) -> Requirement {
    Requirement { id, title: title.into(), status, detail: detail.into(), remedy: remedy.map(str::to_string) }
}

struct Cx<'a> {
    roots: &'a Roots,
    env: &'a Env,
    component: Component,
    p: &'a Probe,
    opts: &'a CheckOptions,
    euid: u32,
}

const DELEGATE_REMEDY: &str = "systemctl edit user@.service and add:\n[Service]\nDelegate=cpu cpuset io memory pids\n\
     then: systemctl daemon-reload, and log out and back in";

impl Cx<'_> {
    fn linux(&self) -> Requirement {
        let t = "Linux with /proc";
        if !self.roots.proc.is_dir() {
            return req("linux", t, Status::Fail, "/proc is not mounted", Some("mount -t proc proc /proc"));
        }
        if self.p.kernel.is_none() {
            return req(
                "linux",
                t,
                Status::Fail,
                "/proc/sys/kernel/osrelease is unreadable",
                Some("lindesk needs a Linux /proc (not a restricted or emulated one)"),
            );
        }
        req("linux", t, Status::Pass, "Linux, /proc mounted", None)
    }

    fn kernel(&self) -> Requirement {
        let t = "Kernel version";
        let Some(rel) = self.p.kernel.as_deref() else {
            return req("kernel", t, Status::Warn, "unknown kernel version", None);
        };
        let Some(v) = kernel_version(rel) else {
            return req("kernel", t, Status::Warn, format!("{rel}: version not recognised"), None);
        };
        if v < (3, 10) {
            req(
                "kernel",
                t,
                Status::Fail,
                format!("{rel}: older than 3.10 (no usable cgroups or ioprio interfaces)"),
                Some("upgrade to a kernel ≥ 5.4 (≥ 6.1 recommended)"),
            )
        } else if v < (4, 20) {
            req(
                "kernel",
                t,
                Status::Warn,
                format!("{rel}: older than 4.20, no PSI: lindesk runs reactive-blind with static class weights only"),
                Some("upgrade to a kernel ≥ 5.4 for PSI and cgroup uclamp"),
            )
        } else if v < (5, 4) {
            req(
                "kernel",
                t,
                Status::Warn,
                format!("{rel}: older than 5.4, no cgroup uclamp: foreground boosts use weights only"),
                Some("upgrade to a kernel ≥ 5.4"),
            )
        } else {
            req("kernel", t, Status::Pass, rel, None)
        }
    }

    fn cgroups(&self) -> Requirement {
        let t = "cgroup filesystem";
        let unified = "boot with systemd.unified_cgroup_hierarchy=1 for cgroup v2";
        match self.p.cgroup_mode {
            CgroupMode::V2 => {
                let missing: Vec<&str> =
                    ["cpu", "io", "memory"].into_iter().filter(|c| !self.p.has_controller(c)).collect();
                if missing.is_empty() {
                    req("cgroups", t, Status::Pass, format!("cgroup v2 ({})", self.p.root_controllers.join(" ")), None)
                } else {
                    req(
                        "cgroups",
                        t,
                        Status::Warn,
                        format!("cgroup v2 without the {} controller(s)", missing.join(", ")),
                        Some("rebuild the kernel with CONFIG_CGROUP_SCHED/CONFIG_BLK_CGROUP/CONFIG_MEMCG"),
                    )
                }
            }
            CgroupMode::Hybrid => req(
                "cgroups",
                t,
                Status::Warn,
                "hybrid hierarchy (controllers on v1): CPU weights only, no I/O control",
                Some(unified),
            ),
            CgroupMode::V1 => {
                req("cgroups", t, Status::Warn, "cgroup v1 only: CPU weights only, no I/O control", Some(unified))
            }
            CgroupMode::None => req(
                "cgroups",
                t,
                Status::Warn,
                "no cgroup filesystem: per-process backend only (nice, ioprio, uclamp)",
                Some("mount cgroup2 at /sys/fs/cgroup"),
            ),
        }
    }

    fn psi(&self) -> Requirement {
        let t = "Pressure Stall Information";
        if self.p.psi.global {
            return req("psi", t, Status::Pass, "/proc/pressure available", None);
        }
        let cmdline = read_lossy(&self.roots.proc.join("cmdline")).unwrap_or_default();
        let has_arg = |a: &str| cmdline.split_whitespace().any(|x| x == a);
        let kcfg = |k: &str| self.p.kconfig.as_ref().and_then(|c| c.get(k)).map(String::as_str);
        let blind = "lindesk has no stall feedback and applies static class weights only";
        if has_arg("psi=0") {
            return req(
                "psi",
                t,
                Status::Warn,
                format!("disabled on the kernel command line (psi=0): {blind}"),
                Some("replace psi=0 with psi=1 on the kernel command line"),
            );
        }
        if kcfg("PSI_DEFAULT_DISABLED") == Some("y") {
            return req(
                "psi",
                t,
                Status::Warn,
                format!("compiled but disabled by default (CONFIG_PSI_DEFAULT_DISABLED): {blind}"),
                Some("add psi=1 to the kernel command line"),
            );
        }
        if kcfg("PSI") == Some("n") {
            return req(
                "psi",
                t,
                Status::Warn,
                format!("kernel built without CONFIG_PSI: {blind}"),
                Some("use a kernel with CONFIG_PSI=y"),
            );
        }
        req(
            "psi",
            t,
            Status::Warn,
            format!("/proc/pressure missing: {blind}"),
            Some("add psi=1 to the kernel command line (needs CONFIG_PSI=y, kernel ≥ 4.20)"),
        )
    }

    fn systemd(&self) -> Requirement {
        let t = "systemd";
        if !self.p.systemd.running {
            return req(
                "systemd",
                t,
                Status::Warn,
                "non-systemd host: using the raw cgroup backend",
                Some("lindesk-session needs a writable cgroup delegated to the user (or runs per-process)"),
            );
        }
        let Some(v) = self.p.systemd.version.as_deref() else {
            return req("systemd", t, Status::Pass, "running (version unknown)", None);
        };
        match v.parse::<u32>() {
            Ok(n) if n < 244 => req(
                "systemd",
                t,
                Status::Warn,
                format!("{n}: older than 244, some units reject runtime cgroup properties"),
                Some("upgrade systemd to ≥ 252"),
            ),
            Ok(n) if n < 252 => req(
                "systemd",
                t,
                Status::Pass,
                format!("{n} (running); CPUWeight=idle needs 252, so idle classes write cpu.idle directly"),
                None,
            ),
            _ => req("systemd", t, Status::Pass, format!("{v} (running)"), None),
        }
    }

    fn user_delegation(&self) -> Requirement {
        let t = "User cgroup delegation";
        if self.p.cgroup_mode != CgroupMode::V2 {
            return req("user_delegation", t, Status::Warn, "needs cgroup v2: per-process control only", None);
        }
        if !self.p.systemd.running {
            return req(
                "user_delegation",
                t,
                Status::Warn,
                "no systemd user manager: lindesk-session uses a delegated cgroup if one is writable",
                None,
            );
        }
        let Some(m) = self.p.user_managers.iter().find(|m| m.uid == self.euid) else {
            return req(
                "user_delegation",
                t,
                Status::Warn,
                format!("user@{}.service not found (no user session?): per-process control only", self.euid),
                Some("start lindesk-session from a logged-in user session"),
            );
        };
        let missing: Vec<&str> =
            ["cpu", "io", "memory"].into_iter().filter(|c| !m.controllers.iter().any(|x| x == c)).collect();
        if missing.is_empty() {
            req("user_delegation", t, Status::Pass, format!("delegated: {}", m.controllers.join(" ")), None)
        } else {
            req(
                "user_delegation",
                t,
                Status::Warn,
                format!("{} not delegated to user@{}.service: those knobs are skipped", missing.join(", "), m.uid),
                Some(DELEGATE_REMEDY),
            )
        }
    }

    fn root(&self) -> Requirement {
        let t = "Root privileges";
        if self.euid == 0 {
            return req("root", t, Status::Pass, "running as root", None);
        }
        if self.env.get("LINDESK_ALLOW_UNPRIVILEGED") == Some("1") {
            return req(
                "root",
                t,
                Status::Warn,
                format!("running as uid {} (LINDESK_ALLOW_UNPRIVILEGED=1): writes will fail, dry-run only", self.euid),
                None,
            );
        }
        req(
            "root",
            t,
            Status::Fail,
            format!("running as uid {}", self.euid),
            Some("lindeskd must run as root; run lindesk-session for per-user mode"),
        )
    }

    fn system_bus(&self) -> Option<String> {
        if let Some(a) = self.env.get("DBUS_SYSTEM_BUS_ADDRESS") {
            return Some(a.to_string());
        }
        let sock = self.roots.run.join("dbus/system_bus_socket");
        sock.exists().then(|| format!("unix:path={}", sock.display()))
    }

    fn session_bus(&self) -> Option<String> {
        if let Some(a) = self.env.get("DBUS_SESSION_BUS_ADDRESS") {
            return Some(a.to_string());
        }
        let dir = self.env.get("XDG_RUNTIME_DIR")?;
        let sock = std::path::Path::new(dir).join("bus");
        sock.exists().then(|| format!("unix:path={}", sock.display()))
    }

    fn dbus(&self) -> Requirement {
        let t = "D-Bus";
        let (sys_needed, sess_needed) = match self.component {
            Component::Broker => (true, false),
            Component::Session => (false, true),
            Component::Cli => (true, true),
        };
        let mut found = Vec::new();
        let mut missing = Vec::new();
        if sys_needed {
            match self.system_bus() {
                Some(a) => found.push(format!("system bus {a}")),
                None => missing.push("system bus (/run/dbus/system_bus_socket)"),
            }
        }
        if sess_needed {
            match self.session_bus() {
                Some(a) => found.push(format!("session bus {a}")),
                None => missing.push("session bus (DBUS_SESSION_BUS_ADDRESS or $XDG_RUNTIME_DIR/bus)"),
            }
        }
        if missing.is_empty() {
            let note =
                if self.opts.dbus_required || self.component == Component::Cli { "" } else { " (unused: --no-dbus)" };
            return req("dbus", t, Status::Pass, format!("{}{note}", found.join(", ")), None);
        }
        let what = missing.join(", ");
        match self.component {
            Component::Cli => req(
                "dbus",
                t,
                Status::Warn,
                format!("no {what}: lindeskctl cannot reach the daemons"),
                Some("run lindeskctl inside a session with D-Bus"),
            ),
            _ if !self.opts.dbus_required => req(
                "dbus",
                t,
                Status::Warn,
                format!("no {what}: running with --no-dbus, no IPC and no lindeskctl control"),
                None,
            ),
            Component::Broker => req(
                "dbus",
                t,
                Status::Fail,
                format!("no {what}"),
                Some("start dbus (systemctl start dbus), or run lindeskd --no-dbus"),
            ),
            Component::Session => req(
                "dbus",
                t,
                Status::Fail,
                format!("no {what}"),
                Some("start lindesk-session inside a user session (dbus-run-session), or with --no-dbus"),
            ),
        }
    }

    fn polkit(&self) -> Requirement {
        let t = "polkit";
        let action = self.roots.usr.join("share/polkit-1/actions/org.lindesk.policy");
        let daemon = ["lib/polkit-1/polkitd", "libexec/polkitd", "libexec/polkit-1/polkitd", "lib/polkit/polkitd"]
            .iter()
            .any(|p| self.roots.usr.join(p).exists())
            || ["lib/systemd/system/polkit.service", "lib/systemd/system/polkitd.service"]
                .iter()
                .any(|p| self.roots.usr.join(p).exists());
        let consequence = "only root can change classes";
        match (action.exists(), daemon) {
            (true, true) => req("polkit", t, Status::Pass, "polkitd and org.lindesk.policy installed", None),
            (false, _) => req(
                "polkit",
                t,
                Status::Warn,
                format!("org.lindesk.policy not installed: {consequence}"),
                Some("install /usr/share/polkit-1/actions/org.lindesk.policy (packaging/polkit)"),
            ),
            (true, false) => req(
                "polkit",
                t,
                Status::Warn,
                format!("polkitd not found: {consequence}"),
                Some("install polkit (policykit-1 / polkit)"),
            ),
        }
    }

    fn io_control(&self) -> Requirement {
        let t = "I/O control";
        let mut have = Vec::new();
        if self.p.io_latency == IoLatency::Available {
            have.push("io.latency");
        }
        if self.p.io_cost.available {
            have.push("iocost");
        }
        let devs = self.p.devices();
        if devs.iter().any(|d| d.is_bfq() || d.schedulers.iter().any(|s| s == "bfq")) {
            have.push("BFQ");
        }
        if self.p.io_max {
            have.push("io.max");
        }
        if have.is_empty() {
            req(
                "io_control",
                t,
                Status::Warn,
                "no io.latency, iocost, BFQ or io.max: background I/O cannot be throttled",
                Some("use cgroup v2 with the io controller, or the bfq scheduler"),
            )
        } else {
            req("io_control", t, Status::Pass, have.join(", "), None)
        }
    }

    fn writeback(&self) -> Requirement {
        let t = "cgroup writeback";
        if self.p.mounts.is_empty() {
            return req("writeback", t, Status::Warn, "could not resolve the filesystems behind / and /home", None);
        }
        let limited: Vec<String> = self
            .p
            .mounts
            .iter()
            .filter(|m| m.writeback != Writeback::Supported)
            .map(|m| format!("{} ({})", m.path, m.fs_type))
            .collect();
        if limited.is_empty() {
            let mut fs: Vec<&str> = self.p.mounts.iter().map(|m| m.fs_type.as_str()).collect();
            fs.dedup();
            req("writeback", t, Status::Pass, format!("supported ({})", fs.join(", ")), None)
        } else {
            req(
                "writeback",
                t,
                Status::Warn,
                format!(
                    "limited on {}: buffered writes are charged to the root cgroup and escape I/O limits",
                    limited.join(", ")
                ),
                Some("ext4, xfs, btrfs or f2fs support cgroup writeback"),
            )
        }
    }

    fn conflicts(&self) -> Requirement {
        let t = "Conflicting daemons";
        let running: Vec<&str> =
            self.p.conflicts.iter().filter(|c| c.running && c.name != "gamemode").map(|c| c.name.as_str()).collect();
        if !running.is_empty() {
            let units: Vec<String> = self
                .p
                .conflicts
                .iter()
                .filter(|c| running.contains(&c.name.as_str()))
                .map(|c| c.unit.clone().unwrap_or_else(|| format!("{}.service", c.name)))
                .collect();
            let remedy = format!("systemctl disable --now {}", units.join(" "));
            return Requirement {
                id: "conflicts",
                title: t.into(),
                status: Status::Warn,
                detail: format!(
                    "{} running: it also adjusts priorities/cgroups and will fight lindesk",
                    running.join(", ")
                ),
                remedy: Some(remedy),
            };
        }
        let mut notes = Vec::new();
        if self.p.conflicts.iter().any(|c| c.name == "gamemode" && c.running) {
            notes.push("gamemode running (compatible: it renices games, lindesk leaves them alone)".to_string());
        }
        let idle: Vec<&str> =
            self.p.conflicts.iter().filter(|c| !c.running && c.name != "gamemode").map(|c| c.name.as_str()).collect();
        if !idle.is_empty() {
            notes.push(format!("{} installed but inactive (keep it disabled)", idle.join(", ")));
        }
        let detail = if notes.is_empty() { "none".to_string() } else { notes.join("; ") };
        req("conflicts", t, Status::Pass, detail, None)
    }

    fn desktop(&self) -> Requirement {
        let t = "Graphical session";
        let wl = self.env.get("WAYLAND_DISPLAY");
        let x = self.env.get("DISPLAY");
        match (wl, x) {
            (Some(w), _) => req("desktop", t, Status::Pass, format!("Wayland ({w})"), None),
            (None, Some(d)) => req("desktop", t, Status::Pass, format!("X11 ({d})"), None),
            (None, None) => req(
                "desktop",
                t,
                Status::Warn,
                "no WAYLAND_DISPLAY or DISPLAY: no foreground tracking; lindesk will still protect session.slice",
                Some("start lindesk-session from the graphical session (systemctl --user import-environment)"),
            ),
        }
    }

    fn hardware(&self) -> Requirement {
        req("hardware", "Hardware", Status::Pass, summary_line(&self.p.hardware), None)
    }

    fn virtualization(&self) -> Requirement {
        let t = "Virtualization";
        match self.p.hardware.virtualization.as_deref() {
            None => req("virtualization", t, Status::Pass, "bare metal", None),
            Some(v) if hardware::is_container(v) => req(
                "virtualization",
                t,
                Status::Warn,
                format!("{v} container: cgroup control is likely restricted"),
                Some("run lindesk on the host, or give the container a delegated cgroup v2 subtree"),
            ),
            Some(v) => req(
                "virtualization",
                t,
                Status::Pass,
                format!("{v} guest: PSI and I/O control reflect the guest only"),
                None,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{self, Fixture};
    use crate::probe::probe;
    use std::path::Path;

    struct Host {
        _d: tempfile::TempDir,
        roots: Roots,
        env: Env,
        p: Probe,
    }

    fn host(build: fn(&Path) -> (Roots, Env)) -> Host {
        host_with(build, |_| {})
    }

    fn host_with(build: fn(&Path) -> (Roots, Env), tweak: impl FnOnce(&Fixture)) -> Host {
        let d = tempfile::tempdir().unwrap();
        let (roots, env) = build(d.path());
        tweak(&Fixture::new(d.path()));
        let p = probe(&roots, &env);
        Host { _d: d, roots, env, p }
    }

    impl Host {
        fn check(&self, c: Component, opts: CheckOptions) -> RequirementsReport {
            check_with(&self.roots, &self.env, c, &self.p, &opts)
        }
        fn status(&self, c: Component, opts: CheckOptions, id: &str) -> (Status, String, Option<String>) {
            let r = self.check(c, opts);
            let i = r.get(id).unwrap_or_else(|| panic!("no {id} in {r:#?}"));
            (i.status, i.detail.clone(), i.remedy.clone())
        }
    }

    fn root() -> CheckOptions {
        CheckOptions::default().euid(0)
    }
    fn user() -> CheckOptions {
        CheckOptions::default().euid(1000)
    }

    /// The dev host with both buses and polkit present.
    fn dev_host(f: &Fixture) {
        f.file("run/dbus/system_bus_socket", "")
            .file("run/user/1000/bus", "")
            .file("usr/share/polkit-1/actions/org.lindesk.policy", "")
            .file("usr/lib/polkit-1/polkitd", "");
    }

    fn session_env(h: &Host) -> Env {
        h.env.clone().with("XDG_RUNTIME_DIR", h.roots.run.join("user/1000").to_string_lossy())
    }

    #[test]
    fn dev_host_broker_and_session_pass() {
        let mut h = host_with(fixture::pop_os, dev_host);
        let r = h.check(Component::Broker, root());
        assert!(r.ok(), "{}", r.render_text(false));
        assert_eq!(r.warnings().count(), 0, "{}", r.render_text(false));
        let ids: Vec<&str> = r.items.iter().map(|i| i.id).collect();
        assert_eq!(
            ids,
            [
                "linux",
                "kernel",
                "cgroups",
                "psi",
                "systemd",
                "root",
                "dbus",
                "polkit",
                "io_control",
                "writeback",
                "conflicts",
                "hardware",
                "virtualization"
            ]
        );
        assert_eq!(r.get("conflicts").unwrap().detail, "system76-scheduler installed but inactive (keep it disabled)");
        assert_eq!(r.get("io_control").unwrap().detail, "iocost, BFQ, io.max");
        assert!(r.get("hardware").unwrap().detail.starts_with("x86_64 24×CPU (16C, SMT, hybrid 8P+8E)"));

        h.env = session_env(&h);
        let r = h.check(Component::Session, user());
        assert!(r.ok() && r.warnings().count() == 0, "{}", r.render_text(false));
        assert!(r.get("root").is_none() && r.get("polkit").is_none());
        assert_eq!(r.get("user_delegation").unwrap().detail, "delegated: cpu io memory pids");
        assert_eq!(r.get("desktop").unwrap().detail, "Wayland (wayland-0)");
    }

    #[test]
    fn root_requirement() {
        let mut h = host_with(fixture::pop_os, dev_host);
        let (s, d, rem) = h.status(Component::Broker, user(), "root");
        assert_eq!((s, d.as_str()), (Status::Fail, "running as uid 1000"));
        assert_eq!(rem.as_deref(), Some("lindeskd must run as root; run lindesk-session for per-user mode"));
        let r = h.check(Component::Broker, user());
        assert!(!r.ok());
        assert_eq!(r.failures().map(|f| f.id).collect::<Vec<_>>(), ["root"]);

        h.env = h.env.clone().with("LINDESK_ALLOW_UNPRIVILEGED", "1");
        assert_eq!(h.status(Component::Broker, user(), "root").0, Status::Warn);
        assert!(h.check(Component::Broker, user()).ok());
        h.env = h.env.clone().with("LINDESK_ALLOW_UNPRIVILEGED", "0");
        assert_eq!(h.status(Component::Broker, user(), "root").0, Status::Fail);
    }

    #[test]
    fn linux_and_kernel_versions() {
        let d = tempfile::tempdir().unwrap();
        let roots = Roots::under(d.path());
        let p = probe(&roots, &Env::new());
        let r = check_with(&roots, &Env::new(), Component::Cli, &p, &root());
        assert_eq!(r.get("linux").unwrap().detail, "/proc is not mounted");
        assert_eq!(r.get("linux").unwrap().status, Status::Fail);

        let f = Fixture::new(d.path());
        f.dir("proc");
        let p = probe(&roots, &Env::new());
        let r = check_with(&roots, &Env::new(), Component::Cli, &p, &root());
        assert_eq!(r.get("linux").unwrap().status, Status::Fail);
        assert!(r.get("linux").unwrap().detail.contains("osrelease"));
        assert_eq!(r.get("cgroups").unwrap().status, Status::Warn, "no cgroup fs = per-process");

        let kernel = |rel: &str| {
            f.file("proc/sys/kernel/osrelease", format!("{rel}\n"));
            let p = probe(&roots, &Env::new());
            let r = check_with(&roots, &Env::new(), Component::Cli, &p, &root());
            assert_eq!(r.get("linux").unwrap().status, Status::Pass);
            let k = r.get("kernel").unwrap().clone();
            (k.status, k.detail)
        };
        assert_eq!(kernel("3.2.0-4-amd64").0, Status::Fail);
        let (s, d) = kernel("4.19.0-27-amd64");
        assert_eq!(s, Status::Warn);
        assert!(d.contains("reactive-blind with static class weights only"), "{d}");
        let (s, d) = kernel("5.3.18");
        assert_eq!(s, Status::Warn);
        assert!(d.contains("no cgroup uclamp"), "{d}");
        assert_eq!(kernel("5.4.0-150-generic").0, Status::Pass);
        assert_eq!(kernel("weird").0, Status::Warn);
        assert_eq!(kernel_version("6.12.9-arch1-1"), Some((6, 12)));
        assert_eq!(kernel_version("4"), Some((4, 0)));
    }

    #[test]
    fn cgroup_modes() {
        let h = host(fixture::cgroup_v1);
        let (s, d, rem) = h.status(Component::Broker, root(), "cgroups");
        assert_eq!(s, Status::Warn);
        assert!(d.contains("cgroup v1"));
        assert!(rem.unwrap().contains("unified_cgroup_hierarchy"));
        // v1: delegation is not applicable, but reported.
        assert_eq!(h.status(Component::Session, user(), "user_delegation").0, Status::Warn);
        let h = host_with(fixture::pop_os, |f| {
            f.file("sys/fs/cgroup/cgroup.controllers", "cpu memory\n");
        });
        let (s, d, _) = h.status(Component::Broker, root(), "cgroups");
        assert_eq!((s, d.as_str()), (Status::Warn, "cgroup v2 without the io controller(s)"));
    }

    #[test]
    fn psi_paths() {
        let h = host(fixture::no_psi);
        let (s, _, rem) = h.status(Component::Broker, root(), "psi");
        assert_eq!(s, Status::Warn);
        assert!(rem.unwrap().contains("psi=1"));

        let h = host_with(fixture::no_psi, |f| {
            f.file("proc/cmdline", "BOOT_IMAGE=/vmlinuz root=/dev/sda1 psi=0 quiet\n");
        });
        let (s, d, rem) = h.status(Component::Broker, root(), "psi");
        assert_eq!(s, Status::Warn);
        assert!(d.contains("psi=0"));
        assert_eq!(rem.as_deref(), Some("replace psi=0 with psi=1 on the kernel command line"));

        let h = host_with(fixture::no_psi, |f| {
            f.file(
                "boot/config-7.0.11-76070011-generic",
                fixture::kconfig(&[("PSI", "y"), ("PSI_DEFAULT_DISABLED", "y")]),
            );
        });
        let (s, d, rem) = h.status(Component::Broker, root(), "psi");
        assert_eq!(s, Status::Warn);
        assert!(d.contains("CONFIG_PSI_DEFAULT_DISABLED"));
        assert_eq!(rem.as_deref(), Some("add psi=1 to the kernel command line"));

        let h = host_with(fixture::no_psi, |f| {
            f.file("boot/config-7.0.11-76070011-generic", fixture::kconfig(&[("PSI", "n")]));
        });
        assert!(h.status(Component::Broker, root(), "psi").1.contains("without CONFIG_PSI"));
    }

    #[test]
    fn systemd_versions() {
        let without = host_with(fixture::pop_os, |f| {
            f.remove("run/systemd");
        });
        let (s, d, _) = without.status(Component::Broker, root(), "systemd");
        assert_eq!((s, d.as_str()), (Status::Warn, "non-systemd host: using the raw cgroup backend"));
        assert_eq!(without.status(Component::Session, user(), "user_delegation").0, Status::Warn);

        let ver = |v: &str| {
            let v = v.to_string();
            let h = host_with(fixture::pop_os, move |f| {
                f.remove("usr/lib/systemd/libsystemd-shared-255.so")
                    .file(&format!("usr/lib/systemd/libsystemd-shared-{v}.so"), "");
            });
            let (s, d, _) = h.status(Component::Broker, root(), "systemd");
            (s, d)
        };
        assert_eq!(ver("241").0, Status::Warn);
        let (s, d) = ver("249");
        assert_eq!(s, Status::Pass);
        assert!(d.contains("cpu.idle directly"), "{d}");
        assert_eq!(ver("256"), (Status::Pass, "256 (running)".to_string()));
    }

    #[test]
    fn user_delegation_missing_controllers() {
        let d = tempfile::tempdir().unwrap();
        let f = Fixture::new(d.path());
        fixture::v2_host(&f, &fixture::V2Host { user_controllers: "pids", ..Default::default() });
        let roots = f.roots();
        let p = probe(&roots, &Env::new());
        let r = check_with(&roots, &Env::new(), Component::Session, &p, &user());
        let i = r.get("user_delegation").unwrap();
        assert_eq!(i.status, Status::Warn);
        assert_eq!(i.detail, "cpu, io, memory not delegated to user@1000.service: those knobs are skipped");
        assert!(i.remedy.as_deref().unwrap().contains("Delegate=cpu cpuset io memory pids"));
        // A different uid has no user manager.
        let r = check_with(&roots, &Env::new(), Component::Session, &p, &CheckOptions::default().euid(1001));
        assert!(r.get("user_delegation").unwrap().detail.contains("user@1001.service not found"));
    }

    #[test]
    fn dbus_paths() {
        let h = host(fixture::pop_os);
        // Broker: no system bus socket.
        let (s, d, _) = h.status(Component::Broker, root(), "dbus");
        assert_eq!((s, d.as_str()), (Status::Fail, "no system bus (/run/dbus/system_bus_socket)"));
        let (s, _, _) = h.status(Component::Broker, root().dbus_required(false), "dbus");
        assert_eq!(s, Status::Warn, "--no-dbus");
        // Cli: missing buses only warn.
        assert_eq!(h.status(Component::Cli, user(), "dbus").0, Status::Warn);
        // Session: no session bus.
        assert_eq!(h.status(Component::Session, user(), "dbus").0, Status::Fail);
        let mut h = h;
        h.env = h.env.clone().with("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus");
        assert_eq!(h.status(Component::Session, user(), "dbus").0, Status::Pass);
        h.env = h.env.clone().with("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/x");
        let (s, d, _) = h.status(Component::Broker, root().dbus_required(false), "dbus");
        assert_eq!((s, d.as_str()), (Status::Pass, "system bus unix:path=/x (unused: --no-dbus)"));
        // $XDG_RUNTIME_DIR/bus.
        let h = host_with(fixture::pop_os, dev_host);
        let env = session_env(&h);
        let r = check_with(&h.roots, &env, Component::Session, &h.p, &user());
        assert_eq!(r.get("dbus").unwrap().status, Status::Pass);
    }

    #[test]
    fn polkit_paths() {
        let h = host(fixture::pop_os);
        let (s, d, _) = h.status(Component::Broker, root(), "polkit");
        assert_eq!(s, Status::Warn);
        assert!(d.contains("only root can change classes"));
        let h = host_with(fixture::pop_os, |f| {
            f.file("usr/share/polkit-1/actions/org.lindesk.policy", "");
        });
        let (s, d, _) = h.status(Component::Broker, root(), "polkit");
        assert_eq!((s, d.as_str()), (Status::Warn, "polkitd not found: only root can change classes"));
        let h = host_with(fixture::pop_os, |f| {
            f.file("usr/share/polkit-1/actions/org.lindesk.policy", "")
                .file("usr/lib/systemd/system/polkit.service", "");
        });
        assert_eq!(h.status(Component::Broker, root(), "polkit").0, Status::Pass);
    }

    #[test]
    fn io_control_and_writeback() {
        let h = host(fixture::zfs_root);
        let (s, d, _) = h.status(Component::Broker, root(), "writeback");
        assert_eq!(s, Status::Warn);
        assert!(d.starts_with("limited on / (zfs), /home (zfs)"), "{d}");
        assert_eq!(h.status(Component::Broker, root(), "io_control").0, Status::Pass);

        let d = tempfile::tempdir().unwrap();
        let f = Fixture::new(d.path());
        f.file("proc/sys/kernel/osrelease", "6.1.0\n")
            .file("sys/fs/cgroup/cpu/cpu.shares", "1024\n")
            .disk("sda", "8:0", "[mq-deadline] none", false)
            .mounts(&[("/", "8:0", "ext4", "/dev/sda")]);
        let p = probe(&f.roots(), &Env::new());
        let r = check_with(&f.roots(), &Env::new(), Component::Broker, &p, &root());
        assert_eq!(r.get("io_control").unwrap().status, Status::Warn);
        assert_eq!(r.get("writeback").unwrap().status, Status::Pass);
        let r = check_with(
            &Roots::under("/nonexistent"),
            &Env::new(),
            Component::Cli,
            &probe(&Roots::under("/nonexistent"), &Env::new()),
            &root(),
        );
        assert_eq!(r.get("writeback").unwrap().status, Status::Warn);
    }

    #[test]
    fn conflicts_paths() {
        let h = host(fixture::cgroup_v1);
        let (s, d, rem) = h.status(Component::Broker, root(), "conflicts");
        assert_eq!(s, Status::Warn);
        assert!(d.starts_with("ananicy-cpp running"));
        assert_eq!(rem.as_deref(), Some("systemctl disable --now ananicy-cpp.service"));

        let h = host_with(fixture::pop_os, |f| {
            f.process(3000, "gamemoded");
        });
        let (s, d, _) = h.status(Component::Broker, root(), "conflicts");
        assert_eq!(s, Status::Pass);
        assert!(d.starts_with("gamemode running (compatible"), "{d}");

        let h = host_with(fixture::pop_os, |f| {
            f.process(3000, "system76-schedu");
        });
        let (s, _, rem) = h.status(Component::Broker, root(), "conflicts");
        assert_eq!(s, Status::Warn);
        assert_eq!(rem.as_deref(), Some("systemctl disable --now com.system76.Scheduler.service"));
    }

    #[test]
    fn desktop_paths() {
        let h = host(fixture::kvm_guest);
        let (s, d, _) = h.status(Component::Session, user(), "desktop");
        assert_eq!(s, Status::Warn);
        assert!(d.contains("lindesk will still protect session.slice"));
        let h = host(fixture::hdd_desktop);
        assert_eq!(h.status(Component::Session, user(), "desktop"), (Status::Pass, "X11 (:0)".into(), None));
        assert!(h.check(Component::Broker, root()).get("desktop").is_none());
    }

    #[test]
    fn virtualization_paths() {
        let h = host(fixture::kvm_guest);
        let (s, d, _) = h.status(Component::Broker, root(), "virtualization");
        assert_eq!((s, d.as_str()), (Status::Pass, "kvm guest: PSI and I/O control reflect the guest only"));
        let h = host_with(fixture::pop_os, |f| {
            f.file(".dockerenv", "");
        });
        let (s, d, _) = h.status(Component::Broker, root(), "virtualization");
        assert_eq!((s, d.as_str()), (Status::Warn, "docker container: cgroup control is likely restricted"));
        let h = host(fixture::pop_os);
        assert_eq!(h.status(Component::Cli, user(), "virtualization").1, "bare metal");
    }

    #[test]
    fn rendering() {
        let h = host(fixture::pop_os);
        let r = h.check(Component::Broker, user());
        let text = r.render_text(false);
        assert!(text.starts_with("lindeskd requirements\n  PASS  linux            Linux, /proc mounted\n"), "{text}");
        assert!(text.contains("\n  FAIL  root             running as uid 1000\n"), "{text}");
        assert!(text.contains("\n                         → lindeskd must run as root;"), "{text}");
        assert!(text.ends_with("lindeskd: 2 requirements failed, 1 warning\n"), "{text}");
        assert!(r.render_text(true).contains("\x1b[1;31mFAIL\x1b[0m"));

        let v: serde_json::Value = serde_json::from_str(&r.render_json()).unwrap();
        assert_eq!(v["component"], "broker");
        assert_eq!(v["program"], "lindeskd");
        assert_eq!(v["ok"], false);
        let item = v["items"].as_array().unwrap().iter().find(|i| i["id"] == "root").unwrap();
        assert_eq!(item["status"], "fail");
        assert!(v["items"][0].get("remedy").is_none());

        let h = host_with(fixture::pop_os, dev_host);
        assert!(h.check(Component::Broker, root()).render_text(false).ends_with("lindeskd: all requirements met\n"));
        assert_eq!(EXIT_REQUIREMENTS, 78);
    }

    #[test]
    #[ignore = "checks the real host"]
    fn real_host() {
        let roots = Roots::host();
        let env = Env::from_process();
        let p = probe(&roots, &env);
        for c in [Component::Broker, Component::Session] {
            eprintln!("{}", check(&roots, &env, c, &p).render_text(false));
        }
    }
}
