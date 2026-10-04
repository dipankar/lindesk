//! Resolve what the user typed (`1234`, `firefox`, `app-foo.scope`) to something the
//! daemons understand, pidof-style, over a `/proc` root so tests can use fixtures.

use std::path::Path;

use anyhow::{Result, bail};
use lindesk_sys::procfs;

/// What a user-supplied argument names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subject {
    Pid(i32),
    /// A unit or target string, passed to the daemon as-is.
    Target(String),
    /// A process name to resolve.
    Name(String),
}

const UNIT_SUFFIXES: &[&str] =
    &[".scope", ".service", ".slice", ".socket", ".mount", ".swap", ".timer", ".target", ".path", ".device"];

/// Classify an argument.
pub fn subject(arg: &str) -> Subject {
    let a = arg.trim();
    if let Ok(pid) = a.parse::<i32>() {
        return Subject::Pid(pid);
    }
    if a.contains(':') || a.starts_with('/') || UNIT_SUFFIXES.iter().any(|s| a.ends_with(s)) {
        return Subject::Target(a.to_string());
    }
    Subject::Name(a.to_string())
}

/// One process matching a name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub pid: i32,
    pub comm: String,
    pub cgroup: String,
    /// Start time in clock ticks since boot (for "oldest").
    pub start: u64,
}

impl Candidate {
    /// The unit (or cgroup) the process lives in, for display.
    pub fn unit(&self) -> String {
        let info = lindesk_core::cgroup_path::parse(&self.cgroup);
        info.unit.or(info.slice).unwrap_or_else(|| self.cgroup.clone())
    }
}

fn argv0_name(proc_root: &Path, pid: i32) -> Option<String> {
    let raw = std::fs::read(proc_root.join(pid.to_string()).join("cmdline")).ok()?;
    let argv0 = raw.split(|b| *b == 0).next()?;
    let s = std::str::from_utf8(argv0).ok()?;
    let base = s.rsplit('/').next()?;
    // Some programs rewrite argv[0] as "name: detail".
    let base = base.split([' ', ':']).next().unwrap_or(base);
    (!base.is_empty()).then(|| base.to_string())
}

fn start_time(proc_root: &Path, pid: i32) -> u64 {
    let Ok(stat) = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")) else { return u64::MAX };
    // comm may contain spaces and parens: fields restart after the last ')'.
    let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else { return u64::MAX };
    // After ')': state is field 3; starttime is field 22 → index 19 here.
    rest.split_whitespace().nth(19).and_then(|v| v.parse().ok()).unwrap_or(u64::MAX)
}

/// Every process whose comm, executable name or argv[0] equals `name` (like pidof),
/// excluding this process.
pub fn find_by_name(proc_root: &Path, name: &str) -> Vec<Candidate> {
    let me = std::process::id() as i32;
    let Ok(pids) = procfs::list_pids(proc_root) else { return Vec::new() };
    let mut out = Vec::new();
    for pid in pids {
        if pid == me {
            continue;
        }
        let Ok(comm) = procfs::comm(proc_root, pid) else { continue };
        // comm is truncated to 15 bytes; accept a prefix match of a long name only through exe/argv0.
        let hit = comm == name
            || procfs::exe_name(proc_root, pid).as_deref() == Some(name)
            || argv0_name(proc_root, pid).as_deref() == Some(name);
        if !hit {
            continue;
        }
        let cgroup = procfs::cgroup(proc_root, pid).ok().flatten().unwrap_or_default();
        out.push(Candidate { pid, comm, cgroup, start: start_time(proc_root, pid) });
    }
    out
}

/// Resolve a name to one pid. Several matches in the same cgroup are one workload: the
/// oldest process wins. Matches in different cgroups are ambiguous.
pub fn resolve_name(proc_root: &Path, name: &str) -> Result<i32> {
    let mut c = find_by_name(proc_root, name);
    if c.is_empty() {
        bail!(
            "no process named '{name}' (matched against comm, executable and argv[0]). Pass a pid or a unit instead."
        );
    }
    c.sort_by_key(|c| (c.start, c.pid));
    let first = &c[0];
    if c.iter().all(|x| x.cgroup == first.cgroup) {
        return Ok(first.pid);
    }
    let mut groups: Vec<(String, Vec<i32>)> = Vec::new();
    for x in &c {
        match groups.iter_mut().find(|(u, _)| *u == x.unit()) {
            Some((_, pids)) => pids.push(x.pid),
            None => groups.push((x.unit(), vec![x.pid])),
        }
    }
    let mut msg = format!("'{name}' matches processes in {} different units; pick one:\n", groups.len());
    for (unit, pids) in groups.iter().take(15) {
        let shown: Vec<String> = pids.iter().take(6).map(|p| p.to_string()).collect();
        let more = if pids.len() > 6 { format!(" (+{} more)", pids.len() - 6) } else { String::new() };
        msg.push_str(&format!("  {:<8} {unit}{more}\n", shown.join(",")));
        let _ = more;
    }
    msg.push_str("Pass one of the pids or the unit name.");
    bail!(msg)
}

/// Resolve an argument to a pid (pids pass through; names are looked up; units are refused).
pub fn resolve_pid(proc_root: &Path, arg: &str) -> Result<i32> {
    match subject(arg) {
        Subject::Pid(p) if p > 0 => Ok(p),
        Subject::Pid(p) => bail!("invalid pid {p}"),
        Subject::Name(n) => resolve_name(proc_root, &n),
        Subject::Target(t) => bail!("'{t}' is a unit, but this needs a process: pass a pid or a process name"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_sys::fixture::Fixture;

    const FF: &str = "/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-org.mozilla.firefox-1234.scope";
    const T1: &str =
        "/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.gnome.Terminal.slice/vte-spawn-1.scope";
    const T2: &str =
        "/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.gnome.Terminal.slice/vte-spawn-2.scope";

    fn proc(f: &Fixture, pid: i32, comm: &str, argv0: &str, cg: &str, start: u64) {
        f.file(&format!("proc/{pid}/comm"), format!("{comm}\n"));
        f.file(&format!("proc/{pid}/cmdline"), format!("{argv0}\0--flag\0"));
        f.file(&format!("proc/{pid}/cgroup"), format!("0::{cg}\n"));
        f.file(
            &format!("proc/{pid}/stat"),
            format!("{pid} ({comm}) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {start} 0 0\n"),
        );
        f.file(&format!("proc/{pid}/status"), "Name:\tx\nUid:\t1000\t1000\t1000\t1000\n");
    }

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let f = Fixture::new(d.path());
        proc(&f, 100, "firefox", "/usr/lib/firefox/firefox", FF, 500);
        proc(&f, 120, "firefox", "/usr/lib/firefox/firefox", FF, 400);
        proc(&f, 130, "Isolated Web Co", "/usr/lib/firefox/firefox", FF, 600);
        proc(&f, 200, "bash", "-bash", T1, 10);
        proc(&f, 201, "bash", "/bin/bash", T2, 20);
        proc(&f, 300, "kworker/0:1", "", "/", 1);
        let root = f.roots().proc;
        (d, root)
    }

    #[test]
    fn classifies_arguments() {
        assert_eq!(subject("1234"), Subject::Pid(1234));
        assert_eq!(subject("firefox"), Subject::Name("firefox".into()));
        assert_eq!(subject("eldenring.exe"), Subject::Name("eldenring.exe".into()));
        assert_eq!(subject("app-foo-1.scope"), Subject::Target("app-foo-1.scope".into()));
        assert_eq!(subject("user/1000:app.slice"), Subject::Target("user/1000:app.slice".into()));
        assert_eq!(subject("system:docker.service"), Subject::Target("system:docker.service".into()));
    }

    #[test]
    fn same_unit_resolves_to_oldest() {
        let (_d, root) = fixture();
        // comm match, plus argv0 match for the content process: all in the firefox scope.
        assert_eq!(resolve_name(&root, "firefox").unwrap(), 120);
        assert_eq!(resolve_pid(&root, "130").unwrap(), 130);
    }

    #[test]
    fn different_units_are_ambiguous() {
        let (_d, root) = fixture();
        let e = resolve_name(&root, "bash").unwrap_err().to_string();
        assert!(e.contains("2 different units"), "{e}");
        assert!(e.contains("vte-spawn-1.scope") && e.contains("vte-spawn-2.scope"), "{e}");
        assert!(e.contains("200") && e.contains("201"), "{e}");
    }

    #[test]
    fn missing_and_units() {
        let (_d, root) = fixture();
        assert!(resolve_name(&root, "nope").unwrap_err().to_string().contains("no process named 'nope'"));
        assert!(resolve_pid(&root, "app.slice").is_err());
        assert!(resolve_pid(&root, "-5").is_err());
    }
}
