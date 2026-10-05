//! Helpers for the D-Bus integration tests: private buses and daemon processes.
#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// A private `dbus-daemon`, killed on drop.
pub struct Bus {
    pub child: Child,
    pub address: String,
}

impl Bus {
    /// A session-type bus with the stock session config.
    pub fn session() -> Bus {
        Self::spawn(&["--session"])
    }

    /// A "system-like" bus: a permissive config written to `dir` (no polkit, any user may
    /// own any name).
    pub fn system_like(dir: &Path) -> Bus {
        let socket = dir.join("system_bus_socket");
        let conf = dir.join("system.conf");
        std::fs::write(
            &conf,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>system</type>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow user="*"/>
    <allow own="*"/>
    <allow send_destination="*"/>
    <allow receive_sender="*"/>
  </policy>
</busconfig>
"#,
                socket.display()
            ),
        )
        .unwrap();
        Self::spawn(&[&format!("--config-file={}", conf.display())])
    }

    fn spawn(args: &[&str]) -> Bus {
        let mut child = Command::new("dbus-daemon")
            .args(args)
            .args(["--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("dbus-daemon is installed");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let address = line.trim().to_string();
        assert!(!address.is_empty(), "dbus-daemon printed no address");
        Bus { child, address }
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A daemon process, SIGKILLed on drop if still running.
pub struct Daemon {
    pub child: Option<Child>,
    pub name: &'static str,
}

impl Daemon {
    pub fn spawn(name: &'static str, bin: &Path, args: &[&str], env: &[(&str, &str)]) -> Daemon {
        let mut cmd = Command::new(bin);
        cmd.args(args).stdin(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.env_remove("JOURNAL_STREAM").env("RUST_LOG", std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()));
        Daemon { child: Some(cmd.spawn().unwrap_or_else(|e| panic!("spawning {}: {e}", bin.display()))), name }
    }

    /// Like [`Daemon::spawn`] but with an empty environment plus `env`.
    pub fn spawn_clean(name: &'static str, bin: &Path, args: &[&str], env: &[(&str, &str)]) -> Daemon {
        let mut cmd = Command::new(bin);
        cmd.args(args).stdin(Stdio::null()).env_clear();
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.env("RUST_LOG", std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()));
        Daemon { child: Some(cmd.spawn().unwrap_or_else(|e| panic!("spawning {}: {e}", bin.display()))), name }
    }

    /// SIGTERM and wait (up to 30 s) for a clean exit.
    pub fn terminate(&mut self) -> ExitStatus {
        let mut child = self.child.take().expect("running");
        rustix::process::kill_process(rustix::process::Pid::from_child(&child), rustix::process::Signal::TERM).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(st) = child.try_wait().unwrap() {
                return st;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("{} did not exit after SIGTERM", self.name);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// The `lindesk-session` binary under test.
pub fn session_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lindesk-session"))
}

/// `lindeskd` from the same target directory (built when the workspace or both packages
/// are tested together).
pub fn lindeskd_bin() -> Option<PathBuf> {
    let p = session_bin().parent()?.join("lindeskd");
    p.exists().then_some(p)
}

pub async fn connect(address: &str) -> zbus::Connection {
    zbus::connection::Builder::address(address).unwrap().build().await.unwrap()
}

/// Wait until `name` has an owner on `conn`.
pub async fn wait_for_name(conn: &zbus::Connection, name: &str, timeout: Duration) {
    let dbus = zbus::fdo::DBusProxy::new(conn).await.unwrap();
    let deadline = Instant::now() + timeout;
    loop {
        let n = zbus::names::BusName::try_from(name).unwrap();
        if dbus.name_has_owner(n).await.unwrap_or(false) {
            return;
        }
        assert!(Instant::now() < deadline, "{name} did not appear within {timeout:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Poll `f` until it returns `Some`, or panic after `timeout`.
pub async fn eventually<T, F, Fut>(what: &str, timeout: Duration, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Like [`eventually`], but `None` after `timeout` instead of a panic.
#[allow(dead_code)]
pub async fn eventually_opt<T, F, Fut>(timeout: Duration, mut f: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f().await {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[allow(unused_imports)]
pub use broker_client::BrokerTestProxy;
#[allow(unused_imports)]
pub use qos_client::QoSTestProxy;

mod qos_client {
    /// Client view of `org.lindesk.QoS1`, including the members `lindesk-ipc`'s proxy lacks.
    #[zbus::proxy(
        interface = "org.lindesk.QoS1",
        default_service = "org.lindesk.QoS1",
        default_path = "/org/lindesk/QoS1"
    )]
    pub trait QoSTest {
        fn set_foreground_process(&self, pid: i32) -> zbus::Result<()>;
        fn set_foreground_app(&self, app_id: &str) -> zbus::Result<()>;
        fn begin_interaction(&self, pid: i32) -> zbus::Result<()>;
        fn end_interaction(&self, pid: i32) -> zbus::Result<()>;
        fn set_class(&self, pid: i32, class: &str) -> zbus::Result<()>;
        fn report_metric(
            &self,
            name: &str,
            value: f64,
            labels: std::collections::HashMap<String, String>,
        ) -> zbus::Result<()>;
        fn status(&self) -> zbus::Result<String>;
        fn explain(&self, what: &str) -> zbus::Result<String>;
        fn recent_events(&self, n: u32) -> zbus::Result<String>;
        fn set_paused(&self, paused: bool, seconds: u32) -> zbus::Result<String>;
        fn get_paused(&self) -> zbus::Result<String>;
        #[zbus(signal)]
        fn level_changed(&self, level: String) -> zbus::Result<()>;
        #[zbus(signal)]
        fn events(&self, json: String) -> zbus::Result<()>;
    }
}

mod broker_client {
    /// Client view of `org.lindesk.Broker1`, including `RecentEvents` and `Events`.
    #[zbus::proxy(
        interface = "org.lindesk.Broker1",
        default_service = "org.lindesk.Broker1",
        default_path = "/org/lindesk/Broker1"
    )]
    pub trait BrokerTest {
        fn register_session(&self) -> zbus::Result<String>;
        fn get_status(&self) -> zbus::Result<String>;
        fn explain(&self, what: &str) -> zbus::Result<String>;
        fn recent_events(&self, n: u32) -> zbus::Result<String>;
        fn reload(&self) -> zbus::Result<()>;
        #[zbus(signal)]
        fn events(&self, json: String) -> zbus::Result<()>;
    }
}

/// A temporary directory on tmpfs when available: the telemetry writer fsyncs, which can
/// stall for many seconds on a saturated disk.
pub fn fast_tempdir() -> tempfile::TempDir {
    let uid = rustix::process::getuid().as_raw();
    [PathBuf::from("/dev/shm"), PathBuf::from(format!("/run/user/{uid}"))]
        .iter()
        .filter(|d| d.is_dir())
        .find_map(|d| tempfile::tempdir_in(d).ok())
        .unwrap_or_else(|| tempfile::tempdir().unwrap())
}
