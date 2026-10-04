//! The plugin registry: one ordered list of `Box<dyn Trait>` per kind, plus metadata and
//! health for every plugin, built-in or external.
//!
//! Built-ins are registered with `register_*` (which applies `[plugins.<name>]` config and
//! calls [`Plugin::init`]); external plugins with [`Registry::load_external`], which spawns
//! the process once and registers the same handle for each kind it declares.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use globset::{Glob, GlobSet, GlobSetBuilder};
use lindesk_core::config::{Config, PluginConfig};
use lindesk_plugin_api::{
    API_VERSION, Actuator, Adapter, Classifier, ClassifierSection, HostCaps, InitContext, Plugin, PluginError,
    PluginKind, Policy, Prefilter, Privilege, ProcInfo, Sensor, Sink,
};
use serde::{Deserialize, Serialize};

use crate::discovery::DiscoveredPlugin;
use crate::external::{ExternalOptions, ExternalPlugin, HostSender};
use crate::supervisor::{self, Health, PluginStats, SharedSupervisor, Supervisor, SupervisorConfig};

/// Who provides a plugin.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Origin {
    /// Compiled into the host.
    Builtin,
    /// A process started from a `plugin.toml`.
    External { manifest: PathBuf },
}

/// What the host tells plugins about itself in `initialize`.
#[derive(Clone, Debug, PartialEq)]
pub struct HostContext {
    /// "lindeskd", "lindesk-session" or "simulate".
    pub host: String,
    pub privilege: Privilege,
    pub uid: Option<u32>,
    pub caps: HostCaps,
}

impl HostContext {
    /// The [`InitContext`] for a plugin with the given (merged) config.
    pub fn init_context(&self, config: serde_json::Value) -> InitContext {
        InitContext {
            api_version: API_VERSION,
            host: self.host.clone(),
            privilege: self.privilege,
            uid: self.uid,
            caps: self.caps.clone(),
            config,
        }
    }
}

/// A classifier prefilter compiled to glob sets. A process passes when it matches at least
/// one glob of every non-empty list.
#[derive(Clone, Debug, Default)]
pub struct CompiledPrefilter {
    comm: Option<GlobSet>,
    exe: Option<GlobSet>,
    unit: Option<GlobSet>,
    app_id: Option<GlobSet>,
}

impl CompiledPrefilter {
    pub fn new(p: &Prefilter) -> Result<Self, globset::Error> {
        fn set(globs: &[String]) -> Result<Option<GlobSet>, globset::Error> {
            if globs.is_empty() {
                return Ok(None);
            }
            let mut b = GlobSetBuilder::new();
            for g in globs {
                b.add(Glob::new(g)?);
            }
            b.build().map(Some)
        }
        Ok(CompiledPrefilter { comm: set(&p.comm)?, exe: set(&p.exe)?, unit: set(&p.unit)?, app_id: set(&p.app_id)? })
    }

    pub fn matches(&self, p: &ProcInfo) -> bool {
        fn ok(set: &Option<GlobSet>, v: Option<&str>) -> bool {
            match set {
                None => true,
                Some(s) => v.is_some_and(|v| s.is_match(v)),
            }
        }
        ok(&self.comm, Some(&p.comm))
            && ok(&self.exe, p.exe.as_deref())
            && ok(&self.unit, p.unit.as_deref())
            && ok(&self.app_id, p.app_id.as_deref())
    }
}

/// Per-registration metadata.
#[derive(Clone, Debug)]
pub struct SlotMeta {
    pub name: String,
    pub kind: PluginKind,
    pub priority: i32,
    /// The priority came from `[plugins.<name>] priority` and overrides intent priorities.
    pub priority_overridden: bool,
    pub origin: Origin,
    /// Classifiers: host-side filter; `None` = every process.
    pub prefilter: Option<CompiledPrefilter>,
    /// Classifiers: verdicts below this confidence are ignored.
    pub min_confidence: f32,
    /// Sensors: minimum time between polls.
    pub interval: Duration,
    pub(crate) next_due: Option<i64>,
    pub(crate) sup: SharedSupervisor,
    pub(crate) external: Option<ExternalPlugin>,
}

impl SlotMeta {
    /// May the engine call this plugin now?
    pub fn is_available(&self) -> bool {
        match &self.external {
            Some(e) => e.is_available(),
            None => supervisor::lock(&self.sup).is_available(Instant::now()),
        }
    }

    pub fn is_builtin(&self) -> bool {
        self.origin == Origin::Builtin
    }

    /// Run a call, recording statistics for built-ins (externals record their own).
    pub(crate) fn guard<R>(&self, f: impl FnOnce() -> Result<R, PluginError>) -> Result<R, PluginError> {
        if self.external.is_some() {
            return f();
        }
        let start = Instant::now();
        let r = f();
        supervisor::lock(&self.sup).on_call(start.elapsed(), r.as_ref().map(|_| ()));
        r
    }
}

/// A registered plugin of one kind.
pub struct Slot<T: ?Sized> {
    pub meta: SlotMeta,
    pub plugin: Box<T>,
}

/// Registration failures.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("a {kind} named '{name}' is already registered")]
    Duplicate { name: String, kind: PluginKind },
    #[error("plugin '{name}' failed to initialise: {source}")]
    Init { name: String, source: PluginError },
    #[error("plugin '{name}' refused: {reason}")]
    Refused { name: String, reason: String },
    #[error("plugin '{name}': invalid prefilter: {source}")]
    Prefilter { name: String, source: globset::Error },
}

/// One row of the plugin health table.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PluginHealth {
    pub name: String,
    pub kinds: Vec<PluginKind>,
    pub origin: Origin,
    /// `starting`, `running`, `backoff`, `disabled`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub stats: PluginStats,
}

struct Record {
    origin: Origin,
    kinds: BTreeSet<PluginKind>,
    sup: SharedSupervisor,
    external: Option<ExternalPlugin>,
}

/// All plugins of a host, by kind.
pub struct Registry {
    host: HostContext,
    plugin_config: BTreeMap<String, PluginConfig>,
    ext_opts: ExternalOptions,
    pub(crate) sensors: Vec<Slot<dyn Sensor>>,
    pub(crate) adapters: Vec<Slot<dyn Adapter>>,
    pub(crate) classifiers: Vec<Slot<dyn Classifier>>,
    pub(crate) policies: Vec<Slot<dyn Policy>>,
    pub(crate) actuators: Vec<Slot<dyn Actuator>>,
    pub(crate) sinks: Vec<Slot<dyn Sink>>,
    records: BTreeMap<String, Record>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry").field("host", &self.host.host).field("plugins", &self.names()).finish()
    }
}

impl Registry {
    /// A registry for `host`. `config.plugins` supplies enable/priority/config overrides;
    /// `host_tx` receives notifications from external plugins.
    pub fn new(host: HostContext, config: &Config, host_tx: HostSender, supervisor: SupervisorConfig) -> Self {
        Registry {
            host,
            plugin_config: config.plugins.clone(),
            ext_opts: ExternalOptions { host: host_tx, supervisor, timeout_scale: 1.0 },
            sensors: Vec::new(),
            adapters: Vec::new(),
            classifiers: Vec::new(),
            policies: Vec::new(),
            actuators: Vec::new(),
            sinks: Vec::new(),
            records: BTreeMap::new(),
        }
    }

    /// Relax (or tighten) every external timeout, e.g. for `simulate` on a slow machine.
    pub fn set_timeout_scale(&mut self, scale: f64) {
        self.ext_opts.timeout_scale = scale;
    }

    pub fn host(&self) -> &HostContext {
        &self.host
    }

    pub(crate) fn set_caps(&mut self, caps: HostCaps) {
        self.host.caps = caps;
    }

    pub(crate) fn set_plugin_config(&mut self, config: &Config) {
        self.plugin_config = config.plugins.clone();
    }

    /// Re-initialise built-in sensors, classifiers and policies whose
    /// `[plugins.<name>] config` differs between `old` and `new` (e.g. a profile switch).
    /// Actuators, sinks and adapters keep their state. Returns the plugins re-initialised
    /// and any init errors.
    pub(crate) fn reconfigure_builtins(
        &mut self,
        old: &Config,
        new: &Config,
    ) -> Vec<(String, Result<(), PluginError>)> {
        let changed = |name: &str| old.plugin(name).config != new.plugin(name).config;
        let host = self.host.clone();
        let mut out = Vec::new();
        macro_rules! each {
            ($($list:ident),*) => {$(
                for s in self.$list.iter_mut().filter(|s| s.meta.is_builtin() && changed(&s.meta.name)) {
                    let ctx = host.init_context(new.plugin(&s.meta.name).config);
                    out.push((s.meta.name.clone(), s.plugin.init(&ctx)));
                }
            )*};
        }
        each!(sensors, classifiers, policies);
        out
    }

    /// Names of all known plugins (including disabled ones).
    pub fn names(&self) -> Vec<String> {
        self.records.keys().cloned().collect()
    }

    /// The handle of an external plugin.
    pub fn external(&self, name: &str) -> Option<&ExternalPlugin> {
        self.records.get(name).and_then(|r| r.external.as_ref())
    }

    /// All external plugin handles.
    pub fn externals(&self) -> impl Iterator<Item = &ExternalPlugin> {
        self.records.values().filter_map(|r| r.external.as_ref())
    }

    fn record(
        &mut self,
        name: &str,
        kind: PluginKind,
        origin: Origin,
        external: Option<ExternalPlugin>,
    ) -> SharedSupervisor {
        let restartable = external.is_some();
        let sup = external
            .as_ref()
            .map(|e| e.supervisor())
            .unwrap_or_else(|| Supervisor::shared(self.ext_opts.supervisor.clone(), restartable));
        let r = self.records.entry(name.to_string()).or_insert_with(|| Record {
            origin,
            kinds: BTreeSet::new(),
            sup,
            external,
        });
        r.kinds.insert(kind);
        r.sup.clone()
    }

    fn check_duplicate(&self, name: &str, kind: PluginKind) -> Result<(), RegistryError> {
        if let Some(r) = self.records.get(name) {
            if r.kinds.contains(&kind) {
                return Err(RegistryError::Duplicate { name: name.into(), kind });
            }
            if r.external.is_some() || matches!(r.origin, Origin::External { .. }) {
                return Err(RegistryError::Refused {
                    name: name.into(),
                    reason: "an external plugin already uses this name".into(),
                });
            }
        }
        Ok(())
    }

    /// Common built-in registration: config lookup, init, metadata. `None` = disabled.
    fn builtin(
        &mut self,
        plugin: &mut dyn Plugin,
        kind: PluginKind,
        priority: i32,
    ) -> Result<Option<SlotMeta>, RegistryError> {
        let name = plugin.info().name;
        self.check_duplicate(&name, kind)?;
        let pc = self.plugin_config.get(&name).cloned().unwrap_or_default();
        let sup = self.record(&name, kind, Origin::Builtin, None);
        if !pc.enabled {
            supervisor::lock(&sup).disable(format!("disabled by [plugins.{name}] in the config"));
            return Ok(None);
        }
        let ctx = self.host.init_context(pc.config.clone());
        if let Err(source) = plugin.init(&ctx) {
            supervisor::lock(&sup).disable(format!("init failed: {source}"));
            return Err(RegistryError::Init { name, source });
        }
        Ok(Some(SlotMeta {
            name,
            kind,
            priority: pc.priority.unwrap_or(priority),
            priority_overridden: pc.priority.is_some(),
            origin: Origin::Builtin,
            prefilter: None,
            min_confidence: 0.0,
            interval: Duration::from_secs(1),
            next_due: None,
            sup,
            external: None,
        }))
    }

    /// Register a built-in sensor polled at most every `interval`.
    pub fn register_sensor(&mut self, mut s: Box<dyn Sensor>, interval: Duration) -> Result<(), RegistryError> {
        if let Some(mut meta) = self.builtin(s.as_mut(), PluginKind::Sensor, 0)? {
            meta.interval = interval;
            self.sensors.push(Slot { meta, plugin: s });
        }
        Ok(())
    }

    /// Register a built-in adapter (started by `Engine::start`).
    pub fn register_adapter(&mut self, mut a: Box<dyn Adapter>) -> Result<(), RegistryError> {
        if let Some(meta) = self.builtin(a.as_mut(), PluginKind::Adapter, 0)? {
            self.adapters.push(Slot { meta, plugin: a });
        }
        Ok(())
    }

    /// Register a built-in classifier with no prefilter and `min_confidence = 0.5`.
    pub fn register_classifier(&mut self, c: Box<dyn Classifier>, priority: i32) -> Result<(), RegistryError> {
        let section = ClassifierSection { priority, min_confidence: 0.5, prefilter: Prefilter::default() };
        self.register_classifier_with(c, &section)
    }

    /// Register a built-in classifier with an explicit priority, threshold and prefilter.
    pub fn register_classifier_with(
        &mut self,
        mut c: Box<dyn Classifier>,
        section: &ClassifierSection,
    ) -> Result<(), RegistryError> {
        let name = c.info().name;
        let prefilter = compile_prefilter(&name, &section.prefilter)?;
        if let Some(mut meta) = self.builtin(c.as_mut(), PluginKind::Classifier, section.priority)? {
            meta.prefilter = prefilter;
            meta.min_confidence = section.min_confidence;
            insert_sorted(&mut self.classifiers, Slot { meta, plugin: c });
        }
        Ok(())
    }

    /// Register a built-in policy. Built-ins conventionally use priority 0.
    pub fn register_policy(&mut self, mut p: Box<dyn Policy>, priority: i32) -> Result<(), RegistryError> {
        if let Some(meta) = self.builtin(p.as_mut(), PluginKind::Policy, priority)? {
            insert_sorted(&mut self.policies, Slot { meta, plugin: p });
        }
        Ok(())
    }

    /// Register a built-in actuator. The engine uses the highest-priority available actuator
    /// whose caps cover the target kind and knob.
    pub fn register_actuator(&mut self, mut a: Box<dyn Actuator>, priority: i32) -> Result<(), RegistryError> {
        // Built-in actuators are allowed on user hosts (they act through user systemd);
        // only *external* actuators are refused there.
        if let Some(meta) = self.builtin(a.as_mut(), PluginKind::Actuator, priority)? {
            insert_sorted(&mut self.actuators, Slot { meta, plugin: a });
        }
        Ok(())
    }

    /// Register a built-in sink.
    pub fn register_sink(&mut self, mut s: Box<dyn Sink>) -> Result<(), RegistryError> {
        if let Some(meta) = self.builtin(s.as_mut(), PluginKind::Sink, 0)? {
            self.sinks.push(Slot { meta, plugin: s });
        }
        Ok(())
    }

    /// Spawn an external plugin and register it for every kind in its manifest. A failed
    /// first start is not an error: the plugin stays registered in backoff and is retried.
    /// Returns `None` for disabled plugins (they still appear in [`Registry::health`]).
    pub fn load_external(&mut self, d: &DiscoveredPlugin) -> Result<Option<ExternalPlugin>, RegistryError> {
        let m = &d.manifest.plugin;
        if self.host.privilege == Privilege::User {
            if m.privilege == Privilege::System {
                return Err(RegistryError::Refused {
                    name: d.name.clone(),
                    reason: "system plugin on a user host".into(),
                });
            }
            if m.kinds.contains(&PluginKind::Actuator) {
                return Err(RegistryError::Refused {
                    name: d.name.clone(),
                    reason: "actuator plugins need system privilege".into(),
                });
            }
        }
        if self.records.contains_key(&d.name) {
            return Err(RegistryError::Refused {
                name: d.name.clone(),
                reason: "a plugin with this name is already registered".into(),
            });
        }
        let prefilter = match &d.manifest.classifier {
            Some(c) => compile_prefilter(&d.name, &c.prefilter)?,
            None => None,
        };
        let origin = Origin::External { manifest: d.manifest_path.clone() };
        let ext = ExternalPlugin::from_discovered(d, self.host.init_context(d.config.clone()), self.ext_opts.clone());
        for kind in &m.kinds {
            self.record(&d.name, *kind, origin.clone(), Some(ext.clone()));
        }
        if !d.enabled {
            let reason = d.disabled_reason.clone().unwrap_or_else(|| "disabled".into());
            supervisor::lock(&ext.supervisor()).disable(reason);
            return Ok(None);
        }
        if let Err(e) = ext.start() {
            tracing::warn!(plugin = %d.name, "plugin failed to start (will retry): {e}");
        }
        for kind in m.kinds.clone() {
            let meta = SlotMeta {
                name: d.name.clone(),
                kind,
                priority: d.priority(kind),
                priority_overridden: d.priority_override.is_some(),
                origin: origin.clone(),
                prefilter: if kind == PluginKind::Classifier { prefilter.clone() } else { None },
                min_confidence: d.manifest.classifier.as_ref().map_or(0.5, |c| c.min_confidence),
                interval: Duration::from_millis(d.manifest.sensor.as_ref().map_or(1000, |s| s.interval_ms)),
                next_due: None,
                sup: ext.supervisor(),
                external: Some(ext.clone()),
            };
            let b = ext.clone();
            match kind {
                PluginKind::Sensor => self.sensors.push(Slot { meta, plugin: Box::new(b) }),
                PluginKind::Adapter => self.adapters.push(Slot { meta, plugin: Box::new(b) }),
                PluginKind::Classifier => insert_sorted(&mut self.classifiers, Slot { meta, plugin: Box::new(b) }),
                PluginKind::Policy => insert_sorted(&mut self.policies, Slot { meta, plugin: Box::new(b) }),
                PluginKind::Actuator => insert_sorted(&mut self.actuators, Slot { meta, plugin: Box::new(b) }),
                PluginKind::Sink => self.sinks.push(Slot { meta, plugin: Box::new(b) }),
            }
        }
        Ok(Some(ext))
    }

    /// The health table: one row per plugin name.
    pub fn health(&self) -> Vec<PluginHealth> {
        let now = Instant::now();
        self.records
            .iter()
            .map(|(name, r)| {
                let s = supervisor::lock(&r.sup);
                let detail = match s.health() {
                    Health::Disabled { reason } => Some(reason.clone()),
                    Health::Backoff { .. } => s
                        .backoff_remaining(now)
                        .map(|d| format!("retrying in {}ms", d.as_millis()))
                        .or_else(|| Some("retry pending".into())),
                    _ => None,
                };
                PluginHealth {
                    name: name.clone(),
                    kinds: r.kinds.iter().copied().collect(),
                    origin: r.origin.clone(),
                    state: s.health().as_str().into(),
                    detail,
                    pid: r.external.as_ref().and_then(|e| e.pid()),
                    stats: s.stats(),
                }
            })
            .collect()
    }

    /// Health row for one plugin.
    pub fn plugin_health(&self, name: &str) -> Option<PluginHealth> {
        self.health().into_iter().find(|h| h.name == name)
    }

    /// Shut every plugin down: built-ins get [`Plugin::shutdown`], externals the stop sequence.
    pub fn shutdown(&mut self) {
        for s in &mut self.adapters {
            s.plugin.stop();
        }
        macro_rules! builtins {
            ($($list:ident),*) => {$(
                for s in self.$list.iter_mut().filter(|s| s.meta.external.is_none()) {
                    s.plugin.shutdown();
                }
            )*};
        }
        builtins!(sensors, adapters, classifiers, policies, actuators, sinks);
        for r in self.records.values() {
            if let Some(e) = &r.external {
                e.shutdown();
            }
        }
    }
}

fn compile_prefilter(name: &str, p: &Prefilter) -> Result<Option<CompiledPrefilter>, RegistryError> {
    if p.is_empty() {
        return Ok(None);
    }
    CompiledPrefilter::new(p).map(Some).map_err(|source| RegistryError::Prefilter { name: name.into(), source })
}

/// Keep lists ordered by priority (high first), then name, then registration order.
fn insert_sorted<T: ?Sized>(v: &mut Vec<Slot<T>>, slot: Slot<T>) {
    let key = |m: &SlotMeta| (std::cmp::Reverse(m.priority), m.name.clone());
    let pos = v.iter().position(|s| key(&s.meta) > key(&slot.meta)).unwrap_or(v.len());
    v.insert(pos, slot);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefilter_requires_every_nonempty_list() {
        let f = CompiledPrefilter::new(&Prefilter {
            comm: vec!["*.exe".into(), "steam*".into()],
            unit: vec!["app-*.scope".into()],
            ..Default::default()
        })
        .unwrap();
        let mut p = ProcInfo { comm: "game.exe".into(), unit: Some("app-steam-1.scope".into()), ..Default::default() };
        assert!(f.matches(&p));
        p.unit = None;
        assert!(!f.matches(&p), "unit list is non-empty, so a missing unit fails");
        p.unit = Some("app-x.scope".into());
        p.comm = "firefox".into();
        assert!(!f.matches(&p));
    }
}
