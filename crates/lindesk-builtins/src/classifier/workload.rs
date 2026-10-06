//! `workload`: classifies a unit by what is running in it right now.
//!
//! Builds started from a terminal run in the terminal tab's scope (`vte-spawn-*.scope`,
//! `app-…konsole….scope`, a tmux or `systemd-run` scope), next to the shell. The unit's
//! oldest process is that shell, so name-based rules see nothing special and the build
//! competes with the desktop as a normal app. This classifier looks at the whole unit: while
//! it holds compiler/linker processes it is `background`, so it yields under pressure like
//! any other background work. The focused app is never throttled, whatever its class.
//!
//! Register it *after* the `rules` classifier (lower priority): rules decide first, and this
//! only speaks for units they had no opinion on.
//!
//! ```toml
//! [plugins.workload.config]
//! min_processes = 2            # compiler/linker processes needed to call it a build
//! hold = "60s"                 # stay background this long after the last one was seen
//! class = "background"         # or "batch" (SCHED_IDLE while it builds)
//! comms = ["rustc", "cc1"]     # replaces the built-in list
//! extra_comms = ["zig"]        # adds to it
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use lindesk_core::clock::now_ms;
use lindesk_plugin_api::{
    Classification, Classifier, InitContext, Plugin, PluginError, PluginInfo, PluginKind, PluginResult, ProcInfo,
    QosClass, TimestampMs,
};
use lindesk_sys::procfs;
use lindesk_sys::roots::Roots;

pub const NAME: &str = "workload";

/// Process names (`comm`, at most 15 bytes) that mean "a build is running here".
pub const BUILD_COMMS: &[&str] = &[
    "rustc",
    "cc1",
    "cc1plus",
    "cc1obj",
    "lto1",
    "lto-wrapper",
    "collect2",
    "ld",
    "ld.bfd",
    "ld.gold",
    "ld.lld",
    "lld",
    "rust-lld",
    "mold",
    "wild",
    "clang",
    "clang++",
    "gcc",
    "g++",
    "cc",
    "c++",
    "as",
    "ar",
    "ranlib",
    "rustdoc",
    "javac",
    "kotlinc",
    "scalac",
    "go",
    "compile",
    "link",
    "asm",
    "swiftc",
    "swift-frontend",
    "ghc",
    "ocamlopt",
    "zig",
    "nim",
    "dmd",
    "ldc2",
    "tsc",
    "esbuild",
    "swc",
    "webpack",
    "rollup",
    "ninja",
    "make",
    "cmake",
    "bazel",
    "buck2",
    "sccache",
    "ccache",
    "distcc",
];

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct WorkloadConfig {
    /// Build processes that must be present to classify the unit.
    pub min_processes: usize,
    /// Keep the class this long after the last build process was seen, so the gaps between
    /// compiler invocations don't flip the unit back and forth.
    #[serde(with = "lindesk_core::config::dur")]
    pub hold: Duration,
    /// Class given to a unit that is building: `background` or `batch`.
    pub class: QosClass,
    /// Replaces [`BUILD_COMMS`] when set.
    pub comms: Option<Vec<String>>,
    /// Added to the list.
    pub extra_comms: Vec<String>,
    /// Only the first this-many processes of a unit are inspected.
    pub max_processes: usize,
}

impl Default for WorkloadConfig {
    fn default() -> Self {
        WorkloadConfig {
            min_processes: 2,
            hold: Duration::from_secs(60),
            class: QosClass::Background,
            comms: None,
            extra_comms: Vec::new(),
            max_processes: 2048,
        }
    }
}

impl WorkloadConfig {
    pub fn from_json(v: &serde_json::Value) -> Result<Self, String> {
        let cfg: WorkloadConfig = if v.is_null() {
            WorkloadConfig::default()
        } else {
            serde_json::from_value(v.clone()).map_err(|e| e.to_string())?
        };
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !matches!(self.class, QosClass::Background | QosClass::Batch) {
            return Err(format!("class must be background or batch, not {}", self.class));
        }
        if self.min_processes == 0 {
            return Err("min_processes must be at least 1".into());
        }
        Ok(())
    }

    fn comm_set(&self) -> Vec<String> {
        let mut v: Vec<String> = match &self.comms {
            Some(c) => c.clone(),
            None => BUILD_COMMS.iter().map(|s| s.to_string()).collect(),
        };
        v.extend(self.extra_comms.iter().cloned());
        // The kernel truncates comm to 15 bytes.
        for c in &mut v {
            c.truncate(15);
        }
        v.sort();
        v.dedup();
        v
    }
}

/// See the module docs.
pub struct WorkloadClassifier {
    cfg: WorkloadConfig,
    comms: Vec<String>,
    roots: Roots,
    /// Unit cgroup → when a build was last seen in it.
    last_build: BTreeMap<String, TimestampMs>,
    clock: fn() -> TimestampMs,
}

impl Default for WorkloadClassifier {
    fn default() -> Self {
        Self::new(WorkloadConfig::default())
    }
}

impl WorkloadClassifier {
    pub fn new(cfg: WorkloadConfig) -> Self {
        let comms = cfg.comm_set();
        WorkloadClassifier { cfg, comms, roots: Roots::host(), last_build: BTreeMap::new(), clock: now_ms }
    }

    /// Use other `/proc` and cgroup roots (fixtures).
    pub fn with_roots(mut self, roots: Roots) -> Self {
        self.roots = roots;
        self
    }

    /// Use another time source (tests).
    pub fn with_clock(mut self, clock: fn() -> TimestampMs) -> Self {
        self.clock = clock;
        self
    }

    pub fn config(&self) -> &WorkloadConfig {
        &self.cfg
    }

    /// Only transient scopes of a user's apps: terminal tabs, tmux/`systemd-run` scopes,
    /// editors. Never slices, services (the compositor, daemons) or the session scope.
    fn considers(p: &ProcInfo) -> bool {
        let Some(unit) = p.unit.as_deref() else { return false };
        p.pid > 0
            && unit.ends_with(".scope")
            && !unit.starts_with("session-")
            && unit != "init.scope"
            && p.cgroup.contains("/user@")
    }

    /// Build process names in the unit, with counts.
    fn builds_in(&self, cgroup: &str) -> BTreeMap<String, usize> {
        let mut found = BTreeMap::new();
        let Ok(pids) = procfs::pids_in_cgroup_tree(&self.roots.cgroup, cgroup) else { return found };
        for pid in pids.into_iter().take(self.cfg.max_processes) {
            if let Ok(comm) = procfs::comm(&self.roots.proc, pid)
                && self.comms.binary_search(&comm).is_ok()
            {
                *found.entry(comm).or_insert(0) += 1;
            }
        }
        found
    }
}

impl Plugin for WorkloadClassifier {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Classifier],
            description: "Marks units that are running a build (compilers, linkers) as background".into(),
        }
    }

    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
        self.cfg =
            WorkloadConfig::from_json(&ctx.config).map_err(|e| PluginError::Failed(format!("workload config: {e}")))?;
        self.comms = self.cfg.comm_set();
        Ok(())
    }
}

impl Classifier for WorkloadClassifier {
    fn classify(&mut self, p: &ProcInfo) -> PluginResult<Option<Classification>> {
        if !Self::considers(p) {
            return Ok(None);
        }
        let now = (self.clock)();
        let found = self.builds_in(&p.cgroup);
        let total: usize = found.values().sum();
        if total >= self.cfg.min_processes {
            self.last_build.insert(p.cgroup.clone(), now);
            let mut top: Vec<(&String, &usize)> = found.iter().collect();
            top.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
            let names: Vec<String> = top.iter().take(3).map(|(n, c)| format!("{n}×{c}")).collect();
            return Ok(Some(Classification {
                class: self.cfg.class,
                confidence: 0.9,
                reason: format!("workload: build running ({total} processes: {})", names.join(", ")),
            }));
        }
        let hold_ms = self.cfg.hold.as_millis() as i64;
        match self.last_build.get(&p.cgroup) {
            Some(&seen) if now - seen < hold_ms => Ok(Some(Classification {
                class: self.cfg.class,
                confidence: 0.9,
                reason: format!("workload: build finished {}s ago (held {}s)", (now - seen) / 1000, hold_ms / 1000),
            })),
            Some(_) => {
                self.last_build.remove(&p.cgroup);
                Ok(None)
            }
            None => Ok(None),
        }
    }
}

/// `/proc` and cgroup roots under one directory (fixtures).
pub fn roots_under(dir: impl Into<PathBuf>) -> Roots {
    Roots::under(dir.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicI64, Ordering};

    const CG: &str =
        "/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.gnome.Terminal.slice/vte-spawn-1.scope";

    // Per thread: tests run in parallel, each on its own thread, and must not share a clock.
    thread_local! {
        static NOW: AtomicI64 = const { AtomicI64::new(0) };
    }
    fn clock() -> TimestampMs {
        NOW.with(|n| n.load(Ordering::SeqCst))
    }

    struct Fx {
        dir: tempfile::TempDir,
    }

    impl Fx {
        fn new() -> Fx {
            let dir = tempfile::tempdir_in("/dev/shm").unwrap_or_else(|_| tempfile::tempdir().unwrap());
            fs::create_dir_all(dir.path().join("proc")).unwrap();
            Fx { dir }
        }

        /// Replace the unit's processes with `comms` (pids 100, 101, …).
        fn procs(&self, cg: &str, comms: &[&str]) {
            let roots = roots_under(self.dir.path());
            let cgdir = roots.cgroup.join(cg.trim_start_matches('/'));
            fs::create_dir_all(&cgdir).unwrap();
            let mut list = String::new();
            for (i, c) in comms.iter().enumerate() {
                let pid = 100 + i;
                let p = roots.proc.join(pid.to_string());
                fs::create_dir_all(&p).unwrap();
                fs::write(p.join("comm"), format!("{c}\n")).unwrap();
                list.push_str(&format!("{pid}\n"));
            }
            fs::write(cgdir.join("cgroup.procs"), list).unwrap();
        }

        fn classifier(&self) -> WorkloadClassifier {
            WorkloadClassifier::default().with_roots(roots_under(self.dir.path())).with_clock(clock)
        }
    }

    fn proc(unit: &str, cg: &str) -> ProcInfo {
        ProcInfo {
            pid: 100,
            uid: 1000,
            comm: "bash".into(),
            cgroup: cg.into(),
            unit: Some(unit.into()),
            ..Default::default()
        }
    }

    #[test]
    fn a_build_in_a_terminal_tab_is_background_and_released_after_the_hold() {
        let fx = Fx::new();
        let mut c = fx.classifier();
        let p = proc("vte-spawn-1.scope", CG);

        NOW.with(|n| n.store(1_000, Ordering::SeqCst));
        fx.procs(CG, &["bash", "claude"]);
        assert_eq!(c.classify(&p).unwrap(), None, "an idle shell is nobody's business");

        fx.procs(CG, &["bash", "cargo", "rustc", "rustc", "rustc", "cc1", "rust-lld"]);
        let got = c.classify(&p).unwrap().unwrap();
        assert_eq!(got.class, QosClass::Background);
        assert!(got.reason.contains("5 processes") && got.reason.contains("rustc×3"), "{}", got.reason);

        // Build over: stays background for the hold, then goes back to "no opinion".
        fx.procs(CG, &["bash", "claude"]);
        NOW.with(|n| n.store(1_000 + 30_000, Ordering::SeqCst));
        assert!(c.classify(&p).unwrap().unwrap().reason.contains("finished"));
        NOW.with(|n| n.store(1_000 + 61_000, Ordering::SeqCst));
        assert_eq!(c.classify(&p).unwrap(), None);
    }

    #[test]
    fn one_stray_compiler_is_not_a_build_and_only_user_scopes_count() {
        let fx = Fx::new();
        let mut c = fx.classifier();
        NOW.with(|n| n.store(5_000, Ordering::SeqCst));
        fx.procs(CG, &["bash", "make"]);
        assert_eq!(c.classify(&proc("vte-spawn-1.scope", CG)).unwrap(), None, "below min_processes");

        // A service, a slice and a system scope full of compilers are left to other classifiers.
        let svc = "/user.slice/user-1000.slice/user@1000.service/session.slice/org.gnome.Shell@wayland.service";
        fx.procs(svc, &["rustc", "rustc", "cc1"]);
        assert_eq!(c.classify(&proc("org.gnome.Shell@wayland.service", svc)).unwrap(), None);
        let sys = "/system.slice/docker-abc.scope";
        fx.procs(sys, &["rustc", "rustc", "cc1"]);
        assert_eq!(c.classify(&proc("docker-abc.scope", sys)).unwrap(), None);
        let mut slice = proc("app.slice", "/user.slice/user-1000.slice/user@1000.service/app.slice");
        slice.pid = 0;
        assert_eq!(c.classify(&slice).unwrap(), None);
    }

    #[test]
    fn config() {
        let c = WorkloadConfig::from_json(
            &serde_json::json!({"class": "batch", "hold": "10s", "extra_comms": ["zig-very-long-name-here"]}),
        )
        .unwrap();
        assert_eq!((c.class, c.hold), (QosClass::Batch, Duration::from_secs(10)));
        assert!(c.comm_set().contains(&"zig-very-long-n".to_string()), "truncated like the kernel does");
        assert!(WorkloadConfig::from_json(&serde_json::json!({"class": "critical"})).is_err());
        assert!(WorkloadConfig::from_json(&serde_json::json!({"min_processes": 0})).is_err());
        assert!(WorkloadConfig::from_json(&serde_json::json!({"nope": 1})).is_err());
        assert_eq!(WorkloadConfig::from_json(&serde_json::Value::Null).unwrap(), WorkloadConfig::default());
    }
}
