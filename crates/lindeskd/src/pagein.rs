//! Page a swapped-out app back in when it gets focus (`[plugins.pagein]`).
//!
//! Warm apps keep recently used apps in memory, but an app left alone for long enough, or
//! on a machine short of memory, still ends up in swap. Without help it then comes back one
//! page fault at a time, each one a stalled thread: the window is there and nothing in it
//! answers. When focus moves to another app the broker asks the kernel to read that app's
//! swapped-out memory back in one go ([`lindesk_sys::pagein`]), in the background, while
//! the switch boost gives the app the CPU and I/O share to repaint.
//!
//! It is held back by what the machine can afford: at most `budget` per switch, never more
//! than what is available beyond a reserve (pulling one app in must not push another out),
//! and not again for the same app within `cooldown`. The work happens on its own thread; the
//! engine only hands over which app.
//!
//! ```toml
//! [plugins.pagein]
//! enabled = true
//! config = { budget = "1GiB", cooldown = "30s", min = "4MiB", reserve = 0.05 }
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use lindesk_core::config::{dur, size};
use lindesk_host::{Engine, HostSender};
use lindesk_plugin_api::Target;
use lindesk_sys::{Roots, pagein, procfs, read_file};
use serde::Deserialize;
use serde_json::json;

/// Config section / plugin name.
pub const NAME: &str = "pagein";
/// Name of the notice in `lindeskctl events`.
pub const NOTICE: &str = "pagein.requested";
/// Processes looked at per switch (a browser has a few dozen).
const MAX_PROCESSES: usize = 256;

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct PageInConfig {
    /// Most swap brought back per switch.
    #[serde(with = "size")]
    pub budget: u64,
    /// Not again for the same app within this long.
    #[serde(with = "dur")]
    pub cooldown: Duration,
    /// Less than this in swap is not worth a request (or a line in the events).
    #[serde(with = "size")]
    pub min: u64,
    /// Fraction of RAM that must stay available after paging in.
    pub reserve: f64,
}

impl Default for PageInConfig {
    fn default() -> Self {
        PageInConfig { budget: 1 << 30, cooldown: Duration::from_secs(30), min: 4 << 20, reserve: 0.05 }
    }
}

impl PageInConfig {
    /// Parse the plugin's JSON config; `null` yields the defaults.
    pub fn from_json(v: &serde_json::Value) -> Result<Self, String> {
        if v.is_null() {
            return Ok(Self::default());
        }
        let cfg: PageInConfig = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
        if !(0.0..1.0).contains(&cfg.reserve) {
            return Err(format!("reserve = {} must be in 0..1", cfg.reserve));
        }
        Ok(cfg)
    }
}

/// One app to bring back.
#[derive(Debug)]
struct Job {
    target: Target,
    /// The process that owns the window: its memory first.
    pid: Option<i32>,
    cgroups: Vec<String>,
    generation: u64,
}

/// What a job did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub bytes: u64,
    pub processes: usize,
}

/// Watches focus and hands apps to the worker thread.
pub struct PageIn {
    cfg: PageInConfig,
    tx: SyncSender<Job>,
    /// Set by the worker when the kernel or our privileges don't allow it: stop asking.
    unavailable: Arc<AtomicBool>,
    last: Option<Target>,
    generation: Arc<AtomicU64>,
    done: HashMap<Target, Instant>,
}

impl PageIn {
    /// Starts the worker. Notices about what it did go to `events`.
    pub fn spawn(cfg: PageInConfig, roots: Roots, host: String, events: HostSender) -> std::io::Result<PageIn> {
        // One job waiting is enough: if the person has moved on twice, the app in between
        // no longer matters.
        let (tx, rx) = mpsc::sync_channel::<Job>(1);
        let unavailable = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));
        let worker_generation = Arc::clone(&generation);
        let (flag, worker_cfg) = (Arc::clone(&unavailable), cfg.clone());
        std::thread::Builder::new().name("lindesk-pagein".into()).spawn(move || {
            for job in rx {
                match run_checked(&worker_cfg, &roots, &job, || {
                    worker_generation.load(Ordering::Relaxed) == job.generation
                }) {
                    Ok(o) if o.bytes >= worker_cfg.min => {
                        tracing::debug!(target = %job.target, bytes = o.bytes, processes = o.processes, "paging in");
                        events.event(crate::common::startup::notice(
                            NOTICE,
                            &host,
                            format!(
                                "bringing {} of {} back from swap ({} process{})",
                                mib(o.bytes),
                                job.target,
                                o.processes,
                                if o.processes == 1 { "" } else { "es" }
                            ),
                            json!({"target": job.target, "bytes": o.bytes, "processes": o.processes}),
                        ));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::info!("page-in on focus is not available here ({e}); switched off");
                        flag.store(true, Ordering::Relaxed);
                    }
                }
            }
        })?;
        Ok(PageIn { cfg, tx, unavailable, last: None, generation, done: HashMap::new() })
    }

    /// Call after every engine step. Queues the focused app when focus moved to it.
    pub fn observe(&mut self, engine: &Engine) {
        let fg = engine.foreground();
        if engine.dry_run() {
            self.generation.fetch_add(1, Ordering::Relaxed);
            self.last = None;
            return;
        }
        if fg == self.last.as_ref() {
            return;
        }
        self.last = fg.cloned();
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let Some(target) = fg else { return };
        // Paused and dry-run mean "change nothing"; so does a kernel that can't do it.
        if engine.dry_run() || self.unavailable.load(Ordering::Relaxed) {
            return;
        }
        let now = Instant::now();
        self.done.retain(|_, at| now.duration_since(*at) < self.cfg.cooldown);
        if self.done.contains_key(target) {
            return;
        }
        let cgroups: Vec<String> =
            std::iter::once(target).chain(engine.foreground_family()).filter_map(|t| engine.target_cgroup(t)).collect();
        if cgroups.is_empty() {
            return;
        }
        match self.tx.try_send(Job { target: target.clone(), pid: engine.foreground_pid(), cgroups, generation }) {
            Ok(()) => {
                self.done.insert(target.clone(), now);
            }
            // Busy with the previous app: this one is skipped, not queued behind it.
            Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => self.unavailable.store(true, Ordering::Relaxed),
        }
    }
}

fn mib(bytes: u64) -> String {
    format!("{} MiB", bytes.div_ceil(1 << 20))
}

/// `(MemTotal, MemAvailable)` in bytes.
fn memory(roots: &Roots) -> Option<(u64, u64)> {
    let info = read_file(&roots.proc.join("meminfo")).ok()?;
    let field = |name: &str| {
        info.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            .map(|kb| kb * 1024)
    };
    Some((field("MemTotal:")?, field("MemAvailable:")?))
}

/// How much may be paged in right now: the budget, less whatever would eat into the reserve.
fn affordable(cfg: &PageInConfig, total: u64, available: u64) -> u64 {
    let reserve = (total as f64 * cfg.reserve) as u64;
    cfg.budget.min(available.saturating_sub(reserve))
}

/// Missing PSI is unknown; retain the existing reserve guard on older kernels.
fn reclaim_busy(roots: &Roots) -> bool {
    lindesk_sys::psi::PsiReading::read(&roots.proc.join("pressure/memory"))
        .is_ok_and(|p| p.some.avg10 >= 5.0 || p.full.is_some_and(|f| f.avg10 >= 1.0))
}

/// Brings one app back. `Err` only when paging in can't work at all on this host.
fn run_checked(cfg: &PageInConfig, roots: &Roots, job: &Job, current: impl Fn() -> bool) -> std::io::Result<Outcome> {
    if !current() || reclaim_busy(roots) {
        return Ok(Outcome::default());
    }
    let Some((total, available)) = memory(roots) else { return Ok(Outcome::default()) };
    let mut left = affordable(cfg, total, available);
    if left < cfg.min {
        tracing::debug!(target = %job.target, available, "not paging in: memory is too tight");
        return Ok(Outcome::default());
    }
    // Every process of the app with something in swap; the window's own process first,
    // then whoever has most to bring back.
    let mut procs: Vec<(i32, u64)> = job
        .cgroups
        .iter()
        .flat_map(|cg| procfs::pids_in_cgroup_tree(&roots.cgroup, cg).unwrap_or_default())
        .take(MAX_PROCESSES)
        .filter_map(|pid| {
            let status = read_file(&roots.proc.join(pid.to_string()).join("status")).ok()?;
            Some((pid, pagein::vm_swap(&status))).filter(|(_, swap)| *swap > 0)
        })
        .collect();
    procs.sort_by_key(|(pid, swap)| (Some(*pid) != job.pid, std::cmp::Reverse(*swap)));
    procs.dedup_by_key(|(pid, _)| *pid);

    let mut out = Outcome::default();
    for (pid, _) in procs {
        if !current() || reclaim_busy(roots) {
            break;
        }
        // Recheck available memory before each process: conditions may change mid-job.
        let headroom = memory(roots).map(|(total, available)| affordable(cfg, total, available)).unwrap_or(0);
        left = left.min(headroom);
        if left < 4096 {
            break;
        }
        match pagein::page_in(&roots.proc, pid, left) {
            Ok(p) if p.bytes > 0 => {
                left = left.saturating_sub(p.bytes);
                out.bytes += p.bytes;
                out.processes += 1;
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied) => {
                return Err(e);
            }
            // The process exited, or its maps changed under us: the next one.
            Err(_) => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_and_stale_focus_skip_speculative_work() {
        let dir = crate::common::test_dir();
        let roots = Roots::under(dir.path());
        std::fs::create_dir_all(roots.proc.join("pressure")).unwrap();
        std::fs::write(roots.proc.join("pressure/memory"), "some avg10=6.0 avg60=0 avg300=0 total=10\n").unwrap();
        assert!(reclaim_busy(&roots));
        let job = Job { target: Target::system_unit("app.scope"), pid: None, cgroups: vec![], generation: 1 };
        assert_eq!(run_checked(&PageInConfig::default(), &roots, &job, || false).unwrap(), Outcome::default());
        assert_eq!(run_checked(&PageInConfig::default(), &roots, &job, || true).unwrap(), Outcome::default());
    }

    #[test]
    fn config() {
        assert_eq!(PageInConfig::from_json(&serde_json::Value::Null).unwrap(), PageInConfig::default());
        let c = PageInConfig::from_json(&json!({"budget": "256MiB", "cooldown": "5s", "reserve": 0.1})).unwrap();
        assert_eq!((c.budget, c.cooldown, c.reserve), (256 << 20, Duration::from_secs(5), 0.1));
        assert_eq!(c.min, 4 << 20);
        assert!(PageInConfig::from_json(&json!({"reserve": 1.5})).is_err());
        assert!(PageInConfig::from_json(&json!({"bugdet": "1GiB"})).is_err());
    }

    #[test]
    fn never_into_the_reserve() {
        let cfg = PageInConfig::default();
        let gib = 1u64 << 30;
        // Plenty free: the budget.
        assert_eq!(affordable(&cfg, 64 * gib, 20 * gib), gib);
        // 16 GiB machine, 5 % reserve = 0.8 GiB; 1 GiB available leaves 0.2 GiB to use.
        let a = affordable(&cfg, 16 * gib, gib);
        assert!(a > 200 << 20 && a < 210 << 20, "{a}");
        // Nothing to spare.
        assert_eq!(affordable(&cfg, 16 * gib, 500 << 20), 0);
    }

    #[test]
    fn an_app_with_nothing_in_swap_costs_nothing() {
        let dir = crate::common::test_dir();
        let roots = Roots::under(dir.path());
        let cg = "/user.slice/user-1000.slice/user@1000.service/app.slice/app-x-1.scope";
        let put = |rel: &str, body: &str| {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        put("proc/meminfo", "MemTotal:       65536000 kB\nMemAvailable:   30000000 kB\n");
        put(&format!("sys/fs/cgroup{cg}/cgroup.procs"), "100\n101\n");
        put("proc/100/status", "Name:\tx\nVmSwap:\t       0 kB\n");
        put("proc/101/status", "Name:\tx\n");
        let job = Job {
            target: Target::user_unit(1000, "app-x-1.scope"),
            pid: Some(100),
            cgroups: vec![cg.into()],
            generation: 0,
        };
        assert_eq!(run_checked(&PageInConfig::default(), &roots, &job, || true).unwrap(), Outcome::default());
        // No memory to spare: not even looked at.
        put("proc/meminfo", "MemTotal:       65536000 kB\nMemAvailable:   1000000 kB\n");
        put("proc/100/status", "Name:\tx\nVmSwap:\t  500000 kB\n");
        assert_eq!(run_checked(&PageInConfig::default(), &roots, &job, || true).unwrap(), Outcome::default());
    }
}
