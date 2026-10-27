//! Per-thread priorities inside the app in use (`[plugins.thread-qos]`).
//!
//! lindesk's classes apply to whole units, but inside an app not every thread is equal:
//! a browser has a compositor and an I/O thread a person is waiting on, and a pool of
//! twenty workers running scripts and decoding images. When the pool is busy, the kernel
//! gives the compositor one share in twenty-odd. macOS avoids that because apps tag their
//! threads with a QoS class. On Linux nothing tags them, but browser engines name them, and
//! the names are enough: this component recognises an engine by its marker threads, and
//! while that app has focus raises its UI threads (nice −4 by default) and lowers the ones
//! that only keep house. Niceness competes inside the app's own cgroup, so this moves CPU
//! between the app's threads and takes nothing from anyone else.
//!
//! Everything is put back when focus moves on, when lindesk is paused or stops, and by
//! `lindeskd --restore` after a crash (what was changed is kept in
//! `<state-dir>/threads.json`). A thread whose niceness isn't the default is left alone:
//! the app chose that. Needs `CAP_SYS_NICE`, so it runs in the broker.
//!
//! ```toml
//! [plugins.thread-qos]
//! enabled = true
//! config = { ui_nice = -4, background_nice = 5, refresh = "5s" }
//!
//! # More engines, consulted before the built-in ones:
//! [[plugins.thread-qos.config.engines]]
//! name = "my-toolkit"
//! markers = ["MyIOThread"]            # a process with a thread of this name is one
//! ui = ["MyRenderThread", "MyIO*"]    # exact names, or a prefix with *
//! background = ["MyIndexer*"]
//! main_is_ui = true
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use lindesk_core::config::dur;
use lindesk_host::{Engine, HostSender};
use lindesk_plugin_api::Target;
use lindesk_sys::threads::{self, Thread};
use lindesk_sys::{Roots, procfs, syscalls};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Config section / plugin name.
pub const NAME: &str = "thread-qos";
/// Name of the notice in `lindeskctl events`.
pub const NOTICE: &str = "thread-qos.applied";
/// File name inside the state directory.
pub const FILE: &str = "threads.json";
/// Processes looked at per app.
const MAX_PROCESSES: usize = 256;
/// The same app is announced in the events at most this often.
const ANNOUNCE_EVERY: Duration = Duration::from_secs(600);

/// How to recognise a program's threads.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EngineRule {
    pub name: String,
    /// A process with a thread matching one of these is this engine.
    pub markers: Vec<String>,
    /// Threads a person is waiting on.
    #[serde(default)]
    pub ui: Vec<String>,
    /// Threads that only keep house.
    #[serde(default)]
    pub background: Vec<String>,
    /// The process's main thread (the one whose tid is the pid) is a UI thread.
    #[serde(default = "yes")]
    pub main_is_ui: bool,
}

fn yes() -> bool {
    true
}

fn rule(name: &str, markers: &[&str], ui: &[&str], background: &[&str]) -> EngineRule {
    let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
    EngineRule { name: name.into(), markers: own(markers), ui: own(ui), background: own(background), main_is_ui: true }
}

/// The engines known out of the box. Chromium covers Chrome, Edge, Brave, Wavebox and every
/// Electron app; its worker pool is `ThreadPoolForeg…` and stays where it is.
pub fn builtin_engines() -> Vec<EngineRule> {
    vec![
        rule(
            "chromium",
            &["Chrome_IOThread", "Chrome_ChildIOT"],
            &["Compositor", "VizCompositorTh", "Chrome_IOThread", "Chrome_ChildIOT", "CompositorTileW*", "CrGpuMain"],
            &["HangWatcher", "MemoryInfra", "PerfettoTrace", "ThreadPoolBackg*"],
        ),
        rule(
            "gecko",
            &["IPC I/O Parent", "IPC I/O Child", "Gecko_IOThread"],
            &["Compositor", "Renderer", "WRRenderBackend*", "IPC I/O Parent", "IPC I/O Child", "Socket Thread"],
            &["BackgroundThre*", "BgIOThreadPool*"],
        ),
    ]
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ThreadQosConfig {
    /// Niceness of the focused app's UI threads. Never below `[safety] nice_min`.
    pub ui_nice: i8,
    /// Niceness of its housekeeping threads.
    pub background_nice: i8,
    /// How often the focused app's threads are looked at again (they come and go).
    #[serde(with = "dur")]
    pub refresh: Duration,
    /// Extra engines, consulted before the built-in ones.
    pub engines: Vec<EngineRule>,
}

impl Default for ThreadQosConfig {
    fn default() -> Self {
        ThreadQosConfig { ui_nice: -4, background_nice: 5, refresh: Duration::from_secs(5), engines: Vec::new() }
    }
}

impl ThreadQosConfig {
    /// Parse the plugin's JSON config; `null` yields the defaults. `nice_min` is the safety cap.
    pub fn from_json(v: &serde_json::Value, nice_min: i8) -> Result<Self, String> {
        let mut cfg: ThreadQosConfig =
            if v.is_null() { Self::default() } else { serde_json::from_value(v.clone()).map_err(|e| e.to_string())? };
        if !(-20..=0).contains(&cfg.ui_nice) {
            return Err(format!("ui_nice = {} must be in -20..=0", cfg.ui_nice));
        }
        if !(0..=19).contains(&cfg.background_nice) {
            return Err(format!("background_nice = {} must be in 0..=19", cfg.background_nice));
        }
        if cfg.refresh < Duration::from_secs(1) {
            return Err("refresh must be at least 1s".into());
        }
        if cfg.engines.iter().any(|e| e.markers.is_empty()) {
            return Err("an engine needs at least one marker thread name".into());
        }
        cfg.ui_nice = cfg.ui_nice.max(nice_min);
        cfg.engines.extend(builtin_engines());
        Ok(cfg)
    }
}

/// What a thread is to the person using the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Ui,
    Background,
}

/// The roles of a process's threads: the first engine one of whose markers it has decides.
/// Threads of no particular role are not listed. `None` when no engine matches.
pub fn roles<'r>(pid: i32, threads: &[Thread], engines: &'r [EngineRule]) -> Option<(&'r str, Vec<(i32, Role)>)> {
    let any = |patterns: &[String], name: &str| patterns.iter().any(|p| threads::name_matches(p, name));
    let engine = engines.iter().find(|e| threads.iter().any(|t| any(&e.markers, &t.name)))?;
    let out = threads
        .iter()
        .filter_map(|t| {
            if (engine.main_is_ui && t.tid == pid) || any(&engine.ui, &t.name) {
                Some((t.tid, Role::Ui))
            } else if any(&engine.background, &t.name) {
                Some((t.tid, Role::Background))
            } else {
                None
            }
        })
        .collect();
    Some((&engine.name, out))
}

/// One thread whose niceness was changed.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Changed {
    pub pid: i32,
    pub tid: i32,
    /// What it was.
    pub original: i8,
    /// What it was set to.
    pub set: i8,
}

/// The syscalls, so the logic can be tested without privileges.
pub trait Nice: Send {
    fn get(&self, tid: i32) -> std::io::Result<i8>;
    fn set(&self, tid: i32, nice: i8) -> std::io::Result<()>;
}

/// The real thing.
pub struct KernelNice;

impl Nice for KernelNice {
    fn get(&self, tid: i32) -> std::io::Result<i8> {
        syscalls::get_nice(tid)
    }
    fn set(&self, tid: i32, nice: i8) -> std::io::Result<()> {
        syscalls::set_nice(tid, nice)
    }
}

/// What is currently changed, and how to undo it.
struct Book<N: Nice> {
    nice: N,
    proc_root: PathBuf,
    file: PathBuf,
    changed: BTreeMap<i32, Changed>,
}

impl<N: Nice> Book<N> {
    fn new(nice: N, proc_root: &Path, state_dir: &Path) -> Self {
        Book { nice, proc_root: proc_root.to_path_buf(), file: state_dir.join(FILE), changed: BTreeMap::new() }
    }

    fn load(&mut self) -> std::io::Result<()> {
        let raw = match std::fs::read(&self.file) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let saved: Vec<Changed> = serde_json::from_slice(&raw).map_err(std::io::Error::other)?;
        self.changed = saved.into_iter().map(|c| (c.tid, c)).collect();
        Ok(())
    }

    fn save(&self) -> std::io::Result<()> {
        if self.changed.is_empty() {
            return match std::fs::remove_file(&self.file) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let all: Vec<&Changed> = self.changed.values().collect();
        lindesk_core::reconcile::write_journal(&self.file, &serde_json::to_vec(&all)?)
    }

    /// Puts one thread back, if it is still the thread that was changed and nobody else
    /// has touched its niceness since.
    fn undo(&self, c: &Changed) -> bool {
        if !threads::belongs_to(&self.proc_root, c.pid, c.tid) {
            return true;
        }
        match self.nice.get(c.tid) {
            Ok(n) if n == c.set => self.nice.set(c.tid, c.original).is_ok(),
            Ok(_) => true, // Another actor changed it; leave that choice alone.
            Err(_) => false,
        }
    }

    fn undo_all(&mut self) -> usize {
        let restored: Vec<_> = self.changed.iter().filter(|(_, c)| self.undo(c)).map(|(tid, _)| *tid).collect();
        for tid in &restored {
            self.changed.remove(tid);
        }
        if let Err(e) = self.save() {
            tracing::warn!("thread-qos: saving recovery state: {e}");
        }
        restored.len()
    }

    /// Makes the threads of `pids` match their roles. Returns (engine, UI threads, background
    /// threads) now changed, or the error that says this can't work here.
    fn apply(&mut self, cfg: &ThreadQosConfig, pids: &[i32]) -> std::io::Result<(Option<String>, usize, usize)> {
        let mut engine = None;
        let mut wanted: BTreeMap<i32, (i32, i8)> = BTreeMap::new();
        for pid in pids {
            let list = threads::list(&self.proc_root, *pid);
            if let Some((name, roles)) = roles(*pid, &list, &cfg.engines) {
                engine.get_or_insert_with(|| name.to_string());
                for (tid, role) in roles {
                    let nice = if role == Role::Ui { cfg.ui_nice } else { cfg.background_nice };
                    wanted.insert(tid, (*pid, nice));
                }
            }
        }
        // Threads that exited, or changed role (renamed), go back first.
        let stale: Vec<i32> = self
            .changed
            .iter()
            .filter(|(tid, c)| wanted.get(tid).map(|w| w.1) != Some(c.set))
            .map(|(t, _)| *t)
            .collect();
        let mut dirty = !stale.is_empty();
        for tid in stale {
            if self.changed.get(&tid).is_some_and(|c| self.undo(c)) {
                self.changed.remove(&tid);
            }
        }
        let mut pending = Vec::new();
        for (tid, (pid, nice)) in &wanted {
            if self.changed.contains_key(tid) || *nice == 0 || self.nice.get(*tid).ok() != Some(0) {
                continue;
            }
            self.changed.insert(*tid, Changed { pid: *pid, tid: *tid, original: 0, set: *nice });
            pending.push(*tid);
        }
        if !pending.is_empty() {
            if let Err(e) = self.save() {
                for tid in pending {
                    self.changed.remove(&tid);
                }
                return Err(e);
            }
            dirty = true;
        }
        for tid in pending {
            let nice = self.changed[&tid].set;
            if let Err(e) = self.nice.set(tid, nice) {
                self.changed.remove(&tid);
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    self.undo_all();
                    return Err(e);
                }
            }
        }
        if dirty {
            self.save()?;
        }
        let ui = self.changed.values().filter(|c| c.set == cfg.ui_nice).count();
        Ok((engine, ui, self.changed.len() - ui))
    }
}

/// Undo what a crashed run left behind (`lindeskd --restore`). Returns how many threads
/// were put back.
pub fn restore(state_dir: &Path, roots: &Roots) -> usize {
    let file = state_dir.join(FILE);
    let Ok(raw) = std::fs::read(&file) else { return 0 };
    let saved: Vec<Changed> = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("thread-qos: invalid recovery journal: {e}");
            return 0;
        }
    };
    let mut book = Book::new(KernelNice, &roots.proc, state_dir);
    book.changed = saved.into_iter().map(|c| (c.tid, c)).collect();
    book.undo_all()
}

enum Job {
    /// The app in use: its unit, and the cgroups it runs in.
    App { target: Target, cgroups: Vec<String> },
    /// Nothing is in use, or lindesk must not change anything.
    Clear,
    /// Put everything back and say when done.
    Stop(SyncSender<()>),
}

/// Watches focus and hands the app in use to the worker thread.
pub struct ThreadQos {
    refresh: Duration,
    tx: SyncSender<Job>,
    unavailable: Arc<AtomicBool>,
    last: Option<Target>,
    sent: Instant,
}

impl ThreadQos {
    /// Starts the worker.
    pub fn spawn(
        cfg: ThreadQosConfig,
        roots: Roots,
        state_dir: &Path,
        host: String,
        events: HostSender,
    ) -> std::io::Result<ThreadQos> {
        let (tx, rx) = mpsc::sync_channel::<Job>(2);
        let unavailable = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&unavailable);
        let refresh = cfg.refresh;
        let mut book = Book::new(KernelNice, &roots.proc, state_dir);
        book.load()?;
        std::thread::Builder::new()
            .name("lindesk-threads".into())
            .spawn(move || work(rx, cfg, roots, book, flag, &host, &events))?;
        Ok(ThreadQos { refresh, tx, unavailable, last: None, sent: Instant::now() })
    }

    /// Call after every engine step.
    pub fn observe(&mut self, engine: &Engine) {
        if self.unavailable.load(Ordering::Relaxed) {
            return;
        }
        // Paused and dry-run mean "change nothing".
        let fg = engine.foreground().filter(|_| !engine.dry_run());
        let changed = fg != self.last.as_ref();
        if !changed && (fg.is_none() || self.sent.elapsed() < self.refresh) {
            return;
        }
        let job = match fg {
            Some(target) => {
                let cgroups: Vec<String> = std::iter::once(target)
                    .chain(engine.foreground_family())
                    .filter_map(|t| engine.target_cgroup(t))
                    .collect();
                Job::App { target: target.clone(), cgroups }
            }
            None => Job::Clear,
        };
        match self.tx.try_send(job) {
            Ok(()) => {
                self.last = fg.cloned();
                self.sent = Instant::now();
            }
            // Busy: this step's look is skipped, the next one tries again.
            Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => self.unavailable.store(true, Ordering::Relaxed),
        }
    }

    /// Put every thread back and wait for it (shutdown).
    pub fn stop(&mut self) {
        let (done, wait) = mpsc::sync_channel(1);
        if self.tx.send(Job::Stop(done)).is_ok() {
            let _ = wait.recv_timeout(Duration::from_secs(5));
        }
    }
}

fn work<N: Nice>(
    rx: Receiver<Job>,
    cfg: ThreadQosConfig,
    roots: Roots,
    mut book: Book<N>,
    unavailable: Arc<AtomicBool>,
    host: &str,
    events: &HostSender,
) {
    let mut current: Option<Target> = None;
    let mut announced: BTreeMap<Target, Instant> = BTreeMap::new();
    for job in rx {
        match job {
            Job::Clear => {
                book.undo_all();
                current = None;
            }
            Job::Stop(done) => {
                book.undo_all();
                let _ = done.send(());
                return;
            }
            Job::App { target, cgroups } => {
                if current.as_ref() != Some(&target) {
                    book.undo_all();
                    current = Some(target.clone());
                }
                let pids: Vec<i32> = cgroups
                    .iter()
                    .flat_map(|cg| procfs::pids_in_cgroup_tree(&roots.cgroup, cg).unwrap_or_default())
                    .take(MAX_PROCESSES)
                    .collect();
                match book.apply(&cfg, &pids) {
                    Ok((Some(engine), ui, background)) if ui + background > 0 => {
                        let now = Instant::now();
                        announced.retain(|_, at| now.duration_since(*at) < ANNOUNCE_EVERY);
                        if !announced.contains_key(&target) {
                            announced.insert(target.clone(), now);
                            events.event(crate::common::startup::notice(
                                NOTICE,
                                host,
                                format!(
                                    "{ui} UI threads of {target} put ahead of its other threads ({engine}; \
                                     {background} housekeeping threads lowered)"
                                ),
                                json!({"target": target, "engine": engine, "ui": ui, "background": background}),
                            ));
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::info!("per-thread priorities are not available here ({e}); switched off");
                        unavailable.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            }
        }
    }
    book.undo_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Niceness kept in a map; `deny` makes every set fail like a missing capability.
    #[derive(Clone, Default)]
    struct FakeNice {
        values: Arc<Mutex<BTreeMap<i32, i8>>>,
        deny: bool,
    }

    impl Nice for FakeNice {
        fn get(&self, tid: i32) -> std::io::Result<i8> {
            Ok(self.values.lock().unwrap().get(&tid).copied().unwrap_or(0))
        }
        fn set(&self, tid: i32, nice: i8) -> std::io::Result<()> {
            if self.deny {
                return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
            }
            self.values.lock().unwrap().insert(tid, nice);
            Ok(())
        }
    }

    fn process(root: &Path, pid: i32, threads: &[(i32, &str)]) {
        let _ = std::fs::remove_dir_all(root.join(pid.to_string()));
        for (tid, name) in threads {
            let t = root.join(format!("{pid}/task/{tid}"));
            std::fs::create_dir_all(&t).unwrap();
            std::fs::write(t.join("comm"), name).unwrap();
        }
    }

    const BROWSER: &[(i32, &str)] = &[
        (100, "wavebox"),
        (101, "Chrome_IOThread"),
        (102, "ThreadPoolForeg"),
        (103, "ThreadPoolForeg"),
        (104, "CompositorTileW"),
        (105, "HangWatcher"),
        (106, "gmain"),
    ];
    const RENDERER: &[(i32, &str)] = &[
        (200, "wavebox"),
        (201, "Chrome_ChildIOT"),
        (202, "Compositor"),
        (203, "ThreadPoolForeg"),
        (204, "MemoryInfra"),
    ];

    fn cfg() -> ThreadQosConfig {
        ThreadQosConfig::from_json(&serde_json::Value::Null, -5).unwrap()
    }

    #[test]
    fn engines_are_recognised_by_their_threads() {
        let list =
            |t: &[(i32, &str)]| t.iter().map(|(tid, n)| Thread { tid: *tid, name: n.to_string() }).collect::<Vec<_>>();
        let c = cfg();
        let (engine, r) = roles(100, &list(BROWSER), &c.engines).unwrap();
        assert_eq!(engine, "chromium");
        assert_eq!(r, [(100, Role::Ui), (101, Role::Ui), (104, Role::Ui), (105, Role::Background)]);
        // The worker pool and toolkit threads have no particular role.
        assert!(!r.iter().any(|(tid, _)| [102, 103, 106].contains(tid)));
        // Firefox.
        let fx = list(&[
            (300, "firefox"),
            (301, "IPC I/O Parent"),
            (302, "Renderer"),
            (303, "BackgroundThre"),
            (304, "DOM Worker"),
        ]);
        let (engine, r) = roles(300, &fx, &c.engines).unwrap();
        assert_eq!(engine, "gecko");
        assert_eq!(r, [(300, Role::Ui), (301, Role::Ui), (302, Role::Ui), (303, Role::Background)]);
        // A shell is not an engine: nothing in it is touched.
        assert!(roles(400, &list(&[(400, "bash")]), &c.engines).is_none());
        // An engine from the config comes first and may leave the main thread alone.
        let mine = json!({"engines": [{"name": "mine", "markers": ["Chrome_IOThread"], "ui": ["gmain"], "main_is_ui": false}]});
        let c = ThreadQosConfig::from_json(&mine, -5).unwrap();
        assert_eq!(roles(100, &list(BROWSER), &c.engines).unwrap(), ("mine", vec![(106, Role::Ui)]));
    }

    #[test]
    fn config_is_checked_and_capped() {
        let c = ThreadQosConfig::from_json(&json!({"ui_nice": -15}), -5).unwrap();
        assert_eq!(c.ui_nice, -5, "the safety cap wins");
        assert_eq!(c.engines.len(), builtin_engines().len());
        for bad in [json!({"ui_nice": 3}), json!({"background_nice": -1}), json!({"refresh": "100ms"})] {
            assert!(ThreadQosConfig::from_json(&bad, -5).is_err(), "{bad}");
        }
        assert!(ThreadQosConfig::from_json(&json!({"engines": [{"name": "x", "markers": []}]}), -5).is_err());
    }

    #[test]
    fn ui_threads_go_up_and_everything_goes_back() {
        let dir = crate::common::test_dir();
        let (proc, state) = (dir.path().join("proc"), dir.path().join("state"));
        std::fs::create_dir_all(&state).unwrap();
        process(&proc, 100, BROWSER);
        process(&proc, 200, RENDERER);
        process(&proc, 400, &[(400, "bash"), (401, "rustc")]);
        let nice = FakeNice::default();
        // The app set one of its own threads: that stays as it is.
        nice.set(202, 3).unwrap();
        let mut book = Book::new(nice.clone(), &proc, &state);

        let (engine, ui, background) = book.apply(&cfg(), &[100, 200, 400]).unwrap();
        assert_eq!((engine.as_deref(), ui, background), (Some("chromium"), 5, 2));
        let now = |tid: i32| nice.get(tid).unwrap();
        assert_eq!([100, 101, 104, 200, 201].map(now), [-4; 5], "UI threads");
        assert_eq!([105, 204].map(now), [5; 2], "housekeeping");
        assert_eq!([102, 103, 106, 203, 400, 401].map(now), [0; 6], "the pool, and anything that is no engine");
        assert_eq!(now(202), 3, "the app's own choice is kept");
        let saved: Vec<Changed> = serde_json::from_slice(&std::fs::read(state.join(FILE)).unwrap()).unwrap();
        assert_eq!(saved.len(), 7);

        // A new tab: one more renderer. Looking again adds its threads and repeats nothing.
        process(&proc, 250, &[(250, "wavebox"), (251, "Chrome_ChildIOT"), (252, "ThreadPoolForeg")]);
        let (_, ui, _) = book.apply(&cfg(), &[100, 200, 250, 400]).unwrap();
        assert_eq!(ui, 7);
        // A thread the person renamed their way out of a role, one that exited and whose id
        // went to another process, and one somebody else re-niced: each handled.
        process(&proc, 200, &[(200, "wavebox"), (201, "Chrome_ChildIOT"), (202, "Compositor")]);
        nice.set(204, 0).unwrap(); // tid 204 is a new, unrelated thread now
        nice.set(101, -10).unwrap();
        book.apply(&cfg(), &[100, 200, 250, 400]).unwrap();
        assert_eq!(now(204), 0, "a reused thread id is not ours to touch");

        assert!(book.undo_all() > 0);
        assert_eq!([100, 104, 200, 201, 250, 251, 105].map(now), [0; 7], "all back to where they were");
        assert_eq!(now(101), -10, "changed by someone else since: left alone");
        assert_eq!(now(202), 3);
        assert!(!state.join(FILE).exists());
    }

    #[test]
    fn without_the_capability_nothing_is_left_changed() {
        let dir = crate::common::test_dir();
        let proc = dir.path().join("proc");
        process(&proc, 100, BROWSER);
        let nice = FakeNice { deny: true, ..Default::default() };
        let mut book = Book::new(nice.clone(), &proc, dir.path());
        let e = book.apply(&cfg(), &[100]).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(book.changed.is_empty() && nice.values.lock().unwrap().is_empty());
    }

    #[test]
    fn journal_failure_prevents_priority_changes() {
        let dir = crate::common::test_dir();
        let proc = dir.path().join("proc");
        process(&proc, 100, BROWSER);
        let state = dir.path().join("not-a-directory");
        std::fs::write(&state, "occupied").unwrap();
        let nice = FakeNice::default();
        let mut book = Book::new(nice.clone(), &proc, &state);
        assert!(book.apply(&cfg(), &[100]).is_err());
        assert!(book.changed.is_empty());
        assert!(nice.values.lock().unwrap().is_empty());
    }

    #[test]
    fn failed_restore_keeps_recovery_records_for_retry() {
        let dir = crate::common::test_dir();
        let proc = dir.path().join("proc");
        process(&proc, 100, BROWSER);
        let mut book = Book::new(FakeNice::default(), &proc, dir.path());
        book.apply(&cfg(), &[100]).unwrap();
        let count = book.changed.len();
        book.nice.deny = true;
        assert_eq!(book.undo_all(), 0);
        let saved: Vec<Changed> = serde_json::from_slice(&std::fs::read(dir.path().join(FILE)).unwrap()).unwrap();
        assert_eq!(saved.len(), count);
        // A new worker must adopt these originals, even if ExecStopPost could not restore them.
        let mut restarted = Book::new(book.nice.clone(), &proc, dir.path());
        restarted.load().unwrap();
        assert_eq!(restarted.changed, book.changed);
        book.nice.deny = false;
        assert_eq!(book.undo_all(), count);
        assert!(!dir.path().join(FILE).exists());
    }

    #[test]
    fn corrupt_thread_journal_is_not_overwritten() {
        let dir = crate::common::test_dir();
        std::fs::write(dir.path().join(FILE), "invalid").unwrap();
        let mut book = Book::new(FakeNice::default(), dir.path(), dir.path());
        assert!(book.load().is_err());
        assert_eq!(restore(dir.path(), &Roots::under(dir.path())), 0);
        assert_eq!(std::fs::read_to_string(dir.path().join(FILE)).unwrap(), "invalid");
    }

    #[test]
    fn a_crashed_run_is_restored_from_the_state_file() {
        let dir = crate::common::test_dir();
        let roots = Roots::under(dir.path());
        let me = std::process::id() as i32;
        // A record for a thread that no longer exists: restoring must not touch anything
        // (and must not fail), and the file goes away.
        let gone = Changed { pid: me, tid: i32::MAX - 7, original: 0, set: -4 };
        std::fs::write(dir.path().join(FILE), serde_json::to_vec(&vec![gone]).unwrap()).unwrap();
        assert_eq!(restore(dir.path(), &roots), 1);
        assert!(!dir.path().join(FILE).exists());
        assert_eq!(restore(dir.path(), &roots), 0);
    }
}
