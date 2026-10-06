//! The `psi` sensor: Pressure Stall Information for the system and for watched cgroups.
//!
//! Every poll reads `/proc/pressure/{cpu,io,memory}` ([`Scope::System`]) and, for each
//! [`WatchedTarget`] with a cgroup path, `<cgroup>/{cpu,io,memory}.pressure`
//! ([`Scope::Cgroup`]). [`PsiTracker`] turns the cumulative `total=` counters into
//! per-interval stall deltas; the first poll of a scope only sets the baseline. Cgroups that
//! disappear, or that the host stops watching, are forgotten so a recreated cgroup starts
//! from a fresh baseline.
//!
//! # What is read when
//!
//! Reading a pressure file is not free: the kernel folds per-CPU counters for that cgroup on
//! every read, and a desktop has a couple of hundred cgroups. Only two things decide
//! anything quickly, so only they are read on every poll, through descriptors kept open
//! (one `pread` each): the system files, and the cgroups of the focused application
//! ([`SensorInput::foreground`] and [`SensorInput::foreground_family`], at most
//! [`HOT_MAX`]). Every other watched cgroup is read once per `background_interval`, spread
//! evenly over the polls in between. Samples carry their own interval, so the stall ratios
//! stay exact at either cadence.
//!
//! # Triggers
//!
//! With `triggers = true` the sensor registers kernel PSI triggers
//! ([`PsiTrigger`]) on the system `io`, `memory` and `cpu` files and on the foreground
//! cgroup, and returns their fds from [`Sensor::wake_fds`] so the host loop wakes as soon as
//! stall builds up instead of waiting for the next tick. The kernel only accepts sub-2 s
//! windows from `CAP_SYS_RESOURCE` holders; without it the window must be a multiple of 2 s.
//! Triggers are armed when the process is privileged (root) or when the configured window
//! meets the unprivileged rule; otherwise the sensor polls only.
//!
//! The kernel reports a trigger at most once per window, and polling the fd consumes the
//! event, so a fired trigger never makes the host spin. [`Sensor::poll`] additionally drains
//! each fd with a zero-timeout poll and reports a `psi_trigger` metric per fired trigger.
//!
//! The foreground cgroup is not part of [`SensorInput`]; the daemon hands it over through
//! the [`ForegroundCgroup`] handle from [`PsiSensor::foreground_handle`]. The sensor
//! re-arms the foreground triggers on the next poll after it changes.
//!
//! # Config
//!
//! ```json
//! {"triggers": true, "stall": "100ms", "window": "1s", "kind": "some", "background_interval": "2s"}
//! ```

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lindesk_core::config::dur;
use lindesk_plugin_api::{
    InitContext, Metric, Plugin, PluginError, PluginInfo, PluginKind, PluginResult, Resource, Scope, Sensor,
    SensorInput, SensorOutput, Target, TimestampMs, WatchedTarget,
};
use lindesk_sys::Roots;
use lindesk_sys::cgroupfs::CgroupFs;
use lindesk_sys::psi::{self, PsiFiles, PsiTracker, PsiTrigger, Stall};
use serde::{Deserialize, Serialize};

/// Plugin name.
pub const NAME: &str = "psi";

/// Name of the metric emitted for every fired trigger.
pub const TRIGGER_METRIC: &str = "psi_trigger";

/// Most cgroups read on every poll (the focused application's units).
pub const HOT_MAX: usize = 16;

/// Order in which system triggers are armed.
const TRIGGER_RESOURCES: [Resource; 3] = [Resource::Io, Resource::Memory, Resource::Cpu];

/// Configuration of the `psi` sensor (the plugin's `[config]` table).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct PsiConfig {
    /// Register PSI triggers and expose them as wake fds.
    pub triggers: bool,
    /// Stall time within `window` that fires a trigger.
    #[serde(with = "dur")]
    pub stall: Duration,
    /// Trigger window. Without `CAP_SYS_RESOURCE` it must be a multiple of 2 s.
    #[serde(with = "dur")]
    pub window: Duration,
    /// Which PSI line the triggers watch.
    pub kind: Stall,
    /// Also arm triggers on the foreground cgroup.
    pub foreground: bool,
    /// How often cgroups other than the focused application's are read. `0s`: every poll.
    #[serde(with = "dur")]
    pub background_interval: Duration,
    /// Override privilege detection (normally: effective uid 0). Mostly for tests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub privileged: Option<bool>,
}

impl Default for PsiConfig {
    fn default() -> Self {
        PsiConfig {
            triggers: true,
            stall: Duration::from_millis(100),
            window: Duration::from_secs(1),
            kind: Stall::Some,
            foreground: true,
            background_interval: Duration::from_secs(2),
            privileged: None,
        }
    }
}

impl PsiConfig {
    /// Parse the plugin's JSON config; `null` yields the defaults.
    pub fn from_json(v: &serde_json::Value) -> Result<Self, String> {
        if v.is_null() {
            return Ok(PsiConfig::default());
        }
        let cfg: PsiConfig = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
        // Validate the privilege-independent bounds now, so a typo fails at startup.
        PsiTrigger::spec(cfg.kind, cfg.stall, cfg.window).map_err(|e| e.to_string())?;
        Ok(cfg)
    }
}

/// Shared slot through which the daemon tells the sensor which cgroup is in the foreground
/// (relative to the cgroup mount). Cheap to clone; clones share the value.
#[derive(Clone, Debug, Default)]
pub struct ForegroundCgroup(Arc<Mutex<Option<String>>>);

impl ForegroundCgroup {
    /// Set (or clear) the foreground cgroup.
    pub fn set(&self, cgroup: Option<String>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = cgroup;
    }

    /// The current foreground cgroup.
    pub fn get(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// One registered trigger and where it points.
#[derive(Debug)]
struct Armed {
    scope: Scope,
    resource: Resource,
    trigger: PsiTrigger,
}

/// The built-in `psi` sensor. See the [module docs](self).
#[derive(Debug)]
pub struct PsiSensor {
    roots: Roots,
    cfg: PsiConfig,
    privileged: bool,
    tracker: PsiTracker,
    /// Descriptors kept open for the scopes read on every poll.
    files: PsiFiles,
    /// Cgroups watched in the previous poll.
    cgroups: BTreeSet<String>,
    /// When each cgroup outside the focused application was last read.
    read_at: HashMap<String, TimestampMs>,
    system_triggers: Vec<Armed>,
    foreground: ForegroundCgroup,
    /// Foreground cgroup the `fg_triggers` are armed on (`None` = not armed).
    fg_armed: Option<String>,
    fg_triggers: Vec<Armed>,
}

impl Default for PsiSensor {
    fn default() -> Self {
        Self::new()
    }
}

impl PsiSensor {
    /// A sensor reading the real host (`/proc`, `/sys/fs/cgroup`).
    pub fn new() -> Self {
        Self::with_roots(Roots::host())
    }

    /// A sensor reading the given roots (fixtures in tests).
    pub fn with_roots(roots: Roots) -> Self {
        PsiSensor {
            roots,
            cfg: PsiConfig::default(),
            privileged: is_root(),
            tracker: PsiTracker::new(),
            files: PsiFiles::new(),
            cgroups: BTreeSet::new(),
            read_at: HashMap::new(),
            system_triggers: Vec::new(),
            foreground: ForegroundCgroup::default(),
            fg_armed: None,
            fg_triggers: Vec::new(),
        }
    }

    /// Replace the configuration (what [`Plugin::init`] does with the plugin config) and
    /// re-arm the system triggers.
    pub fn configure(&mut self, cfg: PsiConfig) {
        self.privileged = cfg.privileged.unwrap_or_else(is_root);
        self.cfg = cfg;
        self.disarm();
        self.arm_system();
    }

    /// The handle through which the daemon reports the foreground cgroup.
    pub fn foreground_handle(&self) -> ForegroundCgroup {
        self.foreground.clone()
    }

    /// The active configuration.
    pub fn config(&self) -> &PsiConfig {
        &self.cfg
    }

    /// True when the configured triggers are acceptable to the kernel for this process.
    pub fn triggers_allowed(&self) -> bool {
        self.cfg.triggers && PsiTrigger::check(self.cfg.stall, self.cfg.window, self.privileged).is_ok()
    }

    /// The pressure files triggers are currently armed on.
    pub fn armed_paths(&self) -> Vec<std::path::PathBuf> {
        self.system_triggers.iter().chain(&self.fg_triggers).map(|a| a.trigger.path().to_path_buf()).collect()
    }

    fn disarm(&mut self) {
        self.system_triggers.clear();
        self.fg_triggers.clear();
        self.fg_armed = None;
    }

    fn arm(&self, scope: &Scope) -> Vec<Armed> {
        let mut out = Vec::new();
        for resource in TRIGGER_RESOURCES {
            let path = match psi::scope_path(&self.roots, scope, resource) {
                Ok(p) if p.exists() => p,
                _ => continue,
            };
            match PsiTrigger::new(&path, self.cfg.kind, self.cfg.stall, self.cfg.window) {
                Ok(trigger) => out.push(Armed { scope: scope.clone(), resource, trigger }),
                Err(e) => tracing::debug!(path = %path.display(), error = %e, "psi: cannot arm trigger"),
            }
        }
        out
    }

    fn arm_system(&mut self) {
        if !self.triggers_allowed() {
            if self.cfg.triggers {
                tracing::info!(
                    window = ?self.cfg.window,
                    "psi: triggers disabled: unprivileged trigger windows must be a multiple of 2s"
                );
            }
            return;
        }
        self.system_triggers = self.arm(&Scope::System);
    }

    /// Drain fired triggers into metrics and follow the foreground cgroup.
    fn service_triggers(&mut self, now: TimestampMs, metrics: &mut Vec<Metric>) {
        let mut fg_gone = false;
        for (is_fg, list) in [(false, &mut self.system_triggers), (true, &mut self.fg_triggers)] {
            list.retain(|a| match a.trigger.fired(Duration::ZERO) {
                Ok(fired) => {
                    if fired {
                        metrics.push(trigger_metric(now, a));
                    }
                    true
                }
                Err(e) => {
                    tracing::debug!(path = %a.trigger.path().display(), error = %e, "psi: trigger source went away");
                    fg_gone |= is_fg;
                    false
                }
            });
        }
        if !(self.cfg.foreground && self.triggers_allowed()) {
            self.fg_triggers.clear();
            self.fg_armed = None;
            return;
        }
        let want = self.foreground.get().filter(|cg| CgroupFs::new(&self.roots.cgroup).exists(cg));
        if want != self.fg_armed || fg_gone {
            self.fg_triggers = match &want {
                Some(cg) => self.arm(&Scope::Cgroup(cg.clone())),
                None => Vec::new(),
            };
            self.fg_armed = want;
        }
    }
}

fn trigger_metric(now: TimestampMs, a: &Armed) -> Metric {
    Metric {
        ts: now,
        name: TRIGGER_METRIC.into(),
        value: 1.0,
        labels: BTreeMap::from([("resource".into(), a.resource.to_string()), ("scope".into(), a.scope.to_string())]),
        plugin: String::new(),
    }
}

/// True when running with effective uid 0.
fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}

/// Where in its interval a cgroup's reads fall: a stable offset in `0..every`.
fn stagger(cgroup: &str, every: i64) -> i64 {
    // FNV-1a; any stable spread will do.
    let h = cgroup.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    if every <= 0 { 0 } else { i64::try_from(h % every.unsigned_abs()).unwrap_or(0) }
}

/// Distinct cgroup paths of the watched targets.
fn watched_cgroups(targets: &[WatchedTarget]) -> BTreeSet<String> {
    targets.iter().filter_map(|w| w.cgroup.clone()).collect()
}

impl Plugin for PsiSensor {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Sensor],
            description: "Pressure Stall Information for the system and watched cgroups, with PSI triggers".into(),
        }
    }

    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
        let cfg = PsiConfig::from_json(&ctx.config).map_err(|e| PluginError::Failed(format!("{NAME}: {e}")))?;
        self.tracker = PsiTracker::new();
        self.files = PsiFiles::new();
        self.cgroups.clear();
        self.read_at.clear();
        self.configure(cfg);
        Ok(())
    }

    fn shutdown(&mut self) {
        self.disarm();
    }
}

impl Sensor for PsiSensor {
    fn poll(&mut self, input: &SensorInput) -> PluginResult<SensorOutput> {
        let now = input.now;
        let mut out = SensorOutput::default();
        for r in Resource::ALL {
            // A missing file means no PSI on this host (or this resource); nothing to report.
            if let Ok(Some(s)) = self.tracker.sample_open(&mut self.files, &self.roots, now, Scope::System, r) {
                out.samples.push(s);
            }
        }

        let cgroup_of = |t: &Target| input.targets.iter().find(|w| &w.target == t).and_then(|w| w.cgroup.clone());
        let fg = input.foreground.as_ref().and_then(cgroup_of);
        if fg.is_some() && fg != self.foreground.get() {
            self.foreground.set(fg.clone());
        }
        let hot: BTreeSet<String> =
            fg.into_iter().chain(input.foreground_family.iter().filter_map(cgroup_of)).take(HOT_MAX).collect();

        let every = i64::try_from(self.cfg.background_interval.as_millis()).unwrap_or(i64::MAX);
        let watched = watched_cgroups(&input.targets);
        let mut gone = Vec::new();
        for cg in &watched {
            let scope = Scope::Cgroup(cg.clone());
            let read = if hot.contains(cg) {
                self.read_at.remove(cg);
                Resource::ALL.map(|r| self.tracker.sample_open(&mut self.files, &self.roots, now, scope.clone(), r))
            } else {
                match self.read_at.get(cg) {
                    Some(at) if now >= *at && now - at < every => continue,
                    Some(_) => self.read_at.insert(cg.clone(), now),
                    // First sight: take the baseline now, and stagger the next read so the
                    // cgroups don't all come due on the same poll.
                    None => self.read_at.insert(cg.clone(), now - stagger(cg, every)),
                };
                Resource::ALL.map(|r| self.tracker.sample(&self.roots, now, scope.clone(), r))
            };
            // A cgroup without the directory is gone; one without a controller's pressure
            // file (or with PSI turned off) just has nothing to report.
            if read.iter().all(Result::is_err) && !CgroupFs::new(&self.roots.cgroup).exists(cg) {
                gone.push(cg.clone());
            }
            out.samples.extend(read.into_iter().filter_map(|r| r.ok().flatten()));
        }
        for cg in gone.iter().chain(self.cgroups.difference(&watched)) {
            self.tracker.forget(&Scope::Cgroup(cg.clone()));
            self.read_at.remove(cg);
        }
        self.files.retain(|s| match s {
            Scope::System => true,
            Scope::Cgroup(cg) => hot.contains(cg) && !gone.contains(cg),
        });
        self.cgroups = watched;

        self.service_triggers(now, &mut out.metrics);
        Ok(out)
    }

    fn wake_fds(&self) -> Vec<RawFd> {
        self.system_triggers.iter().chain(&self.fg_triggers).map(|a| a.trigger.as_raw_fd()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_plugin_api::{API_VERSION, HostCaps, Privilege, Sample};
    use lindesk_sys::fixture::{self, Fixture};

    const UNIT: &str = "/user.slice/user-1000.slice/user@1000.service/app.slice/app-foo-1.scope";

    fn psi(some: u64, full: u64) -> String {
        format!(
            "some avg10=1.50 avg60=1.00 avg300=0.50 total={some}\nfull avg10=0.50 avg60=0.20 avg300=0.10 total={full}\n"
        )
    }

    fn write_psi(f: &Fixture, cg: Option<&str>, some: u64, full: u64) {
        for r in ["cpu", "io", "memory"] {
            match cg {
                None => f.file(&format!("proc/pressure/{r}"), psi(some, full)),
                Some(cg) => {
                    f.file(&format!("sys/fs/cgroup/{}/{r}.pressure", cg.trim_start_matches('/')), psi(some, full))
                }
            };
        }
    }

    fn input(now: TimestampMs, cgroups: &[&str]) -> SensorInput {
        SensorInput {
            now,
            targets: cgroups
                .iter()
                .map(|cg| WatchedTarget {
                    target: Target::Cgroup { path: cg.to_string() },
                    cgroup: Some(cg.to_string()),
                })
                .collect(),
            foreground: None,
            foreground_family: Vec::new(),
        }
    }

    fn ctx(config: serde_json::Value) -> InitContext {
        InitContext {
            api_version: API_VERSION,
            host: "test".into(),
            privilege: Privilege::User,
            uid: Some(1000),
            caps: HostCaps::default(),
            config,
        }
    }

    fn sensor(f: &Fixture, config: serde_json::Value) -> PsiSensor {
        let mut s = PsiSensor::with_roots(f.roots());
        s.init(&ctx(config)).unwrap();
        s
    }

    #[test]
    fn system_and_cgroup_deltas() {
        let dir = crate::actuators::test_dir();
        let (_roots, _) = fixture::pop_os(dir.path());
        let f = Fixture::new(dir.path());
        write_psi(&f, None, 1_000, 100);
        write_psi(&f, Some(UNIT), 50, 0);
        let mut s = sensor(&f, serde_json::json!({"triggers": false, "background_interval": "0s"}));
        assert!(s.wake_fds().is_empty());

        let out = s.poll(&input(1_000, &[UNIT, "/user.slice", "/missing.scope"])).unwrap();
        assert!(out.samples.is_empty(), "first poll only sets baselines");

        write_psi(&f, None, 401_000, 100_100);
        write_psi(&f, Some(UNIT), 250_050, 0);
        let out = s.poll(&input(2_000, &[UNIT, "/user.slice", "/missing.scope"])).unwrap();
        let system: Vec<_> = out.samples.iter().filter(|x| x.scope == Scope::System).collect();
        assert_eq!(system.len(), 3);
        assert!(system.iter().all(|x| x.some_delta_us == 400_000 && x.full_delta_us == Some(100_000)));
        assert!(system.iter().all(|x| x.interval_ms == 1_000 && x.some_avg10 == 1.5));
        let unit: Vec<_> = out.samples.iter().filter(|x| x.scope == Scope::Cgroup(UNIT.into())).collect();
        assert_eq!(unit.len(), 3);
        assert!(unit.iter().all(|x| x.some_delta_us == 250_000));
        // user.slice has (zero) PSI files in the fixture; the missing scope is skipped.
        assert_eq!(out.samples.iter().filter(|x| x.scope == Scope::Cgroup("/user.slice".into())).count(), 3);
        assert!(!out.samples.iter().any(|x| x.scope == Scope::Cgroup("/missing.scope".into())));
    }

    #[test]
    fn vanished_and_unwatched_cgroups_are_forgotten() {
        let dir = crate::actuators::test_dir();
        fixture::pop_os(dir.path());
        let f = Fixture::new(dir.path());
        write_psi(&f, Some(UNIT), 5_000_000, 0);
        let mut s = sensor(&f, serde_json::json!({"triggers": false, "background_interval": "0s"}));
        let count = |out: &SensorOutput| out.samples.iter().filter(|x| x.scope == Scope::Cgroup(UNIT.into())).count();

        s.poll(&input(0, &[UNIT])).unwrap();
        assert_eq!(count(&s.poll(&input(1_000, &[UNIT])).unwrap()), 3);

        // The cgroup goes away and comes back with fresh counters: no bogus delta.
        f.remove(&format!("sys/fs/cgroup{UNIT}"));
        assert_eq!(count(&s.poll(&input(2_000, &[UNIT])).unwrap()), 0);
        write_psi(&f, Some(UNIT), 10, 0);
        assert_eq!(count(&s.poll(&input(3_000, &[UNIT])).unwrap()), 0, "re-baselined");
        write_psi(&f, Some(UNIT), 20, 0);
        let out = s.poll(&input(4_000, &[UNIT])).unwrap();
        assert_eq!(count(&out), 3);
        assert!(out.samples.iter().filter(|x| x.scope != Scope::System).all(|x| x.some_delta_us == 10));

        // No longer watched: forgotten as well.
        s.poll(&input(5_000, &[])).unwrap();
        write_psi(&f, Some(UNIT), 9_999_999, 0);
        assert_eq!(count(&s.poll(&input(6_000, &[UNIT])).unwrap()), 0);
    }

    #[test]
    fn only_the_focused_app_is_read_on_every_poll() {
        let dir = crate::actuators::test_dir();
        fixture::pop_os(dir.path());
        let f = Fixture::new(dir.path());
        let renderers = "/user.slice/user-1000.slice/user@1000.service/app.slice/app-foo-2.scope";
        let build = "/user.slice/user-1000.slice/user@1000.service/app.slice/app-build-3.scope";
        let all = [UNIT, renderers, build];
        let mut s = sensor(&f, serde_json::json!({"triggers": false}));
        assert_eq!(s.config().background_interval, Duration::from_secs(2));

        let focused = |now| SensorInput {
            foreground: Some(Target::Cgroup { path: UNIT.into() }),
            foreground_family: vec![Target::Cgroup { path: renderers.into() }],
            ..input(now, &all)
        };
        let mut seen: BTreeMap<&str, Vec<Sample>> = BTreeMap::new();
        for step in 0..=16u64 {
            for cg in all {
                write_psi(&f, Some(cg), step * 1_000, 0);
            }
            let out = s.poll(&focused(i64::try_from(step).unwrap() * 250)).unwrap();
            for cg in all {
                seen.entry(cg).or_default().extend(
                    out.samples
                        .iter()
                        .filter(|x| x.scope == Scope::Cgroup(cg.into()) && x.resource == Resource::Io)
                        .cloned(),
                );
            }
        }
        // The focused app's units: every poll after the baseline, through open descriptors.
        for cg in [UNIT, renderers] {
            assert_eq!(seen[cg].len(), 16, "{cg}");
            assert!(seen[cg].iter().all(|x| x.interval_ms == 250 && x.some_delta_us == 1_000));
        }
        assert_eq!(s.files.len(), 9, "system + two focused cgroups, three files each");
        // Everything else: once per background interval, and the stall adds up all the same.
        let cold = &seen[build];
        assert_eq!(cold.len(), 2, "{cold:?}");
        assert_eq!(cold[1].interval_ms, 2_000);
        assert_eq!(cold[1].some_delta_us, 8_000);
        assert!(cold.iter().all(|x| x.some_delta_us == u64::from(x.interval_ms) * 4));

        // Focus moves: the build is read every poll now, the browser at the slow cadence.
        let moved = |now| SensorInput { foreground: Some(Target::Cgroup { path: build.into() }), ..input(now, &all) };
        s.poll(&moved(4_250)).unwrap();
        let (mut hot, mut left) = (0, 0);
        for now in (4_500..=6_250).step_by(250) {
            let out = s.poll(&moved(now)).unwrap();
            hot += out.samples.iter().filter(|x| x.scope == Scope::Cgroup(build.into())).count();
            left += out.samples.iter().filter(|x| x.scope == Scope::Cgroup(UNIT.into())).count();
        }
        assert_eq!((hot, left), (8 * 3, 3));
        assert_eq!(s.files.len(), 6);
    }

    #[test]
    fn host_without_psi_reports_nothing() {
        let dir = crate::actuators::test_dir();
        fixture::no_psi(dir.path());
        let f = Fixture::new(dir.path());
        let mut s = sensor(&f, serde_json::json!({"privileged": true}));
        assert!(s.wake_fds().is_empty());
        s.poll(&input(0, &["/user.slice"])).unwrap();
        let out = s.poll(&input(1_000, &["/user.slice"])).unwrap();
        assert!(out.samples.is_empty() && out.metrics.is_empty());
    }

    #[test]
    fn triggers_follow_privilege_rules() {
        let dir = crate::actuators::test_dir();
        fixture::pop_os(dir.path());
        let f = Fixture::new(dir.path());
        // Privileged: the default 1s window is fine.
        let s = sensor(&f, serde_json::json!({"privileged": true}));
        assert_eq!(s.wake_fds().len(), 3);
        let spec = std::fs::read_to_string(dir.path().join("proc/pressure/io")).unwrap();
        assert!(spec.starts_with("some 100000 1000000\0"), "{spec:?}");
        // Unprivileged: 1s is refused by the kernel, so nothing is armed ...
        let s = sensor(&f, serde_json::json!({"privileged": false}));
        assert!(s.wake_fds().is_empty() && !s.triggers_allowed());
        // ... but a 2s window is accepted.
        let s = sensor(&f, serde_json::json!({"privileged": false, "window": "2s", "stall": "150ms", "kind": "full"}));
        assert_eq!(s.wake_fds().len(), 3);
        let spec = std::fs::read_to_string(dir.path().join("proc/pressure/memory")).unwrap();
        assert!(spec.starts_with("full 150000 2000000\0"), "{spec:?}");
        // Disabled.
        let s = sensor(&f, serde_json::json!({"triggers": false, "privileged": true}));
        assert!(s.wake_fds().is_empty());
        // Invalid configs fail init.
        let mut bad = PsiSensor::with_roots(f.roots());
        assert!(bad.init(&ctx(serde_json::json!({"window": "100ms"}))).is_err());
        assert!(bad.init(&ctx(serde_json::json!({"bogus": 1}))).is_err());
    }

    #[test]
    fn foreground_triggers_rearm_on_change() {
        let dir = crate::actuators::test_dir();
        fixture::pop_os(dir.path());
        let f = Fixture::new(dir.path());
        let other = "/user.slice/user-1000.slice/user@1000.service/app.slice/app-bar-2.scope";
        write_psi(&f, Some(UNIT), 0, 0);
        write_psi(&f, Some(other), 0, 0);
        let mut s = sensor(&f, serde_json::json!({"privileged": true}));
        let fg = s.foreground_handle();
        assert_eq!(s.wake_fds().len(), 3);

        fg.set(Some(UNIT.into()));
        s.poll(&input(0, &[])).unwrap();
        assert_eq!(s.wake_fds().len(), 6);
        let unit_io = dir.path().join(format!("sys/fs/cgroup{UNIT}/io.pressure"));
        assert!(std::fs::read_to_string(&unit_io).unwrap().starts_with("some 100000 1000000\0"));
        assert!(s.armed_paths().contains(&unit_io));

        fg.set(Some(other.into()));
        s.poll(&input(1_000, &[])).unwrap();
        assert_eq!(s.wake_fds().len(), 6);
        assert!(!s.armed_paths().contains(&unit_io));

        // A foreground cgroup that does not exist (yet) arms nothing; clearing disarms.
        fg.set(Some("/nope.scope".into()));
        s.poll(&input(2_000, &[])).unwrap();
        assert_eq!(s.wake_fds().len(), 3);
        fg.set(None);
        s.poll(&input(3_000, &[])).unwrap();
        assert_eq!(s.wake_fds().len(), 3);

        // Disabled foreground triggers.
        let mut s = sensor(&f, serde_json::json!({"privileged": true, "foreground": false}));
        s.foreground_handle().set(Some(UNIT.into()));
        s.poll(&input(0, &[])).unwrap();
        assert_eq!(s.wake_fds().len(), 3);
    }

    /// Runs against the real `/proc/pressure` and this process's own cgroup.
    #[test]
    #[ignore = "reads the host's PSI files"]
    fn real_host() {
        let own = std::fs::read_to_string("/proc/self/cgroup").unwrap();
        let cg = lindesk_core::cgroup_path::from_proc_cgroup(&own).unwrap();
        let mut s = PsiSensor::new();
        // Unprivileged hosts need a 2s multiple; root can use the 1s default.
        let window = if is_root() { "1s" } else { "2s" };
        s.init(&ctx(serde_json::json!({"window": window}))).unwrap();
        s.foreground_handle().set(Some(cg.clone()));
        let inp = |now| SensorInput {
            now,
            targets: vec![WatchedTarget { target: Target::Cgroup { path: cg.clone() }, cgroup: Some(cg.clone()) }],
            foreground: None,
            foreground_family: Vec::new(),
        };
        let t0 = lindesk_core::clock::now_ms();
        s.poll(&inp(t0)).unwrap();
        println!("wake fds: {:?} armed on {:?}", s.wake_fds(), s.armed_paths());
        std::thread::sleep(Duration::from_millis(500));
        let out = s.poll(&inp(t0 + 500)).unwrap();
        for x in &out.samples {
            println!(
                "{:<70} {:<6} some_avg10={:>6.2} some={:>4.1}% full={:?}",
                x.scope.to_string(),
                x.resource.to_string(),
                x.some_avg10,
                x.some_ratio() * 100.0,
                x.full_ratio().map(|r| (r * 100.0).round())
            );
        }
        println!("metrics: {:?}", out.metrics);
        assert_eq!(out.samples.iter().filter(|x| x.scope == Scope::System).count(), 3);
        assert_eq!(out.samples.iter().filter(|x| x.scope == Scope::Cgroup(cg.clone())).count(), 3);
        assert_eq!(s.wake_fds().len(), 6, "system + foreground triggers");
    }
}
