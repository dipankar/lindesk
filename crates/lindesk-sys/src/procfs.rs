//! `/proc` access: process lists, [`ProcInfo`] for classifiers, and session environment.

use std::fs;
use std::io;
use std::path::Path;

use lindesk_core::cgroup_path;
use lindesk_plugin_api::ProcInfo;

use crate::cgroupfs::{CgroupFs, parse_pid_list};
use crate::util::{invalid_data, read_trimmed};
use crate::{Env, Roots};

fn numeric_entries(dir: &Path) -> io::Result<Vec<i32>> {
    let mut out: Vec<i32> =
        fs::read_dir(dir)?.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).filter(|p| *p > 0).collect();
    out.sort_unstable();
    Ok(out)
}

/// All pids in `proc_root`, sorted.
pub fn list_pids(proc_root: &Path) -> io::Result<Vec<i32>> {
    numeric_entries(proc_root)
}

/// Thread ids of a process (`/proc/<pid>/task`), sorted. `nice`, `ioprio` and
/// `sched_setattr` act on single threads, so per-process actuation iterates these.
pub fn threads(proc_root: &Path, pid: i32) -> io::Result<Vec<i32>> {
    numeric_entries(&proc_root.join(pid.to_string()).join("task"))
}

/// `/proc/<pid>/comm`.
pub fn comm(proc_root: &Path, pid: i32) -> io::Result<String> {
    read_trimmed(&proc_root.join(pid.to_string()).join("comm"))
}

/// Basename of `/proc/<pid>/exe`, without a trailing ` (deleted)`. `None` when the link
/// can't be read (kernel threads, other users' processes without privileges).
pub fn exe_name(proc_root: &Path, pid: i32) -> Option<String> {
    let target = fs::read_link(proc_root.join(pid.to_string()).join("exe")).ok()?;
    let name = target.file_name()?.to_str()?;
    let name = name.strip_suffix(" (deleted)").unwrap_or(name);
    (!name.is_empty()).then(|| name.to_string())
}

/// Real uid from the `Uid:` line of a `status` file.
pub fn parse_status_uid(status: &str) -> Option<u32> {
    status.lines().find_map(|l| l.strip_prefix("Uid:")).and_then(|r| r.split_whitespace().next()?.parse().ok())
}

/// Real uid of a process.
pub fn uid(proc_root: &Path, pid: i32) -> io::Result<u32> {
    let s = fs::read_to_string(proc_root.join(pid.to_string()).join("status"))?;
    parse_status_uid(&s).ok_or_else(|| invalid_data(format!("no Uid line in /proc/{pid}/status")))
}

/// The cgroup path of a process from the contents of `/proc/<pid>/cgroup`: the v2 (`0::`)
/// entry, or on v1-only hosts the hierarchy holding the `cpu` controller, else the
/// systemd named hierarchy.
pub fn parse_proc_cgroup(contents: &str) -> Option<String> {
    if let Some(p) = cgroup_path::from_proc_cgroup(contents) {
        return Some(p);
    }
    let v1 = |pred: &dyn Fn(&str) -> bool| {
        contents.lines().find_map(|l| {
            let mut it = l.splitn(3, ':');
            let (_, ctrls, path) = (it.next()?, it.next()?, it.next()?);
            pred(ctrls).then(|| path.trim().to_string())
        })
    };
    v1(&|c| c.split(',').any(|c| c == "cpu")).or_else(|| v1(&|c| c == "name=systemd"))
}

/// Cgroup path of a process (see [`parse_proc_cgroup`]).
pub fn cgroup(proc_root: &Path, pid: i32) -> io::Result<Option<String>> {
    Ok(parse_proc_cgroup(&fs::read_to_string(proc_root.join(pid.to_string()).join("cgroup"))?))
}

/// Everything classifiers get to know about a process.
///
/// `comm`, `uid` and the cgroup must be readable (otherwise the process is gone or the
/// error is returned); `exe` is best-effort. `unit`/`slice` come from the cgroup path and
/// `app_id` from the unit name as launched by the desktop.
pub fn proc_info(roots: &Roots, pid: i32) -> io::Result<ProcInfo> {
    let proc_root = &roots.proc;
    let comm = comm(proc_root, pid)?;
    let uid = uid(proc_root, pid)?;
    let cgroup = cgroup(proc_root, pid)?.unwrap_or_default();
    let info = cgroup_path::parse(&cgroup);
    let app_id = info.unit.as_deref().and_then(cgroup_path::app_id_from_unit);
    Ok(ProcInfo { pid, uid, comm, exe: exe_name(proc_root, pid), cgroup, unit: info.unit, slice: info.slice, app_id })
}

/// Pids directly in a cgroup (its `cgroup.procs`), sorted.
pub fn pids_in_cgroup(cgroup_root: &Path, cg: &str) -> io::Result<Vec<i32>> {
    let mut v = CgroupFs::new(cgroup_root).procs(cg)?;
    v.sort_unstable();
    Ok(v)
}

/// Pids in a cgroup and all of its descendants, sorted and deduplicated.
pub fn pids_in_cgroup_tree(cgroup_root: &Path, cg: &str) -> io::Result<Vec<i32>> {
    let fs = CgroupFs::new(cgroup_root);
    let mut out = Vec::new();
    let mut stack = vec![cg.to_string()];
    while let Some(c) = stack.pop() {
        match fs.read_attr_opt(&c, "cgroup.procs")? {
            Some(s) => out.extend(parse_pid_list(&s)?),
            None if c == cg => return Err(io::Error::new(io::ErrorKind::NotFound, format!("cgroup {cg} not found"))),
            None => {}
        }
        stack.extend(fs.children(&c)?);
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// One variable from `/proc/<pid>/environ` (readable for one's own processes or as root).
pub fn environ_var(proc_root: &Path, pid: i32, name: &str) -> io::Result<Option<String>> {
    let raw = fs::read(proc_root.join(pid.to_string()).join("environ"))?;
    let prefix = format!("{name}=");
    Ok(raw.split(|b| *b == 0).find_map(|kv| {
        let kv = std::str::from_utf8(kv).ok()?;
        kv.strip_prefix(&prefix).map(str::to_string)
    }))
}

/// The desktop variables (`DISPLAY`, `WAYLAND_DISPLAY`, ...) of a process's environment,
/// used to map a pid to its graphical session.
pub fn desktop_env(proc_root: &Path, pid: i32) -> io::Result<Env> {
    Ok(Env::from_environ(&fs::read(proc_root.join(pid.to_string()).join("environ"))?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn proc_fixture() -> (tempfile::TempDir, Roots) {
        let d = tempfile::tempdir().unwrap();
        let roots = Roots::under(d.path());
        let p = roots.proc.join("4321");
        fs::create_dir_all(p.join("task/4321")).unwrap();
        fs::create_dir_all(p.join("task/4330")).unwrap();
        fs::write(p.join("comm"), "firefox\n").unwrap();
        fs::write(p.join("status"), "Name:\tfirefox\nUmask:\t0002\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\n")
            .unwrap();
        fs::write(
            p.join("cgroup"),
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-org.mozilla.firefox-4321.scope\n",
        )
        .unwrap();
        symlink("/usr/lib/firefox/firefox (deleted)", p.join("exe")).unwrap();
        fs::write(p.join("environ"), b"HOME=/home/u\0WAYLAND_DISPLAY=wayland-0\0DISPLAY=:0\0").unwrap();
        // Kernel thread: no exe, root cgroup.
        let k = roots.proc.join("2");
        fs::create_dir_all(&k).unwrap();
        fs::write(k.join("comm"), "kthreadd\n").unwrap();
        fs::write(k.join("status"), "Name:\tkthreadd\nUid:\t0\t0\t0\t0\n").unwrap();
        fs::write(k.join("cgroup"), "0::/\n").unwrap();
        fs::create_dir_all(roots.proc.join("self")).unwrap();
        fs::write(roots.proc.join("meminfo"), "").unwrap();
        (d, roots)
    }

    #[test]
    fn pids_and_info() {
        let (_d, roots) = proc_fixture();
        assert_eq!(list_pids(&roots.proc).unwrap(), [2, 4321]);
        assert_eq!(threads(&roots.proc, 4321).unwrap(), [4321, 4330]);
        let i = proc_info(&roots, 4321).unwrap();
        assert_eq!(i.comm, "firefox");
        assert_eq!(i.uid, 1000);
        assert_eq!(i.exe.as_deref(), Some("firefox"));
        assert_eq!(i.unit.as_deref(), Some("app-gnome-org.mozilla.firefox-4321.scope"));
        assert_eq!(i.slice.as_deref(), Some("app.slice"));
        assert_eq!(i.app_id.as_deref(), Some("org.mozilla.firefox"));
        let k = proc_info(&roots, 2).unwrap();
        assert_eq!((k.exe, k.cgroup.as_str(), k.unit, k.uid), (None, "/", None, 0));
        assert!(proc_info(&roots, 99).is_err());
    }

    #[test]
    fn environ() {
        let (_d, roots) = proc_fixture();
        assert_eq!(environ_var(&roots.proc, 4321, "WAYLAND_DISPLAY").unwrap().as_deref(), Some("wayland-0"));
        assert_eq!(environ_var(&roots.proc, 4321, "WAYLAND").unwrap(), None);
        let env = desktop_env(&roots.proc, 4321).unwrap();
        assert_eq!(env.get("DISPLAY"), Some(":0"));
        assert_eq!(env.get("HOME"), None);
    }

    #[test]
    fn v1_cgroup_lines() {
        let v1 = "12:pids:/user.slice\n4:cpu,cpuacct:/user.slice/foo.scope\n1:name=systemd:/user.slice/bar.scope\n";
        assert_eq!(parse_proc_cgroup(v1).as_deref(), Some("/user.slice/foo.scope"));
        assert_eq!(parse_proc_cgroup("1:name=systemd:/x.scope\n").as_deref(), Some("/x.scope"));
        assert_eq!(parse_proc_cgroup("0::/a\n4:cpu:/b\n").as_deref(), Some("/a"));
        assert_eq!(parse_proc_cgroup(""), None);
        assert_eq!(parse_status_uid("Uid:\t1001\t0\t0\t0\n"), Some(1001));
        assert_eq!(parse_status_uid("Name: x\n"), None);
    }

    #[test]
    fn cgroup_pids() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/cgroup.procs"), "30\n10\n").unwrap();
        fs::write(root.join("a/b/cgroup.procs"), "20\n10\n").unwrap();
        assert_eq!(pids_in_cgroup(root, "/a").unwrap(), [10, 30]);
        assert_eq!(pids_in_cgroup_tree(root, "/a").unwrap(), [10, 20, 30]);
        assert!(pids_in_cgroup_tree(root, "/zz").is_err());
    }

    #[test]
    fn real_self() {
        let me = std::process::id() as i32;
        let roots = Roots::host();
        let i = proc_info(&roots, me).unwrap();
        assert!(!i.comm.is_empty());
        assert!(list_pids(&roots.proc).unwrap().contains(&me));
    }
}
