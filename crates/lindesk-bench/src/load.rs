//! `lindesk-bench load`: built-in background load (no stress-ng or fio needed).
//!
//! Every generator is optional and bounded:
//!
//! * `--cpu N`: N threads doing random read-modify-write over 8 MiB each (busy and cache
//!   thrashing);
//! * `--write`: sequential buffered 1 MiB writes with an fsync every `--fsync-every`, into
//!   scratch files until the write cap, then delete and start over; `--direct` adds an
//!   O_DIRECT writer;
//! * `--read`: sequential reads of a scratch file, dropped from the page cache as it goes
//!   so every pass hits the disk;
//! * `--mem SIZE|N%`: allocate and keep touching memory (reclaim pressure), capped at a
//!   fraction of MemAvailable and given back if MemAvailable runs low;
//! * `--build --cmd "..."`: a real command, stopped with the load.
//!
//! SIGINT/SIGTERM stop everything at once and delete the scratch files; the report goes
//! to stdout as JSON. SIGUSR1 marks the start of the measurement window (`window`
//! counters) and SIGUSR2 its end, after which the load stops normally. Threads stuck in
//! a slow syscall (an fsync on a saturated disk) are given [`STOP_GRACE`]; after that the
//! scratch files are deleted anyway (unlinked files are freed when the process exits).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::Args;

use crate::results::{BuildReport, LoadConfig, LoadReport, Progress};
use crate::safety::{self, DEFAULT_MAX_MEM_FRAC, GIB, MIB, MemInfo, MemSpec, parse_size};
use crate::sys::{self, AlignedBuf, Rng, ScratchDir};

/// Hard ceiling on a single load run.
const MAX_DURATION: Duration = Duration::from_secs(3600);
const CHUNK: usize = MIB as usize;
const MEM_CHUNK: usize = 64 * MIB as usize;
const PAGE: usize = 4096;
/// How long to wait for generator threads blocked in I/O when stopping.
const STOP_GRACE: Duration = Duration::from_secs(3);

#[derive(Args, Clone, Debug)]
pub struct LoadArgs {
    /// Stop after this long (at most 1h).
    #[arg(long, default_value = "60s", value_parser = humantime::parse_duration)]
    pub duration: Duration,
    /// CPU worker threads.
    #[arg(long, default_value_t = 0)]
    pub cpu: usize,
    /// Sequential buffered writes.
    #[arg(long)]
    pub write: bool,
    /// Also O_DIRECT writes.
    #[arg(long)]
    pub direct: bool,
    /// Sequential reads.
    #[arg(long)]
    pub read: bool,
    /// Memory pressure: a size (8G) or a share of MemAvailable (40%).
    #[arg(long, value_parser = MemSpec::parse)]
    pub mem: Option<MemSpec>,
    /// Never take more than this fraction of MemAvailable (hard limit 0.75).
    #[arg(long, default_value_t = DEFAULT_MAX_MEM_FRAC)]
    pub max_mem_frac: f64,
    /// Run a real build (requires --cmd).
    #[arg(long)]
    pub build: bool,
    /// The build command, run with `sh -c` (implies --build).
    #[arg(long)]
    pub cmd: Option<String>,
    /// Total bytes the load may keep on disk at once.
    #[arg(long, alias = "write-cap", default_value = "4G", value_parser = parse_size)]
    pub cap: u64,
    /// fsync every this many bytes written.
    #[arg(long, default_value = "64M", value_parser = parse_size)]
    pub fsync_every: u64,
    /// Scratch directory [default: $XDG_CACHE_HOME/lindesk-bench].
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Single-line JSON even on a terminal.
    #[arg(long)]
    pub compact: bool,
    /// Print `{"event":"ready"}` once every generator is running (used by `run`).
    #[arg(long, hide = true)]
    pub announce_ready: bool,
}

impl LoadArgs {
    pub fn config(&self) -> LoadConfig {
        LoadConfig {
            cpu: self.cpu,
            write: self.write,
            direct: self.direct,
            read: self.read,
            mem: self.mem.map(|m| match m {
                MemSpec::Bytes(b) => safety::fmt_bytes(b),
                MemSpec::Percent(p) => format!("{p}%"),
            }),
            build_cmd: self.cmd.clone(),
            write_cap: self.cap,
            fsync_every: self.fsync_every,
            max_mem_frac: self.max_mem_frac,
        }
    }
}

#[derive(Default)]
struct Counters {
    bytes_written: AtomicU64,
    bytes_written_direct: AtomicU64,
    bytes_read: AtomicU64,
    fsyncs: AtomicU64,
    files_cycled: AtomicU64,
    cpu_ops: AtomicU64,
    mem_pages: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> Progress {
        Progress {
            bytes_written: self.bytes_written.load(Relaxed),
            bytes_written_direct: self.bytes_written_direct.load(Relaxed),
            bytes_read: self.bytes_read.load(Relaxed),
            fsyncs: self.fsyncs.load(Relaxed),
            files_cycled: self.files_cycled.load(Relaxed),
            cpu_ops: self.cpu_ops.load(Relaxed),
            mem_pages_touched: self.mem_pages.load(Relaxed),
        }
    }
}

struct Shared {
    done: AtomicBool,
    c: Counters,
    errors: Mutex<Vec<String>>,
    /// Bytes the memory worker currently holds.
    mem_held: AtomicU64,
    mem_peak: AtomicU64,
    /// Memory worker: release down to this many bytes (u64::MAX = no limit).
    mem_limit: AtomicU64,
    /// Generators still setting up (memory allocation, read file).
    pending_setup: AtomicUsize,
    /// Files left behind that could not be deleted.
    leftovers: Mutex<Vec<PathBuf>>,
}

impl Shared {
    fn stopping(&self) -> bool {
        self.done.load(Relaxed) || sys::stop_requested()
    }

    fn error(&self, e: impl std::fmt::Display) {
        let mut v = self.errors.lock().unwrap();
        if v.len() < 20 {
            v.push(e.to_string());
        }
    }
}

pub fn run(mut args: LoadArgs) -> Result<i32> {
    if args.cmd.is_some() {
        args.build = true;
    }
    if args.build && args.cmd.is_none() {
        bail!("--build needs --cmd \"<command>\"");
    }
    if args.cpu == 0 && !args.write && !args.direct && !args.read && args.mem.is_none() && !args.build {
        bail!("nothing to do: pass at least one of --cpu N, --write, --direct, --read, --mem SIZE, --build --cmd CMD");
    }
    if args.duration > MAX_DURATION {
        bail!("--duration is limited to {}", humantime::format_duration(MAX_DURATION));
    }
    let pretty = !args.compact && std::io::IsTerminal::is_terminal(&std::io::stdout());
    let config = args.config();

    let shared = Arc::new(Shared {
        done: AtomicBool::new(false),
        c: Counters::default(),
        errors: Mutex::default(),
        mem_held: AtomicU64::new(0),
        mem_peak: AtomicU64::new(0),
        mem_limit: AtomicU64::new(u64::MAX),
        pending_setup: AtomicUsize::new(0),
        leftovers: Mutex::default(),
    });

    // Disk budget.
    let needs_disk = args.write || args.direct || args.read;
    let scratch = if needs_disk {
        let base = sys::scratch_base(args.dir.as_deref());
        Some(ScratchDir::create(&base, &format!("load-{}", std::process::id()))?)
    } else {
        None
    };
    let disk = match &scratch {
        Some(s) => {
            let plan = safety::plan_disk(args.cap, safety::free_space(s.path())?);
            if plan.effective < 16 * MIB {
                bail!(
                    "not enough free space for disk load in {} (free {}, {} always kept free)",
                    s.path().display(),
                    safety::fmt_bytes(plan.free_at_start),
                    safety::fmt_bytes(safety::DISK_RESERVE)
                );
            }
            if plan.clamped {
                eprintln!("lindesk-bench: write cap reduced to {} by free space", safety::fmt_bytes(plan.effective));
            }
            Some(plan)
        }
        None => None,
    };
    let budget = disk.map_or(0, |d| d.effective);
    let read_size = if args.read { (budget / 4).min(GIB) / MIB * MIB } else { 0 };
    let write_budget = budget - read_size;
    let (buffered_cap, direct_cap) = match (args.write, args.direct) {
        (true, true) => (write_budget / 2, write_budget / 2),
        (true, false) => (write_budget, 0),
        (false, true) => (0, write_budget),
        _ => (0, 0),
    };

    // Memory budget.
    let meminfo = MemInfo::read()?;
    let mem = args.mem.map(|spec| safety::plan_mem(spec, &meminfo, args.max_mem_frac));
    if let Some(m) = &mem
        && m.clamped
    {
        eprintln!(
            "lindesk-bench: memory load capped at {} ({:.0}% of MemAvailable)",
            safety::fmt_bytes(m.effective),
            m.max_frac * 100.0
        );
    }

    let started = Instant::now();
    let mut threads = Vec::new();
    let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| std::thread::Builder::new().name(name.into()).spawn(f);

    // Setup (writing the read file, allocating memory) happens in the generator threads,
    // so the main loop sees a stop request at once.
    if read_size > 0 {
        let dir = scratch.as_ref().unwrap().path().to_path_buf();
        let path = dir.join("read.dat");
        shared.pending_setup.fetch_add(1, Relaxed);
        let s = shared.clone();
        threads.push(spawn("read", Box::new(move || reader(&s, &path, read_size)))?);
    }
    for i in 0..args.cpu {
        let s = shared.clone();
        threads.push(spawn(&format!("cpu{i}"), Box::new(move || cpu_worker(&s)))?);
    }
    if buffered_cap > 0 {
        let s = shared.clone();
        let dir = scratch.as_ref().unwrap().path().to_path_buf();
        let fe = args.fsync_every;
        threads.push(spawn("write", Box::new(move || writer(&s, &dir, "buffered", buffered_cap, fe, false)))?);
    }
    if direct_cap > 0 {
        let s = shared.clone();
        let dir = scratch.as_ref().unwrap().path().to_path_buf();
        let fe = args.fsync_every;
        threads.push(spawn("direct", Box::new(move || writer(&s, &dir, "direct", direct_cap, fe, true)))?);
    }
    if let Some(m) = mem
        && m.effective > 0
    {
        shared.pending_setup.fetch_add(1, Relaxed);
        let s = shared.clone();
        threads.push(spawn("mem", Box::new(move || mem_worker(&s, m.effective, m.floor)))?);
    }
    let mut build = match &args.cmd {
        Some(cmd) => Some((cmd.clone(), spawn_build(cmd)?, Instant::now(), None::<i32>)),
        None => None,
    };

    // Wait for the generators' setup (bounded), then announce.
    let ready_deadline = Instant::now() + Duration::from_secs(120);
    while shared.pending_setup.load(Relaxed) > 0 && !sys::stop_requested() && Instant::now() < ready_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if args.announce_ready {
        sys::emit_json(&serde_json::json!({"event": "ready", "pid": std::process::id()}), false);
    }

    let mem_floor = mem.map_or(0, |m| m.floor);
    let mut mark: Option<(Instant, Progress)> = None;
    let mut interrupted = false;
    let mut last_mem_check = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(20));
        if sys::stop_requested() {
            interrupted = true;
            break;
        }
        if sys::take_mark() {
            mark = Some((Instant::now(), shared.c.snapshot()));
        }
        if sys::finish_requested() || started.elapsed() >= args.duration {
            break;
        }
        // Memory watchdog: give memory back when the system runs low.
        if mem_floor > 0 && last_mem_check.elapsed() >= Duration::from_millis(250) {
            last_mem_check = Instant::now();
            if let Ok(mi) = MemInfo::read()
                && mi.available < mem_floor
            {
                let held = shared.mem_held.load(Relaxed);
                shared.mem_limit.fetch_min(held / 2, Relaxed);
            }
        }
        if let Some((_, child, t0, code)) = &mut build
            && code.is_none()
            && let Ok(Some(st)) = child.try_wait()
        {
            *code = Some(st.code().unwrap_or(-1));
            eprintln!("lindesk-bench: build finished after {:.1}s ({st})", t0.elapsed().as_secs_f64());
        }
    }

    // Stop everything. The window ends now, not when the threads are done.
    let end = (Instant::now(), shared.c.snapshot());
    shared.done.store(true, Relaxed);
    let build = build.map(|(cmd, mut child, t0, code)| {
        let finished = code.is_some();
        let exit_code = code.or_else(|| {
            stop_build(&mut child);
            None
        });
        BuildReport { cmd, finished, exit_code, elapsed_s: t0.elapsed().as_secs_f64() }
    });
    let grace = Instant::now() + STOP_GRACE;
    while threads.iter().any(|t| !t.is_finished()) && Instant::now() < grace {
        std::thread::sleep(Duration::from_millis(10));
    }
    let stuck = threads.iter().filter(|t| !t.is_finished()).count();
    let total = end.1;
    let elapsed = end.0.duration_since(started);
    let (window, window_s) = match mark {
        Some((t, base)) => (total.minus(&base), end.0.duration_since(t).as_secs_f64()),
        None => (total, elapsed.as_secs_f64()),
    };
    let mut errors = std::mem::take(&mut *shared.errors.lock().unwrap());
    if stuck > 0 {
        errors.push(format!(
            "{stuck} generator thread(s) still blocked in I/O after {}s; files deleted anyway",
            STOP_GRACE.as_secs()
        ));
    }
    let leftovers = std::mem::take(&mut *shared.leftovers.lock().unwrap());
    let scratch_path = scratch.as_ref().map(|s| s.path().to_path_buf());
    drop(scratch);
    let cleaned_up = leftovers.is_empty() && scratch_path.is_none_or(|p| !p.exists());
    if !cleaned_up {
        errors.push(format!("scratch files left behind: {leftovers:?}"));
    }

    let mem_released = shared.mem_limit.load(Relaxed) != u64::MAX;
    let report = LoadReport {
        kind: "load-report".into(),
        config,
        elapsed_s: round3(elapsed.as_secs_f64()),
        total,
        window,
        window_s: round3(window_s),
        mem,
        mem_peak: shared.mem_peak.load(Relaxed),
        mem_released_for_safety: mem_released,
        disk,
        build,
        interrupted,
        cleaned_up,
        errors,
    };
    sys::emit_json(&report, pretty);
    Ok(if interrupted { 130 } else { 0 })
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn cpu_worker(s: &Shared) {
    const WORDS: usize = 8 * MIB as usize / 8;
    const BATCH: u64 = 1 << 16;
    let mut v = vec![0u64; WORDS];
    let mut rng = Rng::seeded();
    let mut x = rng.next();
    while !s.stopping() {
        for _ in 0..BATCH {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let i = (x as usize) & (WORDS - 1);
            v[i] = v[i].wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ x;
        }
        s.c.cpu_ops.fetch_add(BATCH, Relaxed);
    }
    std::hint::black_box(&v);
}

fn writer(s: &Shared, dir: &Path, tag: &str, cap: u64, fsync_every: u64, direct: bool) {
    let file_size = cap.min(256 * MIB).max(CHUNK as u64) / CHUNK as u64 * CHUNK as u64;
    let mut buf = AlignedBuf::new(CHUNK, 4096);
    let mut rng = Rng::seeded();
    rng.fill(buf.as_mut_slice());
    let counter = if direct { &s.c.bytes_written_direct } else { &s.c.bytes_written };
    let mut files: Vec<PathBuf> = Vec::new();
    let mut seq = 0u64;
    let mut since_sync = 0u64;
    let cleanup = |files: &mut Vec<PathBuf>| {
        for f in files.drain(..) {
            if let Err(e) = std::fs::remove_file(&f)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                s.leftovers.lock().unwrap().push(f);
            }
        }
    };
    'outer: while !s.stopping() {
        if (files.len() as u64 + 1) * file_size > cap {
            s.c.files_cycled.fetch_add(files.len() as u64, Relaxed);
            cleanup(&mut files);
        }
        let path = dir.join(format!("{tag}-{seq}.dat"));
        seq += 1;
        let mut oo = OpenOptions::new();
        oo.write(true).create_new(true);
        if direct {
            oo.custom_flags(libc::O_DIRECT);
        }
        let mut f: File = match oo.open(&path) {
            Ok(f) => f,
            Err(e) => {
                s.error(format!("{tag} writer: open {}: {e}", path.display()));
                break;
            }
        };
        files.push(path);
        let mut written = 0u64;
        while written < file_size {
            if s.stopping() {
                break 'outer;
            }
            buf.as_mut_slice()[..8].copy_from_slice(&rng.next().to_le_bytes());
            if let Err(e) = f.write_all(buf.as_slice()) {
                s.error(format!("{tag} writer: {e}"));
                break 'outer;
            }
            written += CHUNK as u64;
            counter.fetch_add(CHUNK as u64, Relaxed);
            since_sync += CHUNK as u64;
            if since_sync >= fsync_every {
                since_sync = 0;
                if f.sync_data().is_ok() {
                    s.c.fsyncs.fetch_add(1, Relaxed);
                }
            }
        }
    }
    cleanup(&mut files);
}

fn reader(s: &Shared, path: &Path, size: u64) {
    let prepared = crate::probe::prepare_file(path, size);
    s.pending_setup.fetch_sub(1, Relaxed);
    if let Err(e) = prepared {
        let _ = std::fs::remove_file(path);
        return s.error(format!("reader: {e:#}"));
    }
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) => return s.error(format!("reader: {e}")),
    };
    let mut buf = vec![0u8; CHUNK];
    let mut off = 0u64;
    while !s.stopping() {
        match f.read_at(&mut buf, off) {
            Ok(0) => {
                off = 0;
                continue;
            }
            Ok(n) => {
                s.c.bytes_read.fetch_add(n as u64, Relaxed);
                sys::drop_cache(&f, off, n as u64);
                off += n as u64;
                if off >= size {
                    off = 0;
                }
            }
            Err(e) => return s.error(format!("reader: {e}")),
        }
    }
    let _ = std::fs::remove_file(path);
}

fn mem_worker(s: &Shared, target: u64, floor: u64) {
    struct Region(*mut libc::c_void, usize);
    let mut regions: Vec<Region> = Vec::new();
    let unmap = |r: Region| {
        // SAFETY: a mapping we created and no longer reference.
        unsafe { libc::munmap(r.0, r.1) };
    };
    let touch = |r: &Region, s: &Shared, val: u8| -> bool {
        let base = r.0 as *mut u8;
        for (n, off) in (0..r.1).step_by(PAGE).enumerate() {
            // SAFETY: within the mapping.
            unsafe { base.add(off).write_volatile(val) };
            if n % 256 == 255 {
                s.c.mem_pages.fetch_add(256, Relaxed);
                if s.stopping() {
                    return false;
                }
            }
        }
        true
    };
    let held = |regions: &Vec<Region>| regions.iter().map(|r| r.1 as u64).sum::<u64>();

    // Grow to the target, checking MemAvailable before each chunk.
    while held(&regions) < target && !s.stopping() {
        let len = (target - held(&regions)).min(MEM_CHUNK as u64) as usize;
        if MemInfo::read().is_ok_and(|mi| mi.available < floor + len as u64) {
            s.error(format!(
                "memory load stopped growing at {}: MemAvailable near the floor",
                safety::fmt_bytes(held(&regions))
            ));
            break;
        }
        // SAFETY: anonymous private mapping.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            s.error("memory load: mmap failed");
            break;
        }
        let r = Region(p, len);
        let ok = touch(&r, s, 1);
        regions.push(r);
        let h = held(&regions);
        s.mem_held.store(h, Relaxed);
        s.mem_peak.fetch_max(h, Relaxed);
        if !ok {
            break;
        }
    }
    s.pending_setup.fetch_sub(1, Relaxed);

    // Keep touching; give memory back when the watchdog lowers the limit.
    let mut val = 2u8;
    'outer: while !s.stopping() {
        for i in 0..regions.len() {
            let limit = s.mem_limit.load(Relaxed);
            while held(&regions) > limit
                && let Some(r) = regions.pop()
            {
                unmap(r);
                s.mem_held.store(held(&regions), Relaxed);
            }
            let Some(r) = regions.get(i) else { break };
            if !touch(r, s, val) {
                break 'outer;
            }
        }
        if regions.is_empty() {
            while !s.stopping() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        val = val.wrapping_add(1);
    }
    for r in regions {
        unmap(r);
    }
    s.mem_held.store(0, Relaxed);
}

fn spawn_build(cmd: &str) -> Result<Child> {
    let mut c = Command::new("sh");
    c.arg("-c").arg(cmd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
    Ok(c.spawn()?)
}

/// SIGTERM the build's process group, then SIGKILL after 3 s.
fn stop_build(child: &mut Child) {
    let pgid = child.id() as i32;
    // SAFETY: signalling our own child's process group.
    unsafe { libc::kill(-pgid, libc::SIGTERM) };
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: as above.
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
    let _ = child.wait();
}
