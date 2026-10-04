//! Latency calibration: a bounded, idle-machine baseline of CPU, memory, storage and swap
//! latencies ([`LatencyMap`], `docs/CONTROL-PLANE.md` §3).
//!
//! [`run`] takes about 15 s ([`CalibrateOptions::quick`]: about 3 s), split into phase
//! budgets (CPU 35%, memory 25%, storage 35%, swap 5%); every loop checks its deadline, so
//! the total never exceeds the budget by more than one sample. Before starting it reads
//! system PSI: when any `some avg10` is above [`CalibrateOptions::busy_psi`] it refuses
//! (an error) unless [`CalibrateOptions::force`] is set, in which case the map is marked
//! `noisy`; a forced run skips the storage phase while I/O pressure is high, since a single
//! write to a saturated disk can block longer than the whole budget. PSI is checked again at the end. Scratch files are removed on every path
//! (including panics and errors) by a drop guard.
//!
//! # What is measured
//!
//! * **CPU**, per sampled logical CPU, on a thread pinned with `sched_setaffinity` (and
//!   timer slack set to 1 ns): timer-wakeup overshoot of a 50 µs `clock_nanosleep`
//!   (p50/p99), and single-thread
//!   integer throughput relative to the fastest sampled CPU. On machines with more than 8
//!   CPUs (4 with `quick`) only representative CPUs are sampled: up to 4 (2) evenly spaced
//!   CPUs of each type (performance/efficiency on hybrid CPUs, else all CPUs as one type).
//!   Every other online CPU gets the **median of the sampled CPUs of its type**, so the map
//!   always has one row per online CPU. CPUs outside this process's affinity mask are
//!   skipped the same way.
//! * **Memory**: dependent-load latency of a random single-cycle pointer chase over 256 MiB
//!   (64 MiB with `quick`), one pointer per 64-byte line; `memcpy` bandwidth (bytes copied
//!   per second); first-touch page-fault cost on a fresh anonymous mapping.
//! * **Storage**, per managed disk (the disks behind `/` and `/home`), on a scratch file in
//!   the first writable directory on that disk among [`CalibrateOptions::scratch_dirs`],
//!   `/var/lib/lindesk`, `/var/tmp`, `$HOME`, `/home`, `/`: sequential write (≤ 256 MiB,
//!   32 MiB with `quick`, `O_DIRECT` when possible, then `fsync`), sequential read, and 4 KiB
//!   random reads (p50/p99) with `O_DIRECT`; where `O_DIRECT` is refused the page cache is dropped with
//!   `posix_fadvise(DONTNEED)` and every page is read only once. Disks without a writable
//!   directory, or with less than 4× the file size free, are skipped (see the notes).
//! * **Swap**, per swap area: zram page-in is **estimated** from a pure-Rust LZ77
//!   decompression micro-benchmark on synthetic, typically compressible pages, scaled by a
//!   per-algorithm factor (lz4 1.0, lzo/lzo-rle 1.2, zstd 2.5, deflate 4, 842 3, other 2),
//!   plus the measured page-fault cost. It is an approximation of the kernel's compressor,
//!   not a measurement of it. Disk swap uses the measured 4 KiB read p50 of its backing
//!   disk, or a per-kind nominal value when that disk wasn't measured. With zswap enabled
//!   disk swap is reported as `zswap+disk` with the disk (pool-miss) cost.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::fs::{self, File, OpenOptions};
use std::hint::black_box;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lindesk_plugin_api::{CpuLatency, Hardware, LatencyMap, MemoryLatency, StorageKind, StorageLatency, SwapLatency};
use serde::{Deserialize, Serialize};

use crate::Roots;
use crate::blockdev::{self, BlockDev};
use crate::hardware;
use crate::swap::{self, SwapKind};
use crate::util::read_lossy;

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

/// Options for [`run`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct CalibrateOptions {
    /// ≈3 s instead of ≈15 s: fewer samples, smaller buffers and files.
    pub quick: bool,
    /// Run even when the machine is busy (the map is then marked `noisy`).
    pub force: bool,
    /// Preferred scratch directories for storage tests (tried before the defaults).
    pub scratch_dirs: Vec<PathBuf>,
    /// PSI `some avg10` percentage (any resource) above which the machine counts as busy.
    pub busy_psi: f64,
    /// Override the total time budget.
    pub budget: Option<Duration>,
    /// Phases to run (all by default).
    pub cpu: bool,
    pub memory: bool,
    pub storage: bool,
    pub swap: bool,
}

impl Default for CalibrateOptions {
    fn default() -> Self {
        CalibrateOptions {
            quick: false,
            force: false,
            scratch_dirs: Vec::new(),
            busy_psi: 10.0,
            budget: None,
            cpu: true,
            memory: true,
            storage: true,
            swap: true,
        }
    }
}

impl CalibrateOptions {
    /// The total time budget.
    pub fn total_budget(&self) -> Duration {
        self.budget.unwrap_or(if self.quick { Duration::from_secs(3) } else { Duration::from_secs(15) })
    }
}

/// A calibration result with human-readable notes (what was skipped or estimated, and why).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Calibration {
    pub map: LatencyMap,
    pub notes: Vec<String>,
}

/// Run the calibration (see the [module docs](self)).
pub fn run(roots: &Roots, opts: &CalibrateOptions) -> io::Result<LatencyMap> {
    run_detailed(roots, opts).map(|c| c.map)
}

/// [`run`], also returning notes about skipped devices and estimates.
pub fn run_detailed(roots: &Roots, opts: &CalibrateOptions) -> io::Result<Calibration> {
    let start = Instant::now();
    let mut notes = Vec::new();
    let mut noisy = false;
    if let Some(why) = busy(roots, opts.busy_psi) {
        if !opts.force {
            return Err(io::Error::other(format!(
                "the machine is busy ({why}); calibrate when idle, or force it (results are marked noisy)"
            )));
        }
        notes.push(format!("busy at start ({why}): results are noisy"));
        noisy = true;
    }
    let budget = opts.total_budget();
    let devices = blockdev::devices_for(roots, crate::probe::PROBED_PATHS);
    let refs: Vec<&BlockDev> = devices.iter().collect();
    let hw = hardware::detect_for(roots, &refs);

    let mut map = LatencyMap {
        measured_at: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64),
        ..Default::default()
    };
    let phase = |share: f64| start.elapsed() + budget.mul_f64(share);
    if opts.cpu {
        let deadline = start + phase(0.35);
        map.cpus = cpu_phase(roots, &hw, opts.quick, deadline, &mut notes);
    }
    if opts.memory {
        let deadline = start + phase(0.25);
        map.memory = memory_phase(opts.quick, deadline);
    }
    // A saturated disk can block a single write for longer than the whole budget, so forced
    // runs skip storage while I/O pressure is high (the map then uses nominal values).
    let io_busy = busy_resources(roots, opts.busy_psi).iter().any(|(r, _)| *r == "io");
    if opts.storage && io_busy {
        notes.push("storage skipped: the disks are busy (io pressure), results would be unbounded".into());
    } else if opts.storage {
        let deadline = start + phase(0.35);
        map.storage = storage_phase(roots, &devices, opts, deadline, &mut notes);
    }
    if opts.swap {
        let deadline = start + phase(0.05);
        map.swap = swap_phase(roots, &map, opts.quick, deadline, &mut notes);
    }
    if let Some(why) = busy(roots, opts.busy_psi) {
        notes.push(format!("busy at the end ({why}): results are noisy"));
        noisy = true;
    }
    map.noisy = noisy;
    map.duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(Calibration { map, notes })
}

/// `Some("cpu some avg10 23.0%")` when any system PSI `some avg10` exceeds `limit` percent.
fn busy(roots: &Roots, limit: f64) -> Option<String> {
    let hot = busy_resources(roots, limit);
    (!hot.is_empty()).then(|| {
        hot.iter().map(|(r, v)| format!("{r} some avg10 {v:.1}% > {limit:.0}%")).collect::<Vec<_>>().join(", ")
    })
}

/// PSI resources whose `some avg10` exceeds `limit` percent.
fn busy_resources(roots: &Roots, limit: f64) -> Vec<(&'static str, f64)> {
    ["cpu", "io", "memory"]
        .into_iter()
        .filter_map(|r| {
            let text = read_lossy(&roots.proc.join("pressure").join(r))?;
            let avg10 = text
                .lines()
                .find(|l| l.starts_with("some"))
                .and_then(|l| l.split_whitespace().find_map(|kv| kv.strip_prefix("avg10=")))
                .and_then(|v| v.parse::<f64>().ok())?;
            (avg10 > limit).then_some((r, avg10))
        })
        .collect()
}

/// Where calibration results live: `/var/lib/lindesk/calibration.json` for the system,
/// `$XDG_STATE_HOME/lindesk/calibration.json` (or `~/.local/state/...`) for a user.
pub fn default_path(system: bool) -> PathBuf {
    if system {
        return PathBuf::from("/var/lib/lindesk/calibration.json");
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));
    state.join("lindesk/calibration.json")
}

/// Load a saved map.
pub fn load(path: &Path) -> io::Result<LatencyMap> {
    serde_json::from_slice(&fs::read(path)?).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Save a map as pretty JSON (temporary file + rename, no fsync), creating parent dirs.
pub fn save(path: &Path, map: &LatencyMap) -> io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
    let res = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(map).map_err(io::Error::other)?)?;
        // No fsync: the map is regenerable, and a torn file fails to parse, which callers
        // treat as "not calibrated" (nominal values). fsync on a busy disk can take seconds.
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

/// Fold a saved map into detected hardware (`hw.latency = map`).
pub fn attach(hw: &mut Hardware, map: LatencyMap) {
    hw.latency = Some(map);
}

// ── statistics and a tiny PRNG ──────────────────────────────────────────────────────────

/// Percentile (0..=100) of unsorted samples, nearest-rank. Empty → 0.
pub fn percentile(samples: &[f64], p: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut v = samples.to_vec();
    v.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    v[rank.clamp(1, v.len()) - 1]
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

// ── CPU ─────────────────────────────────────────────────────────────────────────────────

fn allowed_cpus() -> Option<Vec<u32>> {
    // SAFETY: a zeroed cpu_set_t is valid; sched_getaffinity fills it for this thread.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            return None;
        }
        Some((0..libc::CPU_SETSIZE as u32).filter(|c| libc::CPU_ISSET(*c as usize, &set)).collect())
    }
}

fn pin_current_thread(cpu: u32) -> io::Result<()> {
    // SAFETY: as above; the set only contains `cpu`.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu as usize, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn sleep_ns(ns: i64) {
    let ts = libc::timespec { tv_sec: 0, tv_nsec: ns as _ };
    // SAFETY: valid timespec, no remainder pointer.
    unsafe {
        libc::clock_nanosleep(libc::CLOCK_MONOTONIC, 0, &ts, std::ptr::null_mut());
    }
}

/// Integer work units per second over `dur` (after a short warm-up so the CPU ramps up).
fn throughput(dur: Duration) -> f64 {
    let work = |iters: u64| {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        for i in 0..iters {
            x ^= x << 13;
            x ^= x >> 7;
            x = x.wrapping_mul(0x2545_F491_4F6C_DD1D).wrapping_add(i);
        }
        black_box(x)
    };
    let warm = Instant::now();
    while warm.elapsed() < dur / 4 {
        work(10_000);
    }
    let t = Instant::now();
    let mut n = 0u64;
    while t.elapsed() < dur {
        work(10_000);
        n += 10_000;
    }
    n as f64 / t.elapsed().as_secs_f64()
}

struct CpuSample {
    cpu: u32,
    p50: f64,
    p99: f64,
    rate: f64,
}

fn measure_cpu(cpu: u32, wakeups: usize, work: Duration) -> io::Result<CpuSample> {
    std::thread::scope(|s| {
        s.spawn(move || {
            pin_current_thread(cpu)?;
            // Measure scheduler wakeup latency, not the default 50 µs timer slack.
            // SAFETY: PR_SET_TIMERSLACK takes a plain integer and affects this thread only.
            unsafe {
                let one_ns: libc::c_ulong = 1;
                libc::prctl(libc::PR_SET_TIMERSLACK, one_ns);
            }
            let rate = throughput(work);
            let mut over = Vec::with_capacity(wakeups);
            for _ in 0..wakeups {
                let t = Instant::now();
                sleep_ns(50_000);
                over.push((t.elapsed().as_secs_f64() * 1e6 - 50.0).max(0.0));
            }
            Ok(CpuSample { cpu, p50: percentile(&over, 50.0), p99: percentile(&over, 99.0), rate })
        })
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("calibration thread panicked")))
    })
}

/// Representative CPUs: up to `k` evenly spaced members of `group`.
fn spread(group: &[u32], k: usize) -> Vec<u32> {
    if group.len() <= k {
        return group.to_vec();
    }
    (0..k).map(|i| group[i * (group.len() - 1) / (k - 1).max(1)]).collect()
}

fn median(v: &mut [f64]) -> f64 {
    percentile(v, 50.0)
}

fn cpu_phase(roots: &Roots, hw: &Hardware, quick: bool, deadline: Instant, notes: &mut Vec<String>) -> Vec<CpuLatency> {
    let online = read_lossy(&roots.sys.join("devices/system/cpu/online"))
        .and_then(|s| hardware::parse_cpulist(&s))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| (0..std::thread::available_parallelism().map_or(1, |n| n.get() as u32)).collect());
    let allowed = allowed_cpus().unwrap_or_else(|| online.clone());
    let groups: Vec<Vec<u32>> = if hw.is_hybrid() {
        vec![hw.performance_cpus.clone(), hw.efficiency_cpus.clone()]
    } else {
        vec![online.clone()]
    };
    let all_limit = if quick { 4 } else { 8 };
    let per_group = if quick { 2 } else { 4 };
    let (wakeups, work) = if quick { (30, Duration::from_millis(15)) } else { (120, Duration::from_millis(60)) };

    let mut samples: Vec<(usize, CpuSample)> = Vec::new();
    'groups: for (gi, g) in groups.iter().enumerate() {
        let usable: Vec<u32> = g.iter().copied().filter(|c| allowed.contains(c)).collect();
        let pick = if online.len() <= all_limit { usable.clone() } else { spread(&usable, per_group) };
        for cpu in pick {
            if Instant::now() >= deadline && !samples.is_empty() {
                notes.push("CPU phase hit its time budget; remaining CPUs extrapolated".into());
                break 'groups;
            }
            match measure_cpu(cpu, wakeups, work) {
                Ok(s) => samples.push((gi, s)),
                Err(e) => notes.push(format!("cpu{cpu}: not measured ({e})")),
            }
        }
    }
    let best = samples.iter().map(|(_, s)| s.rate).fold(0.0, f64::max);
    if best <= 0.0 {
        return Vec::new();
    }
    let sampled = samples.len();
    let mut out = Vec::new();
    for c in &online {
        let gi = groups.iter().position(|g| g.contains(c)).unwrap_or(0);
        let row = if let Some((_, s)) = samples.iter().find(|(_, s)| s.cpu == *c) {
            CpuLatency { cpu: *c, wakeup_p50_us: s.p50, wakeup_p99_us: s.p99, relative_perf: s.rate / best }
        } else {
            let same: Vec<&CpuSample> = samples.iter().filter(|(g, _)| *g == gi).map(|(_, s)| s).collect();
            let same: Vec<&CpuSample> = if same.is_empty() { samples.iter().map(|(_, s)| s).collect() } else { same };
            let m = |f: &dyn Fn(&CpuSample) -> f64| median(&mut same.iter().map(|s| f(s)).collect::<Vec<_>>());
            CpuLatency {
                cpu: *c,
                wakeup_p50_us: m(&|s| s.p50),
                wakeup_p99_us: m(&|s| s.p99),
                relative_perf: m(&|s| s.rate) / best,
            }
        };
        out.push(row);
    }
    if sampled < online.len() {
        notes.push(format!(
            "sampled {sampled} of {} CPUs; the others got the median of their type ({})",
            online.len(),
            if hw.is_hybrid() { "P/E" } else { "uniform" }
        ));
    }
    out
}

// ── memory ──────────────────────────────────────────────────────────────────────────────

/// ns per dependent load over a random single cycle of 64-byte lines in `bytes`.
fn pointer_chase(bytes: usize, max_steps: usize, deadline: Instant) -> f64 {
    let stride = 8; // u64 slots per 64-byte line
    let lines = (bytes / 64).max(2);
    let mut next: Vec<u32> = (0..lines as u32).collect();
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    // Sattolo's algorithm: a uniformly random permutation that is one single cycle.
    for i in (1..lines).rev() {
        let j = rng.below(i);
        next.swap(i, j);
    }
    let mut buf = vec![0u64; lines * stride];
    for (i, n) in next.iter().enumerate() {
        buf[i * stride] = u64::from(*n) * stride as u64;
    }
    drop(next);
    let mut p = 0usize;
    let mut steps = 0usize;
    let t = Instant::now();
    while steps < max_steps {
        for _ in 0..4096 {
            p = buf[p] as usize;
        }
        steps += 4096;
        if Instant::now() >= deadline {
            break;
        }
    }
    black_box(p);
    t.elapsed().as_nanos() as f64 / steps as f64
}

/// Bytes copied per second (`copy_from_slice`).
fn copy_bandwidth(bytes: usize, deadline: Instant) -> u64 {
    let src = vec![0x5Au8; bytes];
    let mut dst = vec![0u8; bytes];
    dst.copy_from_slice(&src); // fault the pages in first
    let t = Instant::now();
    let mut copied = 0usize;
    for _ in 0..8 {
        dst.copy_from_slice(black_box(&src));
        copied += bytes;
        if Instant::now() >= deadline && copied >= 2 * bytes {
            break;
        }
    }
    black_box(&dst);
    (copied as f64 / t.elapsed().as_secs_f64()) as u64
}

/// µs per first-touch page fault on a fresh anonymous private mapping.
fn fault_cost(bytes: usize) -> f64 {
    // SAFETY: sysconf has no preconditions.
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096).max(4096);
    // SAFETY: anonymous private mapping, checked for MAP_FAILED, unmapped below.
    unsafe {
        let p = libc::mmap(
            std::ptr::null_mut(),
            bytes,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        if p == libc::MAP_FAILED {
            return 0.0;
        }
        let base = p.cast::<u8>();
        let pages = bytes / page;
        let t = Instant::now();
        for i in 0..pages {
            std::ptr::write_volatile(base.add(i * page), 1);
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / pages.max(1) as f64;
        libc::munmap(p, bytes);
        us
    }
}

fn memory_phase(quick: bool, deadline: Instant) -> MemoryLatency {
    let now = Instant::now();
    let left = deadline.saturating_duration_since(now);
    let (chase, copy, fault) = if quick { (64 * MIB, 16 * MIB, 16 * MIB) } else { (256 * MIB, 64 * MIB, 64 * MIB) };
    let chase_deadline = now + left.mul_f64(0.6);
    let copy_deadline = now + left.mul_f64(0.85);
    MemoryLatency {
        load_latency_ns: pointer_chase(chase, if quick { 500_000 } else { 4_000_000 }, chase_deadline),
        copy_bps: copy_bandwidth(copy, copy_deadline),
        fault_us: fault_cost(fault),
    }
}

// ── storage ─────────────────────────────────────────────────────────────────────────────

/// A page-aligned heap buffer (for `O_DIRECT`).
struct Aligned {
    ptr: *mut u8,
    layout: Layout,
}

impl Aligned {
    fn new(len: usize) -> Self {
        let layout = Layout::from_size_align(len, 4096).expect("valid layout");
        // SAFETY: non-zero size, checked for null.
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "allocation failed");
        Aligned { ptr, layout }
    }
    fn as_mut(&mut self) -> &mut [u8] {
        // SAFETY: ptr is valid for layout.size() bytes and uniquely borrowed.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl Drop for Aligned {
    fn drop(&mut self) {
        // SAFETY: allocated with this layout.
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

/// Removes the scratch file however the measurement ends.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[allow(clippy::unnecessary_cast)] // statvfs field widths differ between architectures
fn free_bytes(dir: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: valid C string and a zeroed out-parameter.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        (libc::statvfs(c.as_ptr(), &mut st) == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
    }
}

fn drop_cache(f: &File) {
    // SAFETY: valid fd.
    unsafe {
        libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
}

/// Measure one filesystem through a scratch file in `dir`. The file is always removed.
pub fn measure_dir(
    dir: &Path,
    dev: &str,
    size: usize,
    samples: usize,
    deadline: Instant,
) -> io::Result<StorageLatency> {
    let path = dir.join(format!(".lindesk-calibrate-{}.tmp", std::process::id()));
    let _guard = Scratch(path.clone());
    // O_DIRECT writes measure the device and keep the final fsync short (no pile of dirty
    // pages to flush on a busy disk), so the deadline holds; buffered writes are the fallback.
    let new = || OpenOptions::new().read(true).write(true).create_new(true).clone();
    let mut f = match new().custom_flags(libc::O_DIRECT).open(&path) {
        Ok(f) => f,
        Err(_) => {
            let _ = fs::remove_file(&path);
            new().open(&path)?
        }
    };
    let chunk = MIB;
    let size = size.max(chunk) / chunk * chunk;

    // Sequential write of incompressible data, then fsync.
    let mut rng = Rng(0xC0FF_EE00_D15E_A5E5);
    let mut block = Aligned::new(chunk);
    for w in block.as_mut().chunks_mut(8) {
        w.copy_from_slice(&rng.next().to_ne_bytes()[..w.len()]);
    }
    let t = Instant::now();
    let mut written = 0;
    while written < size {
        let b = block.as_mut();
        b[0] = b[0].wrapping_add(1);
        if let Err(e) = f.write_all(b) {
            // Some filesystems accept O_DIRECT at open time but not for writes.
            if written == 0 && e.raw_os_error() == Some(libc::EINVAL) {
                drop(f);
                let _ = fs::remove_file(&path);
                f = new().open(&path)?;
                continue;
            }
            return Err(e);
        }
        written += chunk;
        if Instant::now() >= deadline {
            break;
        }
    }
    f.sync_all()?;
    let seq_write_bps = (written as f64 / t.elapsed().as_secs_f64()) as u64;
    let size = written;

    // Reads: O_DIRECT when the filesystem allows it, else drop the page cache.
    let direct = OpenOptions::new().read(true).custom_flags(libc::O_DIRECT).open(&path).ok();
    let rf = match direct {
        Some(d) => d,
        None => {
            drop_cache(&f);
            File::open(&path)?
        }
    };
    let mut buf = Aligned::new(chunk);
    let t = Instant::now();
    let mut read = 0;
    while read < size {
        let n = rf.read_at(buf.as_mut(), read as u64)?;
        if n == 0 {
            break;
        }
        read += n;
        if Instant::now() >= deadline {
            break;
        }
    }
    let seq_read_bps = (read as f64 / t.elapsed().as_secs_f64()) as u64;

    // Random 4 KiB reads, each page at most once (so the fadvise fallback never hits cache).
    drop_cache(&rf);
    let pages = size / 4096;
    let mut order: Vec<usize> = (0..pages).collect();
    for i in (1..pages).rev() {
        let j = rng.below(i + 1);
        order.swap(i, j);
    }
    let mut page = Aligned::new(4096);
    let mut lat = Vec::with_capacity(samples);
    for p in order.into_iter().take(samples) {
        let t = Instant::now();
        rf.read_exact_at(page.as_mut(), (p * 4096) as u64)?;
        lat.push(t.elapsed().as_secs_f64() * 1e6);
        if Instant::now() >= deadline && lat.len() >= 16 {
            break;
        }
    }
    Ok(StorageLatency {
        dev: dev.to_string(),
        read_4k_p50_us: percentile(&lat, 50.0),
        read_4k_p99_us: percentile(&lat, 99.0),
        seq_read_bps,
        seq_write_bps,
    })
}

fn candidate_dirs(opts: &CalibrateOptions) -> Vec<PathBuf> {
    let mut v = opts.scratch_dirs.clone();
    v.push("/var/lib/lindesk".into());
    v.push("/var/tmp".into());
    if let Some(h) = std::env::var_os("HOME") {
        v.push(h.into());
    }
    v.push("/home".into());
    v.push("/".into());
    v
}

fn writable(dir: &Path) -> bool {
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok();
    // SAFETY: valid C string.
    c.is_some_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0) && dir.is_dir()
}

fn storage_phase(
    roots: &Roots,
    devices: &[BlockDev],
    opts: &CalibrateOptions,
    deadline: Instant,
    notes: &mut Vec<String>,
) -> Vec<StorageLatency> {
    let mounts = blockdev::mounts(roots).unwrap_or_default();
    let dirs = candidate_dirs(opts);
    let (size, samples) = if opts.quick { (32 * MIB, 300) } else { (256 * MIB, 2000) };
    let mut out = Vec::new();
    for (i, d) in devices.iter().enumerate() {
        let dev = d.dev.to_string();
        let dir = dirs.iter().find(|dir| {
            writable(dir)
                && blockdev::mount_for(&mounts, dir)
                    .is_some_and(|m| blockdev::mount_devices(&roots.sys, m).contains(&d.dev))
        });
        let Some(dir) = dir else {
            notes.push(format!("{}: skipped, no writable directory on it (pass a scratch dir)", d.name));
            continue;
        };
        let free = free_bytes(dir).unwrap_or(0) as usize;
        let size = size.min(free / 4);
        if size < 4 * MIB {
            notes.push(format!("{}: skipped, too little free space in {}", d.name, dir.display()));
            continue;
        }
        // Share what is left of the phase between the remaining devices.
        let left = deadline.saturating_duration_since(Instant::now());
        let dev_deadline = Instant::now() + left / (devices.len() - i) as u32;
        match measure_dir(dir, &dev, size, samples, dev_deadline) {
            Ok(s) => out.push(s),
            Err(e) => notes.push(format!("{}: not measured in {} ({e})", d.name, dir.display())),
        }
    }
    out
}

// ── swap ────────────────────────────────────────────────────────────────────────────────

/// A tiny LZ77 codec (greedy, 4-byte hash, 16-bit offsets) standing in for the kernel's
/// lz4/lzo when estimating zram page-in cost. Format: a control byte `0lllllll` is followed
/// by `l + 1` literals; `1mmmmmmm` is a match of length `m + 4` followed by a 2-byte offset.
pub mod lz {
    const MIN: usize = 4;
    const MAX_MATCH: usize = 127 + MIN;

    pub fn compress(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len() / 2);
        let mut table = [usize::MAX; 4096];
        let mut lit_start = 0;
        let mut i = 0;
        let flush = |out: &mut Vec<u8>, lits: &[u8]| {
            for c in lits.chunks(128) {
                out.push((c.len() - 1) as u8);
                out.extend_from_slice(c);
            }
        };
        while i + MIN <= input.len() {
            let key = u32::from_le_bytes([input[i], input[i + 1], input[i + 2], input[i + 3]]);
            let h = (key.wrapping_mul(2_654_435_761) >> 20) as usize;
            let cand = table[h];
            table[h] = i;
            if cand != usize::MAX && i - cand <= u16::MAX as usize && input[cand..cand + MIN] == input[i..i + MIN] {
                let mut len = MIN;
                while i + len < input.len() && len < MAX_MATCH && input[cand + len] == input[i + len] {
                    len += 1;
                }
                flush(&mut out, &input[lit_start..i]);
                out.push(0x80 | (len - MIN) as u8);
                out.extend_from_slice(&((i - cand) as u16).to_le_bytes());
                i += len;
                lit_start = i;
            } else {
                i += 1;
            }
        }
        flush(&mut out, &input[lit_start..]);
        out
    }

    pub fn decompress(input: &[u8], out: &mut Vec<u8>) {
        out.clear();
        let mut i = 0;
        while i < input.len() {
            let c = input[i] as usize;
            i += 1;
            if c & 0x80 == 0 {
                out.extend_from_slice(&input[i..i + c + 1]);
                i += c + 1;
            } else {
                let len = (c & 0x7f) + MIN;
                let off = u16::from_le_bytes([input[i], input[i + 1]]) as usize;
                i += 2;
                let start = out.len() - off;
                for k in 0..len {
                    let b = out[start + k];
                    out.push(b);
                }
            }
        }
    }
}

/// Synthetic anonymous-memory pages: a quarter zero pages, the rest text-like structures
/// with some random bytes (≈3:1 compressible, like typical desktop heaps).
fn synthetic_pages(n: usize) -> Vec<Vec<u8>> {
    let words: [&[u8]; 8] =
        [b"window", b"\x00\x00\x00\x00", b"0x7ffe", b"label", b"pointer", b" ", b"{\"id\":", b"\x01\x00"];
    let mut rng = Rng(42);
    (0..n)
        .map(|i| {
            let mut p = Vec::with_capacity(4096);
            if i % 4 == 0 {
                p.resize(4096, 0);
                return p;
            }
            while p.len() < 4096 {
                if rng.below(8) == 0 {
                    p.extend_from_slice(&rng.next().to_le_bytes());
                } else {
                    p.extend_from_slice(words[rng.below(words.len())]);
                }
            }
            p.truncate(4096);
            p
        })
        .collect()
}

/// Estimated µs to decompress one 4 KiB page with our LZ77 (see [`lz`]).
fn decompress_us(pages: usize, deadline: Instant) -> f64 {
    let comp: Vec<Vec<u8>> = synthetic_pages(pages).iter().map(|p| lz::compress(p)).collect();
    let mut out = Vec::with_capacity(4096);
    let t = Instant::now();
    let mut n = 0usize;
    for _ in 0..16 {
        for c in &comp {
            lz::decompress(black_box(c), &mut out);
            black_box(&out);
            n += 1;
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    t.elapsed().as_secs_f64() * 1e6 / n.max(1) as f64
}

/// Relative cost of the kernel's zram algorithms against our lz4-class codec.
pub fn algorithm_factor(alg: Option<&str>) -> f64 {
    match alg {
        Some("lz4" | "lz4hc") => 1.0,
        Some("lzo" | "lzo-rle") => 1.2,
        Some("zstd") => 2.5,
        Some("deflate") => 4.0,
        Some("842") => 3.0,
        _ => 2.0,
    }
}

/// Typical 4 KiB random-read latency by device kind, µs (used when a disk wasn't measured).
pub fn nominal_read_4k_us(kind: StorageKind) -> f64 {
    match kind {
        StorageKind::Nvme => 80.0,
        StorageKind::Ssd => 150.0,
        StorageKind::Hdd => 8000.0,
        StorageKind::Emmc => 400.0,
        StorageKind::Sd => 2000.0,
        StorageKind::Virtual => 200.0,
        StorageKind::Network => 1000.0,
        StorageKind::Unknown => 500.0,
    }
}

fn swap_phase(
    roots: &Roots,
    map: &LatencyMap,
    quick: bool,
    deadline: Instant,
    notes: &mut Vec<String>,
) -> Vec<SwapLatency> {
    let devs = swap::swap_devices(roots);
    if devs.is_empty() {
        return Vec::new();
    }
    let zswap = read_lossy(&roots.sys.join("module/zswap/parameters/enabled")).is_some_and(|v| v == "Y" || v == "1");
    let fault = if map.memory.fault_us > 0.0 { map.memory.fault_us } else { 0.5 };
    let mut lz_us = None;
    let mut out = Vec::new();
    for d in devs {
        let (kind, us) = match d.kind {
            SwapKind::Zram => {
                let base = *lz_us.get_or_insert_with(|| decompress_us(if quick { 128 } else { 1024 }, deadline));
                let f = algorithm_factor(d.compressor.as_deref());
                notes.push(format!(
                    "{}: zram page-in estimated as {base:.2} µs (LZ77 micro-benchmark) × {f} ({}) + {fault:.2} µs fault",
                    d.path,
                    d.compressor.as_deref().unwrap_or("unknown algorithm")
                ));
                ("zram".to_string(), base * f + fault)
            }
            k => {
                let measured = d
                    .backing_dev
                    .as_ref()
                    .and_then(|b| map.storage.iter().find(|s| &s.dev == b))
                    .map(|s| s.read_4k_p50_us)
                    .filter(|v| *v > 0.0);
                let us = measured.unwrap_or_else(|| {
                    let kind = d.backing.unwrap_or(StorageKind::Unknown);
                    notes.push(format!(
                        "{}: backing disk not measured; using the nominal {} 4k read latency",
                        d.path,
                        hardware::kind_label(kind)
                    ));
                    nominal_read_4k_us(kind)
                });
                let base = if k == SwapKind::File { "file" } else { "partition" };
                let kind = if zswap { "zswap+disk".to_string() } else { base.to_string() };
                (kind, us + fault)
            }
        };
        out.push(SwapLatency { path: d.path, kind, page_in_us: us });
    }
    out
}

fn fmt_bw(bps: u64) -> String {
    if bps < MIB as u64 { format!("{} KiB/s", bps / KIB as u64) } else { format!("{}/s", hardware::fmt_bytes(bps)) }
}

/// One-screen summary of a map for CLIs.
pub fn render_text(c: &Calibration) -> String {
    use std::fmt::Write as _;
    let m = &c.map;
    let mut s = String::new();
    let _ = writeln!(s, "calibration: {} ms{}", m.duration_ms, if m.noisy { " (NOISY)" } else { "" });
    if !m.cpus.is_empty() {
        let p50: Vec<f64> = m.cpus.iter().map(|c| c.wakeup_p50_us).collect();
        let p99: Vec<f64> = m.cpus.iter().map(|c| c.wakeup_p99_us).collect();
        let _ = writeln!(
            s,
            "  cpu      {} CPUs, wakeup overshoot p50 {:.1} µs / p99 {:.1} µs (medians)",
            m.cpus.len(),
            percentile(&p50, 50.0),
            percentile(&p99, 50.0)
        );
        let perf: Vec<String> = m.cpus.iter().map(|c| format!("{}:{:.2}", c.cpu, c.relative_perf)).collect();
        let _ = writeln!(s, "           relative perf {}", perf.join(" "));
    }
    let mm = &m.memory;
    if mm.load_latency_ns > 0.0 {
        let _ = writeln!(
            s,
            "  memory   load {:.1} ns, copy {}, first-touch fault {:.2} µs",
            mm.load_latency_ns,
            fmt_bw(mm.copy_bps),
            mm.fault_us
        );
    }
    for d in &m.storage {
        let _ = writeln!(
            s,
            "  storage  {}: 4k read p50 {:.0} µs / p99 {:.0} µs, seq read {}, write {}",
            d.dev,
            d.read_4k_p50_us,
            d.read_4k_p99_us,
            fmt_bw(d.seq_read_bps),
            fmt_bw(d.seq_write_bps)
        );
    }
    for w in &m.swap {
        let _ = writeln!(s, "  swap     {} ({}): page-in ≈ {:.1} µs", w.path, w.kind, w.page_in_us);
    }
    for n in &c.notes {
        let _ = writeln!(s, "  note     {n}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{self, Fixture};

    #[test]
    fn lz_roundtrip_and_ratio() {
        let pages = synthetic_pages(64);
        let mut out = Vec::new();
        let (mut raw, mut packed) = (0, 0);
        for p in &pages {
            let c = lz::compress(p);
            lz::decompress(&c, &mut out);
            assert_eq!(&out, p);
            raw += p.len();
            packed += c.len();
        }
        assert!(raw / packed >= 2, "synthetic pages compress ~3:1, got {raw}/{packed}");
        let mut out = Vec::new();
        lz::decompress(&lz::compress(b""), &mut out);
        assert!(out.is_empty());
        let odd = b"abcabcabcabcabcabcabcXYZ".repeat(50);
        lz::decompress(&lz::compress(&odd), &mut out);
        assert_eq!(out, odd);
    }

    #[test]
    fn percentiles() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&v, 50.0), 50.0);
        assert_eq!(percentile(&v, 99.0), 99.0);
        assert_eq!(percentile(&[], 50.0), 0.0);
        assert_eq!(spread(&[0, 1, 2, 3, 4, 5, 6, 7, 8], 3), [0, 4, 8]);
        assert_eq!(spread(&[3, 4], 4), [3, 4]);
    }

    #[test]
    fn busy_machine_is_refused_unless_forced() {
        let d = tempfile::tempdir().unwrap();
        let (roots, _) = fixture::kvm_guest(d.path());
        Fixture::new(d.path()).file(
            "proc/pressure/cpu",
            "some avg10=42.00 avg60=10.00 avg300=1.00 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        );
        let off = CalibrateOptions { cpu: false, memory: false, storage: false, swap: false, ..Default::default() };
        let e = run(&roots, &off).unwrap_err();
        assert!(e.to_string().contains("cpu some avg10 42.0%"), "{e}");
        let c = run_detailed(&roots, &CalibrateOptions { force: true, ..off }).unwrap();
        assert!(c.map.noisy);
        assert!(c.notes[0].starts_with("busy at start"));
    }

    #[test]
    fn bounded_quick_run_on_a_fixture() {
        let d = tempfile::tempdir().unwrap();
        let (roots, _) = fixture::intel_hybrid_laptop(d.path());
        // Only CPUs this machine actually has can be pinned; the rest are extrapolated.
        let opts = CalibrateOptions {
            quick: true,
            storage: false,
            budget: Some(Duration::from_millis(600)),
            ..Default::default()
        };
        let t = Instant::now();
        let c = run_detailed(&roots, &opts).unwrap();
        assert!(t.elapsed() < Duration::from_secs(5), "bounded: {:?}", t.elapsed());
        let m = &c.map;
        assert_eq!(m.cpus.len(), 22, "one row per online CPU: {:?}", c.notes);
        assert!(m.cpus.iter().all(|c| c.relative_perf > 0.0 && c.relative_perf <= 1.0));
        assert!(m.memory.load_latency_ns > 0.0 && m.memory.copy_bps > 0 && m.memory.fault_us > 0.0);
        assert_eq!(m.swap.len(), 1);
        assert_eq!(m.swap[0].kind, "zram");
        assert!(m.swap[0].page_in_us > 0.0);
        assert!(c.notes.iter().any(|n| n.contains("LZ77 micro-benchmark")));
        assert!(!m.noisy);
        assert!(render_text(&c).contains("swap     /dev/zram0 (zram)"));
    }

    #[test]
    fn storage_scratch_file_is_measured_and_removed() {
        let d = tempfile::tempdir().unwrap();
        let s = measure_dir(d.path(), "8:0", 4 * MIB, 64, Instant::now() + Duration::from_secs(5)).unwrap();
        assert!(s.seq_read_bps > 0 && s.seq_write_bps > 0);
        assert!(s.read_4k_p50_us > 0.0 && s.read_4k_p99_us >= s.read_4k_p50_us);
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 0, "scratch file removed");
    }

    #[test]
    fn disk_swap_uses_measured_or_nominal_latency() {
        let d = tempfile::tempdir().unwrap();
        let (roots, _) = fixture::hdd_desktop(d.path());
        let mut notes = Vec::new();
        let mut map = LatencyMap::default();
        let far = Instant::now() + Duration::from_secs(1);
        let s = swap_phase(&roots, &map, true, far, &mut notes);
        assert_eq!((s[0].kind.as_str(), s[0].page_in_us), ("partition", 8000.5));
        assert!(notes[0].contains("nominal HDD"));
        map.storage.push(StorageLatency { dev: "8:0".into(), read_4k_p50_us: 6000.0, ..Default::default() });
        map.memory.fault_us = 1.0;
        let s = swap_phase(&roots, &map, true, far, &mut notes);
        assert_eq!(s[0].page_in_us, 6001.0);
    }

    #[test]
    fn save_and_load() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("state/lindesk/calibration.json");
        let map = LatencyMap { duration_ms: 1234, noisy: true, ..Default::default() };
        save(&path, &map).unwrap();
        assert_eq!(load(&path).unwrap(), map);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1, "no temp file left");
        assert!(load(&d.path().join("missing.json")).is_err());
        assert!(default_path(true).ends_with("lindesk/calibration.json"));
        let mut hw = Hardware::default();
        attach(&mut hw, map.clone());
        assert_eq!(hw.latency, Some(map));
    }
}
