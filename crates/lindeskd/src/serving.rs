//! What the app in use is waiting on is not throttled (`[plugins.serving]`).
//!
//! macOS lends a thread's priority to whoever it is waiting on, across processes. Linux has
//! no such thing outside a few kernel locks, and nothing in userspace can add it. What
//! lindesk can do is not make it worse: when the app in front has an open connection to
//! something lindesk is holding back, holding that back holds the foreground back. A
//! browser showing a page from a dev server that is rebuilding in a terminal tab is the
//! everyday case; the tab is a build, it yields, and the page you are looking at loads
//! slowly because of lindesk.
//!
//! So while the level is raised, the broker looks at the loopback TCP connections of the
//! focused app ([`lindesk_sys::sockets`]) and finds which throttled units hold the other
//! ends. Those are marked as serving the app in use, and the policy leaves them alone for
//! as long as the connection is there.
//!
//! Only TCP on loopback in the focused application's network namespaces. Unix sockets, D-Bus and
//! containers with their own network are not seen; see the module docs of
//! [`lindesk_sys::sockets`].
//!
//! ```toml
//! [plugins.serving]
//! enabled = true
//! config = { refresh = "5s" }
//! ```

use std::collections::BTreeSet;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lindesk_core::config::dur;
use lindesk_host::{Engine, HostSender};
use lindesk_plugin_api::{Level, Target};
use lindesk_sys::{Roots, procfs, sockets};
use serde::Deserialize;
use serde_json::json;

/// Config section / plugin name.
pub const NAME: &str = "serving";
/// Name of the notice in `lindeskctl events`.
pub const NOTICE: &str = "serving.found";
/// Processes looked at per unit.
const MAX_PROCESSES: usize = 512;

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ServingConfig {
    /// How often the focused app's connections are looked at while the level is raised.
    #[serde(with = "dur")]
    pub refresh: Duration,
}

impl Default for ServingConfig {
    fn default() -> Self {
        ServingConfig { refresh: Duration::from_secs(5) }
    }
}

impl ServingConfig {
    /// Parse the plugin's JSON config; `null` yields the defaults.
    pub fn from_json(v: &serde_json::Value) -> Result<Self, String> {
        if v.is_null() {
            return Ok(Self::default());
        }
        let cfg: ServingConfig = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
        if cfg.refresh < Duration::from_secs(1) {
            return Err("refresh must be at least 1s".into());
        }
        Ok(cfg)
    }
}

struct Job {
    target: Target,
    /// The cgroups of the app in use.
    app: Vec<String>,
    /// Units that are (or would be) throttled, and their cgroups.
    candidates: Vec<(Target, String)>,
}

/// The candidates that hold the other end of one of the app's loopback connections.
pub fn find(roots: &Roots, app: &[String], candidates: &[(Target, String)]) -> BTreeSet<Target> {
    let pids = |cg: &String| procfs::pids_in_cgroup_tree(&roots.cgroup, cg).unwrap_or_default();
    let app_pids: Vec<i32> = app.iter().flat_map(pids).take(MAX_PROCESSES).collect();
    let mine: BTreeSet<u64> = app_pids.iter().flat_map(|pid| sockets::socket_inodes(&roots.proc, *pid)).collect();
    if mine.is_empty() {
        return BTreeSet::new();
    }
    let peers = sockets::loopback_peers(&sockets::tcp_sockets_for_pids(&roots.proc, &app_pids), &mine);
    if peers.is_empty() {
        return BTreeSet::new();
    }
    candidates
        .iter()
        .filter(|(_, cg)| {
            pids(cg)
                .into_iter()
                .take(MAX_PROCESSES)
                .any(|pid| !sockets::socket_inodes(&roots.proc, pid).is_disjoint(&peers))
        })
        .map(|(t, _)| t.clone())
        .collect()
}

/// Watches the app in use and tells the engine what it is connected to.
pub struct Serving {
    refresh: Duration,
    tx: SyncSender<Job>,
    /// The worker's latest answer, taken by the next step.
    found: Arc<Mutex<Option<BTreeSet<Target>>>>,
    sent: Option<Instant>,
    active: bool,
}

impl Serving {
    /// Starts the worker.
    pub fn spawn(cfg: ServingConfig, roots: Roots, host: String, events: HostSender) -> std::io::Result<Serving> {
        let (tx, rx) = mpsc::sync_channel::<Job>(1);
        let found: Arc<Mutex<Option<BTreeSet<Target>>>> = Arc::default();
        let out = Arc::clone(&found);
        std::thread::Builder::new().name("lindesk-serving".into()).spawn(move || {
            let mut last: BTreeSet<Target> = BTreeSet::new();
            for job in rx {
                let now = find(&roots, &job.app, &job.candidates);
                for unit in now.difference(&last) {
                    events.event(crate::common::startup::notice(
                        NOTICE,
                        &host,
                        format!("{} is connected to {unit}: not throttling it while that lasts", job.target),
                        json!({"app": job.target, "unit": unit}),
                    ));
                }
                last = now.clone();
                *out.lock().unwrap_or_else(|e| e.into_inner()) = Some(now);
            }
        })?;
        Ok(Serving { refresh: cfg.refresh, tx, found, sent: None, active: false })
    }

    /// Call after every engine step.
    pub fn observe(&mut self, engine: &mut Engine) {
        if let Some(found) = self.found.lock().unwrap_or_else(|e| e.into_inner()).take() {
            self.active = !found.is_empty();
            engine.set_serving(found);
        }
        // Nothing is held back while GREEN, in a dry run or paused: nothing to look for.
        let fg = engine.foreground().filter(|_| engine.level() != Level::Green && !engine.dry_run()).cloned();
        let Some(target) = fg else {
            if self.active {
                self.active = false;
                engine.set_serving(BTreeSet::new());
            }
            self.sent = None;
            return;
        };
        if self.sent.is_some_and(|at| at.elapsed() < self.refresh) {
            return;
        }
        let candidates = engine.serving_candidates();
        if candidates.is_empty() {
            return;
        }
        let app: Vec<String> = std::iter::once(&target)
            .chain(engine.foreground_family())
            .filter_map(|t| engine.target_cgroup(t))
            .collect();
        match self.tx.try_send(Job { target, app, candidates }) {
            Ok(()) => self.sent = Some(Instant::now()),
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn config() {
        assert_eq!(ServingConfig::from_json(&serde_json::Value::Null).unwrap(), ServingConfig::default());
        assert_eq!(ServingConfig::from_json(&json!({"refresh": "2s"})).unwrap().refresh, Duration::from_secs(2));
        assert!(ServingConfig::from_json(&json!({"refresh": "10ms"})).is_err());
        assert!(ServingConfig::from_json(&json!({"refersh": "2s"})).is_err());
    }

    /// With the real `/proc` and a made-up cgroup tree: a "browser" (this process) connected
    /// to a "dev server" (a child process that accepted the connection) in a build tab.
    #[test]
    fn the_unit_at_the_other_end_of_a_real_connection() {
        let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
            eprintln!("skipped: cannot listen on loopback here");
            return;
        };
        let addr = listener.local_addr().unwrap();
        // The server: `nc`-like, in a child so it is another process. `sh` with /dev/tcp is
        // bash-only, so use a tiny python or skip.
        let script = format!(
            "import socket,sys,time\ns=socket.create_connection(('127.0.0.1',{}))\nsys.stdout.write('ok\\n');sys.stdout.flush()\ntime.sleep(60)\n",
            addr.port()
        );
        let child = std::process::Command::new("python3")
            .args(["-c", &script])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            eprintln!("skipped: no python3 to play the other process");
            return;
        };
        let (mut ours, _) = listener.accept().unwrap();
        let mut line = [0u8; 3];
        child.stdout.as_mut().unwrap().read_exact(&mut line).unwrap();
        ours.write_all(b"x").unwrap();
        // A second connection that stays inside this process must not count.
        let _own = TcpStream::connect(addr).unwrap();
        let _own_served = listener.accept().unwrap();

        let dir = crate::common::test_dir();
        let cgroup = dir.path().join("cgroup");
        let put = |cg: &str, pids: String| {
            let d = cgroup.join(cg.trim_start_matches('/'));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("cgroup.procs"), pids).unwrap();
        };
        put("/app/browser.scope", std::process::id().to_string());
        put("/app/build.scope", child.id().to_string());
        put("/app/idle.scope", "1".into());
        let roots = Roots { cgroup: cgroup.clone(), ..Roots::host() };
        let unit = |n: &str| Target::user_unit(1000, n);
        let candidates = vec![
            (unit("build.scope"), "/app/build.scope".to_string()),
            (unit("idle.scope"), "/app/idle.scope".to_string()),
        ];
        let found = find(&roots, &["/app/browser.scope".to_string()], &candidates);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(found, BTreeSet::from([unit("build.scope")]));
        // An app with no local connections serves nobody.
        assert!(find(&roots, &["/app/idle.scope".to_string()], &candidates).is_empty());
    }
}
