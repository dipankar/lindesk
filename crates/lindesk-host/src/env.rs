//! The engine's view of the machine, injected so tests and `lindeskctl simulate` can fake it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use lindesk_core::cgroup_path;
use lindesk_plugin_api::{Manager, ProcInfo, Target, WatchedTarget};

/// Everything the engine needs to know about processes and units. Daemons implement it
/// over `/proc` and systemd; tests and `simulate` use [`MemoryEnvironment`].
///
/// The engine calls [`Environment::watched_targets`] once per step and the per-target
/// methods only for new targets, focus changes and a periodic refresh, so implementations
/// may do real I/O but should cache where cheap.
pub trait Environment: Send {
    /// Information about a live process.
    fn proc_info(&self, pid: i32) -> Option<ProcInfo>;

    /// The target that owns `pid` (via its cgroup path).
    fn target_for_pid(&self, pid: i32) -> Option<Target>;

    /// The target running a desktop application (`app-*-<app_id>-*.scope` etc).
    fn target_for_app_id(&self, app_id: &str) -> Option<Target>;

    /// Units the host manages and wants telemetry for.
    fn watched_targets(&self) -> Vec<WatchedTarget>;

    /// The process classifiers should see for a target: its main or oldest pid, or for a
    /// slice without processes a pseudo-process with `pid = 0` and `unit` set.
    fn representative_proc(&self, target: &Target) -> Option<ProcInfo>;

    /// Total RAM in bytes (used by safety caps).
    fn mem_total(&self) -> Option<u64>;
}

impl<E: Environment + Sync + ?Sized> Environment for Arc<E> {
    fn proc_info(&self, pid: i32) -> Option<ProcInfo> {
        (**self).proc_info(pid)
    }
    fn target_for_pid(&self, pid: i32) -> Option<Target> {
        (**self).target_for_pid(pid)
    }
    fn target_for_app_id(&self, app_id: &str) -> Option<Target> {
        (**self).target_for_app_id(app_id)
    }
    fn watched_targets(&self) -> Vec<WatchedTarget> {
        (**self).watched_targets()
    }
    fn representative_proc(&self, target: &Target) -> Option<ProcInfo> {
        (**self).representative_proc(target)
    }
    fn mem_total(&self) -> Option<u64> {
        (**self).mem_total()
    }
}

/// The target a cgroup path belongs to: its deepest unit, else its slice, else the raw cgroup.
pub fn target_for_cgroup(path: &str) -> Target {
    // A path ending in a slice (no service/scope below it) names that slice.
    let mut user = None;
    let mut last_slice = None;
    for comp in path.split('/').filter(|c| !c.is_empty()) {
        if let Some(uid) =
            comp.strip_prefix("user@").and_then(|r| r.strip_suffix(".service")).and_then(|u| u.parse().ok())
        {
            user = Some(uid);
            last_slice = None;
        } else if comp.ends_with(".slice") {
            last_slice = Some(comp);
        } else {
            last_slice = None;
        }
    }
    match last_slice {
        Some(name) => Target::Unit { manager: user.map_or(Manager::System, Manager::User), name: name.to_string() },
        None => cgroup_path::parse(path).target().unwrap_or_else(|| Target::Cgroup { path: path.to_string() }),
    }
}

#[derive(Default, Debug)]
struct MemState {
    procs: BTreeMap<i32, ProcInfo>,
    watched: Vec<WatchedTarget>,
    mem_total: Option<u64>,
}

/// A mutable in-memory [`Environment`]. Clones share state, so a test can keep one handle
/// and move another into the engine.
///
/// Processes map to targets through their cgroup path, exactly like the real thing
/// (`/user.slice/user-1000.slice/user@1000.service/app.slice/app-foo-1.scope` →
/// `user/1000:app-foo-1.scope`).
#[derive(Clone, Default, Debug)]
pub struct MemoryEnvironment {
    state: Arc<Mutex<MemState>>,
}

impl MemoryEnvironment {
    pub fn new() -> Self {
        Self::default()
    }

    fn st(&self) -> MutexGuard<'_, MemState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Add or replace a process. `unit`/`slice` are filled from the cgroup path if empty.
    pub fn add_proc(&self, mut p: ProcInfo) {
        let info = cgroup_path::parse(&p.cgroup);
        if p.unit.is_none() {
            p.unit = info.unit.clone();
        }
        if p.slice.is_none() {
            p.slice = info.slice.clone();
        }
        if p.app_id.is_none() {
            p.app_id = p.unit.as_deref().and_then(cgroup_path::app_id_from_unit);
        }
        self.st().procs.insert(p.pid, p);
    }

    /// Convenience: a process `comm` with `pid` in `cgroup`.
    pub fn spawn(&self, pid: i32, comm: &str, cgroup: &str) {
        self.add_proc(ProcInfo {
            pid,
            uid: cgroup_path::parse(cgroup).user.unwrap_or(0),
            comm: comm.into(),
            exe: Some(format!("/usr/bin/{comm}")),
            cgroup: cgroup.into(),
            ..Default::default()
        });
    }

    pub fn remove_proc(&self, pid: i32) {
        self.st().procs.remove(&pid);
    }

    /// Watch a target (with its cgroup path, so sensors can report usage for it).
    pub fn watch(&self, target: Target, cgroup: Option<&str>) {
        let mut st = self.st();
        st.watched.retain(|w| w.target != target);
        st.watched.push(WatchedTarget { target, cgroup: cgroup.map(str::to_string) });
    }

    /// Watch the unit that a cgroup path names and return its target.
    pub fn watch_cgroup(&self, cgroup: &str) -> Target {
        let t = target_for_cgroup(cgroup);
        self.watch(t.clone(), Some(cgroup));
        t
    }

    pub fn unwatch(&self, target: &Target) {
        self.st().watched.retain(|w| &w.target != target);
    }

    pub fn set_mem_total(&self, bytes: Option<u64>) {
        self.st().mem_total = bytes;
    }
}

impl Environment for MemoryEnvironment {
    fn proc_info(&self, pid: i32) -> Option<ProcInfo> {
        self.st().procs.get(&pid).cloned()
    }

    fn target_for_pid(&self, pid: i32) -> Option<Target> {
        self.st().procs.get(&pid).map(|p| target_for_cgroup(&p.cgroup))
    }

    fn target_for_app_id(&self, app_id: &str) -> Option<Target> {
        let st = self.st();
        st.procs.values().find(|p| p.app_id.as_deref() == Some(app_id)).map(|p| target_for_cgroup(&p.cgroup)).or_else(
            || {
                st.watched.iter().find_map(|w| match &w.target {
                    Target::Unit { name, .. } if cgroup_path::app_id_from_unit(name).as_deref() == Some(app_id) => {
                        Some(w.target.clone())
                    }
                    _ => None,
                })
            },
        )
    }

    fn watched_targets(&self) -> Vec<WatchedTarget> {
        self.st().watched.clone()
    }

    fn representative_proc(&self, target: &Target) -> Option<ProcInfo> {
        let st = self.st();
        if let Target::Process { pid } = target {
            return st.procs.get(pid).cloned();
        }
        // BTreeMap iterates by pid: the first match is the oldest-looking process.
        if let Some(p) = st.procs.values().find(|p| &target_for_cgroup(&p.cgroup) == target) {
            return Some(p.clone());
        }
        let w = st.watched.iter().find(|w| &w.target == target)?;
        match target {
            Target::Unit { name, .. } => Some(ProcInfo {
                pid: 0,
                uid: target.user().unwrap_or(0),
                comm: String::new(),
                exe: None,
                cgroup: w.cgroup.clone().unwrap_or_default(),
                unit: Some(name.clone()),
                slice: None,
                app_id: cgroup_path::app_id_from_unit(name),
            }),
            _ => None,
        }
    }

    fn mem_total(&self) -> Option<u64> {
        self.st().mem_total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FF: &str = "/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-org.mozilla.firefox-1234.scope";

    #[test]
    fn maps_pids_and_app_ids() {
        let env = MemoryEnvironment::new();
        env.spawn(10, "firefox", FF);
        let t = Target::user_unit(1000, "app-gnome-org.mozilla.firefox-1234.scope");
        assert_eq!(env.target_for_pid(10), Some(t.clone()));
        assert_eq!(env.target_for_app_id("org.mozilla.firefox"), Some(t.clone()));
        assert_eq!(env.representative_proc(&t).unwrap().pid, 10);
        assert_eq!(env.proc_info(10).unwrap().unit.as_deref(), Some("app-gnome-org.mozilla.firefox-1234.scope"));
    }

    #[test]
    fn empty_slice_gets_pseudo_proc() {
        let env = MemoryEnvironment::new();
        let t = env.watch_cgroup("/system.slice/background.slice");
        assert_eq!(t, Target::system_unit("background.slice"));
        let p = env.representative_proc(&t).unwrap();
        assert_eq!((p.pid, p.comm.as_str(), p.unit.as_deref()), (0, "", Some("background.slice")));
        assert_eq!(
            target_for_cgroup("/user.slice/user-1000.slice/user@1000.service/app.slice"),
            Target::user_unit(1000, "app.slice")
        );
        assert_eq!(target_for_cgroup("/"), Target::Cgroup { path: "/".into() });
    }
}
