//! The threads of a process and their names (`/proc/<pid>/task/<tid>/comm`).
//!
//! Linux schedules threads, and every scheduling knob (nice, uclamp, I/O priority) can be
//! set per thread. What the kernel can't know is which thread does what. Some programs say
//! so in their thread names: a browser engine names its compositor, its I/O thread and its
//! worker pool. That is enough to tell the threads a person is waiting on from the rest.

use std::path::Path;

use crate::util::read_trimmed;

/// One thread of a process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    pub tid: i32,
    /// The thread's name (at most 15 bytes, as the kernel keeps it).
    pub name: String,
}

/// The threads of `pid`, in tid order. Empty when the process is gone.
pub fn list(proc_root: &Path, pid: i32) -> Vec<Thread> {
    let dir = proc_root.join(pid.to_string()).join("task");
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out: Vec<Thread> = entries
        .filter_map(|e| {
            let tid: i32 = e.ok()?.file_name().to_str()?.parse().ok()?;
            let name = read_trimmed(&dir.join(tid.to_string()).join("comm")).ok()?;
            Some(Thread { tid, name })
        })
        .collect();
    out.sort_by_key(|t| t.tid);
    out
}

/// True while `tid` is a thread of `pid` (thread ids are reused once a thread exits).
pub fn belongs_to(proc_root: &Path, pid: i32, tid: i32) -> bool {
    proc_root.join(pid.to_string()).join("task").join(tid.to_string()).is_dir()
}

/// Matches a thread name against a pattern: exact, or a prefix when the pattern ends in `*`.
pub fn name_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threads_and_names() {
        let dir = tempfile::tempdir().unwrap();
        for (tid, name) in [(100, "chrome"), (107, "Compositor"), (103, "ThreadPoolForeg")] {
            let t = dir.path().join(format!("100/task/{tid}"));
            std::fs::create_dir_all(&t).unwrap();
            std::fs::write(t.join("comm"), format!("{name}\n")).unwrap();
        }
        let names: Vec<(i32, String)> = list(dir.path(), 100).into_iter().map(|t| (t.tid, t.name)).collect();
        assert_eq!(names, [(100, "chrome".into()), (103, "ThreadPoolForeg".into()), (107, "Compositor".into())]);
        assert!(list(dir.path(), 999).is_empty());
        assert!(belongs_to(dir.path(), 100, 107) && !belongs_to(dir.path(), 100, 108));
        assert!(name_matches("Compositor", "Compositor") && !name_matches("Compositor", "CompositorTileW"));
        assert!(name_matches("CompositorTileW*", "CompositorTileW") && name_matches("ThreadPool*", "ThreadPoolForeg"));
    }

    #[test]
    fn this_process_has_a_main_thread() {
        let me = std::process::id() as i32;
        let threads = list(Path::new("/proc"), me);
        assert!(threads.iter().any(|t| t.tid == me), "the main thread's tid is the pid");
        assert!(belongs_to(Path::new("/proc"), me, me));
    }
}
