//! `recommendations`: turn doctor findings, swap findings, requirement warnings,
//! latency-map anomalies and telemetry into a ranked list of ready-to-plan fixes.
//!
//! [`recommend`] is pure (inputs → ranked list) so it is tested with fixtures; the server
//! gathers the inputs from the daemons or local probing.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use lindesk_plugin_api::{Hardware, PowerSource, StorageKind};

use crate::ops::{self, Risk, Scope};

/// One recommendation.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Recommendation {
    /// Stable id, e.g. `swap.swap-on-a-hard-disk` or `telemetry.red-time`.
    pub id: String,
    pub title: String,
    /// Why it matters, in plain language.
    pub why: String,
    /// The measurements behind it.
    pub evidence: Value,
    /// `ChangeOp` JSON ready for `plan_change` (empty = advice only; lindesk can't apply it).
    pub proposed_ops: Vec<Value>,
    /// Highest guardrail risk of the proposed ops: none | low | medium | high.
    pub risk: Risk,
    /// Scope to pass to plan_change/apply_change: user | system.
    pub scope: Scope,
    /// Where it came from: doctor | swap | requirements | latency | telemetry.
    pub source: String,
    /// Ranking score (higher first).
    pub score: f64,
}

/// A doctor finding (`lindesk_sys::doctor::Finding` JSON).
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct DoctorFinding {
    pub severity: String,
    pub check: String,
    pub status: String,
    pub remedy: Option<String>,
}

/// A swap finding (`lindesk_sys::swap::SwapFinding` JSON).
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct SwapFinding {
    pub severity: String,
    pub title: String,
    pub detail: String,
    pub proposed_ops: Vec<Value>,
}

/// A requirement check (`lindesk_sys::requirements::Requirement` JSON).
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Requirement {
    pub id: String,
    pub title: String,
    pub status: String,
    pub detail: String,
    pub remedy: Option<String>,
}

/// Seconds spent at level RED in one hour.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct RedHour {
    pub hour: String,
    pub red_s: f64,
}

/// How often lindesk throttled a unit.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Throttled {
    pub unit: String,
    pub uid: Option<i64>,
    pub n: i64,
}

/// Telemetry summaries over the last 24 h.
#[derive(Clone, Debug, Default)]
pub struct Telemetry {
    pub red_by_hour: Vec<RedHour>,
    pub throttled: Vec<Throttled>,
}

/// Everything [`recommend`] looks at.
#[derive(Clone, Debug, Default)]
pub struct Inputs {
    pub doctor: Vec<DoctorFinding>,
    pub swap: Vec<SwapFinding>,
    pub requirements: Vec<Requirement>,
    pub hardware: Option<Hardware>,
    pub telemetry: Option<Telemetry>,
    /// Known profile names (empty = unknown; then built-in names are assumed).
    pub profiles: Vec<String>,
}

/// RED seconds per 24 h above which pressure is called frequent.
pub const RED_WARN_S: f64 = 600.0;
/// ... and noticeable.
pub const RED_INFO_S: f64 = 60.0;
/// Throttle actions per 24 h above which a unit is called a repeat offender.
pub const THROTTLE_MIN: i64 = 30;

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

fn severity_score(s: &str) -> f64 {
    match s {
        "error" | "fail" => 100.0,
        "warn" => 70.0,
        "info" => 40.0,
        _ => 20.0,
    }
}

fn rec(
    id: String,
    title: String,
    why: String,
    evidence: Value,
    proposed_ops: Vec<Value>,
    source: &str,
    base: f64,
) -> Recommendation {
    let risk = ops::max_risk_json(&proposed_ops);
    let scope = if proposed_ops.is_empty() { Scope::User } else { ops::scope_json(&proposed_ops) };
    let score = base + if proposed_ops.is_empty() { 0.0 } else { 10.0 };
    Recommendation { id, title, why, evidence, proposed_ops, risk, scope, source: source.into(), score }
}

/// Typical 4 KiB random-read latency by device kind, µs (mirrors `lindesk_sys::calibrate`).
pub fn nominal_read_4k_us(kind: StorageKind) -> f64 {
    lindesk_sys::calibrate::nominal_read_4k_us(kind)
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn latency(hw: &Hardware, out: &mut Vec<Recommendation>) {
    let Some(map) = &hw.latency else {
        out.push(rec(
            "latency.no-baseline".into(),
            "No latency baseline measured yet".into(),
            "Without a calibration run lindesk scales I/O throttles from nominal device speeds and can't spot \
             slow disks or CPUs. Call the `calibrate` tool (system scope, ≈15 s, best on an idle machine)."
                .into(),
            json!({"latency": null}),
            Vec::new(),
            "latency",
            35.0,
        ));
        return;
    };
    if map.noisy {
        out.push(rec(
            "latency.noisy".into(),
            "Calibration was noisy".into(),
            "The last calibration ran while the machine was busy, so its baseline is unreliable. Re-run \
             `calibrate` when the machine is idle."
                .into(),
            json!({"measured_at": map.measured_at, "noisy": true}),
            Vec::new(),
            "latency",
            45.0,
        ));
    }
    for s in &map.storage {
        let dev = hw.storage_for(&s.dev);
        let kind = dev.map(|d| d.kind).unwrap_or_default();
        let nominal = nominal_read_4k_us(kind);
        let slow_p99 = s.read_4k_p99_us > (10.0 * nominal).max(2000.0);
        let slow_p50 = s.read_4k_p50_us > (3.0 * nominal).max(500.0);
        if !(slow_p99 || slow_p50) {
            continue;
        }
        let name = dev.map(|d| d.name.clone()).unwrap_or_else(|| s.dev.clone());
        let scheduler = dev.map(|d| d.scheduler.clone()).unwrap_or_default();
        let mut proposed = Vec::new();
        let advice = match kind {
            StorageKind::Hdd | StorageKind::Sd | StorageKind::Emmc if scheduler != "bfq" => {
                proposed.push(json!({"op": "io_scheduler", "device": s.dev, "scheduler": "bfq"}));
                "BFQ gives interactive reads priority over bulk I/O on slow media (and makes io.weight effective)."
            }
            StorageKind::Nvme | StorageKind::Ssd => {
                "Solid-state reads this slow usually mean thermal throttling, a nearly full drive without TRIM, \
                 or failing hardware: check `smartctl -a`, free space and `fstrim.timer`."
            }
            _ => "Check the device's health and what else is using it.",
        };
        out.push(rec(
            format!("latency.storage.{}", slug(&name)),
            format!("{name} is much slower than a typical {kind:?} device"),
            format!(
                "4 KiB random reads take {:.0} µs at p50 and {:.0} µs at p99; a typical {kind:?} device needs about \
                 {nominal:.0} µs. Slow storage stalls app start-up and swap-in. {advice}",
                s.read_4k_p50_us, s.read_4k_p99_us
            ),
            json!({"device": s.dev, "name": name, "kind": kind, "scheduler": scheduler,
                   "read_4k_p50_us": s.read_4k_p50_us, "read_4k_p99_us": s.read_4k_p99_us, "nominal_us": nominal}),
            proposed,
            "latency",
            if slow_p99 { 65.0 } else { 50.0 },
        ));
    }
    // Slow timer wakeups on a power-saving EPP while on AC.
    let p99s: Vec<f64> = map.cpus.iter().map(|c| c.wakeup_p99_us).filter(|v| *v > 0.0).collect();
    let med = median(p99s);
    let epp = hw.features.epp.clone().unwrap_or_default();
    if med > 150.0 && hw.power == PowerSource::Ac && (epp == "power" || epp == "balance_power") {
        let target = ["balance_performance", "performance"]
            .into_iter()
            .find(|v| hw.features.epp_available.iter().any(|a| a == v));
        if let Some(target) = target {
            out.push(rec(
                "latency.cpu-wakeup-epp".into(),
                "Slow CPU wakeups with a power-saving energy preference on AC power".into(),
                format!(
                    "Median timer-wakeup p99 is {med:.0} µs with EPP '{epp}' while on AC power; deep power saving \
                     delays waking the foreground app. '{target}' trades a little power for responsiveness."
                ),
                json!({"median_wakeup_p99_us": med, "epp": epp, "power": "ac"}),
                vec![json!({"op": "cpu_epp", "preference": target})],
                "latency",
                50.0,
            ));
        }
    }
}

fn telemetry(t: &Telemetry, profiles: &[String], out: &mut Vec<Recommendation>) {
    let total: f64 = t.red_by_hour.iter().map(|h| h.red_s).sum();
    if total >= RED_INFO_S {
        let mut worst = t.red_by_hour.clone();
        worst.sort_by(|a, b| b.red_s.total_cmp(&a.red_s));
        worst.truncate(5);
        let has_latency = profiles.is_empty() || profiles.iter().any(|p| p == "latency");
        let proposed = if has_latency {
            vec![json!({"op": "profile_use", "scope": "user", "name": "latency"})]
        } else {
            Vec::new()
        };
        let minutes = total / 60.0;
        out.push(rec(
            "telemetry.red-time".into(),
            format!("Under heavy pressure (RED) for {minutes:.0} min in the last 24 h"),
            format!(
                "lindesk reported level RED (the foreground app was stalling on CPU, memory or I/O) for {minutes:.0} \
                 minutes over the last 24 hours. The `latency` profile intervenes earlier and protects the \
                 foreground app harder. Use `query` on the `events`/`pressure` tables to see which resource stalls."
            ),
            json!({"red_seconds_24h": total, "worst_hours": worst}),
            proposed,
            "telemetry",
            if total >= RED_WARN_S { 70.0 + (minutes / 6.0).min(20.0) } else { 40.0 },
        ));
    }
    for th in t.throttled.iter().filter(|th| th.n >= THROTTLE_MIN) {
        let unitlike = th.unit.ends_with(".service") || th.unit.ends_with(".scope") || th.unit.ends_with(".slice");
        let proposed = if unitlike && !th.unit.ends_with(".slice") {
            let scope = if th.uid.is_some() { "user" } else { "system" };
            vec![json!({"op": "class_override", "scope": scope, "unit": th.unit, "class": "batch"})]
        } else {
            Vec::new()
        };
        out.push(rec(
            format!("telemetry.throttled.{}", slug(&th.unit)),
            format!("{} was throttled {} times in 24 h", th.unit, th.n),
            format!(
                "lindesk had to rein in {} {} times in the last 24 hours. If it is background work (builds, sync, \
                 indexing), classifying it as `batch` keeps it out of the way from the start instead of throttling \
                 it after the desktop already stalled. Don't do this for something the user interacts with.",
                th.unit, th.n
            ),
            json!({"unit": th.unit, "uid": th.uid, "throttle_actions_24h": th.n}),
            proposed,
            "telemetry",
            35.0 + (th.n as f64 / 30.0).min(15.0),
        ));
    }
}

/// Rank everything worth doing, best first.
pub fn recommend(inputs: &Inputs) -> Vec<Recommendation> {
    let mut out = Vec::new();
    for f in &inputs.doctor {
        if f.severity != "warn" && f.severity != "error" {
            continue;
        }
        let why = match &f.remedy {
            Some(r) => format!("{}. {r}", f.status),
            None => f.status.clone(),
        };
        out.push(rec(
            format!("doctor.{}", slug(&f.check)),
            format!("{}: {}", f.check, f.status),
            why,
            json!({"check": f.check, "status": f.status, "severity": f.severity}),
            Vec::new(),
            "doctor",
            severity_score(&f.severity) - 5.0,
        ));
    }
    for f in &inputs.swap {
        out.push(rec(
            format!("swap.{}", slug(&f.title)),
            f.title.clone(),
            f.detail.clone(),
            json!({"severity": f.severity}),
            f.proposed_ops.clone(),
            "swap",
            severity_score(&f.severity),
        ));
    }
    for r in &inputs.requirements {
        if r.status != "warn" && r.status != "fail" {
            continue;
        }
        let why = match &r.remedy {
            Some(rem) => format!("{} → {rem}", r.detail),
            None => r.detail.clone(),
        };
        // Doctor usually reports the same thing (e.g. a conflicting daemon): skip a
        // requirement that names a check doctor already warned about.
        let text = format!("{} {} {}", r.id, r.title, r.detail).to_lowercase();
        let duplicate = inputs.doctor.iter().any(|f| {
            (f.severity == "warn" || f.severity == "error")
                && (slug(&f.check) == slug(&r.id) || (f.check.len() > 3 && text.contains(&f.check.to_lowercase())))
        });
        if duplicate {
            continue;
        }
        out.push(rec(
            format!("requirements.{}", slug(&r.id)),
            format!("{}: {}", r.title, r.detail),
            why,
            json!({"id": r.id, "status": r.status}),
            Vec::new(),
            "requirements",
            severity_score(&r.status) - 10.0,
        ));
    }
    if let Some(hw) = &inputs.hardware {
        latency(hw, &mut out);
    }
    if let Some(t) = &inputs.telemetry {
        telemetry(t, &inputs.profiles, &mut out);
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    // Ids are unique: later duplicates (same slug) are dropped.
    let mut seen = std::collections::BTreeSet::new();
    out.retain(|r| seen.insert(r.id.clone()));
    out
}

/// SQL: seconds at level RED per hour over the last 24 h (events.kind = 'level').
pub const RED_BY_HOUR_SQL: &str = "\
WITH lv AS (
  SELECT ts, json_extract_string(payload, '$.to') AS lvl, lead(ts) OVER (ORDER BY ts) AS next_ts
  FROM events WHERE kind = 'level' AND ts > now() - INTERVAL 24 HOUR
)
SELECT strftime(date_trunc('hour', ts), '%Y-%m-%dT%H:00Z') AS hour,
       round(sum(epoch(coalesce(next_ts, now()) - ts)), 1) AS red_s
FROM lv WHERE lvl = 'red' GROUP BY 1 ORDER BY 1";

/// SQL: units lindesk throttled most over the last 24 h.
pub const TOP_THROTTLED_SQL: &str = "\
SELECT unit, any_value(uid) AS uid, count(*) AS n
FROM events
WHERE kind = 'action_applied' AND unit IS NOT NULL AND ts > now() - INTERVAL 24 HOUR
GROUP BY unit ORDER BY n DESC LIMIT 5";

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_plugin_api::{CpuLatency, LatencyMap, StorageDevice, StorageLatency};

    fn hdd_machine() -> Hardware {
        Hardware {
            storage: vec![StorageDevice {
                dev: "8:0".into(),
                name: "sda".into(),
                kind: StorageKind::Hdd,
                scheduler: "mq-deadline".into(),
                nominal_bps: 150_000_000,
            }],
            power: PowerSource::Ac,
            latency: Some(LatencyMap {
                storage: vec![StorageLatency {
                    dev: "8:0".into(),
                    read_4k_p50_us: 9000.0,
                    read_4k_p99_us: 120_000.0,
                    ..Default::default()
                }],
                cpus: vec![CpuLatency { cpu: 0, wakeup_p99_us: 60.0, ..Default::default() }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn fixture() -> Inputs {
        Inputs {
            doctor: vec![
                DoctorFinding { severity: "ok".into(), check: "cgroup v2".into(), status: "yes".into(), remedy: None },
                DoctorFinding {
                    severity: "warn".into(),
                    check: "io.latency".into(),
                    status: "not built".into(),
                    remedy: Some("rebuild with CONFIG_BLK_CGROUP_IOLATENCY".into()),
                },
            ],
            swap: vec![SwapFinding {
                severity: "warn".into(),
                title: "Swap on a hard disk".into(),
                detail: "/dev/sda2 is rotational".into(),
                proposed_ops: vec![json!({"op": "sysctl", "key": "vm.swappiness", "value": "10"})],
            }],
            requirements: vec![
                Requirement {
                    id: "conflicts".into(),
                    title: "Conflicting daemons".into(),
                    status: "warn".into(),
                    detail: "io.latency not built either".into(),
                    remedy: None,
                },
                Requirement {
                    id: "psi".into(),
                    title: "PSI".into(),
                    status: "warn".into(),
                    detail: "psi=0 on the kernel command line".into(),
                    remedy: Some("remove psi=0".into()),
                },
            ],
            hardware: Some(hdd_machine()),
            telemetry: Some(Telemetry {
                red_by_hour: vec![
                    RedHour { hour: "2026-10-03T10:00Z".into(), red_s: 900.0 },
                    RedHour { hour: "2026-10-03T11:00Z".into(), red_s: 300.0 },
                ],
                throttled: vec![
                    Throttled { unit: "app-org.chromium.Chromium-1234.scope".into(), uid: Some(1000), n: 120 },
                    Throttled { unit: "cgroup:/foo".into(), uid: None, n: 3 },
                ],
            }),
            profiles: vec!["balanced".into(), "latency".into()],
        }
    }

    #[test]
    fn ranks_fixture_inputs() {
        let recs = recommend(&fixture());
        let ids: Vec<&str> = recs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "telemetry.red-time",
                "swap.swap-on-a-hard-disk",
                "latency.storage.sda",
                "doctor.io-latency",
                "requirements.psi",
                "telemetry.throttled.app-org-chromium-chromium-1234-scope",
            ],
            "{recs:#?}"
        );
        let swap = &recs[1];
        assert_eq!((swap.risk, swap.scope), (Risk::Medium, Scope::System));
        let red = &recs[0];
        assert_eq!(red.proposed_ops, vec![json!({"op": "profile_use", "scope": "user", "name": "latency"})]);
        assert_eq!((red.risk, red.scope), (Risk::Low, Scope::User));
        assert_eq!(red.evidence["red_seconds_24h"], json!(1200.0));
        let disk = &recs[2];
        assert_eq!(disk.proposed_ops[0]["op"], "io_scheduler");
        assert_eq!(disk.risk, Risk::High);
        let doctor = &recs[3];
        assert_eq!((doctor.risk, doctor.proposed_ops.len()), (Risk::None, 0));
        assert!(doctor.why.contains("CONFIG_BLK_CGROUP_IOLATENCY"));
        let th = recs.last().unwrap();
        assert_eq!(th.proposed_ops[0]["class"], "batch");
        assert_eq!(th.scope, Scope::User);
    }

    #[test]
    fn quiet_machine_has_little_to_say() {
        let mut hw = hdd_machine();
        hw.latency = None;
        let recs =
            recommend(&Inputs { hardware: Some(hw), telemetry: Some(Telemetry::default()), ..Default::default() });
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].id, "latency.no-baseline");
    }

    #[test]
    fn noisy_calibration_and_epp() {
        let mut hw = hdd_machine();
        hw.features.epp = Some("power".into());
        hw.features.epp_available = vec!["power".into(), "balance_performance".into()];
        if let Some(l) = hw.latency.as_mut() {
            l.noisy = true;
            l.cpus = vec![CpuLatency { cpu: 0, wakeup_p99_us: 400.0, ..Default::default() }];
            l.storage.clear();
        }
        let recs = recommend(&Inputs { hardware: Some(hw), ..Default::default() });
        let ids: Vec<&str> = recs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["latency.cpu-wakeup-epp", "latency.noisy"]);
        assert_eq!(recs[0].proposed_ops, vec![json!({"op": "cpu_epp", "preference": "balance_performance"})]);
    }
}
