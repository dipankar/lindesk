//! [`SysEnvironment`]: the engine's view of the real machine, over `/proc`, the cgroup tree
//! and (optionally) systemd.
//!
//! It runs in one of two [`Mode`]s:
//!
//! * [`Mode::System`] (the `lindeskd` broker) watches `system.slice` services and scopes
//!   (including `docker-*.scope`, in nested slices too), `user.slice` as a whole and
//!   `machine.slice` children. Processes map to system units and to user units; the latter
//!   are relayed to the owning user's session agent rather than actuated by the broker.
//!   With a [`UserWatch`] ([`SysEnvironment::with_users`]) it also watches the user slices
//!   of the selected users exactly like [`Mode::User`] does (the broker selects the users
//!   whose session agent registered).
//! * [`Mode::User`] (the `lindesk-session` agent) watches its own user manager's
//!   `session.slice`, `app.slice`, `background.slice` and `lindesk-batch.slice` (when it
//!   exists), plus every `.service`/`.scope` directly or nested under them, `init.scope`
//!   excepted. Only `Manager::User(<uid>)` targets are returned.
//!
//! The watched set comes from one walk of the cgroup tree and is cached for about one tick
//! ([`DEFAULT_CACHE_TTL`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lindesk_core::cgroup_path;
use lindesk_host::Environment;
use lindesk_plugin_api::{Manager, ProcInfo, Target, WatchedTarget};
use lindesk_sys::cgroupfs::CgroupFs;
use lindesk_sys::{Roots, procfs};

use crate::actuators::systemd::client::{SystemdClient, UNIT_IFACE, unit_interface};

/// How long a walk of the cgroup tree is reused.
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_millis(900);

/// Slices of a user manager that the session agent watches (with every unit below them).
pub const USER_SLICES: &[&str] = &["session.slice", "app.slice", "background.slice", "lindesk-batch.slice"];

/// Environment variable set by GLib launchers to the `.desktop` file an app was started from.
const DESKTOP_FILE_VAR: &str = "GIO_LAUNCHED_DESKTOP_FILE";

/// Which host the environment serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The system broker (`lindeskd`).
    System,
    /// The session agent of this user (`lindesk-session`).
    User(u32),
}

/// The cgroup of a user's systemd manager, e.g. `/user.slice/user-1000.slice/user@1000.service`.
pub fn user_manager_cgroup(uid: u32) -> String {
    format!("/user.slice/user-{uid}.slice/user@{uid}.service")
}

/// Which users' managers a [`Mode::System`] environment watches in addition to the system
/// units: none (the default), a set of uids, or all users with a running user manager.
///
/// A cheap, cloneable handle: the daemon keeps one clone and updates it at runtime (for
/// example when a session agent registers); the environment picks the change up on its next
/// walk, bypassing the walk cache.
#[derive(Clone, Debug, Default)]
pub struct UserWatch(Arc<UserWatchInner>);

#[derive(Debug, Default)]
struct UserWatchInner {
    sel: Mutex<UserSelection>,
    generation: AtomicU64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct UserSelection {
    all: bool,
    uids: BTreeSet<u32>,
}

impl UserWatch {
    /// Watch no users.
    pub fn new() -> Self {
        Self::default()
    }

    /// Watch these users.
    pub fn with_uids(uids: impl IntoIterator<Item = u32>) -> Self {
        let w = Self::default();
        w.set(uids);
        w
    }

    /// Watch every user that has a `user@UID.service` manager.
    pub fn all_users() -> Self {
        let w = Self::default();
        w.set_all(true);
        w
    }

    fn update(&self, f: impl FnOnce(&mut UserSelection)) -> bool {
        let mut sel = lock(&self.0.sel);
        let before = sel.clone();
        f(&mut sel);
        let changed = *sel != before;
        if changed {
            self.0.generation.fetch_add(1, Ordering::SeqCst);
        }
        changed
    }

    /// Watch (or stop watching) all users. Returns true if the selection changed.
    pub fn set_all(&self, all: bool) -> bool {
        self.update(|s| s.all = all)
    }

    /// Replace the explicit uid set. Returns true if the selection changed.
    pub fn set(&self, uids: impl IntoIterator<Item = u32>) -> bool {
        let uids: BTreeSet<u32> = uids.into_iter().collect();
        self.update(|s| s.uids = uids)
    }

    /// Add one user. Returns true if it was not watched before.
    pub fn add(&self, uid: u32) -> bool {
        self.update(|s| {
            s.uids.insert(uid);
        })
    }

    /// Remove one user. Returns true if it was watched explicitly.
    pub fn remove(&self, uid: u32) -> bool {
        self.update(|s| {
            s.uids.remove(&uid);
        })
    }

    /// The explicitly selected uids.
    pub fn uids(&self) -> Vec<u32> {
        lock(&self.0.sel).uids.iter().copied().collect()
    }

    /// True when every user is watched.
    pub fn is_all(&self) -> bool {
        lock(&self.0.sel).all
    }

    /// True if `uid`'s units are watched.
    pub fn includes(&self, uid: u32) -> bool {
        let sel = lock(&self.0.sel);
        sel.all || sel.uids.contains(&uid)
    }

    fn generation(&self) -> u64 {
        self.0.generation.load(Ordering::SeqCst)
    }
}

#[derive(Debug)]
struct Walk {
    at: Instant,
    /// [`UserWatch`] generation the walk was made with.
    generation: u64,
    targets: Vec<WatchedTarget>,
}

/// The real [`Environment`]. See the [module docs](self).
#[derive(Debug)]
pub struct SysEnvironment {
    roots: Roots,
    mode: Mode,
    systemd: Option<Mutex<SystemdClient>>,
    users: UserWatch,
    ttl: Duration,
    cache: Mutex<Option<Walk>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl SysEnvironment {
    /// An environment over `roots` for `mode`, without systemd.
    pub fn new(roots: Roots, mode: Mode) -> Self {
        SysEnvironment {
            roots,
            mode,
            systemd: None,
            users: UserWatch::new(),
            ttl: DEFAULT_CACHE_TTL,
            cache: Mutex::new(None),
        }
    }

    /// [`Mode::System`] only: also watch the user slices (`session.slice`, `app.slice`,
    /// `background.slice`, `lindesk-batch.slice`) and their units of the users selected by
    /// `users`, like [`Mode::User`] does for its own user. Ignored in [`Mode::User`].
    pub fn with_users(mut self, users: UserWatch) -> Self {
        self.users = users;
        self
    }

    /// The user selection handle (shared with whoever passed it to [`Self::with_users`]).
    pub fn users(&self) -> &UserWatch {
        &self.users
    }

    /// Use systemd (unit start times and main pids) to rank app-id matches. Without it the
    /// start time of a unit's oldest process is used.
    pub fn with_systemd(mut self, client: SystemdClient) -> Self {
        self.systemd = Some(Mutex::new(client));
        self
    }

    /// How long the watched-target walk is cached.
    pub fn with_cache_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// The mode this environment serves.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    fn fs(&self) -> CgroupFs {
        CgroupFs::new(&self.roots.cgroup)
    }

    /// True for targets this host manages (or, for the broker, relays).
    pub fn manages(&self, target: &Target) -> bool {
        match (self.mode, target) {
            (Mode::System, Target::Unit { .. }) => true,
            (Mode::User(uid), Target::Unit { manager: Manager::User(u), .. }) => *u == uid,
            _ => false,
        }
    }

    /// Uids whose user managers this host looks at.
    fn uids(&self) -> Vec<u32> {
        match self.mode {
            Mode::User(uid) => vec![uid],
            Mode::System => self
                .fs()
                .children("/user.slice")
                .unwrap_or_default()
                .iter()
                .filter_map(|p| p.rsplit('/').next()?.strip_prefix("user-")?.strip_suffix(".slice")?.parse().ok())
                .collect(),
        }
    }

    /// Push every `.service`/`.scope` below `cg`, descending into nested slices only.
    fn walk_units(&self, cg: &str, manager: &Manager, out: &mut Vec<WatchedTarget>) {
        let Ok(children) = self.fs().children(cg) else { return };
        for child in children {
            let name = child.rsplit('/').next().unwrap_or_default();
            if name.ends_with(".slice") {
                self.walk_units(&child, manager, out);
            } else if (name.ends_with(".service") || name.ends_with(".scope")) && name != "init.scope" {
                out.push(WatchedTarget {
                    target: Target::Unit { manager: manager.clone(), name: name.to_string() },
                    cgroup: Some(child.clone()),
                });
            }
        }
    }

    /// One walk of the cgroup tree (uncached).
    pub fn scan(&self) -> Vec<WatchedTarget> {
        let fs = self.fs();
        let mut out = Vec::new();
        let slice = |out: &mut Vec<WatchedTarget>, manager: Manager, parent: &str, name: &str| {
            let cg = format!("{}/{name}", parent.trim_end_matches('/'));
            if fs.exists(&cg) {
                out.push(WatchedTarget {
                    target: Target::Unit { manager: manager.clone(), name: name.to_string() },
                    cgroup: Some(cg.clone()),
                });
                self.walk_units(&cg, &manager, out);
            }
        };
        match self.mode {
            Mode::User(uid) => {
                let base = user_manager_cgroup(uid);
                for name in USER_SLICES {
                    slice(&mut out, Manager::User(uid), &base, name);
                }
            }
            Mode::System => {
                self.walk_units("/system.slice", &Manager::System, &mut out);
                if fs.exists("/user.slice") {
                    out.push(WatchedTarget {
                        target: Target::system_unit("user.slice"),
                        cgroup: Some("/user.slice".into()),
                    });
                }
                self.walk_units("/machine.slice", &Manager::System, &mut out);
                slice(&mut out, Manager::System, "/", "lindesk-batch.slice");
                for uid in self.uids().into_iter().filter(|u| self.users.includes(*u)) {
                    let base = user_manager_cgroup(uid);
                    for name in USER_SLICES {
                        slice(&mut out, Manager::User(uid), &base, name);
                    }
                }
            }
        }
        out
    }

    /// The cgroup of a unit target, from the last walk or by looking in the usual places.
    fn unit_cgroup(&self, target: &Target) -> Option<String> {
        let Target::Unit { manager, name } = target else { return None };
        if let Some(w) = lock(&self.cache).as_ref()
            && let Some(cg) = w.targets.iter().find(|w| &w.target == target).and_then(|w| w.cgroup.clone())
            && self.fs().exists(&cg)
        {
            return Some(cg);
        }
        let base = match manager {
            Manager::User(uid) => user_manager_cgroup(*uid),
            Manager::System => {
                let mut quick = vec![format!("/{name}"), format!("/system.slice/{name}")];
                quick.extend(self.uids().iter().map(|u| format!("/user.slice/user-{u}.slice/{name}")));
                if let Some(cg) = quick.into_iter().find(|c| self.fs().exists(c)) {
                    return Some(cg);
                }
                "/".to_string()
            }
        };
        self.find_below(&base, name)
    }

    /// Depth-first search for a cgroup directory called `name` below `cg`, descending only
    /// into slices (so never into units or user managers).
    fn find_below(&self, cg: &str, name: &str) -> Option<String> {
        for child in self.fs().children(cg).ok()? {
            let leaf = child.rsplit('/').next().unwrap_or_default();
            if leaf == name {
                return Some(child);
            }
            if leaf.ends_with(".slice")
                && let Some(found) = self.find_below(&child, name)
            {
                return Some(found);
            }
        }
        None
    }

    /// Pids in a unit's cgroup tree, oldest first (by start time, then pid).
    fn pids_oldest_first(&self, cg: &str) -> Vec<i32> {
        let mut pids = procfs::pids_in_cgroup_tree(&self.roots.cgroup, cg).unwrap_or_default();
        pids.truncate(1024);
        sort_oldest_first(&mut pids, |p| start_time(&self.roots.proc, p));
        pids
    }

    /// When a unit started, for ranking: systemd's `ActiveEnterTimestampMonotonic` when a
    /// client is configured, else its oldest process's start time (both monotonic-ish).
    fn started(&self, w: &WatchedTarget) -> u64 {
        if let (Some(sd), Target::Unit { name, .. }) = (&self.systemd, &w.target)
            && let Ok(t) = lock(sd).property_u64(name, UNIT_IFACE, "ActiveEnterTimestampMonotonic")
            && t > 0
        {
            return t;
        }
        let Some(cg) = w.cgroup.as_deref() else { return 0 };
        self.pids_oldest_first(cg).first().and_then(|p| start_time(&self.roots.proc, *p)).unwrap_or(0)
    }

    /// The process whose environment tells how a unit was launched: systemd's `MainPID` for
    /// services when available, else the oldest process.
    fn main_pid(&self, w: &WatchedTarget) -> Option<i32> {
        if let (Some(sd), Target::Unit { name, .. }) = (&self.systemd, &w.target)
            && name.ends_with(".service")
            && let Some(iface) = unit_interface(name)
            && let Ok(pid) = lock(sd).property(name, iface, "MainPID")
            && let Ok(pid) = u32::try_from(&*pid)
            && pid > 0
        {
            return i32::try_from(pid).ok();
        }
        self.pids_oldest_first(w.cgroup.as_deref()?).first().copied()
    }

    /// Units under the `app.slice` of every user manager this host looks at.
    fn app_units(&self) -> Vec<WatchedTarget> {
        let mut out = Vec::new();
        for uid in self.uids() {
            let cg = format!("{}/app.slice", user_manager_cgroup(uid));
            self.walk_units(&cg, &Manager::User(uid), &mut out);
        }
        out
    }

    fn pseudo_proc(target: &Target, name: &str, cgroup: String) -> ProcInfo {
        ProcInfo {
            pid: 0,
            uid: target.user().unwrap_or(0),
            comm: String::new(),
            exe: None,
            cgroup,
            unit: Some(name.to_string()),
            slice: None,
            app_id: cgroup_path::app_id_from_unit(name),
        }
    }
}

/// Sorts pids by start time (unknown last), then pid. `start` reads `/proc`, so its answer
/// can change while we sort (a process exits, a pid is reused): each pid's key is read
/// exactly once (`sort_by_cached_key`). A key that changes mid-sort makes `sort_by_key`
/// panic ("does not correctly implement a total order"), which took down the engine thread.
fn sort_oldest_first(pids: &mut [i32], mut start: impl FnMut(i32) -> Option<u64>) {
    pids.sort_by_cached_key(|p| (start(*p).unwrap_or(u64::MAX), *p));
}

/// Start time of a process in clock ticks since boot (`/proc/<pid>/stat` field 22).
/// Descendants inspected when collecting an application's units.
const MAX_FAMILY_PROCS: usize = 4096;

/// A process's parent pid, from `/proc/<pid>/stat`.
fn ppid(proc_root: &Path, pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    let rest = stat.get(stat.rfind(')')? + 1..)?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

fn start_time(proc_root: &Path, pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    // `comm` may contain spaces and parentheses: fields resume after the last ')'.
    let rest = stat.get(stat.rfind(')')? + 1..)?;
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// The slice directly containing the last component of a cgroup path.
fn parent_slice(cg: &str) -> Option<String> {
    let mut comps: Vec<&str> = cg.split('/').filter(|c| !c.is_empty()).collect();
    comps.pop()?;
    comps.pop().filter(|c| c.ends_with(".slice")).map(str::to_string)
}

/// Normalise an app id or desktop file name for comparison: basename, no `.desktop`.
fn desktop_id(s: &str) -> &str {
    let base = s.trim().rsplit('/').next().unwrap_or_default();
    base.strip_suffix(".desktop").unwrap_or(base)
}

impl Environment for SysEnvironment {
    fn proc_info(&self, pid: i32) -> Option<ProcInfo> {
        procfs::proc_info(&self.roots, pid).ok()
    }

    fn target_for_pid(&self, pid: i32) -> Option<Target> {
        let cg = procfs::cgroup(&self.roots.proc, pid).ok()??;
        cgroup_path::parse(&cg).target().filter(|t| self.manages(t))
    }

    fn related_targets(&self, pid: i32) -> Vec<Target> {
        // parent → children for every live process, then the units of `pid`'s descendants.
        let mut children: BTreeMap<i32, Vec<i32>> = BTreeMap::new();
        for p in procfs::list_pids(&self.roots.proc).unwrap_or_default() {
            if let Some(pp) = ppid(&self.roots.proc, p) {
                children.entry(pp).or_default().push(p);
            }
        }
        let mut out: Vec<Target> = Vec::new();
        let mut stack = vec![pid];
        let mut seen = 0usize;
        while let Some(p) = stack.pop() {
            seen += 1;
            if seen > MAX_FAMILY_PROCS {
                break;
            }
            for c in children.get(&p).into_iter().flatten() {
                stack.push(*c);
                if let Some(t) = self.target_for_pid(*c)
                    && !out.contains(&t)
                {
                    out.push(t);
                }
            }
        }
        out
    }

    fn target_for_app_id(&self, app_id: &str) -> Option<Target> {
        let needle = desktop_id(app_id);
        if needle.is_empty() {
            return None;
        }
        let units = self.app_units();
        let by_name: Vec<&WatchedTarget> = units
            .iter()
            .filter(|w| match &w.target {
                Target::Unit { name, .. } => {
                    cgroup_path::app_id_from_unit(name).is_some_and(|id| id.eq_ignore_ascii_case(needle))
                }
                _ => false,
            })
            .collect();
        let matches = if by_name.is_empty() {
            units
                .iter()
                .filter(|w| {
                    self.main_pid(w)
                        .and_then(|p| procfs::environ_var(&self.roots.proc, p, DESKTOP_FILE_VAR).ok().flatten())
                        .is_some_and(|f| desktop_id(&f).eq_ignore_ascii_case(needle))
                })
                .collect()
        } else {
            by_name
        };
        matches.into_iter().max_by_key(|w| self.started(w)).map(|w| w.target.clone()).filter(|t| self.manages(t))
    }

    fn watched_targets(&self) -> Vec<WatchedTarget> {
        let mut cache = lock(&self.cache);
        let generation = self.users.generation();
        if let Some(w) = cache.as_ref()
            && w.at.elapsed() < self.ttl
            && w.generation == generation
        {
            return w.targets.clone();
        }
        let targets = self.scan();
        *cache = Some(Walk { at: Instant::now(), generation, targets: targets.clone() });
        targets
    }

    fn representative_proc(&self, target: &Target) -> Option<ProcInfo> {
        match target {
            Target::Process { pid } => self.proc_info(*pid),
            Target::Cgroup { path } => self.pids_oldest_first(path).into_iter().find_map(|p| self.proc_info(p)),
            Target::Unit { name, .. } => {
                let cg = self.unit_cgroup(target)?;
                if name.ends_with(".slice") {
                    return Some(Self::pseudo_proc(target, name, cg));
                }
                let found = self.pids_oldest_first(&cg).into_iter().find_map(|p| self.proc_info(p));
                Some(match found {
                    Some(mut p) => {
                        p.unit = Some(name.clone());
                        p.slice = parent_slice(&cg);
                        p.app_id = p.app_id.or_else(|| cgroup_path::app_id_from_unit(name));
                        p
                    }
                    None => Self::pseudo_proc(target, name, cg),
                })
            }
        }
    }

    fn mem_total(&self) -> Option<u64> {
        let s = std::fs::read_to_string(self.roots.proc.join("meminfo")).ok()?;
        let kb: u64 = s.lines().find_map(|l| l.strip_prefix("MemTotal:"))?.split_whitespace().next()?.parse().ok()?;
        kb.checked_mul(1024)
    }
}

/// Group watched targets by manager for display (used by the real-host test and tools).
pub fn summarize(targets: &[WatchedTarget]) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for w in targets {
        let key = match &w.target {
            Target::Unit { manager: Manager::System, name } => format!("system {}", unit_kind(name)),
            Target::Unit { manager: Manager::User(u), name } => format!("user/{u} {}", unit_kind(name)),
            Target::Cgroup { .. } => "cgroup".into(),
            Target::Process { .. } => "process".into(),
        };
        *m.entry(key).or_default() += 1;
    }
    m
}

fn unit_kind(name: &str) -> &str {
    name.rsplit_once('.').map_or("unit", |(_, k)| k)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: the start time of a pid can change between two reads (exit, reuse) while
    /// the sort runs. Every read here returns a different value; the sort must read each pid
    /// once, not panic, and order by the value it read.
    #[test]
    fn sort_oldest_first_survives_keys_that_change_mid_sort() {
        let n = 1024;
        let mut pids: Vec<i32> = (1..=n).rev().collect();
        let mut reads = std::collections::HashMap::<i32, u32>::new();
        let mut first = std::collections::HashMap::<i32, u64>::new();
        let mut calls = 0u64;
        sort_oldest_first(&mut pids, |p| {
            calls += 1;
            *reads.entry(p).or_default() += 1;
            // Changes on every call; every 7th pid has "exited" (no start time).
            let v = (p as u64 * 7919 + calls * 104_729) % 100_003;
            let v = (p % 7 != 0).then_some(v);
            first.entry(p).or_insert(v.unwrap_or(u64::MAX));
            v
        });
        assert_eq!(pids.len(), n as usize);
        assert!(reads.values().all(|&r| r == 1), "each pid's start time is read once");
        let keys: Vec<(u64, i32)> = pids.iter().map(|p| (first[p], *p)).collect();
        assert!(keys.windows(2).all(|w| w[0] <= w[1]), "sorted by the value read");
        assert!(pids[pids.len() - 1] % 7 == 0, "unknown start times sort last");
    }
    use lindesk_sys::fixture::{self, Fixture};

    const UM: &str = "/user.slice/user-1000.slice/user@1000.service";

    /// A process in `cg` with a start time (clock ticks) and optional desktop file.
    fn process(f: &Fixture, pid: i32, uid: u32, comm: &str, cg: &str, start: u64, desktop: Option<&str>) {
        f.process(pid, comm)
            .file(&format!("proc/{pid}/status"), format!("Name:\t{comm}\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"))
            .file(&format!("proc/{pid}/cgroup"), format!("0::{cg}\n"))
            .file(
                &format!("proc/{pid}/stat"),
                format!("{pid} ({comm} (x)) S 1 {pid} {pid} 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 {start} 1000 10\n"),
            );
        let mut env = String::from("HOME=/home/u\0");
        if let Some(d) = desktop {
            env.push_str(&format!("{DESKTOP_FILE_VAR}={d}\0"));
        }
        f.file(&format!("proc/{pid}/environ"), env);
        let procs = f.root().join(format!("sys/fs/cgroup{cg}/cgroup.procs"));
        let mut cur = std::fs::read_to_string(&procs).unwrap_or_default();
        cur.push_str(&format!("{pid}\n"));
        f.cgroup(cg, &[("cgroup.procs", cur.trim_end())]);
    }

    fn host() -> (tempfile::TempDir, Fixture) {
        let dir = crate::actuators::test_dir();
        fixture::pop_os(dir.path());
        let f = Fixture::new(dir.path());
        f.file("proc/meminfo", "MemTotal:       32617072 kB\nMemFree:         1000 kB\n");
        // User manager.
        process(&f, 1500, 1000, "systemd", &format!("{UM}/init.scope"), 100, None);
        process(
            &f,
            2001,
            1000,
            "firefox",
            &format!("{UM}/app.slice/app-gnome-org.mozilla.firefox-1234.scope"),
            500,
            None,
        );
        // Pid order is not age order: 1999 started after 2001.
        process(
            &f,
            1999,
            1000,
            "Web Content",
            &format!("{UM}/app.slice/app-gnome-org.mozilla.firefox-1234.scope"),
            900,
            None,
        );
        process(
            &f,
            2100,
            1000,
            "firefox",
            &format!("{UM}/app.slice/app-gnome-org.mozilla.firefox-5678.scope"),
            700,
            None,
        );
        process(
            &f,
            2200,
            1000,
            "bash",
            &format!("{UM}/app.slice/app-org.gnome.Terminal.slice/vte-spawn-abc.scope"),
            800,
            None,
        );
        process(
            &f,
            2300,
            1000,
            "gimp",
            &format!("{UM}/app.slice/app-gnome-x-1.scope"),
            600,
            Some("/usr/share/applications/org.gimp.GIMP.desktop"),
        );
        process(
            &f,
            2400,
            1000,
            "gnome-shell",
            &format!("{UM}/session.slice/org.gnome.Shell@wayland.service"),
            200,
            None,
        );
        process(
            &f,
            2500,
            1000,
            "tracker-miner-f",
            &format!("{UM}/background.slice/tracker-miner-fs-3.service"),
            300,
            None,
        );
        f.dir(&format!("sys/fs/cgroup{UM}/app.slice/app-empty-9.scope"));
        process(&f, 2600, 1000, "bash", "/user.slice/user-1000.slice/session-2.scope", 150, None);
        // Another user.
        process(
            &f,
            3001,
            1001,
            "firefox",
            "/user.slice/user-1001.slice/user@1001.service/app.slice/app-gnome-org.mozilla.firefox-1.scope",
            1000,
            None,
        );
        // System.
        process(&f, 400, 0, "sshd", "/system.slice/sshd.service", 50, None);
        process(&f, 410, 0, "containerd-shim", "/system.slice/docker-0123abcd.scope", 60, None);
        process(&f, 420, 0, "agetty", "/system.slice/system-getty.slice/getty@tty1.service", 70, None);
        process(&f, 430, 0, "qemu", "/machine.slice/libpod-xyz.scope", 80, None);
        (dir, f)
    }

    fn env(f: &Fixture, mode: Mode) -> SysEnvironment {
        SysEnvironment::new(f.roots(), mode)
    }

    fn names(ws: &[WatchedTarget]) -> Vec<String> {
        ws.iter().map(|w| w.target.to_string()).collect()
    }

    #[test]
    fn user_mode_watches_own_slices_and_units() {
        let (_d, f) = host();
        let e = env(&f, Mode::User(1000));
        let w = e.watched_targets();
        let n = names(&w);
        for want in [
            "user/1000:session.slice",
            "user/1000:app.slice",
            "user/1000:background.slice",
            "user/1000:app-gnome-org.mozilla.firefox-1234.scope",
            "user/1000:vte-spawn-abc.scope",
            "user/1000:org.gnome.Shell@wayland.service",
            "user/1000:tracker-miner-fs-3.service",
            "user/1000:app-empty-9.scope",
        ] {
            assert!(n.contains(&want.to_string()), "missing {want} in {n:?}");
        }
        assert!(!n.iter().any(|x| x.contains("init.scope") || x.starts_with("system") || x.contains("1001")));
        assert!(!n.iter().any(|x| x.contains("app-org.gnome.Terminal.slice")), "nested slices are walked, not watched");
        let vte = w.iter().find(|x| x.target == Target::user_unit(1000, "vte-spawn-abc.scope")).unwrap();
        assert_eq!(
            vte.cgroup.as_deref(),
            Some(
                "/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.gnome.Terminal.slice/vte-spawn-abc.scope"
            )
        );
        assert!(!n.iter().any(|x| x.contains("lindesk-batch")));
        f.dir(&format!("sys/fs/cgroup{UM}/lindesk-batch.slice/run-u1.service"));
        let n = names(&env(&f, Mode::User(1000)).watched_targets());
        assert!(n.contains(&"user/1000:lindesk-batch.slice".into()) && n.contains(&"user/1000:run-u1.service".into()));
    }

    #[test]
    fn system_mode_watches_system_units() {
        let (_d, f) = host();
        let n = names(&env(&f, Mode::System).watched_targets());
        for want in [
            "system:sshd.service",
            "system:docker-0123abcd.scope",
            "system:getty@tty1.service",
            "system:user.slice",
            "system:libpod-xyz.scope",
        ] {
            assert!(n.contains(&want.to_string()), "missing {want} in {n:?}");
        }
        assert!(!n.iter().any(|x| x.starts_with("user/") || x.contains("system.slice") || x.contains("session-2")));
    }

    #[test]
    fn system_mode_watches_selected_users() {
        let (_d, f) = host();
        let watch = UserWatch::new();
        let e = env(&f, Mode::System).with_users(watch.clone()).with_cache_ttl(Duration::from_secs(60));
        let n = names(&e.watched_targets());
        assert!(!n.iter().any(|x| x.starts_with("user/")), "no users by default: {n:?}");

        assert!(watch.add(1000));
        assert!(!watch.add(1000), "already watched");
        let n = names(&e.watched_targets());
        for want in [
            "system:sshd.service",
            "user/1000:session.slice",
            "user/1000:app.slice",
            "user/1000:background.slice",
            "user/1000:app-gnome-org.mozilla.firefox-1234.scope",
            "user/1000:vte-spawn-abc.scope",
            "user/1000:tracker-miner-fs-3.service",
        ] {
            assert!(n.contains(&want.to_string()), "missing {want} in {n:?} (cache must be bypassed)");
        }
        assert!(!n.iter().any(|x| x.contains("user/1001") || x.contains("init.scope")));

        assert!(watch.set_all(true));
        let n = names(&e.watched_targets());
        assert!(n.contains(&"user/1001:app-gnome-org.mozilla.firefox-1.scope".to_string()), "{n:?}");
        assert!(watch.includes(4242) && watch.is_all());

        watch.set_all(false);
        assert!(watch.remove(1000));
        assert!(watch.uids().is_empty());
        let n = names(&e.watched_targets());
        assert!(!n.iter().any(|x| x.starts_with("user/")), "{n:?}");
        assert_eq!(UserWatch::with_uids([3, 1]).uids(), vec![1, 3]);
        assert!(UserWatch::all_users().includes(7));

        // User mode ignores the selection.
        let u = env(&f, Mode::User(1000)).with_users(UserWatch::all_users());
        assert!(!names(&u.watched_targets()).iter().any(|x| x.contains("1001")));
    }

    #[test]
    fn walk_is_cached() {
        let (_d, f) = host();
        let e = env(&f, Mode::User(1000)).with_cache_ttl(Duration::from_secs(60));
        let before = e.watched_targets().len();
        f.dir(&format!("sys/fs/cgroup{UM}/app.slice/app-new-1.scope"));
        assert_eq!(e.watched_targets().len(), before, "cached");
        assert_eq!(e.scan().len(), before + 1);
        let fresh = env(&f, Mode::User(1000)).with_cache_ttl(Duration::ZERO);
        assert_eq!(fresh.watched_targets().len(), before + 1);
    }

    #[test]
    fn pids_map_to_managed_targets_only() {
        let (_d, f) = host();
        let user = env(&f, Mode::User(1000));
        let ff = Target::user_unit(1000, "app-gnome-org.mozilla.firefox-1234.scope");
        assert_eq!(user.target_for_pid(1999), Some(ff.clone()));
        assert_eq!(user.target_for_pid(2200), Some(Target::user_unit(1000, "vte-spawn-abc.scope")));
        assert_eq!(user.target_for_pid(400), None, "system unit");
        assert_eq!(user.target_for_pid(3001), None, "another user's unit");
        assert_eq!(user.target_for_pid(2600), None, "session scope is a system unit");
        assert_eq!(user.target_for_pid(99999), None, "no such pid");
        let sys = env(&f, Mode::System);
        assert_eq!(sys.target_for_pid(400), Some(Target::system_unit("sshd.service")));
        assert_eq!(sys.target_for_pid(2600), Some(Target::system_unit("session-2.scope")));
        assert_eq!(sys.target_for_pid(1999), Some(ff), "user units are relayed by the broker");
        let p = user.proc_info(1999).unwrap();
        assert_eq!((p.comm.as_str(), p.uid, p.app_id.as_deref()), ("Web Content", 1000, Some("org.mozilla.firefox")));
    }

    #[test]
    fn app_ids_by_unit_name_or_desktop_file() {
        let (_d, f) = host();
        let user = env(&f, Mode::User(1000));
        // Two firefox scopes: the most recently started one (oldest process started at 700
        // vs 500) wins; matching ignores case and a `.desktop` suffix.
        let newest = Some(Target::user_unit(1000, "app-gnome-org.mozilla.firefox-5678.scope"));
        assert_eq!(user.target_for_app_id("org.mozilla.firefox"), newest);
        assert_eq!(user.target_for_app_id("ORG.Mozilla.Firefox.desktop"), newest);
        // Launched through a desktop file whose id is not in the unit name.
        assert_eq!(user.target_for_app_id("org.gimp.GIMP"), Some(Target::user_unit(1000, "app-gnome-x-1.scope")));
        assert_eq!(user.target_for_app_id("org.example.Nope"), None);
        assert_eq!(user.target_for_app_id(""), None);
        // The broker sees every user's apps: user 1001's firefox started last.
        let sys = env(&f, Mode::System);
        assert_eq!(
            sys.target_for_app_id("org.mozilla.firefox"),
            Some(Target::user_unit(1001, "app-gnome-org.mozilla.firefox-1.scope"))
        );
    }

    #[test]
    fn representative_processes() {
        let (_d, f) = host();
        let e = env(&f, Mode::User(1000));
        e.watched_targets();
        let ff = Target::user_unit(1000, "app-gnome-org.mozilla.firefox-1234.scope");
        let p = e.representative_proc(&ff).unwrap();
        assert_eq!(p.pid, 2001, "oldest by start time, not lowest pid");
        assert_eq!(p.unit.as_deref(), Some("app-gnome-org.mozilla.firefox-1234.scope"));
        assert_eq!(p.slice.as_deref(), Some("app.slice"));
        let vte = e.representative_proc(&Target::user_unit(1000, "vte-spawn-abc.scope")).unwrap();
        assert_eq!((vte.pid, vte.slice.as_deref()), (2200, Some("app-org.gnome.Terminal.slice")));

        let slice = e.representative_proc(&Target::user_unit(1000, "background.slice")).unwrap();
        assert_eq!((slice.pid, slice.comm.as_str(), slice.uid), (0, "", 1000));
        assert_eq!(slice.unit.as_deref(), Some("background.slice"));
        assert_eq!(slice.cgroup, format!("{UM}/background.slice"));
        let empty = e.representative_proc(&Target::user_unit(1000, "app-empty-9.scope")).unwrap();
        assert_eq!((empty.pid, empty.app_id.as_deref()), (0, Some("empty")));
        assert_eq!(e.representative_proc(&Target::user_unit(1000, "nope.scope")), None);
        assert_eq!(e.representative_proc(&Target::Process { pid: 2500 }).unwrap().comm, "tracker-miner-f");
        assert_eq!(e.representative_proc(&Target::Cgroup { path: "/system.slice".into() }).unwrap().pid, 400);

        // System units are found without a prior walk.
        let sys = env(&f, Mode::System);
        assert_eq!(sys.representative_proc(&Target::system_unit("getty@tty1.service")).unwrap().pid, 420);
        assert_eq!(sys.representative_proc(&Target::system_unit("session-2.scope")).unwrap().pid, 2600);
        let us = sys.representative_proc(&Target::system_unit("user.slice")).unwrap();
        assert_eq!((us.pid, us.cgroup.as_str()), (0, "/user.slice"));
    }

    #[test]
    fn meminfo_and_helpers() {
        let (_d, f) = host();
        assert_eq!(env(&f, Mode::System).mem_total(), Some(32_617_072 * 1024));
        assert_eq!(start_time(&f.roots().proc, 1999), Some(900));
        assert_eq!(start_time(&f.roots().proc, 1), None);
        assert_eq!(parent_slice("/a.slice/b.slice/c.scope").as_deref(), Some("b.slice"));
        assert_eq!(parent_slice("/x/c.scope"), None);
        assert_eq!(desktop_id("/usr/share/applications/org.gimp.GIMP.desktop"), "org.gimp.GIMP");
        let s = summarize(&env(&f, Mode::User(1000)).watched_targets());
        assert_eq!(s["user/1000 slice"], 3);
    }

    /// Runs in user mode against this machine. `LINDESK_SHELL_PID` names the pid to look up
    /// (defaults to the parent of the test process).
    #[test]
    #[ignore = "reads the real host"]
    fn real_host_user_mode() {
        let uid = rustix::process::getuid().as_raw();
        let e = SysEnvironment::new(Roots::host(), Mode::User(uid))
            .with_systemd(SystemdClient::new(crate::actuators::Bus::User, uid));
        let t = Instant::now();
        let w = e.watched_targets();
        let first = t.elapsed();
        let t = Instant::now();
        let again = e.watched_targets();
        let cached = t.elapsed();
        println!("watched targets: {} (walk {first:?}, cached {cached:?})", w.len());
        println!("by kind: {:?}", summarize(&w));
        for x in w.iter().take(12) {
            println!("  {:<70} {}", x.target.to_string(), x.cgroup.as_deref().unwrap_or("-"));
        }
        assert_eq!(w, again);
        let shell: i32 = std::env::var("LINDESK_SHELL_PID")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| rustix::process::getppid().map_or(1, |p| p.as_raw_nonzero().get()));
        let own = std::process::id() as i32;
        for (label, pid) in [("shell", shell), ("test process", own)] {
            let target = e.target_for_pid(pid);
            println!("{label} pid {pid}: target {:?}", target.as_ref().map(ToString::to_string));
            println!("  proc_info: {:?}", e.proc_info(pid));
            if let Some(t) = &target {
                println!("  representative: {:?}", e.representative_proc(t));
            }
        }
        if let Some(app) = w.iter().find_map(|x| match &x.target {
            Target::Unit { name, .. } => cgroup_path::app_id_from_unit(name),
            _ => None,
        }) {
            let t = Instant::now();
            let found = e.target_for_app_id(&app);
            println!("app id {app:?} -> {:?} ({:?})", found.map(|t| t.to_string()), t.elapsed());
        }
        println!("mem_total: {:?}", e.mem_total());
        assert!(!w.is_empty());
    }

    #[test]
    fn an_app_split_over_two_scopes_is_one_family() {
        let (_dir, f) = host();
        // Like Wavebox/Chromium: the window's process in one scope, its renderers in another.
        let main_scope = format!("{UM}/app.slice/app-org.chromium.Chromium-3000.scope");
        let child_scope = format!("{UM}/app.slice/app-gnome-wavebox-3000.scope");
        let other_scope = format!("{UM}/app.slice/app-gnome-editor-3100.scope");
        process(&f, 3000, 1000, "wavebox", &main_scope, 1000, None);
        process(&f, 3001, 1000, "wavebox", &child_scope, 1001, None);
        process(&f, 3002, 1000, "wavebox", &child_scope, 1002, None);
        process(&f, 3100, 1000, "editor", &other_scope, 1003, None);
        let stat = |pid: i32, ppid: i32| {
            f.file(
                &format!("proc/{pid}/stat"),
                format!("{pid} (wave box) S {ppid} {pid} {pid} 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 1000 1 1\n"),
            );
        };
        stat(3001, 3000); // renderer: child of the main process
        stat(3002, 3001); // grandchild
        let env = SysEnvironment::new(Roots::under(f.root()), Mode::User(1000));
        let main = env.target_for_pid(3000).unwrap();
        let family = env.related_targets(3000);
        assert_eq!(family, vec![Target::user_unit(1000, "app-gnome-wavebox-3000.scope")]);
        assert!(!family.contains(&main));
        assert!(env.related_targets(3100).is_empty(), "an app in one scope has no other units");
    }
}
