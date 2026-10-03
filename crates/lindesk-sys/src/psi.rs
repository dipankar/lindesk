//! Pressure Stall Information: parsing, per-interval deltas and poll(2) triggers.
//!
//! PSI files (`/proc/pressure/{cpu,io,memory}` and a cgroup's `*.pressure`) look like:
//!
//! ```text
//! some avg10=0.12 avg60=0.08 avg300=0.02 total=123456
//! full avg10=0.00 avg60=0.00 avg300=0.00 total=6789
//! ```
//!
//! `total` is cumulative stall time in microseconds. Controllers should look at the stall
//! accrued over their own sampling interval rather than the kernel's fixed averages;
//! [`PsiTracker`] computes that.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use lindesk_plugin_api::{Resource, Sample, Scope, TimestampMs};
use serde::{Deserialize, Serialize};

use crate::Roots;
use crate::util::{invalid_data, invalid_input, under};

/// One line (`some` or `full`) of a PSI file.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct PsiLine {
    /// Percentage of time stalled, averaged over 10 s.
    pub avg10: f32,
    /// ... over 60 s.
    pub avg60: f32,
    /// ... over 300 s.
    pub avg300: f32,
    /// Cumulative stall time, microseconds.
    pub total_us: u64,
}

/// A parsed PSI file.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct PsiReading {
    /// At least one task stalled.
    pub some: PsiLine,
    /// All non-idle tasks stalled. Absent on old kernels for `cpu` (always zero at the
    /// system level on newer ones).
    pub full: Option<PsiLine>,
}

impl PsiReading {
    /// Parse the contents of a PSI file.
    pub fn parse(s: &str) -> io::Result<Self> {
        let mut some = None;
        let mut full = None;
        for line in s.lines() {
            let mut it = line.split_whitespace();
            match it.next() {
                Some("some") => some = Some(parse_line(it)?),
                Some("full") => full = Some(parse_line(it)?),
                _ => {}
            }
        }
        Ok(PsiReading { some: some.ok_or_else(|| invalid_data("PSI data without a 'some' line"))?, full })
    }

    /// Read and parse a PSI file.
    pub fn read(path: &Path) -> io::Result<Self> {
        Self::parse(&std::fs::read_to_string(path)?)
    }
}

fn parse_line<'a>(fields: impl Iterator<Item = &'a str>) -> io::Result<PsiLine> {
    let mut l = PsiLine::default();
    let mut seen_total = false;
    for kv in fields {
        let (k, v) = kv.split_once('=').ok_or_else(|| invalid_data(format!("bad PSI field '{kv}'")))?;
        let f = || v.parse::<f32>().map_err(|_| invalid_data(format!("bad PSI value '{kv}'")));
        match k {
            "avg10" => l.avg10 = f()?,
            "avg60" => l.avg60 = f()?,
            "avg300" => l.avg300 = f()?,
            "total" => {
                l.total_us = v.parse().map_err(|_| invalid_data(format!("bad PSI total '{kv}'")))?;
                seen_total = true;
            }
            _ => {}
        }
    }
    if !seen_total {
        return Err(invalid_data("PSI line without total"));
    }
    Ok(l)
}

/// `/proc/pressure/<resource>` under `proc_root`.
pub fn system_path(proc_root: &Path, resource: Resource) -> PathBuf {
    proc_root.join("pressure").join(resource.as_str())
}

/// `<cgroup>/<resource>.pressure` for a cgroup path relative to the cgroup mount.
pub fn cgroup_path(cgroup_root: &Path, cgroup: &str, resource: Resource) -> io::Result<PathBuf> {
    Ok(under(cgroup_root, cgroup)?.join(format!("{}.pressure", resource.as_str())))
}

/// The PSI file for a [`Scope`].
pub fn scope_path(roots: &Roots, scope: &Scope, resource: Resource) -> io::Result<PathBuf> {
    match scope {
        Scope::System => Ok(system_path(&roots.proc, resource)),
        Scope::Cgroup(cg) => cgroup_path(&roots.cgroup, cg, resource),
    }
}

#[derive(Clone, Copy, Debug)]
struct Prev {
    ts: TimestampMs,
    some_total: u64,
    full_total: Option<u64>,
}

/// Turns successive [`PsiReading`]s into [`Sample`]s carrying per-interval stall deltas.
///
/// State is kept per (scope, resource). The first reading of a pair only sets the baseline
/// and yields `None`. A counter that goes backwards (the cgroup was removed and recreated)
/// is treated as a reset: the new total, capped at the interval, is the delta. A timestamp
/// that goes backwards re-baselines.
#[derive(Debug, Default)]
pub struct PsiTracker {
    prev: HashMap<(Scope, Resource), Prev>,
}

impl PsiTracker {
    /// An empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one reading taken at `ts`.
    pub fn update(&mut self, ts: TimestampMs, scope: Scope, resource: Resource, r: &PsiReading) -> Option<Sample> {
        let cur = Prev { ts, some_total: r.some.total_us, full_total: r.full.map(|f| f.total_us) };
        let key = (scope, resource);
        let prev = self.prev.insert(key.clone(), cur)?;
        if ts < prev.ts {
            return None;
        }
        let interval_ms = u32::try_from(ts - prev.ts).unwrap_or(u32::MAX);
        let interval_us = u64::from(interval_ms) * 1000;
        let full_delta_us = match (prev.full_total, cur.full_total) {
            (Some(p), Some(c)) => Some(counter_delta(p, c, interval_us)),
            (None, Some(c)) => Some(c.min(interval_us)),
            (_, None) => None,
        };
        Some(Sample {
            ts,
            scope: key.0,
            resource,
            some_avg10: r.some.avg10,
            full_avg10: r.full.map(|f| f.avg10),
            some_delta_us: counter_delta(prev.some_total, cur.some_total, interval_us),
            full_delta_us,
            interval_ms,
        })
    }

    /// Read the PSI file for `scope`/`resource` under `roots` and feed it.
    pub fn sample(
        &mut self,
        roots: &Roots,
        ts: TimestampMs,
        scope: Scope,
        resource: Resource,
    ) -> io::Result<Option<Sample>> {
        let r = PsiReading::read(&scope_path(roots, &scope, resource)?)?;
        Ok(self.update(ts, scope, resource, &r))
    }

    /// Drop state for a scope (e.g. the cgroup went away).
    pub fn forget(&mut self, scope: &Scope) {
        self.prev.retain(|(s, _), _| s != scope);
    }

    /// Number of tracked (scope, resource) pairs.
    pub fn len(&self) -> usize {
        self.prev.len()
    }

    /// True when nothing is tracked.
    pub fn is_empty(&self) -> bool {
        self.prev.is_empty()
    }
}

fn counter_delta(prev: u64, cur: u64, interval_us: u64) -> u64 {
    if cur >= prev { cur - prev } else { cur.min(interval_us) }
}

/// Which PSI line a trigger watches.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Stall {
    /// At least one task stalled.
    Some,
    /// All non-idle tasks stalled.
    Full,
}

impl Stall {
    /// `"some"` or `"full"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Stall::Some => "some",
            Stall::Full => "full",
        }
    }
}

/// A kernel PSI trigger: poll(2) reports `POLLPRI` when more than `stall` of stall time
/// accrues within any `window`.
///
/// Window rules (enforced by the kernel):
/// * the window must be between 500 ms and 10 s, and the stall threshold at most the window;
/// * **unprivileged** triggers (without `CAP_SYS_RESOURCE`, Linux 6.5+) are served by the
///   regular 2 s averaging worker, so their window must be a **multiple of 2 s**; anything
///   else fails with `EINVAL` (older kernels refuse unprivileged triggers with `EPERM`).
///   [`PsiTrigger::min_window`] returns the smallest valid window for each case and
///   [`PsiTrigger::check`] validates a pair up front.
///
/// The trigger lives as long as the file descriptor.
#[derive(Debug)]
pub struct PsiTrigger {
    file: File,
    path: PathBuf,
    spec: String,
}

impl PsiTrigger {
    /// Largest window the kernel accepts.
    pub const MAX_WINDOW: Duration = Duration::from_secs(10);
    /// Unprivileged windows must be a multiple of this.
    pub const UNPRIVILEGED_GRANULARITY: Duration = Duration::from_secs(2);

    /// Smallest window accepted: 500 ms with `CAP_SYS_RESOURCE`, 2 s without.
    pub fn min_window(privileged: bool) -> Duration {
        if privileged { Duration::from_millis(500) } else { Self::UNPRIVILEGED_GRANULARITY }
    }

    /// Validate a stall/window pair as the kernel will, including the unprivileged
    /// multiple-of-2 s rule when `privileged` is false.
    pub fn check(stall: Duration, window: Duration, privileged: bool) -> io::Result<()> {
        if window < Self::min_window(privileged) || window > Self::MAX_WINDOW {
            return Err(invalid_input(format!(
                "PSI window {window:?} out of range {:?}..={:?}",
                Self::min_window(privileged),
                Self::MAX_WINDOW
            )));
        }
        if stall.is_zero() || stall > window {
            return Err(invalid_input(format!("PSI stall threshold {stall:?} must be in (0, {window:?}]")));
        }
        if !privileged && window.as_micros().rem_euclid(Self::UNPRIVILEGED_GRANULARITY.as_micros()) != 0 {
            return Err(invalid_input(format!("unprivileged PSI window {window:?} must be a multiple of 2s")));
        }
        Ok(())
    }

    /// The string written to the PSI file, e.g. `"some 150000 1000000"`. Validates the
    /// privilege-independent bounds.
    pub fn spec(kind: Stall, stall: Duration, window: Duration) -> io::Result<String> {
        Self::check(stall, window, true)?;
        Ok(format!("{} {} {}", kind.as_str(), stall.as_micros(), window.as_micros()))
    }

    /// Open `path` (`/proc/pressure/*` or a cgroup `*.pressure`) with `O_RDWR|O_NONBLOCK`
    /// and register the trigger.
    pub fn new(path: impl AsRef<Path>, kind: Stall, stall: Duration, window: Duration) -> io::Result<Self> {
        let spec = Self::spec(kind, stall, window)?;
        let path = path.as_ref().to_path_buf();
        let mut file =
            OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(&path)?;
        // The kernel overwrites the last byte of the buffer with NUL, so the terminator
        // must be part of the write or the window loses its last digit.
        let buf = format!("{spec}\0");
        let n = file.write(buf.as_bytes())?;
        if n != buf.len() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short write registering PSI trigger"));
        }
        Ok(PsiTrigger { file, path, spec })
    }

    /// Wait up to `timeout` (zero = just check) for the trigger to fire.
    ///
    /// Returns `Ok(true)` when it fired, `Ok(false)` on timeout or `EINTR`, and an error
    /// when the source went away (the cgroup was removed, reported as `POLLERR`).
    pub fn fired(&self, timeout: Duration) -> io::Result<bool> {
        let mut pfd = libc::pollfd { fd: self.file.as_raw_fd(), events: libc::POLLPRI, revents: 0 };
        let ms = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
        // SAFETY: `pfd` is a valid pollfd for the duration of the call and nfds is 1.
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted { Ok(false) } else { Err(e) };
        }
        if pfd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(io::Error::other(format!("PSI trigger source {} went away", self.path.display())));
        }
        Ok(pfd.revents & libc::POLLPRI != 0)
    }

    /// The pressure file this trigger watches.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The trigger specification that was registered.
    pub fn spec_str(&self) -> &str {
        &self.spec
    }
}

impl AsRawFd for PsiTrigger {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl AsFd for PsiTrigger {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IO: &str = "some avg10=90.52 avg60=88.70 avg300=55.22 total=80930828973\n\
                      full avg10=76.26 avg60=76.41 avg300=47.86 total=70280649985\n";

    fn reading(some: u64, full: Option<u64>) -> PsiReading {
        PsiReading {
            some: PsiLine { avg10: 1.0, total_us: some, ..Default::default() },
            full: full.map(|t| PsiLine { avg10: 0.5, total_us: t, ..Default::default() }),
        }
    }

    #[test]
    fn parse_full_file() {
        let r = PsiReading::parse(IO).unwrap();
        assert_eq!(r.some.avg10, 90.52);
        assert_eq!(r.some.avg300, 55.22);
        assert_eq!(r.some.total_us, 80930828973);
        assert_eq!(r.full.unwrap().total_us, 70280649985);
    }

    #[test]
    fn parse_some_only_and_errors() {
        let r = PsiReading::parse("some avg10=0.00 avg60=0.00 avg300=0.00 total=5\n").unwrap();
        assert_eq!(r.full, None);
        assert!(PsiReading::parse("").is_err());
        assert!(PsiReading::parse("some avg10=x total=1").is_err());
        assert!(PsiReading::parse("some avg10=1.0").is_err());
    }

    #[test]
    fn tracker_deltas() {
        let mut t = PsiTracker::new();
        assert!(t.update(1000, Scope::System, Resource::Io, &reading(100, Some(50))).is_none());
        let s = t.update(2000, Scope::System, Resource::Io, &reading(400_100, Some(100_050))).unwrap();
        assert_eq!(s.interval_ms, 1000);
        assert_eq!(s.some_delta_us, 400_000);
        assert_eq!(s.full_delta_us, Some(100_000));
        assert!((s.some_ratio() - 0.4).abs() < 1e-6);
        assert_eq!(s.some_avg10, 1.0);
        assert_eq!(s.full_avg10, Some(0.5));
        // A different scope has its own baseline.
        assert!(t.update(2000, Scope::Cgroup("/a".into()), Resource::Io, &reading(9, None)).is_none());
        assert_eq!(t.len(), 2);
        t.forget(&Scope::Cgroup("/a".into()));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn tracker_counter_reset_and_clock_skew() {
        let mut t = PsiTracker::new();
        let cg = Scope::Cgroup("/user.slice".into());
        t.update(0, cg.clone(), Resource::Cpu, &reading(5_000_000, Some(1_000_000)));
        // Cgroup recreated: counters restart.
        let s = t.update(500, cg.clone(), Resource::Cpu, &reading(200_000, Some(10_000))).unwrap();
        assert_eq!(s.some_delta_us, 200_000);
        assert_eq!(s.full_delta_us, Some(10_000));
        // Reset to a value larger than the interval is capped.
        let s = t.update(600, cg.clone(), Resource::Cpu, &reading(150_000, None)).unwrap();
        assert_eq!(s.some_delta_us, 100_000);
        assert_eq!(s.full_delta_us, None);
        // Time going backwards re-baselines.
        assert!(t.update(100, cg.clone(), Resource::Cpu, &reading(1, None)).is_none());
        let s = t.update(1100, cg, Resource::Cpu, &reading(11, None)).unwrap();
        assert_eq!(s.some_delta_us, 10);
    }

    #[test]
    fn tracker_reads_files() {
        let dir = tempfile::tempdir().unwrap();
        let roots = Roots::under(dir.path());
        std::fs::create_dir_all(roots.proc.join("pressure")).unwrap();
        std::fs::create_dir_all(roots.cgroup.join("user.slice")).unwrap();
        std::fs::write(roots.proc.join("pressure/io"), IO).unwrap();
        std::fs::write(roots.cgroup.join("user.slice/memory.pressure"), IO).unwrap();
        let mut t = PsiTracker::new();
        assert!(t.sample(&roots, 0, Scope::System, Resource::Io).unwrap().is_none());
        let s = t.sample(&roots, 1000, Scope::System, Resource::Io).unwrap().unwrap();
        assert_eq!(s.some_delta_us, 0);
        assert!(t.sample(&roots, 0, Scope::Cgroup("/user.slice".into()), Resource::Memory).unwrap().is_none());
        assert!(t.sample(&roots, 0, Scope::Cgroup("/nope".into()), Resource::Memory).is_err());
        assert!(t.sample(&roots, 0, Scope::Cgroup("/../x".into()), Resource::Memory).is_err());
    }

    #[test]
    fn trigger_spec_and_windows() {
        let ms = Duration::from_millis;
        assert_eq!(PsiTrigger::spec(Stall::Some, ms(150), ms(1000)).unwrap(), "some 150000 1000000");
        assert_eq!(PsiTrigger::spec(Stall::Full, ms(100), ms(500)).unwrap(), "full 100000 500000");
        assert!(PsiTrigger::spec(Stall::Some, ms(150), ms(400)).is_err());
        assert!(PsiTrigger::spec(Stall::Some, ms(1500), ms(1000)).is_err());
        assert!(PsiTrigger::spec(Stall::Some, ms(0), ms(1000)).is_err());
        assert!(PsiTrigger::spec(Stall::Some, ms(100), ms(11_000)).is_err());

        assert_eq!(PsiTrigger::min_window(true), ms(500));
        assert_eq!(PsiTrigger::min_window(false), ms(2000));
        assert!(PsiTrigger::check(ms(150), ms(1000), true).is_ok());
        assert!(PsiTrigger::check(ms(150), ms(1000), false).is_err());
        assert!(PsiTrigger::check(ms(300), ms(2000), false).is_ok());
        assert!(PsiTrigger::check(ms(300), ms(3000), false).is_err());
        assert!(PsiTrigger::check(ms(300), ms(4000), false).is_ok());
    }

    #[test]
    fn trigger_on_plain_file_writes_spec_and_never_fires() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cpu.pressure");
        std::fs::write(&p, "").unwrap();
        let t = PsiTrigger::new(&p, Stall::Some, Duration::from_millis(150), Duration::from_secs(1)).unwrap();
        assert!(t.as_raw_fd() >= 0);
        assert_eq!(t.spec_str(), "some 150000 1000000");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "some 150000 1000000\0");
        assert!(!t.fired(Duration::ZERO).unwrap());
        assert!(
            PsiTrigger::new(dir.path().join("missing"), Stall::Some, Duration::from_millis(1), Duration::from_secs(1))
                .is_err()
        );
    }

    /// Exercises a real unprivileged trigger when the kernel allows it.
    #[test]
    #[ignore = "touches the host's /proc/pressure"]
    fn real_unprivileged_trigger() {
        let path = system_path(Path::new("/proc"), Resource::Cpu);
        let t = PsiTrigger::new(path, Stall::Some, Duration::from_millis(100), Duration::from_secs(2)).unwrap();
        let _ = t.fired(Duration::from_millis(10)).unwrap();
        // Sub-2s windows need CAP_SYS_RESOURCE.
        let short = PsiTrigger::new(
            system_path(Path::new("/proc"), Resource::Cpu),
            Stall::Some,
            Duration::from_millis(100),
            Duration::from_secs(1),
        );
        assert!(short.is_err());
    }
}
