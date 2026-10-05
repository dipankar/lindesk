//! Resource limits and scheduler rules that something other than lindesk left in the
//! system configuration.
//!
//! lindesk only ever makes runtime changes, so a *persistent* systemd drop-in that caps a
//! slice's I/O, or a udev rule that forces an I/O scheduler, came from somewhere else: a
//! tuning guide, another tool, an earlier experiment. They matter because their effect
//! looks like a hardware problem: a slice capped at 20 MB/s shows up as 95% I/O pressure
//! with an idle disk and an idle CPU, and lindesk (correctly) reports pressure it can't fix.
//! `lindeskctl doctor` lists them so the cause is one command away.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::doctor::{Finding, Severity};
use crate::roots::Roots;

/// systemd resource-control settings that cap or deprioritise a unit.
const LIMIT_KEYS: &[&str] = &[
    "IOReadBandwidthMax",
    "IOWriteBandwidthMax",
    "IOReadIOPSMax",
    "IOWriteIOPSMax",
    "IOWeight",
    "IODeviceWeight",
    "CPUQuota",
    "CPUWeight",
    "AllowedCPUs",
    "MemoryHigh",
    "MemoryMax",
    "MemorySwapMax",
];

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForeignKind {
    /// A persistent systemd drop-in with resource limits.
    UnitLimit,
    /// A udev rule that sets a block device's I/O scheduler.
    IoSchedulerRule,
}

/// One piece of resource tuning lindesk didn't make.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ForeignTuning {
    pub kind: ForeignKind,
    /// The file that holds it.
    pub path: PathBuf,
    /// The unit a drop-in applies to (`app-org.gnome.Terminal.slice`), when known.
    pub unit: Option<String>,
    /// `Key=value` lines (drop-ins) or the rule text (udev), trimmed.
    pub settings: Vec<String>,
}

/// Scan the system configuration under `roots.etc`, plus `home/.config/systemd/user` when a
/// home directory is given.
pub fn scan(roots: &Roots, home: Option<&Path>) -> Vec<ForeignTuning> {
    let mut out = Vec::new();
    let mut unit_dirs = vec![roots.etc.join("systemd/system"), roots.etc.join("systemd/user")];
    if let Some(h) = home {
        unit_dirs.push(h.join(".config/systemd/user"));
    }
    for dir in unit_dirs {
        scan_dropins(&dir, &mut out);
    }
    scan_udev(&roots.etc.join("udev/rules.d"), &mut out);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
    v.sort();
    v
}

/// `<unit>.d/*.conf` drop-ins in one unit directory.
fn scan_dropins(dir: &Path, out: &mut Vec<ForeignTuning>) {
    for d in entries(dir) {
        let Some(unit) = d.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".d")) else { continue };
        // lindesk's own units ship their limits on purpose.
        if unit.starts_with("lindesk") || !d.is_dir() {
            continue;
        }
        for f in entries(&d) {
            if f.extension().is_none_or(|e| e != "conf") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&f) else { continue };
            let settings = limit_lines(&text);
            if !settings.is_empty() {
                out.push(ForeignTuning {
                    kind: ForeignKind::UnitLimit,
                    path: f,
                    unit: Some(unit.to_string()),
                    settings,
                });
            }
        }
    }
}

/// The resource-limit assignments in a unit file; an empty assignment (`IOWeight=`) only
/// resets a value and is ignored.
fn limit_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && !l.starts_with(';'))
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            let (k, v) = (k.trim(), v.trim());
            (LIMIT_KEYS.contains(&k) && !v.is_empty()).then(|| format!("{k}={v}"))
        })
        .collect()
}

fn scan_udev(dir: &Path, out: &mut Vec<ForeignTuning>) {
    for f in entries(dir) {
        if f.extension().is_none_or(|e| e != "rules") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&f) else { continue };
        let settings: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#') && l.contains("queue/scheduler}=") && !l.contains("queue/scheduler}=="))
            .map(str::to_string)
            .collect();
        if !settings.is_empty() {
            out.push(ForeignTuning { kind: ForeignKind::IoSchedulerRule, path: f, unit: None, settings });
        }
    }
}

/// Doctor lines for what [`scan`] found (none when the configuration is clean).
pub fn findings(found: &[ForeignTuning]) -> Vec<Finding> {
    found
        .iter()
        .map(|t| match t.kind {
            ForeignKind::UnitLimit => {
                let unit = t.unit.as_deref().unwrap_or("?");
                let caps = t.settings.iter().any(|s| s.contains("Max=") || s.starts_with("CPUQuota"));
                Finding {
                    // A hard cap makes pressure lindesk can't relieve; weights are only worth knowing.
                    severity: if caps { Severity::Warn } else { Severity::Info },
                    check: "Limit outside lindesk".into(),
                    status: format!("{unit}: {} ({})", t.settings.join(", "), t.path.display()),
                    remedy: Some(if caps {
                        format!(
                            "everything in {unit} shares this cap, which shows up as pressure with an idle disk/CPU; \
                             if you didn't mean it: rm {} && systemctl{} daemon-reload",
                            t.path.display(),
                            if t.path.to_string_lossy().contains("/user/") { " --user" } else { "" }
                        )
                    } else {
                        "lindesk sets weights at runtime; a fixed one here overrides the defaults it starts from".into()
                    }),
                }
            }
            ForeignKind::IoSchedulerRule => Finding {
                severity: Severity::Info,
                check: "I/O scheduler rule".into(),
                status: format!("{} ({})", t.settings.join(" | "), t.path.display()),
                remedy: Some(
                    "a udev rule pins the I/O scheduler; BFQ on a fast NVMe can collapse throughput under many \
                     parallel writers (lindesk works with any scheduler; with `none` it uses iocost)"
                        .into(),
                ),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn finds_slice_caps_and_scheduler_rules_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = Roots::under(tmp.path());
        let home = tmp.path().join("home");
        // The two things found on a real machine.
        write(
            &home.join(".config/systemd/user/app-org.gnome.Terminal.slice.d/60-io-protection.conf"),
            "[Slice]\n# builds\nIOAccounting=yes\nIOWeight=10\nIOWriteBandwidthMax=/dev/nvme0n1 20M\n",
        );
        write(
            &roots.etc.join("udev/rules.d/99-local-desktop-bfq.rules"),
            "ACTION==\"add|change\", KERNEL==\"nvme*n*\", TEST==\"queue/scheduler\", ATTR{queue/scheduler}=\"bfq\"\n",
        );
        // Not limits: a reset, a comment, an unrelated drop-in, a match-only udev rule, lindesk's own.
        write(&roots.etc.join("systemd/system/foo.service.d/10-reset.conf"), "[Service]\nIOWeight=\n#CPUQuota=10%\n");
        write(&roots.etc.join("systemd/system/bar.service.d/env.conf"), "[Service]\nEnvironment=A=1\n");
        write(&roots.etc.join("systemd/system/lindeskd.service.d/x.conf"), "[Service]\nCPUWeight=500\n");
        write(&roots.etc.join("udev/rules.d/60-match.rules"), "ATTR{queue/scheduler}==\"none\", TAG+=\"x\"\n");
        write(&roots.etc.join("systemd/system/db.service.d/weight.conf"), "[Service]\nCPUWeight=50\n");

        let found = scan(&roots, Some(&home));
        assert_eq!(found.len(), 3, "{found:#?}");
        let slice = found.iter().find(|t| t.unit.as_deref() == Some("app-org.gnome.Terminal.slice")).unwrap();
        assert_eq!(slice.settings, ["IOWeight=10", "IOWriteBandwidthMax=/dev/nvme0n1 20M"]);
        assert!(found.iter().any(|t| t.kind == ForeignKind::IoSchedulerRule));

        let lines = findings(&found);
        let cap = lines.iter().find(|f| f.status.contains("Terminal.slice")).unwrap();
        assert_eq!(cap.severity, Severity::Warn);
        assert!(cap.remedy.as_ref().unwrap().contains("systemctl --user daemon-reload"));
        let weight = lines.iter().find(|f| f.status.contains("db.service")).unwrap();
        assert_eq!(weight.severity, Severity::Info);
        assert!(scan(&Roots::under(tmp.path().join("empty")), None).is_empty());
    }
}
