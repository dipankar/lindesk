//! `lindeskctl doctor`: turn a [`Probe`] into checks with concrete remedies.

use lindesk_plugin_api::{PowerSource, StorageKind};
use serde::{Deserialize, Serialize};

use crate::hardware::{self, describe_cpu, describe_memory, describe_power, describe_storage};
use crate::probe::{Backend, CgroupMode, DesktopKind, IoLatency, Probe, SessionType, SupportLevel, Writeback};

/// How much a finding matters.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Ok,
    Info,
    Warn,
    Error,
}

/// One line of the doctor report.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    /// What was checked, e.g. `io.latency`.
    pub check: String,
    /// What was found, e.g. `not built (CONFIG_BLK_CGROUP_IOLATENCY)`.
    pub status: String,
    /// What to do about it, or what lindesk does instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
}

impl Finding {
    fn new(severity: Severity, check: &str, status: impl Into<String>, remedy: Option<String>) -> Self {
        Finding { severity, check: check.to_string(), status: status.into(), remedy }
    }
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// Every check, in report order.
pub fn findings(p: &Probe) -> Vec<Finding> {
    use Severity::*;
    let mut out = Vec::new();
    let mut push = |sev, check: &str, status: String, remedy: Option<&str>| {
        out.push(Finding::new(sev, check, status, remedy.map(str::to_string)));
    };
    let v2 = p.cgroup_mode == CgroupMode::V2;

    match &p.kernel {
        Some(k) => push(Ok, "Kernel", k.clone(), None),
        None => push(Warn, "Kernel", "unknown".into(), Some("/proc/sys/kernel/osrelease is unreadable")),
    }
    for f in hardware_findings(p).into_iter().chain(swap_findings(p)) {
        push(f.severity, &f.check, f.status, f.remedy.as_deref());
    }

    let unified = "boot with systemd.unified_cgroup_hierarchy=1 to switch to cgroup v2";
    match p.cgroup_mode {
        CgroupMode::V2 => push(Ok, "cgroup v2", "yes".into(), None),
        CgroupMode::Hybrid => push(Warn, "cgroup v2", "hybrid (controllers on v1)".into(), Some(unified)),
        CgroupMode::V1 => push(Warn, "cgroup v2", "no (cgroup v1)".into(), Some(unified)),
        CgroupMode::None => {
            push(Error, "cgroup v2", "no cgroup filesystem".into(), Some("mount cgroup2 at /sys/fs/cgroup"))
        }
    }

    if v2 {
        let missing: Vec<&str> = ["cpu", "io", "memory"].into_iter().filter(|c| !p.has_controller(c)).collect();
        let status = p.root_controllers.join(" ");
        if missing.is_empty() {
            push(Ok, "Controllers", status, None);
        } else {
            let r = format!(
                "kernel lacks the {} controller(s); rebuild with CONFIG_CGROUP_SCHED/CONFIG_BLK_CGROUP/CONFIG_MEMCG",
                missing.join(", ")
            );
            push(Warn, "Controllers", status, Some(r.as_str()));
        }
        if p.user_managers.is_empty() {
            let note = if p.systemd.running { Some("start a user session to see the user manager") } else { None };
            push(Info, "User delegation", "no user manager".into(), note);
        }
        for u in &p.user_managers {
            let check = format!("User delegation ({})", u.uid);
            let status = u.controllers.join(" ");
            if ["cpu", "io", "memory"].iter().all(|c| u.controllers.iter().any(|x| x == c)) {
                push(Ok, &check, status, None);
            } else {
                push(
                    Warn,
                    &check,
                    if status.is_empty() { "none".into() } else { status },
                    Some(
                        "delegate controllers: systemctl edit user@.service → [Service] Delegate=cpu cpuset io memory pids",
                    ),
                );
            }
        }
    } else if !p.v1_controllers.is_empty() {
        push(Info, "Controllers", format!("v1: {}", p.v1_controllers.join(" ")), None);
    }

    match (&p.systemd.running, &p.systemd.version) {
        (true, Some(v)) => push(Ok, "systemd", format!("{v} (running)"), None),
        (true, None) => push(Ok, "systemd", "running".into(), None),
        (false, _) => {
            push(Info, "systemd", "not running".into(), Some("using direct cgroup writes instead of unit properties"))
        }
    }

    match (p.psi.global, p.psi.cgroup) {
        (true, true) => push(Ok, "PSI", "global + per-cgroup".into(), None),
        (true, false) => push(
            Warn,
            "PSI",
            "global only".into(),
            Some("per-cgroup pressure needs cgroup v2; foreground stalls are inferred from global PSI"),
        ),
        (false, _) => push(
            Error,
            "PSI",
            "missing".into(),
            Some("boot with psi=1 (needs CONFIG_PSI=y); without PSI lindesk has no stall feedback"),
        ),
    }

    if p.uclamp {
        push(Ok, "uclamp", "yes".into(), None);
    } else {
        push(
            Info,
            "uclamp",
            "no".into(),
            Some("CONFIG_UCLAMP_TASK_GROUP not set or cpu controller off: boosts use cpu.weight only"),
        );
    }

    let fallback = if p.io_cost.available { "using iocost + io.max" } else { "using io.max" };
    match p.io_latency {
        IoLatency::Available => push(Ok, "io.latency", "available".into(), None),
        IoLatency::NotBuilt => {
            push(Info, "io.latency", "not built (CONFIG_BLK_CGROUP_IOLATENCY)".into(), Some(fallback))
        }
        IoLatency::NotEnabled if !v2 => push(Info, "io.latency", "built, needs cgroup v2".into(), None),
        IoLatency::NotEnabled => push(
            Info,
            "io.latency",
            "built, io controller not enabled".into(),
            Some("enable io in cgroup.subtree_control of the root"),
        ),
        IoLatency::Unknown => {
            push(Info, "io.latency", "not available (no kernel config to tell why)".into(), Some(fallback))
        }
    }

    // iocost: only a warning when nothing else gives proportional I/O control.
    let devs = p.devices();
    if !p.io_cost.available {
        push(Info, "iocost", "not available (CONFIG_BLK_CGROUP_IOCOST)".into(), None);
    } else {
        let on: Vec<&str> = p.io_cost.devices.iter().filter(|d| d.enabled).map(|d| d.name.as_str()).collect();
        let off: Vec<&str> = p.io_cost.devices.iter().filter(|d| !d.enabled).map(|d| d.name.as_str()).collect();
        let needed = !p.io_control_effective();
        let remedy = "lindeskd enables it when io.iocost = \"auto\"";
        if off.is_empty() && !on.is_empty() {
            push(Ok, "iocost", format!("enabled on {}", on.join(", ")), None);
        } else if on.is_empty() {
            push(if needed { Warn } else { Info }, "iocost", "available, disabled".into(), Some(remedy));
        } else {
            let status = format!("enabled on {}, disabled on {}", on.join(", "), off.join(", "));
            push(if needed { Warn } else { Info }, "iocost", status, Some(remedy));
        }
    }

    if devs.is_empty() {
        push(Info, "I/O scheduler", "no block devices resolved behind / and /home".into(), None);
    } else {
        let status: Vec<String> =
            devs.iter().map(|d| format!("{} {}", d.name, d.scheduler.as_deref().unwrap_or("n/a"))).collect();
        let inert = devs.iter().any(|d| !d.is_bfq() && !p.iocost_enabled(d.dev));
        if inert {
            push(
                Info,
                "I/O scheduler",
                status.join(", "),
                Some(
                    "io.weight is inert on 'none'/'mq-deadline'/'kyber' unless iocost is enabled (lindeskd does that with io.iocost = \"auto\")",
                ),
            );
        } else {
            push(Ok, "I/O scheduler", status.join(", "), None);
        }
    }

    if p.io_weight_effective {
        push(Ok, "io.weight", "effective".into(), None);
    } else if !devs.is_empty() {
        push(Warn, "io.weight", "inert".into(), Some("enable iocost (io.iocost = \"auto\") or switch the disk to bfq"));
    } else {
        push(Info, "io.weight", "unknown (no block device)".into(), None);
    }

    if p.io_max {
        push(Ok, "io.max", "yes".into(), None);
    } else {
        push(Info, "io.max", "no".into(), Some(if v2 { "enable the io controller" } else { "needs cgroup v2" }));
    }

    for (i, m) in p.mounts.iter().enumerate() {
        let check = format!("Filesystem {}", m.path);
        if let Some(first) = p.mounts[..i].iter().find(|o| o.mount_point == m.mount_point) {
            push(Ok, &check, format!("{}, same filesystem as {}", m.fs_type, first.path), None);
            continue;
        }
        match m.writeback {
            Writeback::Supported => push(Ok, &check, format!("{}, cgroup writeback supported", m.fs_type), None),
            _ => {
                let fs = if m.fs_type == "zfs" { "ZFS".to_string() } else { m.fs_type.clone() };
                let r = format!("{fs}: cgroup writeback limited, buffered writes are attributed to root");
                push(Warn, &check, format!("{}, cgroup writeback limited", m.fs_type), Some(r.as_str()));
            }
        }
    }
    if p.mounts.is_empty() {
        push(Warn, "Filesystems", "could not read /proc/self/mountinfo".into(), None);
    }

    match (&p.sched_ext.available, &p.sched_ext.ops) {
        (false, _) => push(Info, "sched_ext", "not available".into(), None),
        (true, Some(ops)) if p.sched_ext.enabled() => push(Info, "sched_ext", format!("enabled ({ops})"), None),
        (true, _) => {
            let state = p.sched_ext.state.as_deref().unwrap_or("unknown");
            push(Info, "sched_ext", format!("available, {state}"), None);
        }
    }
    push(if p.btf { Ok } else { Info }, "BPF/BTF", yes_no(p.btf).into(), None);

    let session = match p.desktop.session {
        SessionType::Wayland => "Wayland",
        SessionType::X11 => "X11",
        SessionType::Tty => "tty",
        SessionType::Unknown => "unknown session",
    };
    let (kind, adapter) = match (p.desktop.kind, p.desktop.session) {
        (DesktopKind::Gnome, _) => ("GNOME", Some("install the lindesk GNOME Shell extension for focus tracking")),
        (DesktopKind::Kde, _) => ("KDE", Some("install the lindesk KWin script for focus tracking")),
        (DesktopKind::Cosmic, _) => ("COSMIC", None),
        (DesktopKind::Hyprland, _) => ("Hyprland", None),
        (DesktopKind::Other, SessionType::X11) => ("other", None),
        (DesktopKind::Other, _) => ("other", Some("no built-in focus adapter; write an adapter plugin or use X11")),
        (DesktopKind::Unknown, _) => ("unknown", Some("run doctor from inside the desktop session")),
    };
    let name = p.desktop.name.as_deref().map(|n| format!(" [{n}]")).unwrap_or_default();
    let sev = if matches!(p.desktop.kind, DesktopKind::Unknown | DesktopKind::Other) { Info } else { Ok };
    push(sev, "Desktop", format!("{kind} ({session}){name}"), adapter);

    if p.conflicts.is_empty() {
        push(Ok, "Conflicts", "none".into(), None);
    }
    for c in &p.conflicts {
        let unit = c.unit.clone().unwrap_or_else(|| format!("{}.service", c.name));
        if !lindesk_core::conflicts::is_conflict(&c.name) {
            let state = if c.running { "running (compatible)" } else { "installed (compatible)" };
            push(Info, &c.name, state.into(), Some("coexists with lindesk: temporary, per-game changes"));
            continue;
        }
        if c.running {
            let how = format!(
                "sudo lindeskctl setup conflicts --disable (a revertible change; or systemctl disable --now {unit}). \
                 Why: {CONFLICTS_GUIDE}"
            );
            let r = if c.name == "system76-scheduler" {
                format!("system76-scheduler active: disable its foreground boost or stop it: {how}")
            } else {
                format!("{} active: it also adjusts priorities; stop it: {how}", c.name)
            };
            push(Warn, &c.name, "running".into(), Some(r.as_str()));
        } else {
            push(Info, &c.name, "installed, inactive".into(), Some("keep it disabled while lindesk runs"));
        }
    }

    let backend = p.backend();
    let sev = if backend == Backend::PerProcess { Warn } else { Ok };
    push(sev, "Backend", backend.to_string(), None);
    out
}

/// Why the other priority daemons conflict, and how to coexist.
pub const CONFLICTS_GUIDE: &str = "https://github.com/dipankar/lindesk/blob/main/docs/src/guide/conflicts.md";

/// The hardware section: `CPU`, `Memory`, one `Storage` line per managed disk, `Power` and
/// `Virtualized`, with the hardware-specific advice.
pub fn hardware_findings(p: &Probe) -> Vec<Finding> {
    use Severity::*;
    let hw = &p.hardware;
    let mut out = Vec::new();
    let mut push = |sev, check: &str, status: String, remedy: Option<String>| {
        out.push(Finding::new(sev, check, status, remedy));
    };

    let cpu = describe_cpu(hw);
    if hw.cpus == 0 {
        push(Info, "CPU", cpu, Some("could not read /sys/devices/system/cpu".into()));
    } else if hw.is_hybrid() {
        let note = if p.uclamp {
            "foreground boosts favour P-cores via uclamp.min (EAS/ITMT); batch is capped with uclamp.max \
             under pressure, which keeps it on E-cores"
        } else {
            "no cgroup uclamp: lindesk cannot steer the foreground towards P-cores"
        };
        push(Info, "CPU", cpu, Some(note.into()));
    } else {
        push(Ok, "CPU", cpu, None);
    }

    cpu_feature_findings(hw, &mut push);

    let mem = describe_memory(hw);
    if hw.mem_total > 0 && hw.mem_total < 4 << 30 {
        push(
            Info,
            "Memory",
            mem,
            Some("small RAM: memory.low protections are scaled down (critical ≤ 4%, foreground ≤ 10% of RAM)".into()),
        );
    } else {
        push(if hw.mem_total == 0 { Info } else { Ok }, "Memory", mem, None);
    }

    for s in &hw.storage {
        let status = describe_storage(s);
        if s.kind == StorageKind::Hdd && matches!(s.scheduler.as_str(), "none" | "") {
            let r = format!(
                "rotational disk on 'none': switch to bfq or mq-deadline (echo bfq > /sys/block/{}/queue/scheduler; \
                 persist with a udev rule)",
                s.name
            );
            push(Warn, "Storage", status, Some(r));
        } else {
            push(Ok, "Storage", status, None);
        }
    }

    let power = describe_power(hw);
    match hw.power {
        PowerSource::Ac => push(Ok, "Power", power, None),
        PowerSource::Battery => {
            push(Info, "Power", power, Some("on battery: shorter interaction boosts, background yields harder".into()))
        }
        PowerSource::Unknown => push(Info, "Power", power, None),
    }

    match hw.virtualization.as_deref() {
        None => push(Ok, "Virtualized", "no".into(), None),
        Some(v) if hardware::is_container(v) => push(
            Warn,
            "Virtualized",
            format!("{v} (container)"),
            Some("cgroup and I/O control are likely restricted by the container runtime".into()),
        ),
        Some(v) => push(Info, "Virtualized", v.to_string(), Some("PSI and I/O control reflect the guest only".into())),
    }
    out
}

/// The "Memory & swap" section: each swap area, the reclaim tunables, zswap, MGLRU, swap
/// activity, the top swapped units, then every [`crate::swap::SwapFinding`] (its proposed
/// change ops are listed in the remedy).
pub fn swap_findings(p: &Probe) -> Vec<Finding> {
    use crate::swap::{describe_device, describe_tunables};
    use Severity::*;
    let r = &p.swap;
    let mut out = Vec::new();
    for d in &r.devices {
        out.push(Finding::new(Ok, "Swap", describe_device(d), None));
    }
    if r.devices.is_empty() {
        out.push(Finding::new(Info, "Swap", "none", None));
    }
    if r.vm.swappiness.is_some() {
        out.push(Finding::new(Ok, "Reclaim tunables", describe_tunables(&r.vm), None));
    }
    if let Some(z) = &r.zswap {
        let status = if z.enabled {
            format!(
                "on ({}, max pool {}%)",
                z.compressor.as_deref().unwrap_or("?"),
                z.max_pool_percent.map_or("?".into(), |v| v.to_string())
            )
        } else {
            "off".into()
        };
        out.push(Finding::new(Ok, "zswap", status, None));
    }
    if let Some(m) = &r.mglru {
        out.push(Finding::new(Ok, "MGLRU", format!("{} ({})", if m.enabled { "on" } else { "off" }, m.raw), None));
    }
    if r.counters.pswpin + r.counters.pswpout > 0 {
        let mut s = format!(
            "{} swapped in, {} out since boot",
            hardware::fmt_bytes(r.counters.pswpin * 4096),
            hardware::fmt_bytes(r.counters.pswpout * 4096)
        );
        if let Some(rt) = r.rates {
            s.push_str(&format!("; now {:.0} in / {:.0} out pages/s", rt.pswpin_per_s, rt.pswpout_per_s));
        }
        out.push(Finding::new(Ok, "Swap activity", s, None));
    }
    if !r.top_units.is_empty() {
        let top: Vec<String> = r
            .top_units
            .iter()
            .take(3)
            .map(|u| {
                format!("{} {}", u.cgroup.rsplit('/').next().unwrap_or(&u.cgroup), hardware::fmt_bytes(u.swap_bytes))
            })
            .collect();
        out.push(Finding::new(Ok, "Top swap users", top.join(", "), None));
    }
    for f in &r.findings {
        let ops: Vec<String> = f.proposed_ops.iter().map(|o| o.to_string()).collect();
        let remedy =
            if ops.is_empty() { f.detail.clone() } else { format!("{} Proposed: {}", f.detail, ops.join(" ")) };
        out.push(Finding::new(f.severity, "Swap advice", f.title.clone(), Some(remedy)));
    }
    out
}

/// `CPU features` line plus arch/vendor-specific advice.
fn cpu_feature_findings(
    hw: &lindesk_plugin_api::Hardware,
    push: &mut impl FnMut(Severity, &str, String, Option<String>),
) {
    use Severity::*;
    let f = &hw.features;
    if hw.arch.is_empty() && f.cpufreq_driver.is_none() {
        return;
    }
    push(Ok, "CPU features", hardware::describe_features(hw), None);
    let arm = matches!(hw.arch.as_str(), "aarch64" | "arm" | "armv7l" | "armv8l");
    if arm && hw.is_hybrid() && !f.eas {
        push(
            Warn,
            "EAS",
            "off on a big.LITTLE CPU".into(),
            Some(
                "uclamp won't steer tasks between big and little cores; EAS needs an energy model and the \
                 schedutil governor (echo 1 > /proc/sys/kernel/sched_energy_aware)"
                    .into(),
            ),
        );
    }
    if hw.vendor == lindesk_plugin_api::CpuVendor::Intel && hw.is_hybrid() && !f.itmt {
        push(
            Info,
            "ITMT",
            "off on a hybrid Intel CPU".into(),
            Some("the scheduler does not rank P-cores first; foreground boosts rely on uclamp alone".into()),
        );
    }
    if f.cpufreq_driver.as_deref() == Some("intel_cpufreq") && f.governor.as_deref() == Some("powersave") {
        push(
            Warn,
            "cpufreq governor",
            "powersave with intel_pstate in passive mode".into(),
            Some(
                "passive-mode powersave pins every CPU at its minimum frequency; use schedutil, or boot with \
                 intel_pstate=active"
                    .into(),
            ),
        );
    }
    if f.epp.as_deref() == Some("power") && hw.power == PowerSource::Ac {
        push(
            Warn,
            "EPP",
            "power while on AC".into(),
            Some(
                "energy_performance_preference=power ramps frequency slowly and hurts responsiveness; use \
                 balance_performance on AC"
                    .into(),
            ),
        );
    }
}

const WIDTH: usize = 25;

fn color_code(s: Severity) -> &'static str {
    match s {
        Severity::Ok => "32",
        Severity::Info => "36",
        Severity::Warn => "33",
        Severity::Error => "31",
    }
}

/// The aligned, human-readable report, ending with the responsiveness level and the
/// reasons it isn't FULL. `color` adds ANSI colours to statuses.
pub fn render_text(p: &Probe, color: bool) -> String {
    let mut s = String::new();
    for f in findings(p) {
        let mut status = f.status.clone();
        if let Some(r) = &f.remedy {
            status.push_str(" → ");
            status.push_str(r);
        }
        if color && f.severity != Severity::Ok {
            status = format!("\x1b[{}m{status}\x1b[0m", color_code(f.severity));
        }
        let check = if f.check.chars().count() >= WIDTH { format!("{} ", f.check) } else { f.check.clone() };
        s.push_str(&format!("{check:<WIDTH$}{status}\n"));
    }
    let (level, why) = p.support();
    let lvl = if color {
        let c = match level {
            SupportLevel::Full => "32",
            SupportLevel::IoPartial => "33",
            _ => "31",
        };
        format!("\x1b[1;{c}m{level}\x1b[0m")
    } else {
        level.to_string()
    };
    s.push_str(&format!("Responsiveness level: {lvl}\n"));
    for w in why {
        s.push_str(&format!("  - {w}\n"));
    }
    s
}

/// The machine-readable report: probe, findings, level, reasons, backend and host caps.
pub fn render_json(p: &Probe) -> String {
    let (level, reasons) = p.support();
    let v = serde_json::json!({
        "level": level,
        "reasons": reasons,
        "backend": p.backend(),
        "host_caps": p.host_caps(),
        "findings": findings(p),
        "probe": p,
    });
    serde_json::to_string_pretty(&v).expect("probe serialises")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::probe::probe;
    use crate::{Env, Roots};
    use std::path::Path;

    fn run(build: fn(&Path) -> (Roots, Env)) -> (tempfile::TempDir, Probe) {
        let d = tempfile::tempdir().unwrap();
        let (roots, env) = build(d.path());
        let p = probe(&roots, &env);
        (d, p)
    }

    fn find<'a>(fs: &'a [Finding], check: &str) -> &'a Finding {
        fs.iter().find(|f| f.check == check).unwrap_or_else(|| panic!("no finding {check}: {fs:#?}"))
    }

    #[test]
    fn this_host_report() {
        let (_d, p) = run(fixture::pop_os);
        let text = render_text(&p, false);
        assert!(text.starts_with("Kernel                   7.0.11-76070011-generic\n"), "{text}");
        assert!(text.contains("\ncgroup v2                yes\n"), "{text}");
        assert!(
            text.contains(
                "\nio.latency               not built (CONFIG_BLK_CGROUP_IOLATENCY) → using iocost + io.max\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "\niocost                   available, disabled → lindeskd enables it when io.iocost = \"auto\"\n"
            ),
            "{text}"
        );
        assert!(text.contains("Responsiveness level: IO-PARTIAL\n"), "{text}");
        assert!(text.contains("Filesystem /home         ext4, same filesystem as /"), "{text}");

        let fs = findings(&p);
        assert_eq!(find(&fs, "iocost").severity, Severity::Warn);
        assert_eq!(
            find(&fs, "io.weight").remedy.as_deref(),
            Some("enable iocost (io.iocost = \"auto\") or switch the disk to bfq")
        );
        assert_eq!(find(&fs, "I/O scheduler").status, "nvme0n1 none");
        let s76 = find(&fs, "system76-scheduler");
        assert_eq!((s76.severity, s76.status.as_str()), (Severity::Info, "installed, inactive"));
        assert_eq!(find(&fs, "Desktop").status, "GNOME (Wayland) [ubuntu:GNOME]");
        assert_eq!(find(&fs, "User delegation (1000)").status, "cpu io memory pids");
        assert_eq!(find(&fs, "systemd").status, "255 (running)");
        assert_eq!(find(&fs, "sched_ext").status, "available, disabled");
        assert_eq!(find(&fs, "Backend").status, "systemd");
        assert!(fs.iter().all(|f| f.severity != Severity::Error));

        // Colour wraps statuses but keeps the layout.
        let colored = render_text(&p, true);
        assert!(colored.contains("\x1b[33m"));
        assert!(colored.contains("Kernel                   7.0.11"));
    }

    #[test]
    fn hardware_section() {
        let (_d, p) = run(fixture::pop_os);
        let text = render_text(&p, false);
        assert!(
            text.contains(
                "\nCPU                      12th Gen Intel Core i9-12950HX — hybrid: 8P (16 threads) + 8E → "
            ),
            "{text}"
        );
        assert!(text.contains("\nMemory                   62.5 GiB RAM, 20 GiB swap (compressed)\n"), "{text}");
        assert!(text.contains("\nStorage                  nvme0n1 NVMe (none)\n"), "{text}");
        assert!(text.contains("\nPower                    AC\n"), "{text}");
        assert!(text.contains("\nVirtualized              no\n"), "{text}");

        let (_d, p) = run(fixture::hdd_desktop);
        let fs = hardware_findings(&p);
        let f = find(&fs, "Storage");
        assert_eq!((f.severity, f.status.as_str()), (Severity::Warn, "sda HDD (none)"));
        assert!(f.remedy.as_deref().unwrap().contains("bfq or mq-deadline"));
        assert_eq!(find(&fs, "CPU").severity, Severity::Ok);

        let (_d, p) = run(fixture::arm_big_little);
        let fs = hardware_findings(&p);
        assert!(find(&fs, "Memory").remedy.as_deref().unwrap().contains("scaled down"));
        assert_eq!(find(&fs, "Storage").status, "mmcblk0 eMMC (mq-deadline)");

        let (_d, p) = run(fixture::kvm_guest);
        let fs = hardware_findings(&p);
        let f = find(&fs, "Virtualized");
        assert_eq!((f.severity, f.status.as_str()), (Severity::Info, "kvm"));
        assert_eq!(f.remedy.as_deref(), Some("PSI and I/O control reflect the guest only"));

        let (_d, p) = run(fixture::intel_hybrid_laptop);
        let fs = hardware_findings(&p);
        assert_eq!(find(&fs, "Power").status, "battery (laptop)");
        assert!(find(&fs, "CPU").remedy.as_deref().unwrap().contains("P-cores"));
    }

    #[test]
    fn cpu_feature_and_swap_sections() {
        let (_d, p) = run(fixture::pop_os);
        let text = render_text(&p, false);
        assert!(
            text.contains("\nCPU features             x86_64, intel; intel_pstate/powersave, EPP balance_performance; 1 NUMA node\n"),
            "{text}"
        );
        assert!(text.contains("\nSwap                     zram0 zram/zstd 16 GiB"), "{text}");
        assert!(text.contains("\nReclaim tunables         swappiness 180, page-cluster 0"), "{text}");
        assert!(text.contains("\nMGLRU                    on (0x0007)\n"), "{text}");
        assert!(text.contains("\nTop swap users           app-firefox.scope 1 GiB, docker.service 50 MiB\n"), "{text}");
        let fs = findings(&p);
        assert!(fs.iter().all(|f| f.check != "Swap advice"), "the dev host is well tuned");
        let ita = fs.iter().find(|f| f.check == "ITMT").unwrap();
        assert_eq!(ita.severity, Severity::Info, "Intel hybrid without ITMT");

        let (_d, p) = run(fixture::asahi_m1);
        let f = find(&findings(&p), "EAS").clone();
        assert_eq!(f.severity, Severity::Warn);
        assert!(f.remedy.unwrap().starts_with("uclamp won't steer"));

        let (_d, p) = run(fixture::arm_big_little);
        assert!(findings(&p).iter().all(|f| f.check != "EAS"), "EAS is on");

        let (_d, p) = run(fixture::hdd_desktop);
        let fs = findings(&p);
        assert_eq!(find(&fs, "cpufreq governor").status, "powersave with intel_pstate in passive mode");
        let adv = fs.iter().find(|f| f.check == "Swap advice" && f.status == "Swap on a hard disk").unwrap();
        assert_eq!(adv.severity, Severity::Warn);
        let rem = adv.remedy.as_deref().unwrap();
        assert!(
            rem.contains("Proposed: {") && rem.contains(r#""key":"vm.swappiness""#) && rem.contains(r#""value":"10""#)
        );

        let (_d, p) = run(fixture::amd_desktop);
        let fs = findings(&p);
        assert_eq!(find(&fs, "EPP").status, "power while on AC");
        assert!(find(&fs, "zswap").status.starts_with("on (zstd"));
    }

    #[test]
    fn system76_running_is_a_warning() {
        let d = tempfile::tempdir().unwrap();
        let (roots, env) = fixture::pop_os(d.path());
        fixture::Fixture::new(d.path()).process(1500, "system76-schedu");
        let fs = findings(&probe(&roots, &env));
        let f = find(&fs, "system76-scheduler");
        assert_eq!(f.severity, Severity::Warn);
        assert!(
            f.remedy
                .as_deref()
                .unwrap()
                .starts_with("system76-scheduler active: disable its foreground boost or stop it")
        );
    }

    #[test]
    fn v1_report() {
        let (_d, p) = run(fixture::cgroup_v1);
        let fs = findings(&p);
        let f = find(&fs, "cgroup v2");
        assert_eq!(f.severity, Severity::Warn);
        assert!(f.remedy.as_deref().unwrap().contains("systemd.unified_cgroup_hierarchy=1"));
        assert_eq!(find(&fs, "ananicy-cpp").severity, Severity::Warn);
        assert_eq!(find(&fs, "Backend").status, "cgroup-v1");
        assert!(render_text(&p, false).contains("Responsiveness level: CPU-ONLY"));
    }

    #[test]
    fn no_psi_report() {
        let (_d, p) = run(fixture::no_psi);
        let fs = findings(&p);
        let f = find(&fs, "PSI");
        assert_eq!((f.severity, f.status.as_str()), (Severity::Error, "missing"));
        assert!(f.remedy.as_deref().unwrap().contains("psi=1"));
        assert!(render_text(&p, false).contains("Responsiveness level: CPU-ONLY\n  - no PSI"));
    }

    #[test]
    fn zfs_report() {
        let (_d, p) = run(fixture::zfs_root);
        let fs = findings(&p);
        let f = find(&fs, "Filesystem /");
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.remedy.as_deref(), Some("ZFS: cgroup writeback limited, buffered writes are attributed to root"));
        assert_eq!(find(&fs, "Filesystem /home").severity, Severity::Warn);
        assert!(render_text(&p, false).contains("Responsiveness level: IO-PARTIAL"));
    }

    #[test]
    fn full_report() {
        let (_d, p) = run(fixture::full_bfq);
        let fs = findings(&p);
        assert_eq!(find(&fs, "io.latency").severity, Severity::Ok);
        assert_eq!(find(&fs, "io.weight").status, "effective");
        assert_eq!(find(&fs, "I/O scheduler").status, "sda bfq, nvme0n1 bfq");
        assert_eq!(find(&fs, "iocost").severity, Severity::Info, "iocost not needed with BFQ + io.latency");
        assert_eq!(find(&fs, "sched_ext").status, "enabled (lavd)");
        assert_eq!(find(&fs, "Conflicts").status, "none");
        assert_eq!(find(&fs, "Desktop").remedy.as_deref(), Some("install the lindesk KWin script for focus tracking"));
        assert!(fs.iter().all(|f| f.severity <= Severity::Info), "{fs:#?}");
        assert!(render_text(&p, false).ends_with("Responsiveness level: FULL\n"));
    }

    #[test]
    fn json_report() {
        let (_d, p) = run(fixture::pop_os);
        let v: serde_json::Value = serde_json::from_str(&render_json(&p)).unwrap();
        assert_eq!(v["level"], "io-partial");
        assert_eq!(v["backend"], "systemd");
        assert_eq!(v["host_caps"]["devices"][0], "259:0");
        assert_eq!(v["probe"]["io_latency"], "not_built");
        assert!(v["findings"].as_array().unwrap().iter().any(|f| f["check"] == "iocost" && f["severity"] == "warn"));
    }
}
