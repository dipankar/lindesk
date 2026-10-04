//! The `ChangeOp` JSON shape (docs/CONTROL-PLANE.md §4), as tool input schemas.
//!
//! The daemons own validation (guardrails, polkit, versioning) through `lindesk-changes`;
//! this module only mirrors the wire shape so MCP clients get a precise JSON Schema, and
//! does the cheap pre-checks that need no privileges (scope consistency, op count), so an
//! agent gets an immediate, specific answer instead of a round trip.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use lindesk_plugin_api::QosClass;

/// Maximum ops per changeset (the daemons enforce the same limit).
pub const MAX_OPS: usize = 16;

/// Which daemon, config file and change history an operation belongs to.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The calling user's lindesk-session (`~/.config/lindesk/lindesk.toml`); no admin rights needed.
    #[default]
    User,
    /// The system broker lindeskd (`/etc/lindesk/lindesk.toml`, sysctls, I/O schedulers, cpufreq);
    /// polkit may ask the user for admin authentication.
    System,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::System => "system",
        }
    }
}

/// One change. `op` selects the kind; every changeset is validated by the daemon's
/// guardrails, recorded with the previous values, versioned and revertible.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ChangeOp {
    /// Set a lindesk.toml key (dotted path, e.g. `plugins.threshold.config.epoch`). Unknown keys
    /// are rejected; `[safety]` may only be tightened. Risk: low.
    ConfigSet {
        scope: Scope,
        /// Dotted path into lindesk.toml, e.g. `general.tick` or `plugins.threshold.config.epoch`.
        key: String,
        /// New value (JSON; strings for durations such as `"800ms"`).
        value: serde_json::Value,
    },
    /// Remove a lindesk.toml key so the default applies again. Risk: low.
    ConfigUnset { scope: Scope, key: String },
    /// Switch the CPU/QoS profile (`general.profile`); `"auto"` re-enables automatic selection. Risk: low.
    ProfileUse { scope: Scope, name: String },
    /// Pin the QoS class of units matching a glob (adds an `[[override]]`). Risk: low.
    ClassOverride {
        scope: Scope,
        /// Unit name glob, e.g. `steam*.scope` or `docker-*.scope`.
        unit: String,
        class: QosClass,
    },
    /// Set an allow-listed sysctl (system scope): `vm.swappiness` 0–200, `vm.page-cluster` 0–3,
    /// `vm.watermark_scale_factor` 10–1000, `vm.vfs_cache_pressure` 50–200. Runtime only. Risk: medium.
    Sysctl {
        key: String,
        /// Value as a string, e.g. `"10"`.
        value: String,
        /// Also write `/etc/sysctl.d/90-lindesk.conf` so it survives a reboot (default false:
        /// runtime only). Only after the user confirmed the runtime change helped.
        #[serde(default, skip_serializing_if = "is_false")]
        persist: bool,
    },
    /// Change a block device's I/O scheduler (system scope); only values offered by the
    /// device's `queue/scheduler`. Risk: high.
    IoScheduler {
        /// `MAJ:MIN` (e.g. `8:0`) or kernel name (e.g. `sda`).
        device: String,
        /// e.g. `bfq`, `mq-deadline`, `kyber`, `none`.
        scheduler: String,
        /// Also write a udev rule so it survives a reboot (default false: runtime only).
        #[serde(default, skip_serializing_if = "is_false")]
        persist: bool,
    },
    /// Configure zswap (system scope). Omitted fields stay unchanged. Risk: medium.
    Zswap {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
        /// e.g. `zstd`, `lz4`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compressor: Option<String>,
        /// Percent of RAM the compressed pool may use (1–100).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_pool_percent: Option<u8>,
    },
    /// Set the cpufreq energy_performance_preference on all policies (system scope); only
    /// advertised values (see `hardware` → features.epp_available). Risk: high.
    CpuEpp { preference: String },
    /// Set the cpufreq governor on all policies (system scope); only advertised values
    /// (features.governors_available). Risk: high.
    CpuGovernor { governor: String },
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Guardrail risk class (CONTROL-PLANE.md §4).
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    None,
    Low,
    Medium,
    High,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::None => "none",
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
        }
    }
}

impl ChangeOp {
    /// The scope the op itself names, or `None` for system-only ops without a scope field.
    pub fn declared_scope(&self) -> Option<Scope> {
        match self {
            ChangeOp::ConfigSet { scope, .. }
            | ChangeOp::ConfigUnset { scope, .. }
            | ChangeOp::ProfileUse { scope, .. }
            | ChangeOp::ClassOverride { scope, .. } => Some(*scope),
            _ => None,
        }
    }

    /// The scope the op must be sent to.
    pub fn required_scope(&self) -> Scope {
        self.declared_scope().unwrap_or(Scope::System)
    }

    pub fn kind(&self) -> &'static str {
        match self {
            ChangeOp::ConfigSet { .. } => "config_set",
            ChangeOp::ConfigUnset { .. } => "config_unset",
            ChangeOp::ProfileUse { .. } => "profile_use",
            ChangeOp::ClassOverride { .. } => "class_override",
            ChangeOp::Sysctl { .. } => "sysctl",
            ChangeOp::IoScheduler { .. } => "io_scheduler",
            ChangeOp::Zswap { .. } => "zswap",
            ChangeOp::CpuEpp { .. } => "cpu_epp",
            ChangeOp::CpuGovernor { .. } => "cpu_governor",
        }
    }

    /// Risk class per CONTROL-PLANE.md §4 (the daemon's verdict is authoritative).
    pub fn risk(&self) -> Risk {
        match self {
            ChangeOp::ConfigSet { .. }
            | ChangeOp::ConfigUnset { .. }
            | ChangeOp::ProfileUse { .. }
            | ChangeOp::ClassOverride { .. } => Risk::Low,
            ChangeOp::Sysctl { .. } | ChangeOp::Zswap { .. } => Risk::Medium,
            ChangeOp::IoScheduler { .. } | ChangeOp::CpuEpp { .. } | ChangeOp::CpuGovernor { .. } => Risk::High,
        }
    }
}

/// Highest risk of a list of ops (`None` when empty).
pub fn max_risk(ops: &[ChangeOp]) -> Risk {
    ops.iter().map(ChangeOp::risk).max().unwrap_or(Risk::None)
}

/// Risk of ops given as raw JSON (unparseable ops count as high).
pub fn max_risk_json(ops: &[serde_json::Value]) -> Risk {
    ops.iter()
        .map(|v| serde_json::from_value::<ChangeOp>(v.clone()).map(|o| o.risk()).unwrap_or(Risk::High))
        .max()
        .unwrap_or(Risk::None)
}

/// The scope a list of raw ops needs (system if any op needs it).
pub fn scope_json(ops: &[serde_json::Value]) -> Scope {
    let any_system = ops.iter().any(|v| {
        serde_json::from_value::<ChangeOp>(v.clone()).map(|o| o.required_scope() == Scope::System).unwrap_or(true)
    });
    if any_system { Scope::System } else { Scope::User }
}

/// Checks that need no daemon: op count and scope consistency. Returns one message per
/// problem, phrased like the daemon's guardrail rejections.
pub fn precheck(scope: Scope, ops: &[ChangeOp]) -> Vec<String> {
    let mut out = Vec::new();
    if ops.is_empty() {
        out.push("rejected: a changeset needs at least one op".into());
    }
    if ops.len() > MAX_OPS {
        out.push(format!("rejected: {} ops; at most {MAX_OPS} per changeset (split it)", ops.len()));
    }
    for (i, op) in ops.iter().enumerate() {
        match op.declared_scope() {
            Some(s) if s != scope => out.push(format!(
                "rejected: op #{i} ({}) names scope={} but the changeset is for scope={}; send it in a separate {} changeset",
                op.kind(),
                s.as_str(),
                scope.as_str(),
                s.as_str()
            )),
            None if scope == Scope::User => out.push(format!(
                "rejected: op #{i} ({}) changes the whole machine and needs scope=system (polkit may ask for admin authentication)",
                op.kind()
            )),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_shape_matches_control_plane_doc() {
        let op: ChangeOp =
            serde_json::from_value(json!({"op": "sysctl", "key": "vm.swappiness", "value": "10"})).unwrap();
        assert_eq!(op, ChangeOp::Sysctl { key: "vm.swappiness".into(), value: "10".into(), persist: false });
        let op: ChangeOp = serde_json::from_value(
            json!({"op": "class_override", "scope": "user", "unit": "steam*.scope", "class": "interactive"}),
        )
        .unwrap();
        assert_eq!(op.required_scope(), Scope::User);
        assert_eq!(
            serde_json::to_value(ChangeOp::Zswap { enabled: Some(true), compressor: None, max_pool_percent: None })
                .unwrap(),
            json!({"op": "zswap", "enabled": true})
        );
    }

    #[test]
    fn same_wire_format_as_lindesk_changes() {
        let samples = [
            json!({"op": "config_set", "scope": "user", "key": "plugins.threshold.config.epoch", "value": "800ms"}),
            json!({"op": "config_unset", "scope": "system", "key": "general.tick"}),
            json!({"op": "profile_use", "scope": "user", "name": "auto"}),
            json!({"op": "class_override", "scope": "user", "unit": "steam*.scope", "class": "interactive"}),
            json!({"op": "sysctl", "key": "vm.swappiness", "value": "10"}),
            json!({"op": "sysctl", "key": "vm.page-cluster", "value": "0", "persist": true}),
            json!({"op": "io_scheduler", "device": "8:0", "scheduler": "bfq"}),
            json!({"op": "zswap", "enabled": true, "compressor": "zstd", "max_pool_percent": 20}),
            json!({"op": "cpu_epp", "preference": "balance_performance"}),
            json!({"op": "cpu_governor", "governor": "schedutil"}),
        ];
        for v in samples {
            let ours: ChangeOp = serde_json::from_value(v.clone()).unwrap();
            let theirs: lindesk_changes::ChangeOp = serde_json::from_value(v.clone()).unwrap();
            assert_eq!(serde_json::to_value(&ours).unwrap(), serde_json::to_value(&theirs).unwrap(), "{v}");
            assert_eq!(serde_json::to_value(&ours).unwrap(), v);
        }
    }

    #[test]
    fn precheck_scope_and_count() {
        let sysctl = ChangeOp::Sysctl { key: "vm.swappiness".into(), value: "10".into(), persist: false };
        assert!(precheck(Scope::System, std::slice::from_ref(&sysctl)).is_empty());
        let e = precheck(Scope::User, std::slice::from_ref(&sysctl));
        assert!(e[0].contains("needs scope=system"), "{e:?}");
        let prof = ChangeOp::ProfileUse { scope: Scope::System, name: "latency".into() };
        assert!(precheck(Scope::User, &[prof])[0].contains("names scope=system"));
        assert!(precheck(Scope::User, &[])[0].contains("at least one"));
        let many = vec![ChangeOp::ConfigUnset { scope: Scope::User, key: "general.tick".into() }; 17];
        assert!(precheck(Scope::User, &many)[0].contains("at most 16"));
    }

    #[test]
    fn risk_classes() {
        assert_eq!(max_risk(&[]), Risk::None);
        assert_eq!(max_risk_json(&[json!({"op": "cpu_epp", "preference": "performance"})]), Risk::High);
        assert_eq!(max_risk_json(&[json!({"op": "profile_use", "scope": "user", "name": "latency"})]), Risk::Low);
        assert_eq!(scope_json(&[json!({"op": "sysctl", "key": "vm.page-cluster", "value": "0"})]), Scope::System);
    }
}
