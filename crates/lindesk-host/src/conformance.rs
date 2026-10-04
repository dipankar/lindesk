//! Protocol conformance checks for an external plugin (`lindeskctl plugin test <dir>`).
//!
//! The plugin is spawned exactly as the host would, then: the manifest is validated, the
//! handshake must succeed, each declared kind gets canned requests whose responses must
//! parse into the right types within the plugin's timeouts, an unknown method must yield
//! `METHOD_NOT_FOUND`, bad params must not kill it, stdout must carry only JSON-RPC, and it
//! must exit by itself on `shutdown`. Nothing is ever applied to the system (actuators
//! only get `caps` and `read`).

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lindesk_plugin_api::protocol::{InitializeResult, Response, codes, methods};
use lindesk_plugin_api::{
    API_VERSION, ActuatorCaps, Classification, Event, HostCaps, InitContext, Knob, Level, Manager, Metric, PluginKind,
    PolicyOutput, Privilege, ProcInfo, QosClass, Record, Resource, Sample, Scope, SensorInput, SensorOutput, Snapshot,
    Target, UnitState, Usage, WatchedTarget,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::discovery;
use crate::external::{ExternalOptions, ExternalPlugin, HostSender, LogLine, ShutdownOutcome};
use crate::supervisor::SupervisorConfig;

/// One check.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub duration_ms: f64,
}

/// The outcome of [`run_conformance`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ConformanceReport {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub kinds: Vec<PluginKind>,
    pub checks: Vec<Check>,
    /// The plugin's stderr and log notifications, for diagnosing failures.
    pub logs: Vec<LogLine>,
}

impl ConformanceReport {
    /// True when every check passed.
    pub fn passed(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|c| c.ok)
    }

    pub fn failures(&self) -> impl Iterator<Item = &Check> {
        self.checks.iter().filter(|c| !c.ok)
    }
}

impl fmt::Display for ConformanceReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kinds: Vec<&str> = self.kinds.iter().map(|k| k.as_str()).collect();
        writeln!(
            f,
            "plugin conformance: {} {} ({})",
            self.plugin.as_deref().unwrap_or("?"),
            self.version.as_deref().unwrap_or(""),
            kinds.join(", ")
        )?;
        for c in &self.checks {
            write!(f, "  {} {} ({:.1}ms)", if c.ok { "✓" } else { "✗" }, c.name, c.duration_ms)?;
            if let Some(d) = &c.detail {
                write!(f, "\n      {}", d.replace('\n', "\n      "))?;
            }
            writeln!(f)?;
        }
        let failed = self.failures().count();
        writeln!(f, "{} passed, {failed} failed", self.checks.len() - failed)?;
        if failed > 0 && !self.logs.is_empty() {
            writeln!(f, "plugin output (most recent last):")?;
            for l in self.logs.iter().rev().take(15).collect::<Vec<_>>().into_iter().rev() {
                writeln!(f, "  [{:?}/{:?}] {}", l.source, l.level, l.message)?;
            }
        }
        Ok(())
    }
}

/// Knobs for [`run_conformance_with`].
#[derive(Clone, Debug)]
pub struct ConformanceOptions {
    /// Multiplies the plugin's timeouts (CI on slow machines may use 2-5).
    pub timeout_scale: f64,
    /// `[config]` passed in `initialize` (default: the manifest's).
    pub config: Option<Value>,
}

impl Default for ConformanceOptions {
    fn default() -> Self {
        ConformanceOptions { timeout_scale: 1.0, config: None }
    }
}

/// Run every check against the plugin at `path` (its directory or its `plugin.toml`).
pub fn run_conformance(path: &Path) -> ConformanceReport {
    run_conformance_with(path, &ConformanceOptions::default())
}

struct Runner {
    report: ConformanceReport,
}

impl Runner {
    fn check<T>(&mut self, name: impl Into<String>, f: impl FnOnce() -> Result<T, String>) -> Option<T> {
        let start = Instant::now();
        let r = f();
        let duration_ms = start.elapsed().as_secs_f64() * 1000.0;
        let (ok, detail, value) = match r {
            Ok(v) => (true, None, Some(v)),
            Err(e) => (false, Some(e), None),
        };
        self.report.checks.push(Check { name: name.into(), ok, detail, duration_ms });
        value
    }
}

fn ctx(config: Value) -> InitContext {
    InitContext {
        api_version: API_VERSION,
        host: "conformance".into(),
        privilege: Privilege::User,
        uid: Some(1000),
        caps: HostCaps { cgroup_v2: true, systemd: true, psi: true, io_max: true, ..Default::default() },
        config,
    }
}

/// Like [`run_conformance`] with options.
pub fn run_conformance_with(path: &Path, opts: &ConformanceOptions) -> ConformanceReport {
    let mut r = Runner {
        report: ConformanceReport {
            path: path.to_path_buf(),
            plugin: None,
            version: None,
            kinds: Vec::new(),
            checks: Vec::new(),
            logs: Vec::new(),
        },
    };
    let Some(d) = r.check("plugin.toml is valid", || discovery::inspect_dev(path).map_err(|e| e.reason)) else {
        return r.report;
    };
    r.report.plugin = Some(d.name.clone());
    r.report.version = Some(d.manifest.plugin.version.clone());
    r.report.kinds = d.manifest.plugin.kinds.clone();

    let config = opts.config.clone().unwrap_or_else(|| d.config.clone());
    let mut d = d;
    d.config = config.clone();
    let ext_opts = ExternalOptions {
        host: HostSender::detached(),
        // Never back off: each check should talk to the plugin.
        supervisor: SupervisorConfig {
            backoff_base: Duration::ZERO,
            backoff_max: Duration::ZERO,
            max_failures: usize::MAX,
            failure_window: Duration::from_secs(1),
        },
        timeout_scale: opts.timeout_scale,
    };
    let plugin = ExternalPlugin::from_discovered(&d, ctx(config), ext_opts);
    let init_limit = plugin.timeout(methods::INITIALIZE);
    let init = r.check(format!("initialize handshake (≤{}ms)", init_limit.as_millis()), || {
        plugin.start().map_err(|e| e.to_string())?;
        plugin.init_result().ok_or_else(|| "no initialize result".to_string())
    });
    let Some(init) = init else {
        plugin.shutdown();
        r.report.logs = plugin.recent_logs();
        return r.report;
    };
    r.check("initialize reports the manifest's name and kinds", || {
        check_init(&init, &d.name, &d.manifest.plugin.kinds)
    });

    for kind in d.manifest.plugin.kinds.clone() {
        kind_checks(&mut r, &plugin, kind, &init);
    }

    r.check("unknown methods get METHOD_NOT_FOUND (-32601)", || {
        let resp = plugin
            .call_raw("lindesk.conformance/no-such-method", Value::Null, plugin.timeout("other"))
            .map_err(|e| e.to_string())?;
        match resp.error {
            Some(e) if e.code == codes::METHOD_NOT_FOUND => Ok(()),
            Some(e) => Err(format!("got error code {} ({}), expected {}", e.code, e.message, codes::METHOD_NOT_FOUND)),
            None => Err(format!("got a result ({:?}) instead of an error", resp.result)),
        }
    });

    if let Some(method) = bad_params_method(&d.manifest.plugin.kinds) {
        r.check(format!("survives invalid params to {method}"), || {
            plugin.call_raw(method, json!({"bogus": true}), plugin.timeout(method)).map_err(|e| e.to_string())?;
            // Still alive and answering?
            let resp = plugin
                .call_raw("lindesk.conformance/ping", Value::Null, plugin.timeout("other"))
                .map_err(|e| format!("no longer answering afterwards: {e}"))?;
            resp.error.map(|_| ()).ok_or_else(|| "unknown method answered with a result".to_string())
        });
    }

    r.check("stdout carries only JSON-RPC messages", || match plugin.malformed_lines() {
        0 => Ok(()),
        n => Err(format!("{n} malformed line(s) on stdout; write logs to stderr or use the SDK's log helper")),
    });

    r.check("exits by itself on shutdown (≤500ms, status 0)", || match plugin.shutdown() {
        ShutdownOutcome::Exited { code: Some(0), .. } => Ok(()),
        ShutdownOutcome::Exited { code, .. } => Err(format!("exited with status {code:?}")),
        ShutdownOutcome::Terminated => Err("did not exit within 500ms of shutdown + stdin EOF; needed SIGTERM".into()),
        ShutdownOutcome::Killed => Err("ignored shutdown and SIGTERM; needed SIGKILL".into()),
        ShutdownOutcome::NotRunning => Err("was not running at shutdown (crashed earlier?)".into()),
    });
    r.report.logs = plugin.recent_logs();
    r.report
}

fn check_init(init: &InitializeResult, name: &str, kinds: &[PluginKind]) -> Result<(), String> {
    if init.name != name {
        return Err(format!("initialize says name '{}', plugin.toml says '{name}'", init.name));
    }
    let extra: Vec<String> = init.kinds.iter().filter(|k| !kinds.contains(k)).map(|k| k.to_string()).collect();
    if !extra.is_empty() {
        return Err(format!("initialize reports kinds not declared in plugin.toml: {}", extra.join(", ")));
    }
    Ok(())
}

fn bad_params_method(kinds: &[PluginKind]) -> Option<&'static str> {
    kinds.iter().find_map(|k| match k {
        PluginKind::Classifier => Some(methods::CLASSIFY),
        PluginKind::Policy => Some(methods::EVALUATE),
        PluginKind::Sensor => Some(methods::SENSOR_POLL),
        PluginKind::Actuator => Some(methods::ACTUATOR_READ),
        PluginKind::Sink => Some(methods::SINK_FLUSH),
        PluginKind::Adapter => None,
    })
}

/// Call `method` and decode the result, enforcing the timeout.
fn typed<T: DeserializeOwned>(p: &ExternalPlugin, method: &str, params: impl Serialize) -> Result<T, String> {
    let limit = p.timeout(method);
    let start = Instant::now();
    let resp: Response = p.call_raw(method, params, limit).map_err(|e| match e {
        lindesk_plugin_api::PluginError::Timeout => format!("no reply within {}ms", limit.as_millis()),
        e => e.to_string(),
    })?;
    let took = start.elapsed();
    if let Some(e) = &resp.error {
        return Err(format!("returned error {}: {}", e.code, e.message));
    }
    let v: T = resp.into_result().map_err(|e| format!("result doesn't match the schema: {e}"))?;
    if took > limit {
        return Err(format!("took {}ms, limit is {}ms", took.as_millis(), limit.as_millis()));
    }
    Ok(v)
}

fn unit(name: &str, class: QosClass, comm: &str, foreground: bool) -> UnitState {
    UnitState {
        target: Target::user_unit(1000, name),
        class,
        foreground,
        pressure: Vec::new(),
        usage: Some(Usage { cpu_usage_us: 400_000, io_wbytes: 50 << 20, interval_ms: 1000, ..Default::default() }),
        applied: Vec::new(),
        top_comm: Some(comm.into()),
    }
}

fn sample(resource: Resource, some: f32) -> Sample {
    Sample {
        ts: 1_700_000_000_000,
        scope: Scope::System,
        resource,
        some_avg10: some,
        full_avg10: Some(some / 2.0),
        some_delta_us: (some * 10_000.0) as u64,
        full_delta_us: Some((some * 5_000.0) as u64),
        interval_ms: 1000,
    }
}

fn snapshot(busy: bool) -> Snapshot {
    let caps = ctx(Value::Null).caps;
    if !busy {
        return Snapshot {
            now: 1_700_000_000_000,
            level: Default::default(),
            pressure: vec![],
            units: vec![],
            events: vec![],
            foreground: None,
            caps,
        };
    }
    let fg = unit("app-gnome-org.mozilla.firefox-1234.scope", QosClass::Normal, "firefox", true);
    Snapshot {
        now: 1_700_000_001_000,
        level: Default::default(),
        pressure: vec![sample(Resource::Cpu, 35.0), sample(Resource::Io, 60.0), sample(Resource::Memory, 2.0)],
        units: vec![
            fg.clone(),
            unit("app-cargo-99.scope", QosClass::Batch, "rustc", false),
            unit("background.slice", QosClass::Background, "", false),
        ],
        events: vec![Event::Focus {
            ts: 1_700_000_000_500,
            pid: Some(1234),
            app_id: Some("org.mozilla.firefox".into()),
            target: Some(fg.target),
            source: "conformance".into(),
        }],
        foreground: Some(Target::user_unit(1000, "app-gnome-org.mozilla.firefox-1234.scope")),
        caps,
    }
}

fn proc_info(pid: i32, comm: &str, unit: &str) -> ProcInfo {
    ProcInfo {
        pid,
        uid: 1000,
        comm: comm.into(),
        exe: (pid != 0).then(|| format!("/usr/bin/{comm}")),
        cgroup: format!("/user.slice/user-1000.slice/user@1000.service/app.slice/{unit}"),
        unit: Some(unit.into()),
        slice: Some("app.slice".into()),
        app_id: None,
    }
}

fn kind_checks(r: &mut Runner, p: &ExternalPlugin, kind: PluginKind, init: &InitializeResult) {
    let ms = |m: &str| p.timeout(m).as_millis();
    match kind {
        PluginKind::Classifier => {
            for (what, proc) in [
                ("a regular process", proc_info(4242, "firefox", "app-gnome-firefox-4242.scope")),
                ("a slice pseudo-process (pid 0)", proc_info(0, "", "background.slice")),
            ] {
                r.check(format!("classifier.classify answers {what} (≤{}ms)", ms(methods::CLASSIFY)), || {
                    let c: Option<Classification> = typed(p, methods::CLASSIFY, json!({ "proc": proc }))?;
                    match c {
                        Some(c) if !(0.0..=1.0).contains(&c.confidence) => {
                            Err(format!("confidence {} is outside 0..=1", c.confidence))
                        }
                        _ => Ok(()),
                    }
                });
            }
        }
        PluginKind::Policy => {
            for (what, busy) in [("an empty snapshot", false), ("a busy snapshot", true)] {
                r.check(format!("policy.evaluate answers {what} (≤{}ms)", ms(methods::EVALUATE)), || {
                    let out: PolicyOutput = typed(p, methods::EVALUATE, json!({ "snapshot": snapshot(busy) }))?;
                    let _: Option<Level> = out.level;
                    Ok(())
                });
            }
        }
        PluginKind::Sensor => {
            r.check(format!("sensor.poll returns a SensorOutput (≤{}ms)", ms(methods::SENSOR_POLL)), || {
                let input = SensorInput {
                    now: 1_700_000_000_000,
                    targets: vec![WatchedTarget {
                        target: Target::Unit { manager: Manager::System, name: "background.slice".into() },
                        cgroup: Some("/background.slice".into()),
                    }],
                    foreground: None,
                };
                let _: SensorOutput = typed(p, methods::SENSOR_POLL, &input)?;
                Ok(())
            });
        }
        PluginKind::Sink => {
            r.check(format!("accepts sink.record, then answers sink.flush (≤{}ms)", ms(methods::SINK_FLUSH)), || {
                let records = vec![
                    Record::Pressure(sample(Resource::Io, 42.0)),
                    Record::Unit {
                        ts: 1_700_000_000_000,
                        target: Target::user_unit(1000, "app-cargo-99.scope"),
                        class: QosClass::Batch,
                        foreground: false,
                        usage: Usage::default(),
                        applied: vec![],
                    },
                    Record::Event(Event::LevelChanged {
                        ts: 1_700_000_000_000,
                        from: Level::Green,
                        to: Level::Red,
                        policy: "conformance".into(),
                    }),
                    Record::Metric(Metric {
                        ts: 1_700_000_000_000,
                        name: "conformance".into(),
                        value: 1.0,
                        labels: Default::default(),
                        plugin: "conformance".into(),
                    }),
                ];
                p.notify(methods::SINK_RECORD, json!({ "records": records })).map_err(|e| e.to_string())?;
                let limit = p.timeout(methods::SINK_FLUSH);
                let resp = p.call_raw(methods::SINK_FLUSH, Value::Null, limit).map_err(|e| e.to_string())?;
                match resp.error {
                    None => Ok(()),
                    Some(e) if e.code == codes::UNSUPPORTED => Ok(()),
                    Some(e) => Err(format!("sink.flush returned error {}: {}", e.code, e.message)),
                }
            });
        }
        PluginKind::Adapter => {
            r.check(format!("adapter.start answers (≤{}ms)", ms(methods::ADAPTER_START)), || {
                let _: Value = typed(p, methods::ADAPTER_START, Value::Null)?;
                Ok(())
            });
        }
        PluginKind::Actuator => {
            let caps = r.check("advertises actuator capabilities", || {
                let caps = match &init.actuator {
                    Some(c) => c.clone(),
                    None => typed::<ActuatorCaps>(p, methods::ACTUATOR_CAPS, Value::Null)?,
                };
                if caps.knobs.is_empty() || caps.targets.is_empty() {
                    return Err("caps list no knobs or no target kinds".into());
                }
                Ok(caps)
            });
            if let Some(caps) = caps {
                let knob = caps.knobs[0].clone();
                let knob = if let Knob::IoMax(_) = knob { Knob::IoMax("8:0".into()) } else { knob };
                let target = match caps.targets[0].as_str() {
                    "cgroup" => Target::Cgroup { path: "/lindesk-conformance".into() },
                    "process" => Target::Process { pid: std::process::id() as i32 },
                    _ => Target::system_unit("lindesk-conformance.service"),
                };
                r.check(format!("actuator.read answers (≤{}ms)", ms(methods::ACTUATOR_READ)), || {
                    let limit = p.timeout(methods::ACTUATOR_READ);
                    let resp = p
                        .call_raw(methods::ACTUATOR_READ, json!({"target": target, "knob": knob}), limit)
                        .map_err(|e| e.to_string())?;
                    match resp.error {
                        // A missing unit is a legitimate failure.
                        Some(e) if e.code == codes::FAILED || e.code == codes::UNSUPPORTED => Ok(()),
                        Some(e) => Err(format!("returned error {}: {}", e.code, e.message)),
                        None => resp
                            .into_result::<Option<lindesk_plugin_api::Setting>>()
                            .map(|_| ())
                            .map_err(|e| format!("result doesn't match the schema: {e}")),
                    }
                });
            }
        }
    }
}
