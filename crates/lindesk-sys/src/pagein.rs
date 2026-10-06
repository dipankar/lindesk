//! Bring a process's swapped-out memory back before it is needed.
//!
//! An app that was pushed to swap while you were elsewhere comes back one page fault at a
//! time: each first touch stops a thread until that page is read in. `process_madvise(2)`
//! with `MADV_WILLNEED` lets a privileged process ask for another's pages ahead of time. For
//! anonymous memory the kernel then reads the swapped pages of the given ranges into the
//! swap cache without waiting; the app's later touches find them there and cost a minor
//! fault instead of a stall.
//!
//! Which ranges: `/proc/<pid>/smaps` says per mapping how much of it is in swap. Mappings
//! with nothing in swap are skipped, so the request costs the kernel a walk of only the
//! page tables that have something to bring back.
//!
//! Needs Linux 5.10, `CAP_SYS_NICE` and ptrace-read access to the target (the caller's own
//! processes, or `CAP_SYS_PTRACE`). On recent kernels a process may advise itself without
//! the capability, which is what the test uses.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

use crate::util::read_file;

/// `process_madvise` takes at most this many ranges per call (`IOV_MAX`).
const MAX_RANGES_PER_CALL: usize = 1024;

/// One mapping of a process with pages in swap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Swapped {
    pub start: u64,
    pub end: u64,
    /// How much of the mapping is in swap, bytes.
    pub swap: u64,
}

/// The mappings with pages in swap, from the text of `/proc/<pid>/smaps`.
pub fn swapped_mappings(smaps: &str) -> Vec<Swapped> {
    let mut out = Vec::new();
    let mut current: Option<(u64, u64)> = None;
    for line in smaps.lines() {
        if let Some(range) = mapping_header(line) {
            current = Some(range);
        } else if let Some(kb) = line.strip_prefix("Swap:")
            && let Some((start, end)) = current
            && let Some(kb) = kb.split_whitespace().next().and_then(|v| v.parse::<u64>().ok())
            && kb > 0
        {
            out.push(Swapped { start, end, swap: kb * 1024 });
        }
    }
    out
}

/// `55d0a1c2d000-55d0a1c4e000 rw-p 00000000 00:00 0  [heap]` → the address range.
fn mapping_header(line: &str) -> Option<(u64, u64)> {
    let (range, rest) = line.split_once(' ')?;
    // Field lines ("Rss:   12 kB") have no '-' before the first space; permissions follow
    // the range on a header.
    let (start, end) = range.split_once('-')?;
    if rest.len() < 4 || !rest.as_bytes()[..4].iter().all(|b| b"rwxsp-".contains(b)) {
        return None;
    }
    let (start, end) = (u64::from_str_radix(start, 16).ok()?, u64::from_str_radix(end, 16).ok()?);
    (end > start).then_some((start, end))
}

/// `VmSwap` from the text of `/proc/<pid>/status`, bytes. 0 for kernel threads and
/// processes with nothing in swap.
pub fn vm_swap(status: &str) -> u64 {
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmSwap:"))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
}

/// Picks ranges worth at most `budget` bytes of swap, largest mappings first. A mapping that
/// holds more than what is left is cut to a proportional prefix (where in the mapping its
/// swapped pages lie is not known at this granularity). Returns the ranges and the swap
/// they are expected to bring back.
pub fn within_budget(mut mappings: Vec<Swapped>, budget: u64) -> (Vec<(u64, u64)>, u64) {
    const PAGE: u64 = 4096;
    mappings.sort_by(|a, b| b.swap.cmp(&a.swap).then(a.start.cmp(&b.start)));
    let mut left = budget;
    let mut ranges = Vec::new();
    for m in mappings {
        if left < PAGE {
            break;
        }
        if m.swap <= left {
            ranges.push((m.start, m.end));
            left -= m.swap;
        } else {
            let len = ((m.end - m.start) as u128 * left as u128 / m.swap as u128) as u64 / PAGE * PAGE;
            if len >= PAGE {
                ranges.push((m.start, m.start + len));
            }
            left = 0;
        }
    }
    ranges.sort_unstable();
    (ranges, budget - left)
}

/// What [`page_in`] asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PagedIn {
    /// Swap the request covers, bytes (an estimate when a mapping was cut to the budget).
    pub bytes: u64,
    /// Mappings (or parts of them) in the request.
    pub ranges: usize,
}

/// Asks the kernel to read back up to `budget` bytes of `pid`'s swapped-out memory. Returns
/// at once; the reads happen in the background.
///
/// `Ok` with zero bytes when the process has nothing in swap. Errors: `NotFound` when the
/// process is gone, `PermissionDenied` without the privileges, `Unsupported` on kernels
/// without `process_madvise` or without `MADV_WILLNEED` for it.
pub fn page_in(proc_root: &Path, pid: i32, budget: u64) -> io::Result<PagedIn> {
    if pid <= 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "pid must be positive"));
    }
    let dir = proc_root.join(pid.to_string());
    if vm_swap(&read_file(&dir.join("status"))?) == 0 || budget == 0 {
        return Ok(PagedIn::default());
    }
    let (ranges, bytes) = within_budget(swapped_mappings(&read_file(&dir.join("smaps"))?), budget);
    if ranges.is_empty() {
        return Ok(PagedIn::default());
    }
    will_need(pid, &ranges)?;
    Ok(PagedIn { bytes, ranges: ranges.len() })
}

/// `process_madvise(pid, ranges, MADV_WILLNEED)`.
pub fn will_need(pid: i32, ranges: &[(u64, u64)]) -> io::Result<()> {
    // SAFETY: plain integer syscall; the returned descriptor is owned from here on.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0 as libc::c_uint) };
    if fd < 0 {
        let e = io::Error::last_os_error();
        return Err(if e.raw_os_error() == Some(libc::ESRCH) { io::Error::new(io::ErrorKind::NotFound, e) } else { e });
    }
    // SAFETY: `fd` is a fresh descriptor nobody else holds.
    let pidfd = unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) };
    let iov: Vec<libc::iovec> = ranges
        .iter()
        .map(|(start, end)| libc::iovec { iov_base: *start as *mut libc::c_void, iov_len: (end - start) as usize })
        .collect();
    for chunk in iov.chunks(MAX_RANGES_PER_CALL) {
        // SAFETY: the iovecs describe ranges in the *target's* address space; the kernel
        // only reads the array itself, which lives for the duration of the call.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_process_madvise,
                pidfd.as_raw_fd(),
                chunk.as_ptr(),
                chunk.len(),
                libc::MADV_WILLNEED,
                0 as libc::c_uint,
            )
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            return Err(match e.raw_os_error() {
                Some(libc::ENOSYS) | Some(libc::EINVAL) => io::Error::new(io::ErrorKind::Unsupported, e),
                Some(libc::ESRCH) => io::Error::new(io::ErrorKind::NotFound, e),
                // A mapping changed between reading smaps and now: the rest of the request
                // is stale too, and what was advised so far stands.
                Some(libc::ENOMEM) => return Ok(()),
                _ => e,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMAPS: &str = "\
55d0a1c2d000-55d0a1c4e000 rw-p 00000000 00:00 0                          [heap]
Size:                132 kB
Rss:                  40 kB
Swap:                 92 kB
SwapPss:              92 kB
VmFlags: rd wr mr mw me ac sd
7f1c00000000-7f1c04000000 rw-p 00000000 00:00 0
Size:              65536 kB
Rss:                   0 kB
Swap:              32768 kB
7f1c10000000-7f1c10021000 r-xp 00000000 103:03 1234                      /usr/lib/libfoo.so
Size:                132 kB
Rss:                 132 kB
Swap:                  0 kB
7ffd4a5fe000-7ffd4a61f000 rw-p 00000000 00:00 0                          [stack]
Swap:                  4 kB
ffffffffff600000-ffffffffff601000 --xp 00000000 00:00 0                  [vsyscall]
Swap:                  0 kB
";

    #[test]
    fn mappings_with_swap() {
        let m = swapped_mappings(SMAPS);
        assert_eq!(
            m,
            [
                Swapped { start: 0x55d0a1c2d000, end: 0x55d0a1c4e000, swap: 92 * 1024 },
                Swapped { start: 0x7f1c00000000, end: 0x7f1c04000000, swap: 32768 * 1024 },
                Swapped { start: 0x7ffd4a5fe000, end: 0x7ffd4a61f000, swap: 4 * 1024 },
            ]
        );
        assert!(swapped_mappings("").is_empty());
        assert!(swapped_mappings("Swap: 12 kB\n").is_empty(), "a field without a mapping");
        assert_eq!(vm_swap("Name:\tfoo\nVmRSS:\t  100 kB\nVmSwap:\t   39200 kB\n"), 39200 * 1024);
        assert_eq!(vm_swap("Name:\tkthreadd\n"), 0);
    }

    #[test]
    fn budget() {
        let m = swapped_mappings(SMAPS);
        // Everything fits.
        let (ranges, bytes) = within_budget(m.clone(), 1 << 30);
        assert_eq!(ranges.len(), 3);
        assert_eq!(bytes, (92 + 32768 + 4) * 1024);
        assert!(ranges.windows(2).all(|w| w[0].0 < w[1].0), "in address order");
        // 16 MiB: half of the big mapping, nothing else.
        let (ranges, bytes) = within_budget(m.clone(), 16 << 20);
        assert_eq!(ranges, [(0x7f1c00000000, 0x7f1c02000000)]);
        assert_eq!(bytes, 16 << 20);
        // The big one and the next largest.
        let (ranges, _) = within_budget(m.clone(), (32768 + 92) * 1024);
        assert_eq!(ranges.len(), 2);
        assert_eq!(within_budget(m, 0), (Vec::new(), 0));
    }

    #[test]
    fn no_such_process_and_nothing_swapped() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(page_in(dir.path(), 4242, 1 << 30).unwrap_err().kind(), io::ErrorKind::NotFound);
        std::fs::create_dir_all(dir.path().join("4242")).unwrap();
        std::fs::write(dir.path().join("4242/status"), "Name:\tx\nVmSwap:\t0 kB\n").unwrap();
        assert_eq!(page_in(dir.path(), 4242, 1 << 30).unwrap(), PagedIn::default());
        assert!(page_in(dir.path(), 0, 1).is_err());
    }

    fn major_faults() -> i64 {
        // SAFETY: getrusage fills the struct it is given.
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
        ru.ru_majflt
    }

    /// The real thing, on this process: push two regions to swap, ask for one back, and
    /// compare what touching each costs. Skips where the host can't show it: no swap, a
    /// swap device that hadn't finished writing the pages out, or a kernel that wants
    /// `CAP_SYS_NICE` even for the caller's own memory.
    #[test]
    fn paged_in_memory_is_not_a_major_fault_again() {
        const LEN: usize = 16 << 20;
        let region = || {
            // SAFETY: a fresh private anonymous mapping, used only through this pointer.
            let p = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    LEN,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(p, libc::MAP_FAILED);
            // SAFETY: `p` is LEN writable bytes that nothing else references.
            let bytes = unsafe { std::slice::from_raw_parts_mut(p.cast::<u8>(), LEN) };
            for (i, page) in bytes.chunks_mut(4096).enumerate() {
                page[0] = (i % 251) as u8 + 1;
                page[1] = (i / 251) as u8; // not compressible to nothing
            }
            // SAFETY: the range is the mapping above.
            let rc = unsafe { libc::madvise(p, LEN, libc::MADV_PAGEOUT) };
            (p, rc == 0)
        };
        let touch = |p: *mut libc::c_void| {
            let before = major_faults();
            // SAFETY: `p` is one of the LEN-byte mappings made above, still mapped.
            let bytes = unsafe { std::slice::from_raw_parts(p.cast::<u8>(), LEN) };
            let sum: u64 = bytes.chunks(4096).map(|page| u64::from(page[0])).sum();
            assert!(sum > 0);
            major_faults() - before
        };
        let (cold, ok_a) = region();
        let (warmed, ok_b) = region();
        let me = std::process::id() as i32;
        let swapped: u64 = swapped_mappings(&read_file(Path::new("/proc/self/smaps")).unwrap())
            .iter()
            .filter(|m| m.start <= warmed as u64 && (warmed as u64) < m.end)
            .map(|m| m.swap)
            .sum();
        if !(ok_a && ok_b) || swapped < (LEN / 2) as u64 {
            eprintln!("skipped: this host did not swap the test regions out ({swapped} bytes in swap)");
            return;
        }
        match will_need(me, &[(warmed as u64, warmed as u64 + LEN as u64)]) {
            Ok(()) => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported) => {
                eprintln!("skipped: process_madvise(MADV_WILLNEED) not available here: {e}");
                return;
            }
            Err(e) => panic!("process_madvise: {e}"),
        }
        // The reads are asynchronous; give them a moment.
        std::thread::sleep(std::time::Duration::from_millis(500));
        let (cold_faults, warmed_faults) = (touch(cold), touch(warmed));
        eprintln!("major faults touching {} pages: {cold_faults} cold, {warmed_faults} after paging in", LEN / 4096);
        if cold_faults < 64 {
            // Swapped out on paper only: the pages were still in the swap cache (a slow swap
            // device that had not finished writing them), so there was nothing to bring back.
            eprintln!("skipped: touching the swapped-out region did not fault from swap here");
            return;
        }
        assert!(warmed_faults * 2 < cold_faults, "{warmed_faults} major faults after paging in vs {cold_faults} cold");
        // SAFETY: unmapping what this test mapped.
        unsafe {
            libc::munmap(cold, LEN);
            libc::munmap(warmed, LEN);
        }
    }
}
