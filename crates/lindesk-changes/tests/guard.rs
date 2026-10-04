mod common;

use common::*;
use lindesk_changes::guard::{SAFETY_DIRECTIONS, check_safety, validate};
use lindesk_changes::*;
use lindesk_core::safety::SafetyCaps;
use lindesk_plugin_api::QosClass;
use serde_json::json;

/// Plan `ops` as the CLI in system scope and return the rules that fired.
fn rules(fx: &Fx, ops: Vec<ChangeOp>) -> Vec<Rule> {
    let store = fx.store(Scope::System);
    store.plan(&ops, &fx.ctx(Scope::System, Origin::Cli)).rejections.into_iter().map(|r| r.rule).collect()
}

fn one(fx: &Fx, op: ChangeOp) -> Rule {
    let r = rules(fx, vec![op]);
    assert_eq!(r.len(), 1, "{r:?}");
    r[0]
}

#[test]
fn rejects_empty_and_too_many_ops() {
    let fx = Fx::new();
    assert_eq!(rules(&fx, vec![]), [Rule::Empty]);
    let ops = (0..17).map(|i| sysctl("vm.swappiness", &i.to_string())).collect();
    assert_eq!(rules(&fx, ops), [Rule::TooManyOps]);
    let ops: Vec<_> = (0..16).map(|i| sysctl("vm.swappiness", &i.to_string())).collect();
    assert!(rules(&fx, ops).is_empty());
}

#[test]
fn rejects_wrong_scope() {
    let fx = Fx::new();
    assert_eq!(
        one(&fx, ChangeOp::ConfigSet { scope: Scope::User, key: "general.dry_run".into(), value: json!(true) }),
        Rule::Scope
    );
    // A context for the wrong store.
    let store = fx.store(Scope::System);
    let plan = store.plan(&[set("general.dry_run", json!(true))], &fx.ctx(Scope::User, Origin::Cli));
    assert!(plan.rejections.iter().any(|r| r.rule == Rule::Scope));
}

#[test]
fn rejects_unknown_config_keys() {
    let fx = Fx::new();
    assert_eq!(one(&fx, set("general.tickk", json!("1s"))), Rule::ConfigKey);
    assert_eq!(one(&fx, set("generl.tick", json!("1s"))), Rule::ConfigKey);
    assert_eq!(one(&fx, set("safety.uclamp_min_maxx", json!(10))), Rule::ConfigKey);
    assert_eq!(one(&fx, set("general..tick", json!("1s"))), Rule::ConfigKey);
    assert_eq!(one(&fx, set("override.unit", json!("x"))), Rule::ConfigKey);
}

#[test]
fn rejects_invalid_config_values() {
    let fx = Fx::new();
    assert_eq!(one(&fx, set("general.tick", json!(5))), Rule::ConfigInvalid);
    assert_eq!(one(&fx, set("general.tick", json!("soon"))), Rule::ConfigInvalid);
    assert_eq!(one(&fx, set("io.iocost", json!("maybe"))), Rule::ConfigKey, "unknown variant");
    assert_eq!(one(&fx, set("general.dry_run", json!(null))), Rule::ConfigInvalid);
    // Through the daemon's validator hook (plugin configs, override globs).
    let store = fx.store(Scope::System);
    let ctx = fx.ctx(Scope::System, Origin::Cli).with_config_validator(|c| {
        let epoch = &c.plugin("threshold").config["epoch"];
        if epoch == "0ms" { Err("threshold: epoch must be > 0".into()) } else { Ok(()) }
    });
    let plan = store.plan(&[set("plugins.threshold.config.epoch", json!("0ms"))], &ctx);
    assert_eq!(plan.rejections[0].rule, Rule::ConfigInvalid);
    assert!(plan.rejections[0].message.contains("epoch must be > 0"));
    assert!(store.plan(&[set("plugins.threshold.config.epoch", json!("700ms"))], &ctx).is_ok());
}

#[test]
fn rejects_an_already_broken_config_file() {
    let fx = Fx::new();
    std::fs::write(fx.paths.system.as_ref().unwrap(), "[general]\ntick = 3\n").unwrap();
    let r = rules(&fx, vec![set("general.dry_run", json!(true))]);
    assert_eq!(r, [Rule::ConfigInvalid]);
}

#[test]
fn rejects_safety_loosening() {
    let fx = Fx::new();
    assert_eq!(one(&fx, set("safety.uclamp_min_max", json!(80.0))), Rule::SafetyLoosened);
    assert_eq!(one(&fx, set("safety.io_bps_floor", json!(1024))), Rule::SafetyLoosened);
    assert_eq!(one(&fx, set("safety.nice_min", json!(-10))), Rule::SafetyLoosened);
    // Tightening is fine.
    assert!(
        rules(&fx, vec![set("safety.uclamp_min_max", json!(40.0)), set("safety.io_bps_floor", json!(16_777_216))])
            .is_empty()
    );
    // Incoherent tightening (min above max) is refused too.
    assert_eq!(one(&fx, set("safety.cpu_weight_min", json!(2000))), Rule::SafetyLoosened);

    // After tightening, unsetting the key would loosen back to the default.
    let store = fx.store(Scope::System);
    fx.apply(&store, Origin::Cli, vec![set("safety.uclamp_min_max", json!(40.0))], None).unwrap();
    let plan = store.plan(
        &[ChangeOp::ConfigUnset { scope: Scope::System, key: "safety.uclamp_min_max".into() }],
        &fx.ctx(Scope::System, Origin::Cli),
    );
    assert_eq!(plan.rejections[0].rule, Rule::SafetyLoosened);
    assert_eq!(plan.rejections[0].index, Some(0));
    // ... but reverting the tightening changeset is always possible.
    store.revert(1, Author::new(0, Origin::Cli), false).unwrap();
    assert!(fx.system_config().contains("uclamp_min_max = 50.0\n"));
}

#[test]
fn safety_comparator_directions() {
    // Every field has a direction.
    let fields: Vec<String> =
        serde_json::to_value(SafetyCaps::default()).unwrap().as_object().unwrap().keys().cloned().collect();
    let mut covered: Vec<String> = SAFETY_DIRECTIONS.iter().map(|d| d.0.to_string()).collect();
    covered.sort();
    let mut fields_sorted = fields.clone();
    fields_sorted.sort();
    assert_eq!(covered, fields_sorted);

    let base = SafetyCaps::default();
    let tighter = |f: fn(&mut SafetyCaps)| {
        let mut s = base.clone();
        f(&mut s);
        let ch = compare_safety(&base, &s);
        assert_eq!(ch.len(), 1);
        ch[0].tighter
    };
    assert!(tighter(|s| s.cpu_weight_min = 10));
    assert!(!tighter(|s| s.cpu_weight_max = 5000));
    assert!(tighter(|s| s.cpu_weight_max = 500));
    assert!(tighter(|s| s.io_weight_min = 5));
    assert!(tighter(|s| s.io_weight_max = 800));
    assert!(tighter(|s| s.uclamp_min_max = 30.0));
    assert!(!tighter(|s| s.uclamp_min_max = 60.0));
    assert!(tighter(|s| s.uclamp_max_floor = 30.0));
    assert!(!tighter(|s| s.uclamp_max_floor = 10.0));
    assert!(tighter(|s| s.io_bps_floor = 16 << 20));
    assert!(!tighter(|s| s.io_bps_floor = 1 << 20));
    assert!(tighter(|s| s.io_iops_floor = 200));
    assert!(tighter(|s| s.memory_high_floor = 512 << 20));
    assert!(tighter(|s| s.memory_high_ratio_floor = 0.7));
    assert!(tighter(|s| s.memory_low_max_ratio = 0.1));
    assert!(!tighter(|s| s.memory_low_max_ratio = 0.5));
    assert!(tighter(|s| s.nice_min = 0));
    assert!(!tighter(|s| s.nice_min = -15));
    assert!(compare_safety(&base, &base).is_empty());
    assert!(check_safety(&base, &base).is_ok());
    let mut bad = base.clone();
    bad.memory_high_ratio_floor = 1.5;
    assert!(check_safety(&base, &bad).is_err(), "tighter but nonsensical");
}

#[test]
fn rejects_unknown_profiles() {
    let fx = Fx::new();
    assert_eq!(one(&fx, ChangeOp::ProfileUse { scope: Scope::System, name: "turbo".into() }), Rule::ProfileUnknown);
    assert_eq!(one(&fx, ChangeOp::ProfileUse { scope: Scope::System, name: "Bad Name".into() }), Rule::ProfileUnknown);
    assert_eq!(one(&fx, set("general.profile", json!("turbo"))), Rule::ProfileUnknown);
    assert!(rules(&fx, vec![ChangeOp::ProfileUse { scope: Scope::System, name: "auto".into() }]).is_empty());
    assert!(rules(&fx, vec![ChangeOp::ProfileUse { scope: Scope::System, name: "gaming".into() }]).is_empty());
}

#[test]
fn rejects_bad_override_globs() {
    let fx = Fx::new();
    let op = |u: &str| ChangeOp::ClassOverride { scope: Scope::System, unit: u.into(), class: QosClass::Batch };
    assert_eq!(one(&fx, op("*")), Rule::Glob);
    assert_eq!(one(&fx, op("a b.scope")), Rule::Glob);
    assert_eq!(one(&fx, op("[abc.scope")), Rule::Glob);
    assert!(rules(&fx, vec![op("docker-*.scope")]).is_empty());
}

#[test]
fn rejects_sysctls_outside_the_allow_list_or_range() {
    let fx = Fx::new();
    assert_eq!(one(&fx, sysctl("vm.overcommit_memory", "1")), Rule::SysctlNotAllowed);
    assert_eq!(one(&fx, sysctl("kernel.panic", "1")), Rule::SysctlNotAllowed);
    assert_eq!(one(&fx, sysctl("vm.swappiness", "201")), Rule::OutOfRange);
    assert_eq!(one(&fx, sysctl("vm.page-cluster", "4")), Rule::OutOfRange);
    assert_eq!(one(&fx, sysctl("vm.watermark_scale_factor", "5")), Rule::OutOfRange);
    assert_eq!(one(&fx, sysctl("vm.vfs_cache_pressure", "1000")), Rule::OutOfRange);
    assert_eq!(one(&fx, sysctl("vm.swappiness", "ten")), Rule::OutOfRange);
    for (k, v) in [
        ("vm.swappiness", "0"),
        ("vm.swappiness", "200"),
        ("vm.page-cluster", "0"),
        ("vm.watermark_scale_factor", "1000"),
    ] {
        assert!(rules(&fx, vec![sysctl(k, v)]).is_empty(), "{k}={v}");
    }
}

#[test]
fn rejects_unknown_devices_and_schedulers() {
    let fx = Fx::new();
    let op = |d: &str, s: &str| ChangeOp::IoScheduler { device: d.into(), scheduler: s.into(), persist: false };
    assert_eq!(one(&fx, op("8:16", "bfq")), Rule::DeviceUnknown);
    assert_eq!(one(&fx, op("../../etc", "bfq")), Rule::DeviceUnknown);
    assert_eq!(one(&fx, op("sdz", "bfq")), Rule::DeviceUnknown);
    // NVMe here only offers none and mq-deadline.
    assert_eq!(one(&fx, op("259:0", "bfq")), Rule::SchedulerUnavailable);
    assert_eq!(one(&fx, op("sda", "cfq")), Rule::SchedulerUnavailable);
    assert!(rules(&fx, vec![op("259:0", "mq-deadline"), op("sda", "kyber")]).is_empty());
}

#[test]
fn rejects_unadvertised_epp_and_governors() {
    let fx = Fx::new();
    assert_eq!(one(&fx, ChangeOp::CpuEpp { preference: "turbo".into() }), Rule::EppUnavailable);
    assert_eq!(one(&fx, ChangeOp::CpuGovernor { governor: "schedutil".into() }), Rule::GovernorUnavailable);
    let mut ctx = fx.ctx(Scope::System, Origin::Cli);
    ctx.hardware.features.epp_available.clear();
    let r = validate(&ChangeOp::CpuEpp { preference: "performance".into() }, &ctx).unwrap_err();
    assert_eq!(r.rule, Rule::EppUnavailable);
    assert!(rules(&fx, vec![ChangeOp::CpuEpp { preference: "balance_power".into() }]).is_empty());
}

#[test]
fn rejects_bad_zswap_settings() {
    let fx = Fx::new();
    let z = |e: Option<bool>, c: Option<&str>, p: Option<u8>| ChangeOp::Zswap {
        enabled: e,
        compressor: c.map(String::from),
        max_pool_percent: p,
    };
    assert_eq!(one(&fx, z(None, None, None)), Rule::Zswap);
    assert_eq!(one(&fx, z(None, Some("rot13"), None)), Rule::Zswap);
    assert_eq!(one(&fx, z(None, Some("842"), None)), Rule::Zswap, "allowed but not in /proc/crypto");
    assert_eq!(one(&fx, z(None, None, Some(90))), Rule::OutOfRange);
    assert!(rules(&fx, vec![z(Some(true), Some("lz4"), Some(30))]).is_empty());
}

#[test]
fn rejects_what_the_kernel_lacks() {
    let fx = Fx::new();
    std::fs::remove_file(fx.roots.proc.join("sys/vm/watermark_scale_factor")).unwrap();
    assert_eq!(one(&fx, sysctl("vm.watermark_scale_factor", "100")), Rule::NotAvailable);
    std::fs::remove_dir_all(fx.roots.sys.join("module/zswap")).unwrap();
    assert_eq!(
        one(&fx, ChangeOp::Zswap { enabled: Some(true), compressor: None, max_pool_percent: None }),
        Rule::NotAvailable
    );
    // No config file configured for the scope.
    let store = fx.store(Scope::System);
    let mut ctx = fx.ctx(Scope::System, Origin::Cli);
    ctx.config_paths.system = None;
    let plan = store.plan(&[set("general.dry_run", json!(true))], &ctx);
    assert_eq!(plan.rejections[0].rule, Rule::NotAvailable);
}

#[test]
fn risk_levels() {
    let fx = Fx::new();
    let ctx = fx.ctx(Scope::System, Origin::Cli);
    assert_eq!(validate(&set("general.dry_run", json!(true)), &ctx).unwrap(), Risk::Low);
    assert_eq!(validate(&ChangeOp::ProfileUse { scope: Scope::System, name: "auto".into() }, &ctx).unwrap(), Risk::Low);
    assert_eq!(validate(&sysctl("vm.swappiness", "10"), &ctx).unwrap(), Risk::Medium);
    assert_eq!(
        validate(&ChangeOp::Zswap { enabled: Some(false), compressor: None, max_pool_percent: None }, &ctx).unwrap(),
        Risk::Medium
    );
    assert_eq!(validate(&ChangeOp::CpuGovernor { governor: "performance".into() }, &ctx).unwrap(), Risk::High);
    let store = fx.store(Scope::System);
    let plan =
        store.plan(&[set("general.dry_run", json!(true)), ChangeOp::CpuEpp { preference: "power".into() }], &ctx);
    assert_eq!(plan.risk, Risk::High);
}

#[test]
fn config_edits_preserve_the_shipped_file() {
    let fx = Fx::new();
    let store = fx.store(Scope::System);
    let before = fx.system_config();
    let ops = vec![
        set("plugins.threshold.config.values.fg_epoch_uclamp_min", json!(40)),
        set("plugins.threshold.config.enter.io", json!(0.12)),
        set("telemetry.retention.raw", json!("3days")),
        ChangeOp::ClassOverride { scope: Scope::System, unit: "docker-*.scope".into(), class: QosClass::Batch },
    ];
    let cs = fx.apply(&store, Origin::Cli, ops, None).unwrap();
    let after = fx.system_config();
    // Only the touched lines change; every comment survives.
    let comments = |s: &str| s.lines().filter(|l| l.trim_start().starts_with('#')).count();
    assert_eq!(comments(&before), comments(&after));
    let d = lindesk_changes::diff::unified(&before, &after, "a", "b", 0);
    let changed: Vec<&str> = d
        .lines()
        .filter(|l| (l.starts_with('-') || l.starts_with('+')) && !l.starts_with("---") && !l.starts_with("+++"))
        .collect();
    assert_eq!(
        &changed[..6],
        [
            "-raw = \"7days\"",
            "+raw = \"3days\"",
            "-io = \"auto\"",
            "+io = 0.12",
            "-fg_epoch_uclamp_min = \"auto\"",
            "+fg_epoch_uclamp_min = 40",
        ],
        "{d}"
    );
    let added: Vec<&str> = changed[6..].iter().map(|l| l.trim_start_matches('+')).filter(|l| !l.is_empty()).collect();
    assert_eq!(added, ["[[override]]", "unit = \"docker-*.scope\"", "class = \"batch\""], "{d}");
    store.revert(cs.id, Author::new(0, Origin::Cli), false).unwrap();
    assert_eq!(fx.system_config(), before);
}
