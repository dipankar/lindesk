//! `lindesk-bench probe`: the foreground latency probe.
//!
//! Meant to run *as the foreground app*. Three light threads model what an interactive
//! app does and time it:
//!
//! * **wakeup**: sleep 1 ms (`clock_nanosleep`, `CLOCK_MONOTONIC`) in a loop and record the
//!   overshoot: CPU scheduling latency, what makes typing and animations stutter;
//! * **io**: one 4 KiB random read every 10 ms from a 256 MiB file with `O_DIRECT`
//!   (buffered + `POSIX_FADV_DONTNEED` where unsupported): what a file dialog, a thumbnail
//!   or a page-in costs;
//! * **fault**: every 10 ms map 64 fresh anonymous pages and time the first touch of each:
//!   allocation latency, which grows with direct reclaim under memory pressure.
//!
//! Plus PSI of its own cgroup and of the system over the window.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args;

use crate::hist::Histogram;
use crate::results::ProbeReport;
use crate::safety::{self, MIB, parse_size};
use crate::sys::{self, AlignedBuf, PsiSource, PsiWindow, Rng, ScratchDir};

const BLOCK: usize = 4096;

#[derive(Args, Clone, Debug)]
pub struct ProbeArgs {
    /// How long to measure.
    #[arg(long, default_value = "10s", value_parser = humantime::parse_duration)]
    pub duration: Duration,
    /// Print one JSON line per interval, then the summary.
    #[arg(long)]
    pub stream: bool,
    /// Interval of --stream lines.
    #[arg(long, default_value = "1s", value_parser = humantime::parse_duration)]
    pub interval: Duration,
    /// Scratch directory [default: $XDG_CACHE_HOME/lindesk-bench].
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Size of the random-read file.
    #[arg(long, default_value = "256M", value_parser = parse_size)]
    pub io_file_size: u64,
    /// Period of the I/O and fault probes.
    #[arg(long, default_value = "10ms", value_parser = humantime::parse_duration)]
    pub period: Duration,
    /// Disable the I/O probe.
    #[arg(long)]
    pub no_io: bool,
    /// Prepare, print `{"event":"ready"}`, then wait for SIGUSR1 before measuring
    /// (used by `run`).
    #[arg(long, hide = true)]
    pub wait_start: bool,
    /// Single-line JSON even on a terminal.
    #[arg(long)]
    pub compact: bool,
}

struct Shared {
    done: AtomicBool,
    wakeup: Mutex<[Histogram; 2]>,
    io: Mutex<[Histogram; 2]>,
    fault: Mutex<[Histogram; 2]>,
    errors: Mutex<Vec<String>>,
}

fn record(m: &Mutex<[Histogram; 2]>, v: u64) {
    let mut g = m.lock().unwrap();
    g[0].record(v);
    g[1].record(v);
}

/// Takes the interval histogram (index 1), leaving an empty one.
fn take_interval(m: &Mutex<[Histogram; 2]>) -> Histogram {
    std::mem::take(&mut m.lock().unwrap()[1])
}

pub fn run(args: ProbeArgs) -> Result<i32> {
    let pretty = !args.compact && !args.stream && std::io::IsTerminal::is_terminal(&std::io::stdout());
    let base = sys::scratch_base(args.dir.as_deref());
    let mut errors = Vec::new();

    // Prepare the read file before measuring anything.
    let scratch;
    let io_file = if args.no_io {
        None
    } else {
        scratch = ScratchDir::create(&base, &format!("probe-{}", std::process::id()))?;
        let free = safety::free_space(scratch.path())?;
        if free < args.io_file_size + safety::DISK_RESERVE {
            bail!(
                "not enough free space in {} for the {} probe file (free {}, keeping {} in reserve)",
                base.display(),
                safety::fmt_bytes(args.io_file_size),
                safety::fmt_bytes(free),
                safety::fmt_bytes(safety::DISK_RESERVE)
            );
        }
        let path = scratch.path().join("probe-io.dat");
        prepare_file(&path, args.io_file_size)?;
        if sys::stop_requested() {
            return Ok(130);
        }
        let (f, mode) = open_for_probe(&path)?;
        Some((f, mode))
    };
    let io_mode = io_file.as_ref().map_or("disabled", |(_, m)| *m).to_string();

    if args.wait_start {
        sys::emit_json(&serde_json::json!({"event": "ready", "pid": std::process::id()}), false);
        let deadline = Instant::now() + Duration::from_secs(600);
        while !sys::take_mark() {
            if sys::stop_requested() || Instant::now() > deadline {
                return Ok(130);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    let own = PsiSource::own_cgroup();
    let cgroup = own.as_ref().map(|(cg, _)| cg.clone());
    if own.is_none() {
        errors.push("own cgroup PSI unavailable (needs cgroup v2 with PSI)".into());
    }
    let sys_src = PsiSource::System;

    let shared = Arc::new(Shared {
        done: AtomicBool::new(false),
        wakeup: Mutex::default(),
        io: Mutex::default(),
        fault: Mutex::default(),
        errors: Mutex::default(),
    });

    let mut threads = Vec::new();
    {
        let s = shared.clone();
        threads.push(std::thread::Builder::new().name("wakeup".into()).spawn(move || wakeup_loop(&s))?);
    }
    if let Some((file, _)) = io_file {
        let s = shared.clone();
        let (period, size) = (args.period, args.io_file_size);
        threads.push(std::thread::Builder::new().name("io".into()).spawn(move || io_loop(&s, &file, size, period))?);
    }
    {
        let s = shared.clone();
        let period = args.period;
        threads.push(std::thread::Builder::new().name("fault".into()).spawn(move || fault_loop(&s, period))?);
    }

    let start = Instant::now();
    let (own0, sys0) = (own.as_ref().map(|(_, s)| s.snapshot()), sys_src.snapshot());
    let (mut own_i, mut sys_i, mut last_i) = (own0.clone(), sys0.clone(), start);
    let mut interrupted = false;
    loop {
        std::thread::sleep(Duration::from_millis(20));
        if sys::stop_requested() {
            interrupted = true;
            break;
        }
        if start.elapsed() >= args.duration {
            break;
        }
        if args.stream && last_i.elapsed() >= args.interval {
            let (own_n, sys_n) = (own.as_ref().map(|(_, s)| s.snapshot()), sys_src.snapshot());
            let rep = ProbeReport {
                kind: "probe-interval".into(),
                duration_s: last_i.elapsed().as_secs_f64(),
                wakeup: take_interval(&shared.wakeup).summary(),
                io: take_interval(&shared.io).summary(),
                io_mode: io_mode.clone(),
                fault: take_interval(&shared.fault).summary(),
                cgroup: cgroup.clone(),
                psi_cgroup: match (&own_i, &own_n) {
                    (Some(a), Some(b)) => PsiWindow::between(a, b),
                    _ => PsiWindow::default(),
                },
                psi_system: PsiWindow::between(&sys_i, &sys_n),
                ..Default::default()
            };
            sys::emit_json(&rep, false);
            (own_i, sys_i, last_i) = (own_n, sys_n, Instant::now());
        }
    }
    let elapsed = start.elapsed();
    let (own1, sys1) = (own.as_ref().map(|(_, s)| s.snapshot()), sys_src.snapshot());
    shared.done.store(true, Ordering::SeqCst);
    for t in threads {
        let _ = t.join();
    }
    errors.extend(shared.errors.lock().unwrap().drain(..));

    let report = ProbeReport {
        kind: "probe-summary".into(),
        duration_s: (elapsed.as_secs_f64() * 1000.0).round() / 1000.0,
        wakeup: shared.wakeup.lock().unwrap()[0].summary(),
        io: shared.io.lock().unwrap()[0].summary(),
        io_mode,
        fault: shared.fault.lock().unwrap()[0].summary(),
        cgroup,
        psi_cgroup: match (&own0, &own1) {
            (Some(a), Some(b)) => PsiWindow::between(a, b),
            _ => PsiWindow::default(),
        },
        psi_system: PsiWindow::between(&sys0, &sys1),
        interrupted,
        errors,
    };
    sys::emit_json(&report, pretty);
    Ok(if interrupted { 130 } else { 0 })
}

/// Writes `size` bytes of random data, fsyncs and drops the file from the page cache.
pub fn prepare_file(path: &Path, size: u64) -> Result<()> {
    let mut f = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    let mut buf = vec![0u8; MIB as usize];
    let mut rng = Rng::seeded();
    rng.fill(&mut buf);
    let mut left = size;
    while left > 0 {
        if sys::stop_requested() {
            return Ok(());
        }
        let n = left.min(buf.len() as u64) as usize;
        buf[..8].copy_from_slice(&rng.next().to_le_bytes());
        f.write_all(&buf[..n]).with_context(|| format!("writing {}", path.display()))?;
        left -= n as u64;
    }
    if sys::stop_requested() {
        return Ok(()); // don't block a stop on a long fsync
    }
    f.sync_all()?;
    sys::drop_cache(&f, 0, 0);
    Ok(())
}

/// Opens with O_DIRECT if the filesystem supports it (checked with a real read).
fn open_for_probe(path: &Path) -> Result<(File, &'static str)> {
    if let Ok(f) = OpenOptions::new().read(true).custom_flags(libc::O_DIRECT).open(path) {
        let mut buf = AlignedBuf::new(BLOCK, BLOCK);
        if f.read_at(buf.as_mut_slice(), 0).is_ok() {
            return Ok((f, "direct"));
        }
    }
    let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    Ok((f, "buffered"))
}

fn wakeup_loop(s: &Shared) {
    let req = libc::timespec { tv_sec: 0, tv_nsec: 1_000_000 };
    while !s.done.load(Ordering::Relaxed) && !sys::stop_requested() {
        let t0 = sys::mono_ns();
        // SAFETY: valid timespec; a null remainder is allowed for relative sleeps.
        unsafe { libc::clock_nanosleep(libc::CLOCK_MONOTONIC, 0, &req, std::ptr::null_mut()) };
        let dt = sys::mono_ns() - t0;
        record(&s.wakeup, dt.saturating_sub(1_000_000));
    }
}

fn io_loop(s: &Shared, file: &File, size: u64, period: Duration) {
    let direct = {
        // SAFETY: F_GETFL on a valid fd.
        let fl = unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(file), libc::F_GETFL) };
        fl >= 0 && fl & libc::O_DIRECT != 0
    };
    let blocks = (size / BLOCK as u64).max(1);
    let mut buf = AlignedBuf::new(BLOCK, BLOCK);
    let mut rng = Rng::seeded();
    let mut next = Instant::now();
    let mut failures = 0;
    while !s.done.load(Ordering::Relaxed) && !sys::stop_requested() {
        let off = rng.next() % blocks * BLOCK as u64;
        if !direct {
            sys::drop_cache(file, off, BLOCK as u64);
        }
        let t0 = sys::mono_ns();
        let r = file.read_at(buf.as_mut_slice(), off);
        let dt = sys::mono_ns() - t0;
        match r {
            Ok(_) => record(&s.io, dt),
            Err(e) => {
                failures += 1;
                if failures == 1 {
                    s.errors.lock().unwrap().push(format!("probe read failed: {e}"));
                }
            }
        }
        next += period;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now; // fell behind (the read itself took longer than the period)
        }
    }
}

fn fault_loop(s: &Shared, period: Duration) {
    const PAGES: usize = 64;
    let page = 4096usize;
    let mut next = Instant::now();
    while !s.done.load(Ordering::Relaxed) && !sys::stop_requested() {
        // SAFETY: fresh private anonymous mapping, unmapped below.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                PAGES * page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            s.errors.lock().unwrap().push("probe mmap failed".into());
            return;
        }
        let base = p as *mut u8;
        let mut times = [0u64; PAGES];
        for (i, t) in times.iter_mut().enumerate() {
            let t0 = sys::mono_ns();
            // SAFETY: within the mapping.
            unsafe { base.add(i * page).write_volatile(1) };
            *t = sys::mono_ns() - t0;
        }
        // SAFETY: the mapping created above.
        unsafe { libc::munmap(p, PAGES * page) };
        {
            let mut g = s.fault.lock().unwrap();
            for t in times {
                g[0].record(t);
                g[1].record(t);
            }
        }
        next += period;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
}
