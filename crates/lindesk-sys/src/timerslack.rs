//! Timer slack: how far the kernel may delay a thread's timer expirations to group them
//! with others (`/proc/<tid>/timerslack_ns`).
//!
//! Every sleep, poll and futex timeout of a thread is a timer. With the default slack of
//! 50 µs each one wakes the CPU more or less on its own; with a slack of tens of
//! milliseconds the kernel lets them expire together, and a hidden app that polls a
//! hundred times a second costs a few wake-ups instead. This is the kernel side of what
//! macOS calls timer coalescing.
//!
//! The slack belongs to a thread and is inherited by the threads and processes it creates.
//! Writing `0` puts a thread back to the default it started with. Changing another
//! process's slack needs `CAP_SYS_NICE`. Realtime threads ignore it.

use std::io;
use std::path::Path;

use crate::util::{read_trimmed, write_attr};

/// The kernel's default slack for normal threads, nanoseconds.
pub const DEFAULT_NS: u64 = 50_000;

/// A thread's current timer slack, nanoseconds.
pub fn get(proc_root: &Path, tid: i32) -> io::Result<u64> {
    let s = read_trimmed(&proc_root.join(tid.to_string()).join("timerslack_ns"))?;
    s.parse().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("timerslack_ns: '{s}'")))
}

/// Sets a thread's timer slack; `0` restores its default.
pub fn set(proc_root: &Path, tid: i32, ns: u64) -> io::Result<()> {
    write_attr(&proc_root.join(tid.to_string()).join("timerslack_ns"), &ns.to_string())
}

/// The threads of a process (`/proc/<pid>/task`). Empty when the process is gone.
pub fn threads(proc_root: &Path, pid: i32) -> Vec<i32> {
    let Ok(dir) = std::fs::read_dir(proc_root.join(pid.to_string()).join("task")) else { return Vec::new() };
    let mut tids: Vec<i32> = dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).collect();
    tids.sort_unstable();
    tids
}

/// Result of [`set_all`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub threads: usize,
    /// Threads that could not be changed for a reason other than having exited.
    pub failed: usize,
}

/// Sets the slack of every thread of every process in `pids`. A thread that exits while
/// this runs is not a failure. The first error of another kind is returned alongside the
/// counts (without privileges every write fails the same way, and one is enough to say so).
pub fn set_all(proc_root: &Path, pids: &[i32], ns: u64) -> (Applied, Option<io::Error>) {
    let mut out = Applied::default();
    let mut first = None;
    for pid in pids {
        for tid in threads(proc_root, *pid) {
            match set(proc_root, tid, ns) {
                Ok(()) => out.threads += 1,
                Err(e) if matches!(e.kind(), io::ErrorKind::NotFound) || e.raw_os_error() == Some(libc::ESRCH) => {}
                Err(e) => {
                    out.failed += 1;
                    first.get_or_insert(e);
                }
            }
        }
    }
    (out, first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(root: &Path, pid: i32, tid: i32, slack: &str) {
        std::fs::create_dir_all(root.join(pid.to_string()).join("task").join(tid.to_string())).unwrap();
        std::fs::create_dir_all(root.join(tid.to_string())).unwrap();
        std::fs::write(root.join(tid.to_string()).join("timerslack_ns"), slack).unwrap();
    }

    #[test]
    fn every_thread_of_every_process() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        task(root, 100, 100, "50000\n");
        task(root, 100, 101, "50000\n");
        task(root, 200, 200, "50000\n");
        assert_eq!(threads(root, 100), [100, 101]);
        assert!(threads(root, 999).is_empty());
        assert_eq!(get(root, 101).unwrap(), DEFAULT_NS);

        let (done, err) = set_all(root, &[100, 200, 999], 50_000_000);
        assert_eq!(done, Applied { threads: 3, failed: 0 });
        assert!(err.is_none());
        assert_eq!(get(root, 101).unwrap(), 50_000_000);
        assert_eq!(get(root, 200).unwrap(), 50_000_000);

        // A thread that exited between the listing and the write is skipped, not counted.
        std::fs::remove_dir_all(root.join("101")).unwrap();
        let (done, err) = set_all(root, &[100], 0);
        assert_eq!(done, Applied { threads: 1, failed: 0 });
        assert!(err.is_none());
        assert_eq!(get(root, 100).unwrap(), 0);
    }

    /// On the real kernel, for this thread: a thread may always change its own slack.
    #[test]
    fn own_thread_on_the_real_kernel() {
        let proc = Path::new("/proc");
        // SAFETY: gettid has no arguments and cannot fail.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let Ok(before) = get(proc, tid) else {
            eprintln!("skipped: no /proc/<tid>/timerslack_ns here");
            return;
        };
        set(proc, tid, 2_000_000).unwrap();
        assert_eq!(get(proc, tid).unwrap(), 2_000_000);
        // 0 restores the default the thread started with.
        set(proc, tid, 0).unwrap();
        assert_eq!(get(proc, tid).unwrap(), before);
    }
}
