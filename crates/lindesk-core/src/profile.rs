//! CPU profiles: named overlays on `lindesk.toml` with optional auto-selection rules.
//!
//! A profile file has three sections:
//!
//! ```toml
//! [profile]
//! name = "gaming"
//! description = "Max foreground protection while a game is focused"
//! inherits = "latency"          # optional; overlays apply base → parent → child
//!
//! [match]                       # optional; all given criteria must hold
//! foreground_app_id = ["steam_app_*", "*.exe"]
//! priority = 60                 # highest matching priority wins; ties → name
//!
//! [config]                      # same schema as lindesk.toml, merged as an overlay
//! [config.plugins.threshold.config]
//! epoch = "2s"
//! ```
//!
//! Profiles are read from the embedded built-ins ([`builtin_profiles`]), then from each
//! directory in order (`/usr/share/lindesk/profiles.d` → `/etc/lindesk/profiles.d` →
//! `$XDG_CONFIG_HOME/lindesk/profiles.d`). A later profile with the same name replaces an
//! earlier one, so the built-ins work out of the box and every layer can override them.
//!
//! Only one profile is active at a time. With `[general] profile = "auto"` (the default)
//! [`ProfileSelector`] picks the highest-priority profile whose `[match]` holds, with a
//! minimum dwell between switches; any other value pins that profile. A profile without
//! `[match]` is never auto-selected (manual only). When nothing matches, `balanced` is used.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use lindesk_plugin_api::{CpuVendor, Event, Hardware, PowerSource, StorageKind, TimestampMs};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Config, PROFILE_AUTO, merge_json};

/// The fallback profile when nothing matches (and the reference for all others).
pub const DEFAULT_PROFILE: &str = "balanced";
/// Minimum time between two automatic profile switches.
pub const DEFAULT_DWELL: Duration = Duration::from_secs(10);
/// Vendor profile directory (packages install the shipped profiles here).
pub const VENDOR_DIR: &str = "/usr/share/lindesk/profiles.d";
/// Administrator profile directory.
pub const ETC_DIR: &str = "/etc/lindesk/profiles.d";
/// Longest allowed `inherits` chain.
const MAX_DEPTH: usize = 16;

/// The shipped profiles, embedded so a binary works without any installed files.
const BUILTIN_FILES: &[(&str, &str)] = &[
    ("00-balanced.toml", include_str!("../../../packaging/config/profiles.d/00-balanced.toml")),
    ("10-latency.toml", include_str!("../../../packaging/config/profiles.d/10-latency.toml")),
    ("20-battery.toml", include_str!("../../../packaging/config/profiles.d/20-battery.toml")),
    ("30-throughput.toml", include_str!("../../../packaging/config/profiles.d/30-throughput.toml")),
    ("40-gaming.toml", include_str!("../../../packaging/config/profiles.d/40-gaming.toml")),
    ("50-arm-eas.toml", include_str!("../../../packaging/config/profiles.d/50-arm-eas.toml")),
    ("51-x86-hybrid.toml", include_str!("../../../packaging/config/profiles.d/51-x86-hybrid.toml")),
    ("60-hdd.toml", include_str!("../../../packaging/config/profiles.d/60-hdd.toml")),
];

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProfileError {
    #[error("reading {path}: {message}")]
    Read { path: PathBuf, message: String },
    #[error("{path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("{path}: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("unknown profile '{0}'")]
    Unknown(String),
    #[error("profile '{profile}' inherits '{parent}', which does not exist")]
    MissingParent { profile: String, parent: String },
    #[error("inheritance cycle: {}", .0.join(" → "))]
    Cycle(Vec<String>),
    #[error("profile '{profile}': resulting config is invalid: {message}")]
    Config { profile: String, message: String },
    #[error("{path}: profile '{name}' is also defined in {previous} (same directory); the later file wins")]
    Duplicate { name: String, path: PathBuf, previous: PathBuf },
}

/// `[profile]`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProfileMeta {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherits: Option<String>,
}

/// CPU/platform features a `[match]` can require (`features = ["eas"]`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    /// Energy Aware Scheduling is active (uclamp steers tasks between big and little cores).
    Eas,
    /// Intel ITMT / Turbo Boost Max core ranking.
    Itmt,
    /// Simultaneous multithreading.
    Smt,
    /// More than one NUMA node.
    Numa,
    /// zram or zswap in use.
    CompressedSwap,
    /// Laptop/convertible chassis.
    Portable,
    /// Running under a hypervisor or in a container.
    Virtualized,
}

impl Feature {
    pub fn label(self) -> &'static str {
        match self {
            Feature::Eas => "EAS",
            Feature::Itmt => "ITMT",
            Feature::Smt => "SMT",
            Feature::Numa => "NUMA",
            Feature::CompressedSwap => "compressed swap",
            Feature::Portable => "portable chassis",
            Feature::Virtualized => "virtualization",
        }
    }
}

/// `[match]`: every criterion that is given must hold. Lists match when any element does.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MatchRules {
    /// Globs (`*`, `?`; case-insensitive) on the focused window's app id.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub foreground_app_id: Vec<String>,
    /// `ac` or `battery`. An unknown power source never matches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power: Option<PowerSource>,
    /// `uname -m` names: `x86_64`, `aarch64`, `riscv64`, …
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub arch: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub vendor: Vec<CpuVendor>,
    /// Hybrid CPU (Intel P/E cores, ARM big.LITTLE).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hybrid: Option<bool>,
    /// All listed features must be present.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<Feature>,
    /// Any managed block device of one of these kinds.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub storage: Vec<StorageKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_cpus: Option<u32>,
    /// RAM bounds (`"8GiB"` or bytes).
    #[serde(with = "opt_size", skip_serializing_if = "Option::is_none")]
    pub max_mem: Option<u64>,
    #[serde(with = "opt_size", skip_serializing_if = "Option::is_none")]
    pub min_mem: Option<u64>,
    /// Highest matching priority wins; ties are broken by name.
    pub priority: i32,
}

impl MatchRules {
    fn has_criteria(&self) -> bool {
        !self.foreground_app_id.is_empty()
            || self.power.is_some()
            || !self.arch.is_empty()
            || !self.vendor.is_empty()
            || self.hybrid.is_some()
            || !self.features.is_empty()
            || !self.storage.is_empty()
            || self.min_cpus.is_some()
            || self.max_mem.is_some()
            || self.min_mem.is_some()
    }

    /// `Some(reason phrases)` when every given criterion holds.
    pub fn evaluate(&self, ctx: &ProfileContext) -> Option<Vec<String>> {
        if !self.has_criteria() {
            return None;
        }
        let mut phrases = Vec::new();
        if let Some(p) = self.power {
            if p == PowerSource::Unknown || ctx.power != p {
                return None;
            }
            phrases.push(if p == PowerSource::Battery { "battery power".into() } else { "AC power".into() });
        }
        // One phrase for the CPU: "<arch> <vendor> hybrid CPU with <features>".
        let mut cpu = Vec::new();
        if !self.arch.is_empty() {
            if !self.arch.iter().any(|a| a.eq_ignore_ascii_case(&ctx.arch)) {
                return None;
            }
            cpu.push(ctx.arch.clone());
        }
        if !self.vendor.is_empty() {
            if !self.vendor.contains(&ctx.vendor) {
                return None;
            }
            cpu.push(vendor_label(ctx.vendor).to_string());
        }
        if let Some(h) = self.hybrid {
            if ctx.hybrid != h {
                return None;
            }
            cpu.push(if h { "hybrid".into() } else { "uniform".into() });
        }
        if (!self.vendor.is_empty() || self.hybrid.is_some()) && !cpu.is_empty() {
            cpu.push("CPU".into());
        }
        let mut cpu = cpu.join(" ");
        if !self.features.is_empty() {
            if !self.features.iter().all(|f| ctx.features.contains(f)) {
                return None;
            }
            let f = self.features.iter().map(|f| f.label()).collect::<Vec<_>>().join(" and ");
            cpu = if cpu.is_empty() { f } else { format!("{cpu} with {f}") };
        }
        if !cpu.is_empty() {
            phrases.push(cpu);
        }
        if !self.storage.is_empty() {
            let hit: Vec<_> = self.storage.iter().filter(|k| ctx.storage.contains(k)).collect();
            if hit.is_empty() {
                return None;
            }
            phrases.push(format!("{} storage", hit.iter().map(|k| storage_label(**k)).collect::<Vec<_>>().join("/")));
        }
        if let Some(n) = self.min_cpus {
            if ctx.cpus < n {
                return None;
            }
            phrases.push(format!("{} CPUs (≥ {n})", ctx.cpus));
        }
        if let Some(m) = self.max_mem {
            if ctx.mem_total == 0 || ctx.mem_total > m {
                return None;
            }
            phrases.push(format!("{} RAM (≤ {})", fmt_size(ctx.mem_total), fmt_size(m)));
        }
        if let Some(m) = self.min_mem {
            if ctx.mem_total < m {
                return None;
            }
            phrases.push(format!("{} RAM (≥ {})", fmt_size(ctx.mem_total), fmt_size(m)));
        }
        if !self.foreground_app_id.is_empty() {
            let app = ctx.foreground_app_id.as_deref()?;
            if !self.foreground_app_id.iter().any(|g| glob_match(g, app)) {
                return None;
            }
            phrases.push(format!("foreground app {app}"));
        }
        Some(phrases)
    }
}

/// On-disk shape of a profile file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    profile: ProfileMeta,
    #[serde(default, rename = "match")]
    matches: Option<MatchRules>,
    #[serde(default)]
    config: Option<toml::Table>,
}

/// A loaded profile.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Profile {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inherits: Option<String>,
    /// Auto-selection rules; `None` = manual only.
    #[serde(rename = "match", skip_serializing_if = "Option::is_none")]
    pub matches: Option<MatchRules>,
    /// The `[config]` overlay as JSON (an object; `{}` when absent).
    pub config: Value,
    /// Where it came from: a file path, or `<builtin>/NN-name.toml`.
    pub source: PathBuf,
    /// Embedded in the binary (not read from disk).
    pub builtin: bool,
}

impl Profile {
    /// Parse a profile file. `source` is only used for messages and [`Profile::source`].
    pub fn parse(text: &str, source: impl Into<PathBuf>) -> Result<Profile, ProfileError> {
        let path = source.into();
        let file: ProfileFile =
            toml::from_str(text).map_err(|e| ProfileError::Parse { path: path.clone(), message: e.to_string() })?;
        let invalid = |message: String| ProfileError::Invalid { path: path.clone(), message };
        let ProfileMeta { name, description, inherits } = file.profile;
        check_name(&name).map_err(&invalid)?;
        if let Some(parent) = &inherits {
            check_name(parent).map_err(|m| invalid(format!("inherits: {m}")))?;
            if parent == &name {
                return Err(invalid(format!("profile '{name}' inherits itself")));
            }
        }
        if let Some(m) = &file.matches {
            if !m.has_criteria() {
                return Err(invalid(
                    "[match] has no criteria; remove the section to make the profile manual-only".into(),
                ));
            }
            if m.foreground_app_id.iter().any(|g| g.trim().is_empty()) || m.arch.iter().any(|a| a.trim().is_empty()) {
                return Err(invalid("[match] has an empty pattern".into()));
            }
        }
        let table = file.config.unwrap_or_default();
        if table.contains_key("safety") {
            return Err(invalid(
                "[config.safety] is not allowed in a profile: safety caps belong in lindesk.toml".into(),
            ));
        }
        if table.get("general").and_then(|g| g.as_table()).is_some_and(|g| g.contains_key("profile")) {
            return Err(invalid("[config.general] profile cannot be set from a profile".into()));
        }
        let config = serde_json::to_value(&table).map_err(|e| invalid(e.to_string()))?;
        // Catch typos early: the overlay alone must give a valid config.
        let mut base = serde_json::to_value(Config::default()).map_err(|e| invalid(e.to_string()))?;
        apply_overlay(&mut base, &config);
        serde_json::from_value::<Config>(base).map_err(|e| invalid(format!("[config]: {e}")))?;
        Ok(Profile { name, description, inherits, matches: file.matches, config, source: path, builtin: false })
    }

    /// Auto-selectable (has a `[match]`).
    pub fn is_auto(&self) -> bool {
        self.matches.is_some()
    }

    pub fn priority(&self) -> Option<i32> {
        self.matches.as_ref().map(|m| m.priority)
    }
}

/// The shipped profiles, embedded in the binary (`packaging/config/profiles.d/*.toml`).
pub fn builtin_profiles() -> Vec<Profile> {
    BUILTIN_FILES
        .iter()
        .filter_map(|(file, text)| {
            let mut p = Profile::parse(text, Path::new("<builtin>").join(file)).ok()?;
            p.builtin = true;
            Some(p)
        })
        .collect()
}

/// The profile directories for a daemon: vendor and `/etc`, plus the user's directory when
/// `xdg_config_home` is given (lindesk-session).
pub fn default_dirs(xdg_config_home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from(VENDOR_DIR), PathBuf::from(ETC_DIR)];
    if let Some(x) = xdg_config_home {
        dirs.push(x.join("lindesk/profiles.d"));
    }
    dirs
}

/// What auto-selection looks at.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ProfileContext {
    pub power: PowerSource,
    pub arch: String,
    pub vendor: CpuVendor,
    pub hybrid: bool,
    pub features: BTreeSet<Feature>,
    /// Kinds of the managed block devices.
    pub storage: Vec<StorageKind>,
    pub cpus: u32,
    pub mem_total: u64,
    pub foreground_app_id: Option<String>,
}

impl ProfileContext {
    pub fn from_hardware(hw: &Hardware, foreground_app_id: Option<&str>) -> Self {
        let mut features = BTreeSet::new();
        let f = &hw.features;
        for (on, feat) in [
            (f.eas, Feature::Eas),
            (f.itmt, Feature::Itmt),
            (hw.smt, Feature::Smt),
            (f.numa_nodes > 1, Feature::Numa),
            (hw.compressed_swap, Feature::CompressedSwap),
            (hw.portable, Feature::Portable),
            (hw.virtualization.is_some(), Feature::Virtualized),
        ] {
            if on {
                features.insert(feat);
            }
        }
        let mut storage: Vec<StorageKind> = Vec::new();
        for d in &hw.storage {
            if !storage.contains(&d.kind) {
                storage.push(d.kind);
            }
        }
        ProfileContext {
            power: hw.power,
            arch: hw.arch.clone(),
            vendor: hw.vendor,
            hybrid: hw.is_hybrid(),
            features,
            storage,
            cpus: hw.cpus,
            mem_total: hw.mem_total,
            foreground_app_id: foreground_app_id.map(str::to_string),
        }
    }

    /// One-line description, e.g. "AC power; x86_64 AMD, 16 CPUs; NVMe".
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        match self.power {
            PowerSource::Ac => parts.push("AC power".to_string()),
            PowerSource::Battery => parts.push("battery power".to_string()),
            PowerSource::Unknown => {}
        }
        let mut cpu = Vec::new();
        if !self.arch.is_empty() {
            cpu.push(self.arch.clone());
        }
        if self.vendor != CpuVendor::Unknown {
            cpu.push(vendor_label(self.vendor).to_string());
        }
        if self.hybrid {
            cpu.push("hybrid".into());
        }
        let mut cpu = cpu.join(" ");
        if self.cpus > 0 {
            cpu = if cpu.is_empty() { format!("{} CPUs", self.cpus) } else { format!("{cpu}, {} CPUs", self.cpus) };
        }
        if !cpu.is_empty() {
            parts.push(cpu);
        }
        if !self.storage.is_empty() {
            parts.push(self.storage.iter().map(|k| storage_label(*k)).collect::<Vec<_>>().join("/"));
        }
        if parts.is_empty() { "unknown hardware".into() } else { parts.join("; ") }
    }
}

/// Result of [`ProfileSet::select`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Selection {
    /// The chosen profile.
    pub name: String,
    /// Why, e.g. "battery power; aarch64 with EAS".
    pub reason: String,
    /// Every profile whose `[match]` holds, best first (the first is `name`).
    pub matched: Vec<String>,
}

/// One row of [`ProfileSet::list`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ProfileInfo {
    pub name: String,
    pub description: String,
    pub source: String,
    pub builtin: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inherits: Option<String>,
    /// `None` for manual-only profiles.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    /// Has a `[match]` section.
    pub auto: bool,
    /// Its `[match]` holds right now.
    pub matches_now: bool,
    /// Why it matches (when it does).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The currently active profile.
    pub active: bool,
    /// Set when the profile can't be used (broken inheritance, invalid overlay).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// All known profiles, by name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProfileSet {
    profiles: BTreeMap<String, Profile>,
}

impl ProfileSet {
    /// Just the embedded built-ins.
    pub fn builtin() -> Self {
        let mut set = ProfileSet::default();
        for p in builtin_profiles() {
            set.insert(p);
        }
        set
    }

    /// The built-ins, then every `*.toml` in `dirs` (in order; files within a directory by
    /// name). A later profile replaces an earlier one with the same name. Missing
    /// directories are skipped. Errors are returned alongside the usable set: a broken file
    /// is skipped and the earlier definition (if any) stays.
    pub fn load(dirs: &[PathBuf]) -> (ProfileSet, Vec<ProfileError>) {
        let mut set = ProfileSet::builtin();
        let mut errors = Vec::new();
        for dir in dirs {
            let rd = match std::fs::read_dir(dir) {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    errors.push(ProfileError::Read { path: dir.clone(), message: e.to_string() });
                    continue;
                }
            };
            let mut files: Vec<PathBuf> = rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "toml") && p.is_file())
                .collect();
            files.sort();
            let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
            for path in files {
                let text = match std::fs::read_to_string(&path) {
                    Ok(t) => t,
                    Err(e) => {
                        errors.push(ProfileError::Read { path, message: e.to_string() });
                        continue;
                    }
                };
                match Profile::parse(&text, &path) {
                    Ok(p) => {
                        if let Some(prev) = seen.insert(p.name.clone(), path.clone()) {
                            errors.push(ProfileError::Duplicate { name: p.name.clone(), path, previous: prev });
                        }
                        set.insert(p);
                    }
                    Err(e) => errors.push(e),
                }
            }
        }
        for name in set.names() {
            if let Err(e) = set.effective_config(&Config::default(), &name) {
                errors.push(e);
            }
        }
        (set, errors)
    }

    /// Add or replace a profile; returns the one it replaced.
    pub fn insert(&mut self, p: Profile) -> Option<Profile> {
        self.profiles.insert(p.name.clone(), p)
    }

    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.profiles.contains_key(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.profiles.keys().cloned().collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Profile> {
        self.profiles.values()
    }

    /// The inheritance chain of `name`, root ancestor first, `name` last.
    pub fn chain(&self, name: &str) -> Result<Vec<&Profile>, ProfileError> {
        let mut chain = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        let mut cur = self.get(name).ok_or_else(|| ProfileError::Unknown(name.to_string()))?;
        loop {
            if seen.contains(&cur.name) {
                seen.push(cur.name.clone());
                let start = seen.iter().position(|n| n == &cur.name).unwrap_or(0);
                return Err(ProfileError::Cycle(seen[start..].to_vec()));
            }
            if seen.len() >= MAX_DEPTH {
                seen.push(cur.name.clone());
                return Err(ProfileError::Cycle(seen));
            }
            seen.push(cur.name.clone());
            chain.push(cur);
            match &cur.inherits {
                None => break,
                Some(parent) => {
                    cur = self.get(parent).ok_or_else(|| ProfileError::MissingParent {
                        profile: cur.name.clone(),
                        parent: parent.clone(),
                    })?;
                }
            }
        }
        chain.reverse();
        Ok(chain)
    }

    /// `base ⊕ root ancestor ⊕ … ⊕ profile`, re-validated as a [`Config`]. Objects merge
    /// recursively, scalars and arrays replace, except `[[override]]` entries, which are
    /// appended after the base's. `general.profile` keeps the base's value.
    pub fn effective_config(&self, base: &Config, name: &str) -> Result<Config, ProfileError> {
        if name == DEFAULT_PROFILE && !self.contains(name) {
            return Ok(base.clone());
        }
        let chain = self.chain(name)?;
        let err = |message: String| ProfileError::Config { profile: name.to_string(), message };
        let mut v = serde_json::to_value(base).map_err(|e| err(e.to_string()))?;
        for p in chain {
            apply_overlay(&mut v, &p.config);
        }
        serde_json::from_value(v).map_err(|e| err(e.to_string()))
    }

    /// The best auto-selectable profile for `ctx`, or `balanced`.
    pub fn select(&self, ctx: &ProfileContext) -> Selection {
        let mut hits: Vec<(i32, &str, Vec<String>)> = self
            .profiles
            .values()
            .filter_map(|p| {
                let m = p.matches.as_ref()?;
                let phrases = m.evaluate(ctx)?;
                self.chain(&p.name).ok()?;
                Some((m.priority, p.name.as_str(), phrases))
            })
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        let matched: Vec<String> = hits.iter().map(|h| h.1.to_string()).collect();
        match hits.into_iter().next() {
            Some((_, name, phrases)) => Selection { name: name.to_string(), reason: phrases.join("; "), matched },
            None => Selection {
                name: DEFAULT_PROFILE.to_string(),
                reason: format!("default: no other profile matches this machine ({})", ctx.summary()),
                matched,
            },
        }
    }

    /// Every profile with whether it matches `ctx` now. `active` marks the active one.
    pub fn list(&self, ctx: &ProfileContext, active: Option<&str>) -> Vec<ProfileInfo> {
        self.profiles
            .values()
            .map(|p| {
                let phrases = p.matches.as_ref().and_then(|m| m.evaluate(ctx));
                ProfileInfo {
                    name: p.name.clone(),
                    description: p.description.clone(),
                    source: p.source.display().to_string(),
                    builtin: p.builtin,
                    inherits: p.inherits.clone(),
                    priority: p.priority(),
                    auto: p.is_auto(),
                    matches_now: phrases.is_some(),
                    reason: phrases.map(|ph| ph.join("; ")),
                    active: active == Some(p.name.as_str()),
                    error: self.effective_config(&Config::default(), &p.name).err().map(|e| e.to_string()),
                }
            })
            .collect()
    }
}

/// A profile change decided by [`ProfileSelector::update`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ProfileSwitch {
    pub from: Option<String>,
    pub to: String,
    pub reason: String,
    /// Chosen by `general.profile` rather than auto-selection.
    pub pinned: bool,
}

impl ProfileSwitch {
    /// `Event::Notice { name: "profile.changed", data: {from, to, reason, pinned} }`.
    pub fn notice(&self, ts: TimestampMs) -> Event {
        Event::Notice {
            ts,
            name: "profile.changed".into(),
            source: "profiles".into(),
            message: format!("profile {} → {} ({})", self.from.as_deref().unwrap_or("none"), self.to, self.reason),
            data: serde_json::to_value(self).unwrap_or_default(),
        }
    }
}

/// Auto-selection with a minimum dwell between switches, so a flapping input (power
/// cable, alt-tabbing out of a game) can't make the config thrash. Time is injected.
#[derive(Clone, Debug)]
pub struct ProfileSelector {
    dwell_ms: i64,
    active: Option<String>,
    reason: String,
    pinned: bool,
    last_switch: Option<TimestampMs>,
}

impl Default for ProfileSelector {
    fn default() -> Self {
        ProfileSelector::new(DEFAULT_DWELL)
    }
}

impl ProfileSelector {
    pub fn new(min_dwell: Duration) -> Self {
        ProfileSelector {
            dwell_ms: min_dwell.as_millis().min(i64::MAX as u128) as i64,
            active: None,
            reason: String::new(),
            pinned: false,
            last_switch: None,
        }
    }

    /// The active profile (`None` before the first [`Self::update`]).
    pub fn active(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// Why the active profile was chosen.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Re-evaluate. `general_profile` is `[general] profile` (`"auto"` or a name). Returns
    /// a switch to apply (recompute [`ProfileSet::effective_config`], reload the engine and
    /// emit [`ProfileSwitch::notice`]). The first call always returns a switch. Pinning or
    /// unpinning switches immediately; automatic switches wait for the dwell to pass since
    /// the previous switch (the decision is re-made every call, so a short blip is ignored).
    pub fn update(
        &mut self,
        set: &ProfileSet,
        general_profile: &str,
        ctx: &ProfileContext,
        now: TimestampMs,
    ) -> Option<ProfileSwitch> {
        let pinned = general_profile != PROFILE_AUTO;
        let (name, reason) = if pinned {
            if set.chain(general_profile).is_ok() {
                (general_profile.to_string(), "pinned by [general] profile".to_string())
            } else {
                (
                    DEFAULT_PROFILE.to_string(),
                    format!("pinned profile '{general_profile}' is missing or broken; using {DEFAULT_PROFILE}"),
                )
            }
        } else {
            let s = set.select(ctx);
            (s.name, s.reason)
        };
        if self.active.as_deref() == Some(name.as_str()) && self.pinned == pinned {
            self.reason = reason;
            return None;
        }
        let mode_change = self.active.is_none() || self.pinned != pinned;
        if !mode_change
            && let Some(t) = self.last_switch
            && now.saturating_sub(t) < self.dwell_ms
        {
            return None;
        }
        let from = self.active.replace(name.clone());
        self.reason = reason.clone();
        self.pinned = pinned;
        self.last_switch = Some(now);
        if from.as_deref() == Some(name.as_str()) {
            return None; // only the pin mode changed
        }
        Some(ProfileSwitch { from, to: name, reason, pinned })
    }
}

/// Merge a profile overlay onto a config value: objects recursively, `override` appended.
fn apply_overlay(base: &mut Value, overlay: &Value) {
    let mut ov = overlay.clone();
    let extra = ov.as_object_mut().and_then(|o| o.remove("override"));
    merge_json(base, &ov);
    if let Some(Value::Array(items)) = extra
        && let Some(obj) = base.as_object_mut()
    {
        match obj.entry("override").or_insert_with(|| Value::Array(Vec::new())) {
            Value::Array(arr) => arr.extend(items),
            other => *other = Value::Array(items),
        }
    }
}

fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !ok {
        return Err(format!("invalid profile name '{name}' (use lowercase letters, digits, '-' and '_')"));
    }
    if name == PROFILE_AUTO {
        return Err("'auto' is reserved".into());
    }
    Ok(())
}

/// Valid profile name (also what `general.profile` may hold besides "auto").
pub fn is_valid_name(name: &str) -> bool {
    check_name(name).is_ok()
}

/// Case-insensitive glob with `*` and `?`.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

fn vendor_label(v: CpuVendor) -> &'static str {
    match v {
        CpuVendor::Intel => "Intel",
        CpuVendor::Amd => "AMD",
        CpuVendor::Arm => "Arm",
        CpuVendor::Apple => "Apple",
        CpuVendor::Qualcomm => "Qualcomm",
        CpuVendor::Ampere => "Ampere",
        CpuVendor::Other => "other-vendor",
        CpuVendor::Unknown => "unknown-vendor",
    }
}

fn storage_label(k: StorageKind) -> &'static str {
    match k {
        StorageKind::Nvme => "NVMe",
        StorageKind::Ssd => "SSD",
        StorageKind::Hdd => "HDD",
        StorageKind::Emmc => "eMMC",
        StorageKind::Sd => "SD card",
        StorageKind::Virtual => "virtual disk",
        StorageKind::Network => "network",
        StorageKind::Unknown => "unknown",
    }
}

/// `8GiB`, `7.6GiB` (no `.0`).
fn fmt_size(b: u64) -> String {
    let s = bytesize::ByteSize::b(b).display().iec().to_string().replace(' ', "");
    match s.find(".0") {
        Some(i) if s[i + 2..].starts_with(|c: char| c.is_ascii_alphabetic()) => format!("{}{}", &s[..i], &s[i + 2..]),
        _ => s,
    }
}

impl fmt::Display for Selection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.name, self.reason)
    }
}

/// `Option<u64>` sizes.
mod opt_size {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(b) => crate::config::size::serialize(b, s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        if v.is_null() {
            return Ok(None);
        }
        crate::config::size::deserialize(v).map(Some).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests;
