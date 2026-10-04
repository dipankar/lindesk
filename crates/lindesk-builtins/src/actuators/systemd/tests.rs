use super::*;
use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::time::Instant;

fn user() -> u32 {
    rustix::process::getuid().as_raw()
}

#[test]
fn cpu_state_composition() {
    // Unset unit: weight, then idle on top, then idle released → weight again.
    let mut st = CpuState::default();
    st.set_base_from_raw(UNSET);
    assert_eq!(st.effective(), UNSET);
    st.weight = Some(20);
    assert_eq!(st.effective(), 20);
    st.idle = Some(true);
    assert_eq!(st.effective(), CPU_WEIGHT_IDLE);
    st.idle = None;
    assert_eq!(st.effective(), 20);
    st.weight = None;
    assert_eq!(st.effective(), UNSET);
    assert!(st.released());

    // uclamp pins the default weight while nothing else sets one.
    st.uclamp[1] = true;
    assert_eq!(st.effective(), DEFAULT_WEIGHT);
    assert!(!st.released());
    st.weight = Some(50);
    assert_eq!(st.effective(), 50);
    st.weight = None;
    st.uclamp[1] = false;
    assert_eq!(st.effective(), UNSET);

    // A unit configured with CPUWeight=300 keeps it; one configured idle stays idle unless
    // lindesk explicitly un-idles it.
    let mut st = CpuState::default();
    st.set_base_from_raw(300);
    st.uclamp[0] = true;
    assert_eq!(st.effective(), 300);
    let mut st = CpuState::default();
    st.set_base_from_raw(CPU_WEIGHT_IDLE);
    assert_eq!(st.effective(), CPU_WEIGHT_IDLE);
    st.weight = Some(80);
    assert_eq!(st.effective(), CPU_WEIGHT_IDLE);
    st.idle = Some(false);
    assert_eq!(st.effective(), 80);
}

#[test]
fn devices_and_targets() {
    assert_eq!(device_of("/dev/block/259:0").as_deref(), Some("259:0"));
    assert_eq!(device_of("/definitely/not/a/device"), None);
    if std::path::Path::new("/dev/null").exists() {
        assert_eq!(device_of("/dev/null").as_deref(), Some("1:3"));
    }
    assert!(check_device("8:0").is_ok());
    assert!(check_device("8").is_err() && check_device("a:1").is_err() && check_device("8:").is_err());

    let me = user();
    let a = SystemdActuator::new(Bus::User);
    assert!(a.manages(&Manager::User(me)));
    assert!(!a.manages(&Manager::User(me + 1)) && !a.manages(&Manager::System));
    assert!(a.unit(&Target::user_unit(me, "x.scope")).is_ok());
    assert!(matches!(a.unit(&Target::system_unit("x.service")), Err(PluginError::Unsupported(_))));
    assert!(matches!(a.unit(&Target::user_unit(me, "x.target")), Err(PluginError::Unsupported(_))));
    assert!(matches!(a.unit(&Target::Cgroup { path: "/x".into() }), Err(PluginError::Unsupported(_))));
    let s = SystemdActuator::new(Bus::System);
    assert!(s.manages(&Manager::System) && !s.manages(&Manager::User(me)));
    assert_eq!(a.caps().targets, ["unit"]);
    assert!(!a.caps().supports_knob(&Knob::Nice));
}

#[test]
fn per_process_knobs_are_unsupported_without_dbus() {
    let me = user();
    let mut a = SystemdActuator::new(Bus::User);
    let t = Target::user_unit(me, "x.scope");
    assert!(matches!(a.apply(&t, &Setting::Nice { value: 1 }), Err(PluginError::Unsupported(_))));
    assert!(matches!(
        a.apply(&t, &Setting::IoPrio { class: lindesk_plugin_api::IoPrioClass::Idle, level: 0 }),
        Err(PluginError::Unsupported(_))
    ));
    assert!(matches!(a.read(&t, &Knob::Nice), Err(PluginError::Unsupported(_))));
    assert!(matches!(a.reset(&t, &Knob::IoPrio, None), Err(PluginError::Unsupported(_))));
    assert!(!a.client.is_connected(), "rejected before touching the bus");
}

#[test]
fn config() {
    let mut a = SystemdActuator::new(Bus::System);
    let ctx = |config| InitContext {
        api_version: lindesk_plugin_api::API_VERSION,
        host: "test".into(),
        privilege: lindesk_plugin_api::Privilege::User,
        uid: Some(user()),
        caps: Default::default(),
        config,
    };
    a.init(&ctx(serde_json::json!({"bus": "user"}))).unwrap();
    assert_eq!(a.bus(), Bus::User);
    a.init(&ctx(serde_json::Value::Null)).unwrap();
    assert_eq!(a.bus(), Bus::User);
    assert!(a.init(&ctx(serde_json::json!({"bus": "session"}))).is_err());
}

// ------------------------------------------------------------------- real machine

/// A transient user scope running `sleep`, stopped on drop.
struct TestScope {
    name: String,
    child: Child,
}

impl TestScope {
    fn start(slice: &str) -> TestScope {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
        let stem = format!("lindesk-it-{}-{}", std::process::id(), nanos % 100_000);
        let child = Command::new("systemd-run")
            .args(["--user", "--scope", "--quiet", "-u", &stem, &format!("--slice={slice}"), "sleep", "120"])
            .stdout(Stdio::null())
            .spawn()
            .expect("systemd-run");
        // From here on the Drop impl stops the unit and reaps the child, even on panic.
        let scope = TestScope { name: format!("{stem}.scope"), child };
        for _ in 0..100 {
            let out = Command::new("systemctl").args(["--user", "is-active", &scope.name]).output().unwrap();
            if String::from_utf8_lossy(&out.stdout).trim() == "active" {
                return scope;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("{} did not become active", scope.name);
    }
}

impl Drop for TestScope {
    fn drop(&mut self) {
        let _ = Command::new("systemctl").args(["--user", "stop", &self.name]).status();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const SHOW: &[&str] = &[
    "CPUWeight",
    "IOWeight",
    "MemoryLow",
    "MemoryHigh",
    "IOReadBandwidthMax",
    "IOWriteBandwidthMax",
    "IOReadIOPSMax",
    "IOWriteIOPSMax",
];
const FILES: &[&str] = &["cpu.weight", "cpu.idle", "cpu.uclamp.max", "io.weight", "io.max", "memory.high"];

fn show(unit: &str) -> BTreeMap<String, String> {
    let mut cmd = Command::new("systemctl");
    cmd.args(["--user", "show", unit]);
    for p in SHOW {
        cmd.args(["-p", p]);
    }
    let out = cmd.output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect()
}

fn files(cg: &str) -> BTreeMap<String, String> {
    FILES
        .iter()
        .map(|f| {
            let p = format!("/sys/fs/cgroup{cg}/{f}");
            (f.to_string(), std::fs::read_to_string(p).map_or("<absent>".into(), |s| s.trim().to_string()))
        })
        .collect()
}

fn timed<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let r = f();
    println!("    {label:<48} {:>7.2} ms", t.elapsed().as_secs_f64() * 1000.0);
    r
}

/// Applies and restores every unit knob on a transient user scope, checking systemd's view
/// and the cgroup files. Run with `--ignored --nocapture` on a systemd host.
#[test]
#[ignore = "creates a transient user scope on the real machine"]
fn real_user_scope_round_trip() {
    let me = user();
    let disk = lindesk_sys::blockdev::devices_for(&Roots::host(), &["/"])
        .first()
        .map(|d| d.dev.to_string())
        .expect("root disk");
    let scope = TestScope::start("background.slice");
    let t = Target::user_unit(me, &scope.name);
    let mut a = SystemdActuator::new(Bus::User);
    timed("connect", || a.connect().unwrap());
    let cg = a.control_group(&t).unwrap();
    println!("unit {} cgroup {cg} disk {disk}", scope.name);

    let show0 = show(&scope.name);
    let files0 = files(&cg);
    println!("before: {show0:?}\n        {files0:?}");

    // uclamp first: the cpu controller is off for this scope, so the actuator must pin
    // CPUWeight=100 to get cpu.uclamp.max, and release the pin on reset.
    let settings = [
        Setting::UclampMax { percent: 60.0 },
        Setting::CpuWeight { value: 20 },
        Setting::IoWeight { value: 20 },
        Setting::MemoryHigh { bytes: Some(1 << 30) },
        Setting::IoMax {
            device: disk.clone(),
            limit: IoLimit { rbps: Some(50 << 20), wiops: Some(500), ..Default::default() },
        },
    ];
    println!("read originals:");
    let originals: Vec<(Knob, Option<Setting>)> = settings
        .iter()
        .map(|s| {
            let k = s.knob();
            let o = timed(&format!("read {k}"), || a.read(&t, &k).unwrap());
            println!("      = {o:?}");
            (k, o)
        })
        .collect();
    println!("apply:");
    for s in &settings {
        timed(&format!("apply {s}"), || a.apply(&t, s).unwrap());
        if matches!(s, Setting::UclampMax { .. }) {
            assert_eq!(show(&scope.name)["CPUWeight"], "100", "pinned for uclamp");
        }
    }
    timed("apply cpu.weight=20 again (idempotent)", || a.apply(&t, &settings[1]).unwrap());

    let show1 = show(&scope.name);
    let files1 = files(&cg);
    println!("applied: {show1:?}\n         {files1:?}");
    assert_eq!(show1["CPUWeight"], "20");
    assert_eq!(show1["IOWeight"], "20");
    assert_eq!(show1["MemoryHigh"], "1073741824");
    assert_eq!(show1["IOReadBandwidthMax"], format!("/dev/block/{disk} 52428800"));
    assert_eq!(show1["IOWriteIOPSMax"], format!("/dev/block/{disk} 500"));
    assert_eq!(files1["cpu.weight"], "20");
    assert_eq!(files1["io.weight"], "default 20");
    assert_eq!(files1["memory.high"], "1073741824");
    assert_eq!(files1["cpu.uclamp.max"], "60.00");
    assert_eq!(files1["io.max"], format!("{disk} rbps=52428800 wbps=max riops=max wiops=500"));
    for (k, _) in &originals {
        let now = a.read(&t, k).unwrap();
        assert_eq!(now.as_ref(), settings.iter().find(|s| &s.knob() == k), "read back {k}");
    }

    // CpuIdle composes with CpuWeight: idle wins, releasing it restores the weight.
    let idle0 = timed("read cpu.idle", || a.read(&t, &Knob::CpuIdle).unwrap());
    timed("apply cpu.idle=1", || a.apply(&t, &Setting::CpuIdle { value: true }).unwrap());
    assert_eq!(show(&scope.name)["CPUWeight"], "idle");
    assert_eq!(files(&cg)["cpu.idle"], "1");
    timed("reset cpu.idle", || a.reset(&t, &Knob::CpuIdle, idle0.as_ref()).unwrap());
    assert_eq!(show(&scope.name)["CPUWeight"], "20");
    assert_eq!(files(&cg)["cpu.idle"], "0");

    println!("reset:");
    for (k, o) in originals.iter().rev() {
        timed(&format!("reset {k} to {o:?}"), || a.reset(&t, k, o.as_ref()).unwrap());
        if k == &Knob::CpuWeight {
            assert_eq!(show(&scope.name)["CPUWeight"], "100", "still pinned while uclamp is held");
        }
    }
    let show2 = show(&scope.name);
    let files2 = files(&cg);
    println!("after:  {show2:?}\n        {files2:?}");
    assert_eq!(show2, show0, "systemd properties restored");
    for f in FILES {
        let (before, after) = (&files0[*f], &files2[*f]);
        // systemd disables an unneeded controller lazily, so a file that was absent may linger
        // with its default value until the parent is re-realised.
        let lingering_default =
            before == "<absent>" && matches!(after.as_str(), "max" | "100" | "0" | "default 100" | "");
        assert!(before == after || lingering_default, "{f}: {before} -> {after}");
    }

    // A vanished unit: apply/read are `gone`, reset is a no-op.
    let ghost = Target::user_unit(me, "lindesk-it-does-not-exist.scope");
    assert!(is_gone(&a.apply(&ghost, &Setting::CpuWeight { value: 50 }).unwrap_err()));
    assert!(is_gone(&a.read(&ghost, &Knob::MemoryHigh).unwrap_err()));
    a.reset(&ghost, &Knob::CpuWeight, None).unwrap();
    drop(scope);
    // systemd unloads a stopped transient unit from its GC queue, shortly after the stop.
    let gone_after = (0..50).find(|_| {
        let g = a.read(&t, &Knob::CpuWeight).is_err_and(|e| is_gone(&e));
        if !g {
            std::thread::sleep(Duration::from_millis(20));
        }
        g
    });
    assert!(gone_after.is_some(), "stopped scope is gone");
    println!("stopped scope reported gone after {} polls", gone_after.unwrap());
}
