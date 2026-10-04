//! Appliers: read the current value an op would change, and write a value.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lindesk_core::config::Config;
use serde_json::{Value, json};
use toml_edit::DocumentMut;

use crate::guard::resolve_device;
use crate::model::{ChangeOp, Scope};
use crate::roots::{ConfigPaths, Roots, atomic_write, io_err, read_opt, remove_opt, write_attr};
use crate::{ChangeError, Result};

/// Reads and writes the thing an op changes. `write(op, v)` puts `v` (an op's
/// [`ChangeOp::target_value`] when applying, a recorded `previous` when reverting) in the
/// place `op` designates; `Value::Null` means "absent". Implementations must be idempotent.
pub trait Applier: Send + Sync {
    fn read(&self, op: &ChangeOp) -> Result<Value>;
    fn write(&self, op: &ChangeOp, v: &Value) -> Result<()>;
    /// The config file a scope's config ops edit (for snapshots and diffs).
    fn config_path(&self, _scope: Scope) -> Option<PathBuf> {
        None
    }
}

/// The real system, relative to [`Roots`] (fixtures in tests).
#[derive(Clone, Debug, PartialEq)]
pub struct RealApplier {
    pub roots: Roots,
    pub config_paths: ConfigPaths,
}

const PERSIST_HEADER: &str = "# Managed by lindesk (versioned changes). Don't edit by hand:\n\
# `lindeskctl change list` shows what wrote each line, `lindeskctl change revert <id>` undoes it.\n";

impl RealApplier {
    pub fn new(roots: Roots, config_paths: ConfigPaths) -> Self {
        RealApplier { roots, config_paths }
    }

    /// The host for a daemon of `scope`.
    pub fn host(scope: Scope) -> Self {
        RealApplier::new(Roots::host(), ConfigPaths::host(scope))
    }

    fn config_file(&self, scope: Scope) -> Result<&Path> {
        self.config_paths.get(scope).ok_or_else(|| ChangeError::Apply(format!("no {scope} config file configured")))
    }

    fn read_doc(&self, scope: Scope) -> Result<(PathBuf, String, DocumentMut)> {
        let path = self.config_file(scope)?.to_path_buf();
        let text = read_opt(&path)?.unwrap_or_default();
        let doc = crate::toml_doc::parse(&text).map_err(|e| ChangeError::Apply(format!("{}: {e}", path.display())))?;
        Ok((path, text, doc))
    }

    fn zswap_dir(&self) -> PathBuf {
        self.roots.sys.join("module/zswap/parameters")
    }

    /// `policyN` directories, in numeric order.
    fn cpufreq_policies(&self) -> Result<Vec<(String, PathBuf)>> {
        let dir = self.roots.sys.join("devices/system/cpu/cpufreq");
        let mut v: Vec<(u32, String, PathBuf)> = std::fs::read_dir(&dir)
            .map_err(|e| io_err(&dir, e))?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_str()?.to_string();
                let n: u32 = name.strip_prefix("policy")?.parse().ok()?;
                Some((n, name, e.path()))
            })
            .collect();
        v.sort();
        if v.is_empty() {
            return Err(ChangeError::Apply(format!("no cpufreq policies under {}", dir.display())));
        }
        Ok(v.into_iter().map(|(_, n, p)| (n, p)).collect())
    }

    fn read_per_policy(&self, file: &str) -> Result<Value> {
        let mut m = serde_json::Map::new();
        for (name, dir) in self.cpufreq_policies()? {
            let p = dir.join(file);
            let v = std::fs::read_to_string(&p).map_err(|e| io_err(&p, e))?;
            m.insert(name, json!(v.trim()));
        }
        Ok(Value::Object(m))
    }

    fn write_per_policy(&self, file: &str, v: &Value) -> Result<()> {
        let policies = self.cpufreq_policies()?;
        match v {
            Value::String(s) => {
                for (_, dir) in &policies {
                    write_attr(&dir.join(file), s)?;
                }
            }
            Value::Object(m) => {
                for (name, dir) in &policies {
                    if let Some(s) = m.get(name).and_then(Value::as_str) {
                        write_attr(&dir.join(file), s)?;
                    }
                }
            }
            other => return Err(ChangeError::Apply(format!("bad cpufreq value {other}"))),
        }
        Ok(())
    }

    fn scheduler_path(&self, device: &str) -> Result<(String, PathBuf)> {
        let name = resolve_device(&self.roots, device).map_err(|r| ChangeError::Apply(r.message))?;
        let p = self.roots.sys.join("block").join(&name).join("queue/scheduler");
        Ok((name, p))
    }
}

/// `/proc/sys/<key with dots → slashes>`; `None` for keys that could escape the tree.
pub(crate) fn sysctl_path(roots: &Roots, key: &str) -> Option<PathBuf> {
    let ok = !key.is_empty()
        && key
            .split('.')
            .all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    ok.then(|| roots.proc.join("sys").join(key.replace('.', "/")))
}

/// Write `v` at the place a config op designates in a parsed document.
pub(crate) fn doc_write(doc: &mut DocumentMut, op: &ChangeOp, v: &Value) -> std::result::Result<(), String> {
    match op {
        ChangeOp::ClassOverride { unit, .. } => crate::toml_doc::set_override_class(doc, unit, v),
        _ => {
            let key = op.config_key().ok_or("not a config op")?;
            let path = crate::toml_doc::split_key(&key)?;
            if v.is_null() {
                crate::toml_doc::unset(doc, &path);
                Ok(())
            } else {
                crate::toml_doc::set(doc, &path, v)
            }
        }
    }
}

/// Read the value at the place a config op designates.
pub(crate) fn doc_read(doc: &DocumentMut, op: &ChangeOp) -> Value {
    match op {
        ChangeOp::ClassOverride { unit, .. } => crate::toml_doc::get_override_class(doc, unit),
        _ => match op.config_key().and_then(|k| crate::toml_doc::split_key(&k).ok()) {
            Some(path) => crate::toml_doc::get(doc, &path),
            None => Value::Null,
        },
    }
}

fn read_trim(p: &Path) -> Result<String> {
    std::fs::read_to_string(p).map(|s| s.trim().to_string()).map_err(|e| io_err(p, e))
}

/// The active scheduler from `"mq-deadline kyber [bfq] none"`.
fn active_scheduler(text: &str) -> String {
    text.split_whitespace()
        .find_map(|s| s.strip_prefix('[').and_then(|s| s.strip_suffix(']')))
        .map(str::to_string)
        .unwrap_or_else(|| text.trim().to_string())
}

/// A managed line-per-key file (`sysctl.d`, udev rules).
struct LineFile<'a> {
    path: PathBuf,
    /// Extract a line's key.
    key_of: &'a dyn Fn(&str) -> Option<String>,
}

impl LineFile<'_> {
    fn lines(&self) -> Result<Vec<String>> {
        Ok(read_opt(&self.path)?
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
            .map(str::to_string)
            .collect())
    }

    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.lines()?.into_iter().find(|l| (self.key_of)(l).as_deref() == Some(key)))
    }

    /// Replace (or remove, with `None`) the line for `key`. Removes the file when empty.
    fn set(&self, key: &str, line: Option<String>) -> Result<()> {
        let mut lines: Vec<String> =
            self.lines()?.into_iter().filter(|l| (self.key_of)(l).as_deref() != Some(key)).collect();
        if let Some(l) = line {
            lines.push(l);
        }
        if lines.is_empty() {
            return remove_opt(&self.path);
        }
        lines.sort();
        let text = format!("{PERSIST_HEADER}{}\n", lines.join("\n"));
        atomic_write(&self.path, text.as_bytes(), 0o644)
    }
}

fn sysctl_key_of(l: &str) -> Option<String> {
    l.split_once('=').map(|(k, _)| k.trim().to_string())
}

fn udev_key_of(l: &str) -> Option<String> {
    let start = l.find("KERNEL==\"")? + 9;
    let end = l[start..].find('"')? + start;
    Some(l[start..end].to_string())
}

fn udev_value(l: &str) -> Option<String> {
    let start = l.find("ATTR{queue/scheduler}=\"")? + 23;
    let end = l[start..].find('"')? + start;
    Some(l[start..end].to_string())
}

/// Split a runtime value into (runtime, persisted?) where persisted is `Some(None)` for
/// "explicitly absent" and `None` for "not managed by this value".
fn split_runtime(v: &Value) -> Result<(Option<String>, Option<Option<String>>)> {
    match v {
        Value::String(s) => Ok((Some(s.clone()), None)),
        Value::Object(m) => {
            let runtime = m.get("runtime").and_then(Value::as_str).map(str::to_string);
            let persisted = m.get("persisted").map(|p| p.as_str().map(str::to_string));
            Ok((runtime, persisted))
        }
        other => Err(ChangeError::Apply(format!("unexpected value {other}"))),
    }
}

impl Applier for RealApplier {
    fn read(&self, op: &ChangeOp) -> Result<Value> {
        match op {
            ChangeOp::ConfigSet { scope, .. }
            | ChangeOp::ConfigUnset { scope, .. }
            | ChangeOp::ProfileUse { scope, .. }
            | ChangeOp::ClassOverride { scope, .. } => {
                let (_, _, doc) = self.read_doc(*scope)?;
                Ok(doc_read(&doc, op))
            }
            ChangeOp::Sysctl { key, persist, .. } => {
                let p = sysctl_path(&self.roots, key).ok_or_else(|| ChangeError::Apply(format!("bad key {key}")))?;
                let runtime = read_trim(&p)?;
                if *persist {
                    let f = LineFile { path: self.roots.sysctl_conf(), key_of: &sysctl_key_of };
                    let persisted = f.get(key)?.and_then(|l| l.split_once('=').map(|(_, v)| v.trim().to_string()));
                    Ok(json!({"runtime": runtime, "persisted": persisted}))
                } else {
                    Ok(json!(runtime))
                }
            }
            ChangeOp::IoScheduler { device, persist, .. } => {
                let (name, p) = self.scheduler_path(device)?;
                let runtime = active_scheduler(&read_trim(&p)?);
                if *persist {
                    let f = LineFile { path: self.roots.udev_rules(), key_of: &udev_key_of };
                    let persisted = f.get(&name)?.and_then(|l| udev_value(&l));
                    Ok(json!({"runtime": runtime, "persisted": persisted}))
                } else {
                    Ok(json!(runtime))
                }
            }
            ChangeOp::Zswap { enabled, compressor, max_pool_percent } => {
                let dir = self.zswap_dir();
                let mut m = serde_json::Map::new();
                if enabled.is_some() {
                    let v = read_trim(&dir.join("enabled"))?;
                    m.insert("enabled".into(), json!(matches!(v.as_str(), "Y" | "y" | "1")));
                }
                if compressor.is_some() {
                    m.insert("compressor".into(), json!(read_trim(&dir.join("compressor"))?));
                }
                if max_pool_percent.is_some() {
                    let p = dir.join("max_pool_percent");
                    let v: u64 = read_trim(&p)?
                        .parse()
                        .map_err(|_| ChangeError::Apply(format!("{}: not a number", p.display())))?;
                    m.insert("max_pool_percent".into(), json!(v));
                }
                Ok(Value::Object(m))
            }
            ChangeOp::CpuEpp { .. } => self.read_per_policy("energy_performance_preference"),
            ChangeOp::CpuGovernor { .. } => self.read_per_policy("scaling_governor"),
        }
    }

    fn write(&self, op: &ChangeOp, v: &Value) -> Result<()> {
        match op {
            ChangeOp::ConfigSet { scope, .. }
            | ChangeOp::ConfigUnset { scope, .. }
            | ChangeOp::ProfileUse { scope, .. }
            | ChangeOp::ClassOverride { scope, .. } => {
                let (path, text, mut doc) = self.read_doc(*scope)?;
                doc_write(&mut doc, op, v).map_err(ChangeError::Apply)?;
                let new = doc.to_string();
                if new == text {
                    return Ok(());
                }
                Config::from_toml(&new)
                    .map_err(|e| ChangeError::Apply(format!("refusing to write an invalid {}: {e}", path.display())))?;
                atomic_write(&path, new.as_bytes(), 0o644)
            }
            ChangeOp::Sysctl { key, .. } => {
                let p = sysctl_path(&self.roots, key).ok_or_else(|| ChangeError::Apply(format!("bad key {key}")))?;
                let (runtime, persisted) = split_runtime(v)?;
                if let Some(r) = runtime {
                    write_attr(&p, &r)?;
                }
                if let Some(persisted) = persisted {
                    let f = LineFile { path: self.roots.sysctl_conf(), key_of: &sysctl_key_of };
                    f.set(key, persisted.map(|val| format!("{key} = {val}")))?;
                }
                Ok(())
            }
            ChangeOp::IoScheduler { device, .. } => {
                let (name, p) = self.scheduler_path(device)?;
                let (runtime, persisted) = split_runtime(v)?;
                if let Some(r) = runtime {
                    write_attr(&p, &r)?;
                }
                if let Some(persisted) = persisted {
                    let f = LineFile { path: self.roots.udev_rules(), key_of: &udev_key_of };
                    f.set(
                        &name,
                        persisted.map(|s| {
                            format!("ACTION==\"add|change\", SUBSYSTEM==\"block\", KERNEL==\"{name}\", ATTR{{queue/scheduler}}=\"{s}\"")
                        }),
                    )?;
                }
                Ok(())
            }
            ChangeOp::Zswap { .. } => {
                let dir = self.zswap_dir();
                let m: BTreeMap<String, Value> = serde_json::from_value(v.clone())
                    .map_err(|e| ChangeError::Apply(format!("bad zswap value: {e}")))?;
                // Compressor and pool size before enabling, so zswap starts with the new ones.
                if let Some(c) = m.get("compressor").and_then(Value::as_str) {
                    write_attr(&dir.join("compressor"), c)?;
                }
                if let Some(p) = m.get("max_pool_percent").and_then(Value::as_u64) {
                    write_attr(&dir.join("max_pool_percent"), &p.to_string())?;
                }
                if let Some(e) = m.get("enabled").and_then(Value::as_bool) {
                    write_attr(&dir.join("enabled"), if e { "Y" } else { "N" })?;
                }
                Ok(())
            }
            ChangeOp::CpuEpp { .. } => self.write_per_policy("energy_performance_preference", v),
            ChangeOp::CpuGovernor { .. } => self.write_per_policy("scaling_governor", v),
        }
    }

    fn config_path(&self, scope: Scope) -> Option<PathBuf> {
        self.config_paths.get(scope).map(Path::to_path_buf)
    }
}
