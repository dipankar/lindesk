//! The JSON documents the bench produces: the probe summary, the load report and the
//! results file of an A/B run. Bump [`RESULTS_VERSION`] on incompatible changes; readers
//! accept missing optional fields (`#[serde(default)]`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::hist::HistSummary;
use crate::safety::{DiskPlan, MemPlan};
use crate::sys::PsiWindow;

pub const RESULTS_VERSION: u32 = 1;

/// Output of `lindesk-bench probe` (final summary, or one interval with `--stream`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ProbeReport {
    /// `probe-summary` or `probe-interval`.
    pub kind: String,
    /// Length of the measured window, seconds.
    pub duration_s: f64,
    /// Overshoot of 1 ms `clock_nanosleep` sleeps.
    pub wakeup: HistSummary,
    /// 4 KiB random read latency.
    pub io: HistSummary,
    /// `direct` (O_DIRECT) or `buffered` (posix_fadvise DONTNEED before each read).
    pub io_mode: String,
    /// First-touch latency of a fresh anonymous 4 KiB page.
    pub fault: HistSummary,
    /// The probe's own cgroup (v2 path).
    #[serde(default)]
    pub cgroup: Option<String>,
    /// Stall of the probe's own cgroup over the window.
    pub psi_cgroup: PsiWindow,
    /// System-wide stall over the window.
    pub psi_system: PsiWindow,
    /// True when the probe was stopped early (Ctrl-C).
    #[serde(default)]
    pub interrupted: bool,
    #[serde(default)]
    pub errors: Vec<String>,
}

/// Monotonic counters of the background load.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct Progress {
    pub bytes_written: u64,
    pub bytes_written_direct: u64,
    pub bytes_read: u64,
    pub fsyncs: u64,
    /// Scratch files deleted after hitting the write cap.
    pub files_cycled: u64,
    /// Random read-modify-write operations of the CPU workers.
    pub cpu_ops: u64,
    /// Pages touched by the memory worker.
    pub mem_pages_touched: u64,
}

impl Progress {
    pub fn minus(&self, base: &Progress) -> Progress {
        Progress {
            bytes_written: self.bytes_written.saturating_sub(base.bytes_written),
            bytes_written_direct: self.bytes_written_direct.saturating_sub(base.bytes_written_direct),
            bytes_read: self.bytes_read.saturating_sub(base.bytes_read),
            fsyncs: self.fsyncs.saturating_sub(base.fsyncs),
            files_cycled: self.files_cycled.saturating_sub(base.files_cycled),
            cpu_ops: self.cpu_ops.saturating_sub(base.cpu_ops),
            mem_pages_touched: self.mem_pages_touched.saturating_sub(base.mem_pages_touched),
        }
    }
}

/// The load mix, as run.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LoadConfig {
    pub cpu: usize,
    pub write: bool,
    pub direct: bool,
    pub read: bool,
    /// `--mem` as given (`40%`, `8G`).
    #[serde(default)]
    pub mem: Option<String>,
    #[serde(default)]
    pub build_cmd: Option<String>,
    pub write_cap: u64,
    pub fsync_every: u64,
    pub max_mem_frac: f64,
}

/// Result of the optional real build.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct BuildReport {
    pub cmd: String,
    /// Finished on its own (rather than being stopped with the load).
    pub finished: bool,
    #[serde(default)]
    pub exit_code: Option<i32>,
    pub elapsed_s: f64,
}

/// Output of `lindesk-bench load`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LoadReport {
    pub kind: String,
    pub config: LoadConfig,
    pub elapsed_s: f64,
    pub total: Progress,
    /// Counters since the mark (SIGUSR1); equals `total` without a mark.
    pub window: Progress,
    pub window_s: f64,
    #[serde(default)]
    pub mem: Option<MemPlan>,
    /// Bytes the memory worker held at its peak.
    #[serde(default)]
    pub mem_peak: u64,
    /// The memory worker gave memory back because MemAvailable fell below the floor.
    #[serde(default)]
    pub mem_released_for_safety: bool,
    #[serde(default)]
    pub disk: Option<DiskPlan>,
    #[serde(default)]
    pub build: Option<BuildReport>,
    #[serde(default)]
    pub interrupted: bool,
    /// Every scratch file was deleted.
    pub cleaned_up: bool,
    #[serde(default)]
    pub errors: Vec<String>,
}

/// What the harness found out about lindesk before a phase.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LindeskState {
    /// `org.lindesk.QoS1` has an owner on the session bus.
    pub session_bus: bool,
    /// `org.lindesk.Broker1` has an owner on the system bus.
    pub broker_bus: bool,
    pub session_unit_active: bool,
    pub system_unit_active: bool,
    /// lindesk processes found in /proc (comm).
    #[serde(default)]
    pub processes: Vec<String>,
}

impl LindeskState {
    pub fn running(&self) -> bool {
        self.session_bus
            || self.broker_bus
            || self.session_unit_active
            || self.system_unit_active
            || !self.processes.is_empty()
    }
}

/// One once-a-second sample taken by the harness during the measurement window.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct TimelinePoint {
    /// Seconds since the start of the window.
    pub t_s: f64,
    /// System PSI over the last second.
    pub psi: PsiWindow,
    pub mem_available: u64,
    /// lindesk's level from `Status()` (on phase only).
    #[serde(default)]
    pub level: Option<String>,
}

/// `off` or `on`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Off,
    On,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Off => "off",
            Phase::On => "on",
        }
    }

    pub fn parse(s: &str) -> Option<Phase> {
        match s.trim() {
            "off" => Some(Phase::Off),
            "on" => Some(Phase::On),
            _ => None,
        }
    }
}

/// One phase of an A/B run.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PhaseResult {
    pub phase: Phase,
    /// RFC 3339 start of the measurement window.
    pub started: String,
    /// Epoch ms of the start of the measurement window.
    pub started_ms: i64,
    pub lindesk: LindeskState,
    /// `SetForegroundProcess(probe pid)` succeeded.
    #[serde(default)]
    pub foreground_marked: bool,
    pub probe: ProbeReport,
    pub load: LoadReport,
    #[serde(default)]
    pub timeline: Vec<TimelinePoint>,
    /// Seconds spent at each lindesk level (from the timeline).
    #[serde(default)]
    pub levels: BTreeMap<String, u32>,
}

/// Run configuration.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct RunConfig {
    pub profile: String,
    pub phases: Vec<Phase>,
    pub duration_s: f64,
    pub ramp_s: f64,
    pub cooldown_s: f64,
    pub slice: String,
    pub scopes: bool,
    pub manage_services: bool,
    pub reassert_foreground: bool,
    pub interact: bool,
    pub load: LoadConfig,
    pub scratch_dir: String,
}

/// Host facts recorded with every run.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct HostInfo {
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub kernel: Option<String>,
    #[serde(default)]
    pub cpu_model: Option<String>,
    pub nproc: usize,
    pub mem_total: u64,
    pub mem_available: u64,
    /// `lindesk_sys::probe` support level (FULL, IO-PARTIAL, CPU-ONLY, MINIMAL).
    pub support_level: String,
    #[serde(default)]
    pub support_notes: Vec<String>,
    pub backend: String,
    /// e.g. `gnome/wayland`.
    #[serde(default)]
    pub desktop: Option<String>,
    /// Filesystem and scheduler of the scratch directory's disk, when known.
    #[serde(default)]
    pub scratch_fs: Option<String>,
}

/// `bench-results-<ts>.json`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Results {
    pub version: u32,
    pub tool: String,
    pub run_id: String,
    pub label: String,
    pub started: String,
    pub config: RunConfig,
    pub host: HostInfo,
    pub phases: Vec<PhaseResult>,
}

impl Results {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Results> {
        use anyhow::Context;
        let s = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let r: Results = serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display()))?;
        if r.version > RESULTS_VERSION {
            anyhow::bail!(
                "{} has results version {}, this lindesk-bench reads up to {}",
                path.display(),
                r.version,
                RESULTS_VERSION
            );
        }
        Ok(r)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::sys::ResourceStall;

    pub fn hs(p50: f64, p99: f64, max: f64) -> HistSummary {
        HistSummary {
            count: 1000,
            min_us: 1.0,
            mean_us: p50,
            p50_us: p50,
            p90_us: p99,
            p95_us: p99,
            p99_us: p99,
            p999_us: max,
            max_us: max,
        }
    }

    fn stall(pct: f64) -> Option<ResourceStall> {
        Some(ResourceStall {
            some_pct: pct,
            full_pct: Some(pct / 2.0),
            some_us: 1,
            full_us: Some(1),
            some_avg10: pct as f32,
        })
    }

    pub fn phase(p: Phase, scale: f64) -> PhaseResult {
        let psi = PsiWindow { cpu: stall(3.0 * scale), io: stall(20.0 * scale), memory: stall(1.5 * scale) };
        PhaseResult {
            phase: p,
            started: "2026-10-04T12:00:00Z".into(),
            started_ms: 1_790_000_000_000,
            lindesk: LindeskState { session_bus: p == Phase::On, ..Default::default() },
            foreground_marked: p == Phase::On,
            probe: ProbeReport {
                kind: "probe-summary".into(),
                duration_s: 10.0,
                wakeup: hs(60.0 * scale, 2_400.0 * scale, 18_000.0 * scale),
                io: hs(150.0 * scale, 45_000.0 * scale, 210_000.0 * scale),
                io_mode: "direct".into(),
                fault: hs(0.3 * scale, 12.0 * scale, 900.0 * scale),
                cgroup: Some("/user.slice/x.scope".into()),
                psi_cgroup: psi.clone(),
                psi_system: psi,
                interrupted: false,
                errors: vec![],
            },
            load: LoadReport {
                kind: "load-report".into(),
                config: LoadConfig { cpu: 24, write: true, write_cap: 4 << 30, ..Default::default() },
                window: Progress {
                    bytes_written: (4_000_000_000.0 / scale.max(0.5)) as u64,
                    cpu_ops: (9e9 / scale.max(0.5)) as u64,
                    ..Default::default()
                },
                window_s: 10.0,
                cleaned_up: true,
                ..Default::default()
            },
            timeline: vec![TimelinePoint {
                t_s: 1.0,
                psi: PsiWindow::default(),
                mem_available: 1 << 30,
                level: Some("red".into()),
            }],
            levels: BTreeMap::from([("red".into(), 4), ("green".into(), 6)]),
        }
    }

    pub fn sample_results() -> Results {
        Results {
            version: RESULTS_VERSION,
            tool: "lindesk-bench test".into(),
            run_id: "01J0000000000000000000000".into(),
            label: "test".into(),
            started: "2026-10-04T12:00:00Z".into(),
            config: RunConfig { profile: "quick".into(), phases: vec![Phase::Off, Phase::On], ..Default::default() },
            host: HostInfo {
                nproc: 24,
                support_level: "IO-PARTIAL".into(),
                backend: "systemd".into(),
                ..Default::default()
            },
            phases: vec![phase(Phase::Off, 1.0), phase(Phase::On, 0.25)],
        }
    }

    #[test]
    fn results_json_round_trip() {
        let r = sample_results();
        let s = serde_json::to_string_pretty(&r).unwrap();
        let back: Results = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);

        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("r.json");
        std::fs::write(&p, &s).unwrap();
        assert_eq!(Results::load(&p).unwrap(), r);

        // Field names other tools rely on.
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["phases"][0]["phase"], "off");
        assert!(v["phases"][1]["probe"]["wakeup"]["p99_us"].is_number());
        assert!(v["phases"][1]["load"]["window"]["bytes_written"].is_number());
    }

    #[test]
    fn newer_versions_are_rejected_and_optional_fields_default() {
        let mut v = serde_json::to_value(sample_results()).unwrap();
        // Older/smaller documents without optional fields still parse.
        v["phases"][0].as_object_mut().unwrap().remove("timeline");
        v["phases"][0]["probe"].as_object_mut().unwrap().remove("errors");
        let r: Results = serde_json::from_value(v.clone()).unwrap();
        assert!(r.phases[0].timeline.is_empty());

        v["version"] = (RESULTS_VERSION + 1).into();
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("r.json");
        std::fs::write(&p, v.to_string()).unwrap();
        assert!(Results::load(&p).is_err());
    }

    #[test]
    fn probe_and_load_reports_round_trip() {
        let r = sample_results();
        let pr = &r.phases[0].probe;
        assert_eq!(&serde_json::from_str::<ProbeReport>(&serde_json::to_string(pr).unwrap()).unwrap(), pr);
        let lr = &r.phases[0].load;
        assert_eq!(&serde_json::from_str::<LoadReport>(&serde_json::to_string(lr).unwrap()).unwrap(), lr);
    }
}
