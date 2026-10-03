//! Test doubles shared by the integration tests.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use lindesk_plugin_api::*;

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn info(name: &str, kind: PluginKind) -> PluginInfo {
    PluginInfo { name: name.into(), version: "0.0.0".into(), kinds: vec![kind], description: String::new() }
}

/// A sensor returning whatever the test put in `out`.
#[derive(Clone, Default)]
pub struct FakeSensor {
    pub out: Arc<Mutex<SensorOutput>>,
    pub polls: Arc<Mutex<Vec<SensorInput>>>,
}

impl FakeSensor {
    pub fn set_pressure(&self, resource: Resource, some: f32) {
        let mut o = lock(&self.out);
        o.samples.retain(|s| s.resource != resource || s.scope != Scope::System);
        o.samples.push(sample(Scope::System, resource, some));
    }

    pub fn set_usage(&self, target: &Target, usage: Usage) {
        let mut o = lock(&self.out);
        o.usage.retain(|u| &u.target != target);
        o.usage.push(TargetUsage { target: target.clone(), usage });
    }
}

pub fn sample(scope: Scope, resource: Resource, some: f32) -> Sample {
    Sample {
        ts: 0,
        scope,
        resource,
        some_avg10: some,
        full_avg10: None,
        some_delta_us: (some * 10_000.0) as u64,
        full_delta_us: None,
        interval_ms: 1000,
    }
}

impl Plugin for FakeSensor {
    fn info(&self) -> PluginInfo {
        info("fake-sensor", PluginKind::Sensor)
    }
}

impl Sensor for FakeSensor {
    fn poll(&mut self, input: &SensorInput) -> PluginResult<SensorOutput> {
        lock(&self.polls).push(input.clone());
        Ok(lock(&self.out).clone())
    }
}

type PolicyFn = Box<dyn FnMut(&Snapshot) -> PluginResult<PolicyOutput> + Send>;

/// A policy defined by a closure; remembers every snapshot it saw.
pub struct FnPolicy {
    pub name: String,
    pub f: PolicyFn,
    pub seen: Arc<Mutex<Vec<Snapshot>>>,
}

impl FnPolicy {
    pub fn new(
        name: &str,
        f: impl FnMut(&Snapshot) -> PluginResult<PolicyOutput> + Send + 'static,
    ) -> (Self, Arc<Mutex<Vec<Snapshot>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (FnPolicy { name: name.into(), f: Box::new(f), seen: seen.clone() }, seen)
    }
}

impl Plugin for FnPolicy {
    fn info(&self) -> PluginInfo {
        info(&self.name, PluginKind::Policy)
    }
}

impl Policy for FnPolicy {
    fn evaluate(&mut self, s: &Snapshot) -> PluginResult<PolicyOutput> {
        lock(&self.seen).push(s.clone());
        (self.f)(s)
    }
}

pub fn intent(target: &Target, setting: Setting, reason: &str) -> Intent {
    Intent { target: target.clone(), setting, priority: 0, reason: reason.into() }
}

type ClassifyFn = Box<dyn FnMut(&ProcInfo) -> PluginResult<Option<Classification>> + Send>;

/// A classifier defined by a closure; counts calls.
pub struct FnClassifier {
    pub name: String,
    pub f: ClassifyFn,
    pub calls: Arc<Mutex<Vec<String>>>,
}

impl FnClassifier {
    pub fn new(
        name: &str,
        f: impl FnMut(&ProcInfo) -> PluginResult<Option<Classification>> + Send + 'static,
    ) -> (Self, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (FnClassifier { name: name.into(), f: Box::new(f), calls: calls.clone() }, calls)
    }
}

impl Plugin for FnClassifier {
    fn info(&self) -> PluginInfo {
        info(&self.name, PluginKind::Classifier)
    }
}

impl Classifier for FnClassifier {
    fn classify(&mut self, p: &ProcInfo) -> PluginResult<Option<Classification>> {
        lock(&self.calls).push(p.comm.clone());
        (self.f)(p)
    }
}

pub fn class(c: QosClass, confidence: f32) -> Option<Classification> {
    Some(Classification { class: c, confidence, reason: format!("test says {c}") })
}

/// What the recording actuator saw.
#[derive(Default, Debug)]
pub struct ActLog {
    /// Current values; `read` falls back to `defaults`.
    pub values: BTreeMap<(Target, Knob), Setting>,
    pub calls: Vec<String>,
    /// When set, `apply` fails with this message.
    pub fail_apply: Option<String>,
}

#[derive(Clone, Default)]
pub struct RecordingActuator {
    pub log: Arc<Mutex<ActLog>>,
}

pub fn default_for(knob: &Knob) -> Option<Setting> {
    match knob {
        Knob::CpuWeight => Some(Setting::CpuWeight { value: 100 }),
        Knob::IoWeight => Some(Setting::IoWeight { value: 100 }),
        Knob::UclampMin => Some(Setting::UclampMin { percent: 0.0 }),
        Knob::MemoryHigh => Some(Setting::MemoryHigh { bytes: None }),
        _ => None,
    }
}

impl Plugin for RecordingActuator {
    fn info(&self) -> PluginInfo {
        info("recorder", PluginKind::Actuator)
    }
}

impl Actuator for RecordingActuator {
    fn caps(&self) -> ActuatorCaps {
        ActuatorCaps {
            knobs: vec![Knob::CpuWeight, Knob::IoWeight, Knob::UclampMin, Knob::CpuIdle, Knob::MemoryHigh],
            targets: vec!["unit".into()],
        }
    }

    fn read(&mut self, target: &Target, knob: &Knob) -> PluginResult<Option<Setting>> {
        let mut l = lock(&self.log);
        l.calls.push(format!("read {knob} {target}"));
        Ok(l.values.get(&(target.clone(), knob.clone())).cloned().or_else(|| default_for(knob)))
    }

    fn apply(&mut self, target: &Target, setting: &Setting) -> PluginResult<()> {
        let mut l = lock(&self.log);
        if let Some(e) = l.fail_apply.clone() {
            l.calls.push(format!("apply-failed {setting} {target}"));
            return Err(PluginError::Failed(e));
        }
        l.calls.push(format!("apply {setting} {target}"));
        l.values.insert((target.clone(), setting.knob()), setting.clone());
        Ok(())
    }

    fn reset(&mut self, target: &Target, knob: &Knob, original: Option<&Setting>) -> PluginResult<()> {
        let mut l = lock(&self.log);
        l.calls.push(format!(
            "reset {knob} {target} to {}",
            original.map_or_else(|| "default".to_string(), ToString::to_string)
        ));
        match original {
            Some(o) => l.values.insert((target.clone(), knob.clone()), o.clone()),
            None => l.values.remove(&(target.clone(), knob.clone())),
        };
        Ok(())
    }
}

/// A sink that keeps every record.
#[derive(Clone)]
pub struct RecordingSink {
    pub name: String,
    pub tables: Vec<String>,
    pub records: Arc<Mutex<Vec<Record>>>,
    pub flushed: Arc<Mutex<u32>>,
}

impl RecordingSink {
    pub fn new(name: &str, tables: &[&str]) -> Self {
        RecordingSink {
            name: name.into(),
            tables: tables.iter().map(|s| s.to_string()).collect(),
            records: Arc::default(),
            flushed: Arc::default(),
        }
    }

    pub fn tables_seen(&self) -> std::collections::BTreeSet<&'static str> {
        lock(&self.records).iter().map(Record::table).collect()
    }
}

impl Plugin for RecordingSink {
    fn info(&self) -> PluginInfo {
        info(&self.name, PluginKind::Sink)
    }
}

impl Sink for RecordingSink {
    fn tables(&self) -> Vec<String> {
        self.tables.clone()
    }

    fn record(&mut self, records: &[Record]) -> PluginResult<()> {
        lock(&self.records).extend_from_slice(records);
        Ok(())
    }

    fn flush(&mut self) -> PluginResult<()> {
        *lock(&self.flushed) += 1;
        Ok(())
    }
}

pub const SDK_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../sdk/python");

/// Write an executable script plus a manifest into `root/<name>/`, returning the dir.
pub fn write_plugin(root: &Path, name: &str, kinds: &[&str], script: &str, manifest_extra: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let kinds: Vec<String> = kinds.iter().map(|k| format!("\"{k}\"")).collect();
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "[plugin]\nname = \"{name}\"\nversion = \"0.1.0\"\napi = 1\nkinds = [{}]\nexec = \"plugin\"\n{manifest_extra}",
            kinds.join(", ")
        ),
    )
    .unwrap();
    let exec = dir.join("plugin");
    std::fs::write(&exec, script).unwrap();
    std::fs::set_permissions(&exec, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

/// A Python plugin using the SDK from this repository.
pub fn python_sdk_script(body: &str) -> String {
    format!(
        "#!/usr/bin/env python3\nimport sys\nsys.path.insert(0, {SDK_DIR:?})\nimport lindesk_plugin as lp\n\n{body}\n\nif __name__ == '__main__':\n    lp.run(name=sys.argv[0].split('/')[-2])\n"
    )
}

/// Poll `f` until it returns true or ~5 s pass.
pub fn eventually(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..500 {
        if f() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}
