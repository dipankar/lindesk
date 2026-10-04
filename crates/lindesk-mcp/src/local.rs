//! [`Local`]: what lindesk-mcp can see on its own, without privileges and without a daemon.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use lindesk_core::config::Config;
use lindesk_core::profile::{ProfileContext, ProfileSet};
use lindesk_host::discovery::{self, DiscoveryOptions, RootOwned};
use lindesk_plugin_api::{Hardware, Privilege, Resource};
use lindesk_query::{Format, Lake};
use lindesk_sys::requirements::{self, Component};
use lindesk_sys::{Env, Roots, calibrate, doctor, hardware, psi, swap};

use crate::backend::Local;
use crate::ops::Scope;

/// The real machine.
pub struct HostLocal {
    roots: Roots,
}

impl Default for HostLocal {
    fn default() -> Self {
        Self::new()
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home().map(|h| h.join(fallback)))
        .unwrap_or_else(|| PathBuf::from(fallback))
}

/// `$XDG_CONFIG_HOME/lindesk/lindesk.toml`.
pub fn user_config_path() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("lindesk/lindesk.toml")
}

/// `$XDG_STATE_HOME/lindesk`.
pub fn user_state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("lindesk")
}

/// Changeset directory of a scope (CONTROL-PLANE.md §4).
pub fn changes_dir(scope: Scope) -> PathBuf {
    match scope {
        Scope::User => user_state_dir().join("changes"),
        Scope::System => PathBuf::from("/var/lib/lindesk/changes"),
    }
}

impl HostLocal {
    pub fn new() -> Self {
        HostLocal { roots: Roots::host() }
    }

    fn user_config(&self) -> Config {
        Config::load(&user_config_path()).unwrap_or_default()
    }

    fn probe(&self) -> lindesk_sys::Probe {
        lindesk_sys::probe(&self.roots, &Env::from_process())
    }

    /// Detected hardware plus the saved latency map (user's, else the system's).
    fn hardware_value(&self) -> Hardware {
        let mut hw = hardware::detect(&self.roots);
        for system in [false, true] {
            if let Ok(map) = calibrate::load(&calibrate::default_path(system)) {
                calibrate::attach(&mut hw, map);
                break;
            }
        }
        hw
    }

    fn lake(&self, scope: Scope) -> Result<Lake, String> {
        let root = match scope {
            Scope::User => {
                self.user_config().telemetry.dir.clone().unwrap_or_else(|| lindesk_query::default_root(false))
            }
            Scope::System => lindesk_query::default_root(true),
        };
        if scope == Scope::System && std::fs::read_dir(&root).is_err() {
            return Err(format!(
                "the system telemetry lake {} is not readable by this user (it is 0750 root:lindesk); \
                 add the user to the `lindesk` group or query scope=user",
                root.display()
            ));
        }
        Lake::open(&root).map_err(|e| format!("opening the telemetry lake {}: {e}", root.display()))
    }

    fn pressure(&self) -> Value {
        let mut out = serde_json::Map::new();
        for (name, r) in [("cpu", Resource::Cpu), ("io", Resource::Io), ("memory", Resource::Memory)] {
            if let Ok(p) = psi::PsiReading::read(&psi::system_path(&self.roots.proc, r)) {
                out.insert(
                    name.into(),
                    json!({
                        "some_avg10": round2(p.some.avg10),
                        "some_avg60": round2(p.some.avg60),
                        "full_avg10": p.full.map(|f| round2(f.avg10)),
                        "full_avg60": p.full.map(|f| round2(f.avg60)),
                    }),
                );
            }
        }
        Value::Object(out)
    }
}

/// PSI percentages as JSON-friendly two-decimal numbers.
fn round2(v: f32) -> f64 {
    (f64::from(v) * 100.0).round() / 100.0
}

/// Every `NNNNNN.json` in a changes directory, newest first.
fn changeset_files(dir: &Path) -> Vec<(u64, PathBuf)> {
    let mut v: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let id = p.file_stem()?.to_str()?.parse::<u64>().ok()?;
            (p.extension()? == "json").then_some((id, p))
        })
        .collect();
    v.sort_by_key(|e| std::cmp::Reverse(e.0));
    v
}

impl Local for HostLocal {
    fn status(&self) -> Result<Value> {
        let p = self.probe();
        let hw = self.hardware_value();
        let reqs = requirements::check(&self.roots, &Env::from_process(), Component::Session, &p);
        let warnings: Vec<Value> =
            reqs.items.iter().filter(|r| r.status != requirements::Status::Pass).map(|r| json!(r)).collect();
        let pressure = self.pressure();
        let (level, _) = p.support();
        let text = format!(
            "No lindesk daemon answered, so nothing is being managed right now.\nhost: {}\nsupport: {level}\npressure (avg10 some %): cpu {} io {} memory {}",
            hardware::summary_line(&hw),
            pressure.pointer("/cpu/some_avg10").map_or("?".into(), |v| v.to_string()),
            pressure.pointer("/io/some_avg10").map_or("?".into(), |v| v.to_string()),
            pressure.pointer("/memory/some_avg10").map_or("?".into(), |v| v.to_string()),
        );
        Ok(json!({
            "daemon": null,
            "support_level": level,
            "pressure": { "system": pressure },
            "hardware": hw,
            "requirements": warnings,
            "text": text,
        }))
    }

    fn doctor(&self) -> Result<Value> {
        let p = self.probe();
        let mut v: Value = serde_json::from_str(&doctor::render_json(&p)).context("doctor JSON")?;
        // The raw probe is large and duplicated by the findings; keep the report focused.
        if let Some(o) = v.as_object_mut() {
            o.remove("probe");
        }
        Ok(v)
    }

    fn hardware(&self) -> Result<Value> {
        Ok(serde_json::to_value(self.hardware_value())?)
    }

    fn swap(&self) -> Result<Value> {
        let hw = hardware::detect(&self.roots);
        let opts = swap::SwapOptions { interval: Some(Duration::from_millis(500)), previous: None };
        Ok(serde_json::to_value(swap::analyze_with(&self.roots, &hw, &opts))?)
    }

    fn requirements(&self) -> Result<Value> {
        let p = self.probe();
        let reqs = requirements::check(&self.roots, &Env::from_process(), Component::Session, &p);
        Ok(serde_json::to_value(&reqs.items)?)
    }

    fn query(&self, scope: Scope, sql: &str) -> Result<Value, String> {
        let lake = self.lake(scope)?;
        let out = lake.query(sql, &Format::Json).map_err(|e| e.to_string())?;
        let out = out.trim();
        if out.is_empty() {
            return Ok(Value::Array(Vec::new()));
        }
        serde_json::from_str(out).map_err(|e| format!("decoding query result: {e}"))
    }

    fn plugins(&self) -> Result<Value> {
        let config = self.user_config();
        let mut dirs = discovery::default_plugin_dirs(Privilege::User);
        dirs.extend(config.general.plugin_dirs.clone());
        let found = discovery::discover(&DiscoveryOptions {
            host: Privilege::User,
            dirs,
            config: &config,
            ownership: &RootOwned,
        });
        let plugins: Vec<Value> = found
            .plugins
            .iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "description": p.manifest.plugin.description,
                    "version": p.manifest.plugin.version,
                    "kinds": p.manifest.plugin.kinds,
                    "privilege": p.manifest.plugin.privilege,
                    "enabled": p.enabled,
                    "disabled_reason": p.disabled_reason,
                    "dir": p.dir,
                })
            })
            .collect();
        let rejected: Vec<String> = found.rejected.iter().map(|r| r.to_string()).collect();
        Ok(json!({ "external": plugins, "rejected": rejected }))
    }

    fn profiles(&self) -> Result<Value> {
        let xdg_config = xdg("XDG_CONFIG_HOME", ".config");
        let (set, errors) = ProfileSet::load(&lindesk_core::profile::default_dirs(Some(&xdg_config)));
        let hw = self.hardware_value();
        let ctx = ProfileContext::from_hardware(&hw, None);
        let config = self.user_config();
        let configured = config.general.profile.clone();
        let selection = set.select(&ctx);
        let active =
            if configured == lindesk_core::config::PROFILE_AUTO { selection.name.clone() } else { configured.clone() };
        Ok(json!({
            "configured": configured,
            "would_select": selection,
            "context": ctx.summary(),
            "profiles": set.list(&ctx, Some(&active)),
            "errors": errors.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
        }))
    }

    fn config(&self) -> Result<(String, String)> {
        let path = user_config_path();
        let config = Config::load(&path).with_context(|| format!("loading {}", path.display()))?;
        Ok((path.display().to_string(), config.to_toml()))
    }

    fn changes(&self, scope: Scope, limit: u32) -> Result<Value> {
        let dir = changes_dir(scope);
        let mut out = Vec::new();
        for (_, path) in changeset_files(&dir).into_iter().take(limit as usize) {
            if let Ok(v) =
                std::fs::read(&path).map_err(anyhow::Error::from).and_then(|b| Ok(serde_json::from_slice::<Value>(&b)?))
            {
                out.push(v);
            }
        }
        Ok(Value::Array(out))
    }

    fn change(&self, scope: Scope, id: u64) -> Result<Option<Value>> {
        let path = changes_dir(scope).join(format!("{id:06}.json"));
        match std::fs::read(&path) {
            Ok(b) => Ok(Some(serde_json::from_slice(&b)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn explain(&self, what: &str) -> Result<Value> {
        let pid = what.strip_prefix("pid:").unwrap_or(what).trim().parse::<i32>().ok();
        let mut v = json!({
            "query": what,
            "known": false,
            "notes": ["No lindesk daemon answered: nothing is managed, so there are no applied settings to explain."],
        });
        if let Some(pid) = pid {
            v["target"] = json!(format!("pid:{pid}"));
            match lindesk_sys::procfs::proc_info(&self.roots, pid) {
                Ok(info) => v["proc"] = serde_json::to_value(info)?,
                Err(e) => v["notes"].as_array_mut().into_iter().for_each(|n| n.push(json!(format!("pid {pid}: {e}")))),
            }
        } else {
            v["target"] = json!(what);
        }
        Ok(v)
    }
}
