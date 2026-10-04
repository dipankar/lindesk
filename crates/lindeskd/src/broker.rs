//! The broker's part of the engine loop: registered users, their watched units and the
//! per-uid desired sets relayed to session agents.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use lindesk_builtins::actuators::RelayTable;
use lindesk_builtins::env::{SysEnvironment, UserWatch};
use lindesk_core::clock::now_ms;
use lindesk_core::config::Config;
use lindesk_core::reconcile::Entry;
use lindesk_host::{Engine, Environment, StepReport};
use lindesk_ipc::{DesiredSetting, SessionState};
use lindesk_plugin_api::{Event, Level, Privilege, Target};
use serde_json::{Value, json};

use crate::common::calibration::Calibrator;
use crate::common::engine_thread::{Audience, Hooks, LoopCx, Notification, ProfilesView, Reply};
use crate::common::observe::PolicyMetrics;
use crate::common::profiles::Profiles;
use crate::common::setup;
use crate::common::startup::HardwareWatch;

/// Broker-specific engine commands.
#[derive(Debug)]
pub enum BrokerExt {
    /// A session agent of `uid` registered: watch the user's units. Replies with the
    /// current `SessionState` JSON.
    RegisterUser { uid: u32, reply: Reply<String> },
    /// The user's last session agent went away: stop watching, forget relayed state.
    UnregisterUser { uid: u32 },
    /// Focus moved to an app known by id. `uid`: only accept a unit of this user.
    ForegroundApp { app_id: String, uid: Option<u32> },
}

/// The `SessionState` JSON for `uid` from the engine's applied state. The applied state (not
/// the relay table) is authoritative: it includes entries adopted from the state file after a
/// restart and simulated entries in dry-run. Extra fields: `"dry_run"` tells the agent the
/// broker is only observing; `"profile"` is the broker's status `daemon.profile` (in
/// connected mode the broker's profile is the one in effect, and the agent shows it).
pub fn session_state_json<'a>(
    uid: u32,
    level: Level,
    dry_run: bool,
    profile: &Value,
    entries: impl Iterator<Item = &'a Entry>,
) -> String {
    let desired = entries
        .filter(|e| e.target.user() == Some(uid))
        .map(|e| DesiredSetting {
            target: e.target.clone(),
            setting: e.current.clone(),
            provenance: e.provenance.clone(),
        })
        .collect();
    let mut v = serde_json::to_value(SessionState { uid, level, desired }).unwrap_or_else(|_| json!({}));
    v["dry_run"] = json!(dry_run);
    if !profile.is_null() {
        v["profile"] = profile.clone();
    }
    v.to_string()
}

/// The uid owning a cgroup path under `user-<uid>.slice`.
fn cgroup_user(path: &str) -> Option<u32> {
    path.split('/').find_map(|c| c.strip_prefix("user-")?.strip_suffix(".slice")?.parse().ok())
}

/// Who may see a broker event. Events about one user's units or processes go to that user
/// (and root) only; system-scope events (system units, levels, plugin errors, notices) go to
/// everyone. An event naming a process whose owner can't be determined is root-only.
/// `pid_uid` maps a pid to its owner (`/proc`).
pub fn event_audience(e: &Event, pid_uid: impl Fn(i32) -> Option<u32>) -> Audience {
    let by_pid = |pid: i32| match pid_uid(pid) {
        Some(0) => None,
        Some(uid) => Some(uid),
        None => Some(0),
    };
    let by_target = |t: &Target| match t {
        Target::Unit { .. } => t.user(),
        Target::Cgroup { path } => cgroup_user(path),
        Target::Process { pid } => by_pid(*pid),
    };
    match e {
        Event::Focus { target: Some(t), .. } | Event::Interaction { target: Some(t), .. } => by_target(t),
        Event::Focus { pid: Some(pid), .. } | Event::Interaction { pid: Some(pid), .. } => by_pid(*pid),
        // An app id we couldn't resolve to a unit: don't tell other users about it.
        Event::Focus { .. } | Event::Interaction { .. } => Some(0),
        Event::ClassSet { target, .. } | Event::ActionApplied { target, .. } | Event::ActionReverted { target, .. } => {
            by_target(target)
        }
        _ => None,
    }
}

/// Static facts shown in the status.
#[derive(Clone, Debug, Default)]
pub struct HostInfo {
    pub support_level: String,
    pub backend: String,
    pub iocost_enabled: Vec<String>,
    pub conflicts: Vec<String>,
}

pub struct BrokerHooks {
    pub env: Arc<SysEnvironment>,
    pub users: UserWatch,
    pub relay: RelayTable,
    pub metrics: PolicyMetrics,
    pub info: HostInfo,
    pub config_path: PathBuf,
    /// Prepares a freshly loaded config like at start-up (CLI overrides, defaults).
    pub prepare: Box<dyn Fn(&mut Config) + Send>,
    /// Profile selection; `None` = no profile overlay.
    pub profiles: Option<Profiles>,
    pub hardware: Option<HardwareWatch>,
    pub requirements: Vec<Value>,
    pub degraded: Vec<String>,
    pub rules_source: &'static str,
    pub calibrator: Option<Calibrator>,
    sessions: BTreeSet<u32>,
    last_sent: BTreeMap<u32, String>,
}

impl BrokerHooks {
    pub fn new(env: Arc<SysEnvironment>, users: UserWatch, relay: RelayTable, metrics: PolicyMetrics) -> Self {
        BrokerHooks {
            env,
            users,
            relay,
            metrics,
            info: HostInfo::default(),
            config_path: PathBuf::new(),
            prepare: Box::new(|_| {}),
            profiles: None,
            hardware: None,
            requirements: Vec::new(),
            degraded: Vec::new(),
            rules_source: "system",
            calibrator: None,
            sessions: BTreeSet::new(),
            last_sent: BTreeMap::new(),
        }
    }

    fn state(&self, engine: &Engine, uid: u32) -> String {
        let profile = self.profiles.as_ref().map_or(Value::Null, Profiles::status);
        session_state_json(uid, engine.level(), engine.dry_run(), &profile, engine.applied().entries())
    }

    fn unregister(&mut self, engine: &mut Engine, uid: u32, cx: &mut LoopCx<'_>) {
        self.sessions.remove(&uid);
        self.users.remove(uid);
        let mut gone: BTreeSet<Target> =
            engine.applied().entries().filter(|e| e.target.user() == Some(uid)).map(|e| e.target.clone()).collect();
        if let Some(fg) = engine.foreground().filter(|t| t.user() == Some(uid)) {
            gone.insert(fg.clone());
        }
        // The agent restores (or already restored) the real units; the broker just forgets.
        for t in &gone {
            engine.forget_target(t);
        }
        self.relay.clear_user(uid);
        self.last_sent.remove(&uid);
        cx.publish(|p| {
            p.users.remove(&uid);
        });
        tracing::info!(uid, forgotten = gone.len(), "session agent unregistered; no longer watching the user's units");
    }
}

impl Hooks for BrokerHooks {
    type Ext = BrokerExt;

    fn status_extra(&self, _engine: &Engine) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        m.insert("mode".into(), json!("broker"));
        m.insert("sessions".into(), json!(self.sessions));
        m.insert("degraded".into(), json!(self.degraded));
        m.insert("rules".into(), json!(self.rules_source));
        m.insert("profile".into(), self.profiles.as_ref().map_or(Value::Null, Profiles::status));
        m.insert("support_level".into(), json!(self.info.support_level));
        m.insert("backend".into(), json!(self.info.backend));
        m.insert("iocost_enabled".into(), json!(self.info.iocost_enabled));
        m.insert("conflicts".into(), json!(self.info.conflicts));
        let relayed: BTreeMap<String, usize> =
            self.relay.snapshot().into_iter().map(|(uid, s)| (uid.to_string(), s.len())).collect();
        m.insert("relayed".into(), json!(relayed));
        m
    }

    fn epoch(&self, _engine: &Engine) -> (Option<bool>, &'static str) {
        (self.metrics.epoch_active(), "threshold")
    }

    fn audience(&self, event: &Event) -> Audience {
        event_audience(event, |pid| crate::common::proc_uid("/proc".as_ref(), pid))
    }

    fn profiles(&self, engine: &Engine) -> ProfilesView {
        self.profiles
            .as_ref()
            .map(|p| p.view(engine, None))
            .unwrap_or_else(|| ProfilesView { names: Vec::new(), list: json!([]) })
    }

    fn unit_candidates(&self, _engine: &Engine, name: &str) -> Vec<Target> {
        let mut v = vec![Target::system_unit(name)];
        v.extend(self.sessions.iter().map(|uid| Target::user_unit(*uid, name)));
        v
    }

    fn ext(&mut self, engine: &mut Engine, ext: BrokerExt, cx: &mut LoopCx<'_>) {
        match ext {
            BrokerExt::RegisterUser { uid, reply } => {
                if self.sessions.insert(uid) {
                    tracing::info!(uid, "session agent registered; watching the user's units");
                }
                self.users.add(uid);
                // Force a UserState after the next step even if nothing changed.
                self.last_sent.remove(&uid);
                let _ = reply.send(self.state(engine, uid));
                cx.want_step = true;
            }
            BrokerExt::UnregisterUser { uid } => {
                self.unregister(engine, uid, cx);
                cx.want_step = true;
            }
            BrokerExt::ForegroundApp { app_id, uid } => match self.env.target_for_app_id(&app_id) {
                Some(t) if uid.is_none() || t.user() == uid => {
                    engine.push_event(Event::Focus {
                        ts: now_ms(),
                        pid: None,
                        app_id: Some(app_id),
                        target: Some(t),
                        source: "dbus".into(),
                    });
                    cx.want_step = true;
                }
                Some(t) => tracing::debug!(%t, ?uid, "ignoring foreground app of another user"),
                None => tracing::debug!(app_id, "foreground app not found"),
            },
        }
    }

    fn after_step(&mut self, engine: &mut Engine, report: &StepReport, cx: &mut LoopCx<'_>) {
        if let Some(p) = self.profiles.as_mut() {
            p.tick(engine, report.now);
        }
        let epoch = self.metrics.epoch_active();
        if let Some(c) = self.calibrator.as_mut() {
            c.tick(engine, report, epoch);
        }
        let mut states = BTreeMap::new();
        for uid in &self.sessions {
            let json = self.state(engine, *uid);
            if self.last_sent.get(uid) != Some(&json) {
                cx.notify(Notification::UserState { uid: *uid, json: json.clone() });
                self.last_sent.insert(*uid, json.clone());
            }
            states.insert(*uid, json);
        }
        cx.publish(|p| p.users = states);
    }

    fn requirements(&self) -> Value {
        Value::Array(self.requirements.clone())
    }

    fn maintenance(&mut self, engine: &mut Engine) {
        if let Some(h) = &self.hardware {
            h.refresh(engine);
        }
    }

    fn reload(&mut self, engine: &mut Engine) -> Result<String, String> {
        let profiles = &mut self.profiles;
        let mut apply = |engine: &mut Engine, cfg: Config| match profiles.as_mut() {
            Some(p) => p.rebase(engine, cfg, now_ms()),
            None => engine.reload_config(cfg).map_err(|e| e.to_string()),
        };
        setup::reload(engine, &self.config_path, Privilege::System, &*self.prepare, &mut apply)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::RwLock;

    use lindesk_builtins::actuators::RelayActuator;
    use lindesk_host::{EngineOptions, MemoryEnvironment};
    use lindesk_plugin_api::{
        InitContext, Intent, Plugin, PluginInfo, PluginKind, PluginResult, Policy, PolicyOutput, Setting, Snapshot,
    };
    use lindesk_sys::Roots;
    use tokio::sync::{mpsc, oneshot};

    use super::*;
    use crate::common::engine_thread::Published;

    /// Boosts the foreground and idles a system service.
    struct Fixed;
    impl Plugin for Fixed {
        fn info(&self) -> PluginInfo {
            PluginInfo {
                name: "fixed".into(),
                version: "0".into(),
                kinds: vec![PluginKind::Policy],
                description: String::new(),
            }
        }
        fn init(&mut self, _: &InitContext) -> PluginResult<()> {
            Ok(())
        }
    }
    impl Policy for Fixed {
        fn evaluate(&mut self, s: &Snapshot) -> PluginResult<PolicyOutput> {
            let mut intents = vec![Intent {
                target: Target::system_unit("packagekit.service"),
                setting: Setting::CpuWeight { value: 10 },
                priority: 0,
                reason: "batch".into(),
            }];
            if let Some(fg) = &s.foreground {
                intents.push(Intent {
                    target: fg.clone(),
                    setting: Setting::CpuWeight { value: 400 },
                    priority: 0,
                    reason: "foreground".into(),
                });
            }
            Ok(PolicyOutput { intents, level: Some(Level::Yellow), metrics: vec![] })
        }
    }

    fn setup(dry_run: bool) -> (Engine, BrokerHooks, MemoryEnvironment) {
        let mem = MemoryEnvironment::new();
        mem.spawn(2001, "firefox", "/user.slice/user-1000.slice/user@1000.service/app.slice/app-firefox-1.scope");
        let mut config = Config::default();
        config.general.dry_run = dry_run;
        config.telemetry.enabled = false;
        let mut engine =
            Engine::new(config, Box::new(mem.clone()), EngineOptions::new("lindeskd", Privilege::System)).unwrap();
        let relay = RelayActuator::new();
        let table = relay.table();
        engine.registry_mut().register_actuator(Box::new(relay), 100).unwrap();
        engine.registry_mut().register_policy(Box::new(Fixed), 0).unwrap();
        let env = Arc::new(SysEnvironment::new(Roots::under("/nonexistent"), lindesk_builtins::env::Mode::System));
        let hooks = BrokerHooks::new(env, UserWatch::new(), table, PolicyMetrics::default());
        (engine, hooks, mem)
    }

    fn run(dry_run: bool) {
        let (mut engine, mut hooks, _mem) = setup(dry_run);
        let published = RwLock::new(Published::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut cx = LoopCx { published: &published, notify: &tx, want_step: false };

        let (rtx, mut rrx) = oneshot::channel();
        hooks.ext(&mut engine, BrokerExt::RegisterUser { uid: 1000, reply: rtx }, &mut cx);
        assert!(cx.want_step);
        let first: SessionState = SessionState::from_json(&rrx.try_recv().unwrap()).unwrap();
        assert_eq!((first.uid, first.desired.len()), (1000, 0));
        assert!(hooks.users.includes(1000));

        engine.push_event(Event::Focus { ts: 1, pid: Some(2001), app_id: None, target: None, source: "t".into() });
        let report = engine.step(1000);
        hooks.after_step(&mut engine, &report, &mut cx);
        let Ok(Notification::UserState { uid, json }) = rx.try_recv() else { panic!("no UserState") };
        assert_eq!(uid, 1000);
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["dry_run"], dry_run);
        let st = SessionState::from_json(&json).unwrap();
        assert_eq!(st.level, Level::Yellow);
        assert_eq!(st.desired.len(), 1, "only the user's unit, not packagekit: {json}");
        assert_eq!(st.desired[0].target, Target::user_unit(1000, "app-firefox-1.scope"));
        assert_eq!(st.desired[0].setting, Setting::CpuWeight { value: 400 });
        assert_eq!(st.desired[0].provenance.plugin, "fixed");
        assert_eq!(hooks.relay.user(1000).len(), usize::from(!dry_run), "relay table only in real mode");
        assert_eq!(published.read().unwrap().users.len(), 1);

        // Unchanged state: no new signal.
        let report = engine.step(2000);
        hooks.after_step(&mut engine, &report, &mut cx);
        assert!(rx.try_recv().is_err());

        // Unregister: the user's entries are forgotten, not reverted.
        hooks.ext(&mut engine, BrokerExt::UnregisterUser { uid: 1000 }, &mut cx);
        assert!(engine.applied().entries().all(|e| e.target.user().is_none()));
        assert!(!hooks.users.includes(1000));
        assert!(hooks.relay.user(1000).is_empty());
        assert!(published.read().unwrap().users.is_empty());
        let report = engine.step(3000);
        hooks.after_step(&mut engine, &report, &mut cx);
        assert!(rx.try_recv().is_err(), "no states for unregistered users");
        assert_eq!(hooks.unit_candidates(&engine, "x.service"), vec![Target::system_unit("x.service")]);
    }

    #[test]
    fn relays_user_state_real() {
        run(false);
    }

    #[test]
    fn relays_user_state_dry_run() {
        run(true);
    }

    #[test]
    fn state_json_filters_by_uid() {
        let mut st = lindesk_core::reconcile::AppliedState::default();
        let p = lindesk_plugin_api::Provenance { plugin: "threshold".into(), ..Default::default() };
        st.record_apply(&Target::user_unit(1000, "a.scope"), &Setting::IoWeight { value: 300 }, None, &p, 1);
        st.record_apply(&Target::user_unit(1001, "b.scope"), &Setting::IoWeight { value: 300 }, None, &p, 1);
        st.record_apply(&Target::system_unit("c.service"), &Setting::IoWeight { value: 30 }, None, &p, 1);
        let profile = json!({"active": "battery", "pinned": true});
        let text = session_state_json(1001, Level::Red, false, &profile, st.entries());
        let s = SessionState::from_json(&text).unwrap();
        assert_eq!(s.desired.len(), 1);
        assert_eq!(s.desired[0].target, Target::user_unit(1001, "b.scope"));
        assert_eq!(s.level, Level::Red);
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["profile"]["active"], "battery");
    }

    #[test]
    fn events_about_a_user_go_to_that_user() {
        let owner = |pid: i32| match pid {
            1 => Some(0),
            4242 => Some(1000),
            _ => None,
        };
        let applied = |target: Target| Event::ActionApplied {
            ts: 1,
            target,
            setting: Setting::CpuWeight { value: 1 },
            provenance: Default::default(),
        };
        assert_eq!(event_audience(&applied(Target::user_unit(1000, "a.scope")), owner), Some(1000));
        assert_eq!(event_audience(&applied(Target::system_unit("x.service")), owner), None);
        let cg = Target::Cgroup { path: "/user.slice/user-1001.slice/session-2.scope".into() };
        assert_eq!(event_audience(&applied(cg), owner), Some(1001));
        assert_eq!(event_audience(&applied(Target::Cgroup { path: "/system.slice".into() }), owner), None);
        let focus = |pid: Option<i32>, target: Option<Target>| Event::Focus {
            ts: 1,
            pid,
            app_id: Some("org.example.App".into()),
            target,
            source: "dbus".into(),
        };
        assert_eq!(event_audience(&focus(Some(4242), None), owner), Some(1000));
        assert_eq!(event_audience(&focus(Some(1), None), owner), None, "root's processes are system scope");
        assert_eq!(event_audience(&focus(Some(9), None), owner), Some(0), "unknown owner: root only");
        assert_eq!(event_audience(&focus(None, None), owner), Some(0), "unresolved app id: root only");
        assert_eq!(event_audience(&focus(None, Some(Target::user_unit(7, "x.scope"))), owner), Some(7));
        let level = Event::LevelChanged { ts: 1, from: Level::Green, to: Level::Red, policy: "threshold".into() };
        assert_eq!(event_audience(&level, owner), None);
        let notice = Event::Notice {
            ts: 1,
            name: "change.applied".into(),
            source: "changes".into(),
            message: String::new(),
            data: Value::Null,
        };
        assert_eq!(event_audience(&notice, owner), None);
    }
}
