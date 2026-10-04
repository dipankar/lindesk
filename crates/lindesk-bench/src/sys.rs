//! Small OS helpers: signals, scratch directories, aligned buffers, PSI windows, monotonic
//! clock and a tiny PRNG.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use lindesk_plugin_api::Resource;
use lindesk_sys::psi::{self, PsiReading};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------------------
// Signals

static STOP: AtomicBool = AtomicBool::new(false);
static MARK: AtomicBool = AtomicBool::new(false);
static FINISH: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

extern "C" fn on_mark(_: libc::c_int) {
    MARK.store(true, Ordering::SeqCst);
}

extern "C" fn on_finish(_: libc::c_int) {
    FINISH.store(true, Ordering::SeqCst);
}

/// SIGINT/SIGTERM/SIGHUP request a stop; SIGUSR1 sets the "mark" (start of the
/// measurement window); SIGUSR2 asks for a normal finish (end of the window). Handlers only
/// set flags; every loop polls them.
pub fn install_signal_handlers() {
    // SAFETY: plain sigaction with async-signal-safe handlers (atomic stores only).
    unsafe {
        for (sig, h) in [
            (libc::SIGINT, on_stop as extern "C" fn(libc::c_int)),
            (libc::SIGTERM, on_stop),
            (libc::SIGHUP, on_stop),
            (libc::SIGUSR1, on_mark),
            (libc::SIGUSR2, on_finish),
        ] {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = h as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

pub fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// SIGUSR2 was received: finish normally.
pub fn finish_requested() -> bool {
    FINISH.load(Ordering::SeqCst)
}

/// Returns and clears the mark flag.
pub fn take_mark() -> bool {
    MARK.swap(false, Ordering::SeqCst)
}

/// Sleeps up to `d` in small steps; returns `false` if a stop was requested.
pub fn sleep_unless_stopped(d: std::time::Duration) -> bool {
    let end = Instant::now() + d;
    loop {
        if stop_requested() {
            return false;
        }
        let now = Instant::now();
        if now >= end {
            return true;
        }
        std::thread::sleep((end - now).min(std::time::Duration::from_millis(50)));
    }
}

/// `kill(pid, sig)`, ignoring errors.
pub fn kill(pid: u32, sig: libc::c_int) {
    // SAFETY: plain syscall.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

/// Asks the kernel to send SIGTERM to the caller when its parent dies (call in pre_exec).
pub fn parent_death_signal() -> std::io::Result<()> {
    // SAFETY: prctl with integer args.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as libc::c_ulong) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Scratch directories

/// Base scratch directory: `--dir`, else `$XDG_CACHE_HOME/lindesk-bench`, else
/// `~/.cache/lindesk-bench`.
pub fn scratch_base(dir: Option<&Path>) -> PathBuf {
    if let Some(d) = dir {
        return d.to_path_buf();
    }
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("lindesk-bench")
}

/// A directory we created; removed (with everything in it) on drop. The parent is removed
/// too if that leaves it empty and we created it.
pub struct ScratchDir {
    path: PathBuf,
    created_parent: Option<PathBuf>,
}

impl ScratchDir {
    pub fn create(base: &Path, name: &str) -> Result<ScratchDir> {
        let created_parent = (!base.exists()).then(|| base.to_path_buf());
        std::fs::create_dir_all(base).with_context(|| format!("creating {}", base.display()))?;
        let path = base.join(name);
        std::fs::create_dir(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(ScratchDir { path, created_parent })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        if let Some(p) = &self.created_parent {
            let _ = std::fs::remove_dir(p); // only succeeds when empty
        }
    }
}

// ---------------------------------------------------------------------------------------
// Aligned buffers (O_DIRECT)

pub struct AlignedBuf {
    ptr: *mut u8,
    layout: Layout,
}

// SAFETY: the buffer is uniquely owned.
unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    pub fn new(len: usize, align: usize) -> AlignedBuf {
        let layout = Layout::from_size_align(len.max(1), align).expect("valid layout");
        // SAFETY: non-zero size.
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "out of memory allocating {len} bytes");
        AlignedBuf { ptr, layout }
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr is valid for layout.size() initialised bytes.
        unsafe { std::slice::from_raw_parts(self.ptr, self.layout.size()) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above, uniquely borrowed.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated with this layout.
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

/// `posix_fadvise(POSIX_FADV_DONTNEED)` on a range (0 len = to end of file).
pub fn drop_cache(fd: &impl std::os::fd::AsRawFd, off: u64, len: u64) {
    // SAFETY: plain syscall on a valid fd.
    unsafe {
        libc::posix_fadvise(fd.as_raw_fd(), off as libc::off_t, len as libc::off_t, libc::POSIX_FADV_DONTNEED);
    }
}

// ---------------------------------------------------------------------------------------
// PRNG

/// xorshift64*: fast, good enough for picking offsets and thrashing caches.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn seeded() -> Rng {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64);
        Rng(t ^ (u64::from(std::process::id()) << 32) | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for c in buf.chunks_mut(8) {
            let v = self.next().to_le_bytes();
            c.copy_from_slice(&v[..c.len()]);
        }
    }
}

// ---------------------------------------------------------------------------------------
// PSI windows

/// Where to read PSI from.
#[derive(Clone, Debug)]
pub enum PsiSource {
    System,
    /// Absolute cgroup directory.
    Cgroup(PathBuf),
}

impl PsiSource {
    /// The calling process's own cgroup (v2).
    pub fn own_cgroup() -> Option<(String, PsiSource)> {
        let s = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        let cg = lindesk_sys::procfs::parse_proc_cgroup(&s)?;
        let dir = psi::cgroup_path(Path::new("/sys/fs/cgroup"), &cg, Resource::Cpu).ok()?.parent()?.to_path_buf();
        dir.join("cpu.pressure").exists().then_some((cg, PsiSource::Cgroup(dir)))
    }

    fn path(&self, r: Resource) -> PathBuf {
        match self {
            PsiSource::System => psi::system_path(Path::new("/proc"), r),
            PsiSource::Cgroup(d) => d.join(format!("{}.pressure", r.as_str())),
        }
    }

    pub fn snapshot(&self) -> PsiSnapshot {
        PsiSnapshot { at: Instant::now(), readings: Resource::ALL.map(|r| PsiReading::read(&self.path(r)).ok()) }
    }
}

/// PSI readings for cpu, io and memory at one instant.
#[derive(Clone, Debug)]
pub struct PsiSnapshot {
    at: Instant,
    readings: [Option<PsiReading>; 3],
}

/// Stall over a window for one resource.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ResourceStall {
    /// Share of the window in which some task stalled, %.
    pub some_pct: f64,
    /// Share of the window in which all non-idle tasks stalled, %.
    #[serde(default)]
    pub full_pct: Option<f64>,
    pub some_us: u64,
    #[serde(default)]
    pub full_us: Option<u64>,
    /// Kernel `some avg10` at the end of the window.
    pub some_avg10: f32,
}

/// Stall for cpu, io and memory over a window. A resource is absent when its PSI file
/// could not be read.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PsiWindow {
    #[serde(default)]
    pub cpu: Option<ResourceStall>,
    #[serde(default)]
    pub io: Option<ResourceStall>,
    #[serde(default)]
    pub memory: Option<ResourceStall>,
}

impl PsiWindow {
    pub fn between(a: &PsiSnapshot, b: &PsiSnapshot) -> PsiWindow {
        let window_us = b.at.saturating_duration_since(a.at).as_micros().max(1) as f64;
        let pct = |us: u64| ((us as f64 / window_us * 100.0).min(100.0) * 100.0).round() / 100.0;
        let one = |i: usize| -> Option<ResourceStall> {
            let (x, y) = (a.readings[i]?, b.readings[i]?);
            let some_us = y.some.total_us.saturating_sub(x.some.total_us);
            let full_us = match (x.full, y.full) {
                (Some(fx), Some(fy)) => Some(fy.total_us.saturating_sub(fx.total_us)),
                _ => None,
            };
            Some(ResourceStall {
                some_pct: pct(some_us),
                full_pct: full_us.map(pct),
                some_us,
                full_us,
                some_avg10: y.some.avg10,
            })
        };
        PsiWindow { cpu: one(0), io: one(1), memory: one(2) }
    }
}

/// Monotonic nanoseconds since an arbitrary epoch.
pub fn mono_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

pub fn nproc() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Prints one JSON value on its own line to stdout and flushes.
pub fn emit_json(v: &impl Serialize, pretty: bool) {
    use std::io::Write;
    let s = if pretty { serde_json::to_string_pretty(v) } else { serde_json::to_string(v) }.unwrap_or_default();
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{s}");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_dir_cleans_up_itself_and_new_parent() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join("lindesk-bench");
        {
            let d = ScratchDir::create(&base, "x").unwrap();
            std::fs::write(d.path().join("f"), b"hi").unwrap();
            assert!(d.path().exists());
        }
        assert!(!base.exists());
        // Pre-existing parents are kept.
        std::fs::create_dir(&base).unwrap();
        drop(ScratchDir::create(&base, "y").unwrap());
        assert!(base.exists() && !base.join("y").exists());
    }

    #[test]
    fn rng_fills() {
        let mut r = Rng::seeded();
        let mut b = [0u8; 13];
        r.fill(&mut b);
        assert!(b.iter().any(|&x| x != 0));
    }
}
