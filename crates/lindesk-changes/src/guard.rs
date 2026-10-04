//! Guardrails: what an op may change, and to what.
//!
//! * Config edits are applied to the TOML document (comments kept), then the result must
//!   parse as a [`Config`] (unknown keys rejected), pass the optional
//!   [`GuardContext::config_validator`] (the daemons plug in override-glob compilation and
//!   plugin-config validation), and leave `[safety]` no looser than before.
//! * Runtime ops are allow-listed: sysctls with ranges, schedulers from the device's own
//!   `queue/scheduler`, EPP/governor from the values the CPU advertises.
//! * Changeset-wide: at most [`Limits::max_ops`] ops; [`Limits::rate_count`] changesets per
//!   [`Limits::rate_window`] per origin; High risk from MCP needs a ttl.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use lindesk_core::config::{Config, PROFILE_AUTO};
use lindesk_core::profile::is_valid_name;
use lindesk_core::safety::SafetyCaps;
use lindesk_plugin_api::Hardware;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::apply::{doc_write, sysctl_path};
use crate::model::{ChangeOp, Origin, Rejection, Risk, Rule, Scope};
use crate::roots::{ConfigPaths, Roots};
use crate::toml_doc;

/// Changeset-wide limits.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Limits {
    /// Ops per changeset.
    pub max_ops: usize,
    /// Changesets per origin within `rate_window` (reverts don't count).
    pub rate_count: usize,
    #[serde(with = "lindesk_core::config::dur")]
    pub rate_window: Duration,
    /// Longest dead-man ttl.
    #[serde(with = "lindesk_core::config::dur")]
    pub max_ttl: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_ops: 16,
            rate_count: 10,
            rate_window: Duration::from_secs(600),
            max_ttl: Duration::from_secs(24 * 3600),
        }
    }
}

/// The ttl lindesk-mcp should pass when the agent gives none.
pub const DEFAULT_MCP_TTL: Duration = Duration::from_secs(600);

/// Allow-listed sysctls and their inclusive ranges.
pub const SYSCTL_ALLOW: &[(&str, i64, i64)] = &[
    ("vm.swappiness", 0, 200),
    ("vm.page-cluster", 0, 3),
    ("vm.watermark_scale_factor", 10, 1000),
    ("vm.vfs_cache_pressure", 50, 200),
];

/// zswap compressors lindesk may select (further limited to those in `/proc/crypto`).
pub const ZSWAP_COMPRESSORS: &[&str] = &["lzo", "lzo-rle", "lz4", "lz4hc", "zstd", "842", "deflate"];
/// Allowed `zswap.max_pool_percent`.
pub const ZSWAP_POOL_PERCENT: (u8, u8) = (1, 50);

/// Extra validation of the resulting config (override globs, plugin configs, …).
pub type ConfigValidator = Arc<dyn Fn(&Config) -> Result<(), String> + Send + Sync>;

/// Everything the guardrails look at.
#[derive(Clone)]
pub struct GuardContext {
    /// Scope of the store the plan is for (the daemon's scope).
    pub scope: Scope,
    /// Who is asking (rate limits, ttl rules).
    pub origin: Origin,
    pub roots: Roots,
    pub config_paths: ConfigPaths,
    /// EPP/governor availability.
    pub hardware: Hardware,
    /// Known profile names; `None` skips the existence check.
    pub profiles: Option<BTreeSet<String>>,
    pub config_validator: Option<ConfigValidator>,
}

impl fmt::Debug for GuardContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GuardContext")
            .field("scope", &self.scope)
            .field("origin", &self.origin)
            .field("roots", &self.roots)
            .field("config_paths", &self.config_paths)
            .field("profiles", &self.profiles)
            .field("config_validator", &self.config_validator.is_some())
            .finish_non_exhaustive()
    }
}

impl GuardContext {
    pub fn new(scope: Scope, origin: Origin, roots: Roots, config_paths: ConfigPaths, hardware: Hardware) -> Self {
        GuardContext { scope, origin, roots, config_paths, hardware, profiles: None, config_validator: None }
    }

    pub fn with_profiles<I: IntoIterator<Item = S>, S: Into<String>>(mut self, names: I) -> Self {
        self.profiles = Some(names.into_iter().map(Into::into).collect());
        self
    }

    pub fn with_config_validator(mut self, f: impl Fn(&Config) -> Result<(), String> + Send + Sync + 'static) -> Self {
        self.config_validator = Some(Arc::new(f));
        self
    }

    pub fn with_origin(mut self, origin: Origin) -> Self {
        self.origin = origin;
        self
    }
}

fn reject(rule: Rule, msg: impl Into<String>) -> Rejection {
    Rejection::new(None, rule, msg)
}

/// Validate one op on its own. Config ops are also simulated against the current file.
/// The returned rejection has no index; [`crate::ChangeStore::plan`] fills it in.
pub fn validate(op: &ChangeOp, ctx: &GuardContext) -> Result<Risk, Rejection> {
    if op.scope() != ctx.scope {
        return Err(reject(
            Rule::Scope,
            format!("{} is a {}-scope op but this is the {} daemon", op.kind(), op.scope(), ctx.scope),
        ));
    }
    match op {
        ChangeOp::ConfigSet { key, value, .. } => {
            check_config_key(key)?;
            if value.is_null() {
                return Err(reject(Rule::ConfigInvalid, "value is null; use config_unset to remove a key"));
            }
            if key == "general.profile" {
                check_profile(value.as_str().unwrap_or(""), ctx)?;
            }
        }
        ChangeOp::ConfigUnset { key, .. } => check_config_key(key)?,
        ChangeOp::ProfileUse { name, .. } => check_profile(name, ctx)?,
        ChangeOp::ClassOverride { unit, .. } => check_glob(unit)?,
        ChangeOp::Sysctl { key, value, .. } => {
            let Some(&(_, lo, hi)) = SYSCTL_ALLOW.iter().find(|(k, ..)| *k == key.as_str()) else {
                let allowed: Vec<_> = SYSCTL_ALLOW.iter().map(|a| a.0).collect();
                return Err(reject(
                    Rule::SysctlNotAllowed,
                    format!("sysctl {key} is not allow-listed (allowed: {})", allowed.join(", ")),
                ));
            };
            let v: i64 = value
                .trim()
                .parse()
                .map_err(|_| reject(Rule::OutOfRange, format!("{key} = '{value}' is not an integer")))?;
            if !(lo..=hi).contains(&v) {
                return Err(reject(Rule::OutOfRange, format!("{key} = {v} is outside {lo}..={hi}")));
            }
            if !sysctl_path(&ctx.roots, key).is_some_and(|p| p.exists()) {
                return Err(reject(Rule::NotAvailable, format!("{key} does not exist on this kernel")));
            }
        }
        ChangeOp::IoScheduler { device, scheduler, .. } => {
            let (name, choices) = scheduler_choices(&ctx.roots, device)?;
            if !choices.iter().any(|c| c == scheduler) {
                return Err(reject(
                    Rule::SchedulerUnavailable,
                    format!("{name} offers {} (not '{scheduler}')", choices.join(", ")),
                ));
            }
        }
        ChangeOp::Zswap { enabled, compressor, max_pool_percent } => {
            if enabled.is_none() && compressor.is_none() && max_pool_percent.is_none() {
                return Err(reject(Rule::Zswap, "zswap op changes nothing"));
            }
            if !ctx.roots.sys.join("module/zswap/parameters").is_dir() {
                return Err(reject(Rule::NotAvailable, "zswap is not available on this kernel"));
            }
            if let Some(c) = compressor {
                if !ZSWAP_COMPRESSORS.contains(&c.as_str()) {
                    return Err(reject(
                        Rule::Zswap,
                        format!("compressor '{c}' is not allowed ({})", ZSWAP_COMPRESSORS.join(", ")),
                    ));
                }
                if let Ok(crypto) = std::fs::read_to_string(ctx.roots.proc.join("crypto")) {
                    let known = crypto
                        .lines()
                        .any(|l| l.split_once(':').is_some_and(|(k, v)| k.trim() == "name" && v.trim() == c));
                    if !known {
                        return Err(reject(Rule::Zswap, format!("compressor '{c}' is not in /proc/crypto")));
                    }
                }
            }
            if let Some(p) = max_pool_percent {
                let (lo, hi) = ZSWAP_POOL_PERCENT;
                if !(lo..=hi).contains(p) {
                    return Err(reject(Rule::OutOfRange, format!("max_pool_percent = {p} is outside {lo}..={hi}")));
                }
            }
        }
        ChangeOp::CpuEpp { preference } => {
            let avail = &ctx.hardware.features.epp_available;
            if avail.is_empty() {
                return Err(reject(Rule::EppUnavailable, "this CPU exposes no energy_performance_preference"));
            }
            if !avail.contains(preference) {
                return Err(reject(
                    Rule::EppUnavailable,
                    format!("EPP '{preference}' is not available ({})", avail.join(", ")),
                ));
            }
        }
        ChangeOp::CpuGovernor { governor } => {
            let avail = &ctx.hardware.features.governors_available;
            if !avail.contains(governor) {
                return Err(reject(
                    Rule::GovernorUnavailable,
                    format!("governor '{governor}' is not available ({})", avail.join(", ")),
                ));
            }
        }
    }
    if op.is_config() {
        let check = check_config(&[(0, op)], ctx);
        if let Some(r) = check.rejections.into_iter().next() {
            return Err(Rejection { index: None, ..r });
        }
    }
    Ok(op.risk())
}

fn check_config_key(key: &str) -> Result<(), Rejection> {
    let parts = toml_doc::split_key(key).map_err(|m| reject(Rule::ConfigKey, m))?;
    match parts[0].as_str() {
        "override" => Err(reject(Rule::ConfigKey, "edit [[override]] with class_override")),
        "general" | "io" | "safety" | "telemetry" | "plugins" => Ok(()),
        other => Err(reject(Rule::ConfigKey, format!("unknown section '{other}'"))),
    }
}

fn check_profile(name: &str, ctx: &GuardContext) -> Result<(), Rejection> {
    if name == PROFILE_AUTO {
        return Ok(());
    }
    if !is_valid_name(name) {
        return Err(reject(Rule::ProfileUnknown, format!("invalid profile name '{name}'")));
    }
    if let Some(known) = &ctx.profiles
        && !known.contains(name)
    {
        let list: Vec<_> = known.iter().map(String::as_str).collect();
        return Err(reject(
            Rule::ProfileUnknown,
            format!("unknown profile '{name}' (known: auto, {})", list.join(", ")),
        ));
    }
    Ok(())
}

fn check_glob(g: &str) -> Result<(), Rejection> {
    let ok = !g.is_empty()
        && g.len() <= 256
        && !g.chars().any(|c| c.is_control() || c.is_whitespace())
        && g.matches('[').count() == g.matches(']').count()
        && g != "*";
    if ok { Ok(()) } else { Err(reject(Rule::Glob, format!("unit glob '{g}' is invalid or too broad"))) }
}

/// Result of simulating config ops.
#[derive(Clone, Debug, Default)]
pub struct ConfigCheck {
    /// The file now ("" when missing).
    pub before: String,
    /// The file after the accepted ops.
    pub after: String,
    pub rejections: Vec<Rejection>,
}

/// Apply config ops (with their plan indices) to the scope's file in memory and validate
/// the result: every intermediate document must parse as a [`Config`], the final one must
/// pass the validator, and `[safety]` must not loosen.
pub fn check_config(ops: &[(usize, &ChangeOp)], ctx: &GuardContext) -> ConfigCheck {
    let mut out = ConfigCheck::default();
    let all = |out: &mut ConfigCheck, rule: Rule, msg: String| {
        for (i, _) in ops {
            out.rejections.push(Rejection::new(Some(*i), rule, msg.clone()));
        }
    };
    let Some(path) = ctx.config_paths.get(ctx.scope) else {
        all(&mut out, Rule::NotAvailable, format!("no {} config file is configured", ctx.scope));
        return out;
    };
    out.before = match crate::roots::read_opt(path) {
        Ok(t) => t.unwrap_or_default(),
        Err(e) => {
            all(&mut out, Rule::NotAvailable, e.to_string());
            return out;
        }
    };
    let before_cfg = match Config::from_toml(&out.before) {
        Ok(c) => c,
        Err(e) => {
            all(&mut out, Rule::ConfigInvalid, format!("{} is invalid; fix it first: {e}", path.display()));
            return out;
        }
    };
    let Ok(mut doc) = toml_doc::parse(&out.before) else {
        all(&mut out, Rule::ConfigInvalid, format!("{} can't be edited", path.display()));
        return out;
    };
    for (i, op) in ops {
        let saved = doc.clone();
        let res = doc_write(&mut doc, op, &op.target_value())
            .map_err(|m| (Rule::ConfigInvalid, m))
            .and_then(|_| Config::from_toml(&doc.to_string()).map(|_| ()).map_err(|e| classify(&e, path)));
        if let Err((rule, msg)) = res {
            out.rejections.push(Rejection::new(Some(*i), rule, msg));
            doc = saved;
        }
    }
    out.after = doc.to_string();
    let Ok(after_cfg) = Config::from_toml(&out.after) else { return out };
    if let Err(msg) = check_safety(&before_cfg.safety, &after_cfg.safety) {
        let idx = ops.iter().find(|(_, op)| op.config_key().is_some_and(|k| k.starts_with("safety"))).map(|(i, _)| *i);
        out.rejections.push(Rejection::new(idx, Rule::SafetyLoosened, msg));
    }
    if let Some(v) = &ctx.config_validator
        && let Err(msg) = v(&after_cfg)
    {
        out.rejections.push(Rejection::new(None, Rule::ConfigInvalid, msg));
    }
    out
}

fn classify(e: &toml::de::Error, path: &Path) -> (Rule, String) {
    let msg = e.message().trim().to_string();
    let rule = if msg.contains("unknown field") || msg.contains("unknown variant") {
        Rule::ConfigKey
    } else {
        Rule::ConfigInvalid
    };
    (rule, format!("{}: {msg}", path.display()))
}

/// Which way a `[safety]` field moves when it gets *safer*.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Tighter {
    Lower,
    Higher,
}

/// Every `[safety]` field and its tightening direction:
///
/// | field | tighter | why |
/// |---|---|---|
/// | `cpu_weight_min`, `io_weight_min` | higher | throttled work keeps a bigger share |
/// | `cpu_weight_max`, `io_weight_max` | lower | boosts are smaller |
/// | `uclamp_min_max` | lower | smaller frequency/placement boosts |
/// | `uclamp_max_floor` | higher | capped work keeps more capacity |
/// | `io_bps_floor`, `io_iops_floor` | higher | io.max throttles starve less |
/// | `memory_high_floor`, `memory_high_ratio_floor` | higher | memory.high reclaims less |
/// | `memory_low_max_ratio` | lower | less RAM can be reserved for protected units |
/// | `nice_min` | higher | less negative niceness |
pub const SAFETY_DIRECTIONS: &[(&str, Tighter)] = &[
    ("cpu_weight_min", Tighter::Higher),
    ("cpu_weight_max", Tighter::Lower),
    ("io_weight_min", Tighter::Higher),
    ("io_weight_max", Tighter::Lower),
    ("uclamp_min_max", Tighter::Lower),
    ("uclamp_max_floor", Tighter::Higher),
    ("io_bps_floor", Tighter::Higher),
    ("io_iops_floor", Tighter::Higher),
    ("memory_high_floor", Tighter::Higher),
    ("memory_high_ratio_floor", Tighter::Higher),
    ("memory_low_max_ratio", Tighter::Lower),
    ("nice_min", Tighter::Higher),
];

/// A changed `[safety]` field.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SafetyChange {
    pub field: String,
    pub before: f64,
    pub after: f64,
    pub tighter: bool,
}

/// The `[safety]` fields that differ, with whether each change tightens.
pub fn compare_safety(before: &SafetyCaps, after: &SafetyCaps) -> Vec<SafetyChange> {
    let b = serde_json::to_value(before).unwrap_or_default();
    let a = serde_json::to_value(after).unwrap_or_default();
    let num = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(f64::NAN);
    SAFETY_DIRECTIONS
        .iter()
        .filter_map(|&(field, dir)| {
            let (x, y) = (num(&b, field), num(&a, field));
            if x == y {
                return None;
            }
            let tighter = match dir {
                Tighter::Lower => y < x,
                Tighter::Higher => y > x,
            };
            Some(SafetyChange { field: field.to_string(), before: x, after: y, tighter })
        })
        .collect()
}

/// Ok when `after` is no looser than `before` in any field and still coherent.
pub fn check_safety(before: &SafetyCaps, after: &SafetyCaps) -> Result<(), String> {
    let loose: Vec<String> = compare_safety(before, after)
        .into_iter()
        .filter(|c| !c.tighter)
        .map(|c| format!("{} {} → {}", c.field, c.before, c.after))
        .collect();
    if !loose.is_empty() {
        return Err(format!("[safety] may only be tightened through changes; loosens {}", loose.join(", ")));
    }
    let s = after;
    let mut bad = Vec::new();
    if s.cpu_weight_min > s.cpu_weight_max || !(1..=10_000).contains(&s.cpu_weight_max) {
        bad.push("cpu_weight_min..cpu_weight_max must be a range within 1..=10000");
    }
    if s.io_weight_min > s.io_weight_max || !(1..=10_000).contains(&s.io_weight_max) {
        bad.push("io_weight_min..io_weight_max must be a range within 1..=10000");
    }
    if !(0.0..=100.0).contains(&s.uclamp_min_max) || !(0.0..=100.0).contains(&s.uclamp_max_floor) {
        bad.push("uclamp limits must be percentages");
    }
    if !(0.0..=1.0).contains(&s.memory_high_ratio_floor) || !(0.0..=1.0).contains(&s.memory_low_max_ratio) {
        bad.push("memory ratios must be in 0..=1");
    }
    if !(-20..=19).contains(&s.nice_min) {
        bad.push("nice_min must be in -20..=19");
    }
    if bad.is_empty() { Ok(()) } else { Err(bad.join("; ")) }
}

/// Resolve `MAJ:MIN` or a device name to the kernel name (`sda`, `nvme0n1`).
pub fn resolve_device(roots: &Roots, device: &str) -> Result<String, Rejection> {
    let unknown = || reject(Rule::DeviceUnknown, format!("unknown block device '{device}'"));
    if let Some((maj, min)) = device.split_once(':') {
        if maj.parse::<u32>().is_err() || min.parse::<u32>().is_err() {
            return Err(unknown());
        }
        let dir = roots.sys.join("dev/block").join(device);
        if let Ok(u) = std::fs::read_to_string(dir.join("uevent"))
            && let Some(n) = u.lines().find_map(|l| l.strip_prefix("DEVNAME="))
        {
            return valid_dev_name(n.trim()).ok_or_else(unknown);
        }
        let link = std::fs::read_link(&dir).map_err(|_| unknown())?;
        let n = link.file_name().and_then(|n| n.to_str()).ok_or_else(unknown)?;
        return valid_dev_name(n).ok_or_else(unknown);
    }
    let n = valid_dev_name(device).ok_or_else(unknown)?;
    if !roots.sys.join("block").join(&n).is_dir() {
        return Err(unknown());
    }
    Ok(n)
}

fn valid_dev_name(n: &str) -> Option<String> {
    (!n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .then(|| n.to_string())
}

/// The device name and the schedulers its `queue/scheduler` offers.
pub fn scheduler_choices(roots: &Roots, device: &str) -> Result<(String, Vec<String>), Rejection> {
    let name = resolve_device(roots, device)?;
    let path = roots.sys.join("block").join(&name).join("queue/scheduler");
    let text =
        std::fs::read_to_string(&path).map_err(|e| reject(Rule::NotAvailable, format!("{}: {e}", path.display())))?;
    let choices = text.split_whitespace().map(|s| s.trim_matches(|c| c == '[' || c == ']').to_string()).collect();
    Ok((name, choices))
}
