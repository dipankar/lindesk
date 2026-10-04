mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use lindesk_changes::*;
use lindesk_plugin_api::QosClass;
use serde_json::json;

const MIN: i64 = 60_000;

#[test]
fn plan_apply_confirm_revert() {
    let fx = Fx::new();
    let store = fx.store(Scope::System);
    let original = fx.system_config();
    let ops = vec![
        set("plugins.threshold.config.epoch", json!("800ms")),
        ChangeOp::ProfileUse { scope: Scope::System, name: "latency".into() },
        sysctl("vm.swappiness", "10"),
    ];
    let ctx = fx.ctx(Scope::System, Origin::Cli);
    let plan = store.plan(&ops, &ctx);
    assert!(plan.is_ok(), "{:?}", plan.rejections);
    assert_eq!(plan.risk, Risk::Medium);
    assert_eq!(plan.ops[0].current, json!("auto"));
    assert_eq!(plan.ops[2].current, json!("60"));
    // A real TOML diff for the config edits, plus a block for the sysctl.
    assert!(plan.diff.contains("-epoch = \"auto\"\n+epoch = \"800ms\"\n"), "{}", plan.diff);
    assert!(plan.diff.contains("+profile = \"latency\""), "{}", plan.diff);
    assert!(plan.diff.contains("--- a/proc/sys/vm/swappiness\n+++ b/proc/sys/vm/swappiness\n"), "{}", plan.diff);
    assert!(plan.diff.contains("-60\n+10\n"));
    // Planning changed nothing.
    assert_eq!(fx.system_config(), original);
    assert_eq!(fx.sysctl("swappiness"), "60");

    let cs = store.apply(&plan, Author::new(0, Origin::Cli).with_client("lindeskctl"), "snappier", None).unwrap();
    assert_eq!((cs.id, cs.status, cs.parent), (1, Status::Applied, None));
    assert_eq!(cs.ops[2].previous, json!("60"));
    assert!(cs.ops.iter().all(|o| o.done));
    assert_eq!(cs.config_snapshot.as_deref(), Some("config/000001.toml"));
    assert_eq!(fx.sysctl("swappiness"), "10");
    let cfg = fx.system_config();
    assert!(cfg.contains("# auto [500ms]: 300ms on battery, 500ms on AC.\nepoch = \"800ms\"\n"), "comment kept");
    let parsed = lindesk_core::config::Config::from_toml(&cfg).unwrap();
    assert_eq!(parsed.general.profile, "latency");

    assert_eq!(store.confirm(1).unwrap().status, Status::Confirmed);
    assert_eq!(store.confirm(1).unwrap().status, Status::Confirmed, "idempotent");

    let rv = store.revert(1, Author::new(0, Origin::Cli), false).unwrap();
    assert_eq!((rv.id, rv.reverts, rv.status, rv.parent), (2, Some(1), Status::Applied, Some(1)));
    // Ops restore in reverse order.
    assert_eq!(rv.ops[0].op, sysctl("vm.swappiness", "60"));
    assert_eq!(rv.ops[1].op, ChangeOp::ConfigUnset { scope: Scope::System, key: "general.profile".into() });
    assert_eq!(fx.sysctl("swappiness"), "60");
    assert_eq!(fx.system_config(), original, "byte-identical after revert");
    let first = store.get(1).unwrap();
    assert_eq!((first.status, first.reverted_by), (Status::Reverted, Some(2)));
    assert!(matches!(
        store.revert(1, Author::new(0, Origin::Cli), false),
        Err(ChangeError::AlreadyReverted { by: Some(2), .. })
    ));
    assert!(matches!(store.confirm(1), Err(ChangeError::AlreadyReverted { .. })));

    let h = store.history(10).unwrap();
    assert_eq!(h.iter().map(|c| c.id).collect::<Vec<_>>(), [2, 1]);
    assert_eq!(store.head().unwrap(), 2);
    assert_eq!(fx.event_names(), ["change.applied", "change.confirmed", "change.reverted"]);
    // Version diff between snapshots.
    let d = store.diff(0, 1).unwrap();
    assert!(d.contains("-epoch = \"auto\"\n+epoch = \"800ms\"\n"), "{d}");
    assert_eq!(store.diff(0, 2).unwrap(), "");
    assert!(store.diff(1, 2).unwrap().contains("+epoch = \"auto\""));
}

#[test]
fn revert_conflicts_and_revert_to() {
    let fx = Fx::new();
    let store = fx.store(Scope::System);
    for v in ["10", "20", "30"] {
        fx.apply(&store, Origin::Cli, vec![sysctl("vm.swappiness", v)], None).unwrap();
    }
    fx.apply(&store, Origin::Cli, vec![sysctl("vm.page-cluster", "0")], None).unwrap();
    assert_eq!(fx.sysctl("swappiness"), "30");

    // Reverting 1 would clobber 2 and 3.
    match store.revert(1, Author::new(0, Origin::Cli), false) {
        Err(e @ ChangeError::Conflict { .. }) => {
            let ChangeError::Conflict { later, keys, .. } = &e else { unreachable!() };
            assert_eq!(later, &[2, 3]);
            assert_eq!(keys, &["sysctl:vm.swappiness"]);
            assert!(e.to_string().contains("revert_to(0)"), "{e}");
        }
        r => panic!("{r:?}"),
    }
    // Unrelated changeset 4 reverts fine.
    store.revert(4, Author::new(0, Origin::Cli), false).unwrap();
    assert_eq!(fx.sysctl("page-cluster"), "3");

    // Back to version 1: one changeset unwinds 5, 4, 3, 2 (newest first).
    let rv = store.revert_to(1, Author::new(0, Origin::Cli)).unwrap().unwrap();
    assert_eq!((rv.id, rv.revert_to, rv.ops.len()), (6, Some(1), 4));
    assert_eq!(fx.sysctl("swappiness"), "10");
    assert_eq!(fx.sysctl("page-cluster"), "3");
    for id in 2..=5 {
        assert_eq!(store.get(id).unwrap().status, Status::Reverted, "{id}");
    }
    assert_eq!(store.get(3).unwrap().reverted_by, Some(6));
    assert!(store.get(1).unwrap().is_effective());
    assert!(store.revert_to(6, Author::new(0, Origin::Cli)).unwrap().is_none(), "nothing after 6");

    // Undoing the revert_to restores the state before it; revert_to(0) unwinds everything.
    store.revert(6, Author::new(0, Origin::Cli), false).unwrap();
    assert_eq!(fx.sysctl("swappiness"), "30");
    assert_eq!(fx.sysctl("page-cluster"), "3");
    store.revert_to(0, Author::new(0, Origin::Cli)).unwrap().unwrap();
    assert_eq!(fx.sysctl("swappiness"), "60");
    assert_eq!(fx.sysctl("page-cluster"), "3");
    let h = store.history(100).unwrap();
    assert_eq!(h.iter().filter(|c| c.is_effective()).count(), 1, "only the revert_to itself");
    assert_eq!(h[0].revert_to, Some(0));

    // force overrides the conflict check.
    let a = fx.apply(&store, Origin::Cli, vec![sysctl("vm.swappiness", "11")], None).unwrap();
    fx.apply(&store, Origin::Cli, vec![sysctl("vm.swappiness", "12")], None).unwrap();
    store.revert(a.id, Author::new(0, Origin::Cli), true).unwrap();
    assert_eq!(fx.sysctl("swappiness"), "60");
}

#[test]
fn dead_man_switch() {
    let fx = Fx::new();
    let store = fx.store(Scope::System);
    let ttl = Some(Duration::from_secs(600));
    let a = fx.apply(&store, Origin::Mcp, vec![sysctl("vm.swappiness", "10")], ttl).unwrap();
    assert_eq!(a.ttl_until, Some(T0 + 10 * MIN));
    let b = fx.apply(&store, Origin::Mcp, vec![sysctl("vm.vfs_cache_pressure", "50")], ttl).unwrap();
    store.confirm(b.id).unwrap();
    let c = fx.apply(&store, Origin::Cli, vec![sysctl("vm.page-cluster", "0")], None).unwrap();
    assert_eq!(store.next_expiry().unwrap(), Some(T0 + 10 * MIN));

    assert!(store.expire_due(T0 + 5 * MIN).unwrap().is_empty());
    assert_eq!(fx.sysctl("swappiness"), "10");
    let expired = store.expire_due(T0 + 10 * MIN).unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!((expired[0].id, expired[0].status), (a.id, Status::Reverted));
    let rv = store.get(expired[0].reverted_by.unwrap()).unwrap();
    assert_eq!(rv.origin, Origin::Daemon);
    assert_eq!(rv.author.client.as_deref(), Some("dead-man"));
    assert_eq!(fx.sysctl("swappiness"), "60");
    assert_eq!(fx.sysctl("vfs_cache_pressure"), "50", "confirmed: kept");
    assert_eq!(fx.sysctl("page-cluster"), "0", "no ttl: kept");
    assert!(store.get(c.id).unwrap().is_effective());
    assert!(fx.event_names().contains(&"change.expired".to_string()));
    assert!(store.expire_due(T0 + 60 * MIN).unwrap().is_empty());
    assert_eq!(store.next_expiry().unwrap(), None);
}

#[test]
fn expiry_leaves_values_a_later_changeset_owns() {
    let fx = Fx::new();
    let store = fx.store(Scope::System);
    let ttl = Some(Duration::from_secs(60));
    fx.apply(&store, Origin::Mcp, vec![sysctl("vm.swappiness", "10"), sysctl("vm.page-cluster", "1")], ttl).unwrap();
    // The admin later sets swappiness for good.
    fx.apply(&store, Origin::Cli, vec![sysctl("vm.swappiness", "30")], None).unwrap();
    let out = store.expire_due(T0 + 2 * MIN).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(fx.sysctl("swappiness"), "30", "owned by changeset 2");
    assert_eq!(fx.sysctl("page-cluster"), "3", "reverted");
}

#[test]
fn ttl_rules() {
    let fx = Fx::new();
    let store = fx.store(Scope::System);
    let ops = vec![ChangeOp::CpuGovernor { governor: "performance".into() }];
    let plan = store.plan(&ops, &fx.ctx(Scope::System, Origin::Mcp));
    assert!(plan.is_ok() && plan.ttl_required && plan.risk == Risk::High);
    let err = store.apply(&plan, Author::new(1000, Origin::Mcp), "", None).unwrap_err();
    assert!(matches!(&err, ChangeError::Rejected(r) if r[0].rule == Rule::TtlRequired), "{err}");
    let err = store.apply(&plan, Author::new(1000, Origin::Mcp), "", Some(Duration::ZERO)).unwrap_err();
    assert!(matches!(&err, ChangeError::Rejected(r) if r[0].rule == Rule::TtlRequired));
    let err = store.apply(&plan, Author::new(1000, Origin::Mcp), "", Some(Duration::from_secs(48 * 3600))).unwrap_err();
    assert!(matches!(&err, ChangeError::Rejected(r) if r[0].rule == Rule::OutOfRange));
    store.apply(&plan, Author::new(1000, Origin::Mcp), "", Some(DEFAULT_MCP_TTL)).unwrap();
    // The CLI may apply High risk without a ttl.
    let plan =
        store.plan(&[ChangeOp::CpuGovernor { governor: "powersave".into() }], &fx.ctx(Scope::System, Origin::Cli));
    assert!(!plan.ttl_required);
    store.apply(&plan, Author::new(0, Origin::Cli), "", None).unwrap();
}

#[test]
fn cpufreq_previous_values_are_per_policy() {
    let fx = Fx::new();
    std::fs::write(fx.roots.sys.join("devices/system/cpu/cpufreq/policy2/energy_performance_preference"), "power\n")
        .unwrap();
    let store = fx.store(Scope::System);
    let plan =
        store.plan(&[ChangeOp::CpuEpp { preference: "performance".into() }], &fx.ctx(Scope::System, Origin::Cli));
    assert!(plan.diff.contains("-policy2: power\n"), "{}", plan.diff);
    assert!(plan.diff.contains("+policy2: performance\n"));
    let cs = store.apply(&plan, Author::new(0, Origin::Cli), "", None).unwrap();
    assert_eq!(cs.ops[0].previous["policy2"], "power");
    assert_eq!(cs.ops[0].previous["policy0"], "balance_performance");
    for i in 0..4 {
        assert_eq!(
            fx.sys(&format!("devices/system/cpu/cpufreq/policy{i}/energy_performance_preference")),
            "performance"
        );
    }
    store.revert(cs.id, Author::new(0, Origin::Cli), false).unwrap();
    assert_eq!(fx.sys("devices/system/cpu/cpufreq/policy2/energy_performance_preference"), "power");
    assert_eq!(fx.sys("devices/system/cpu/cpufreq/policy1/energy_performance_preference"), "balance_performance");
}

#[test]
fn scheduler_zswap_and_persistence() {
    let fx = Fx::new();
    let store = fx.store(Scope::System);
    let conf = fx.roots.sysctl_conf();
    let rules = fx.roots.udev_rules();
    let cs = fx
        .apply(
            &store,
            Origin::Cli,
            vec![
                ChangeOp::IoScheduler { device: "8:0".into(), scheduler: "bfq".into(), persist: true },
                ChangeOp::Sysctl { key: "vm.swappiness".into(), value: "20".into(), persist: true },
                ChangeOp::Zswap { enabled: Some(true), compressor: Some("zstd".into()), max_pool_percent: Some(25) },
            ],
            None,
        )
        .unwrap();
    assert_eq!(fx.sys("block/sda/queue/scheduler"), "bfq");
    assert_eq!(cs.ops[0].previous, json!({"runtime": "mq-deadline", "persisted": null}));
    let r = std::fs::read_to_string(&rules).unwrap();
    assert!(r.contains("KERNEL==\"sda\", ATTR{queue/scheduler}=\"bfq\""), "{r}");
    assert!(std::fs::read_to_string(&conf).unwrap().contains("\nvm.swappiness = 20\n"));
    assert_eq!(fx.sys("module/zswap/parameters/enabled"), "Y");
    assert_eq!(fx.sys("module/zswap/parameters/compressor"), "zstd");
    assert_eq!(fx.sys("module/zswap/parameters/max_pool_percent"), "25");

    // A second persisted sysctl shares the file; reverting the first keeps the second.
    let second = fx
        .apply(
            &store,
            Origin::Cli,
            vec![ChangeOp::Sysctl { key: "vm.page-cluster".into(), value: "0".into(), persist: true }],
            None,
        )
        .unwrap();
    store.revert(cs.id, Author::new(0, Origin::Cli), false).unwrap();
    assert!(!rules.exists(), "udev rule removed with the last line");
    let c = std::fs::read_to_string(&conf).unwrap();
    assert!(!c.contains("swappiness") && c.contains("vm.page-cluster = 0"), "{c}");
    assert_eq!(fx.sys("block/sda/queue/scheduler"), "mq-deadline");
    assert_eq!(fx.sysctl("swappiness"), "60");
    assert_eq!(fx.sys("module/zswap/parameters/enabled"), "N");
    assert_eq!(fx.sys("module/zswap/parameters/compressor"), "lzo-rle");
    store.revert(second.id, Author::new(0, Origin::Cli), false).unwrap();
    assert!(!conf.exists());
}

#[test]
fn failed_write_rolls_back_earlier_ops() {
    let fx = Fx::new();
    let original = fx.system_config();
    let store = fx.store_with(Scope::System, Arc::new(Faulty::new(fx.applier(), 2, false, false)));
    let ops = vec![
        set("general.dry_run", json!(true)),
        sysctl("vm.swappiness", "10"),
        ChangeOp::CpuGovernor { governor: "performance".into() },
    ];
    let plan = store.plan(&ops, &fx.ctx(Scope::System, Origin::Cli));
    let err = store.apply(&plan, Author::new(0, Origin::Cli), "", None).unwrap_err();
    let cs = err.changeset().expect("failed changeset").clone();
    assert_eq!(cs.status, Status::Failed);
    assert!(cs.error.as_deref().unwrap().contains("simulated EBUSY"), "{:?}", cs.error);
    assert!(cs.ops.iter().all(|o| !o.done));
    assert_eq!(fx.sysctl("swappiness"), "60");
    assert_eq!(fx.system_config(), original);
    assert_eq!(fx.sys("devices/system/cpu/cpufreq/policy0/scaling_governor"), "powersave");
    assert_eq!(store.get(1).unwrap().status, Status::Failed);
    assert_eq!(fx.event_names(), ["change.failed"]);
    // A failed changeset can't be reverted or confirmed.
    assert!(matches!(store.revert(1, Author::new(0, Origin::Cli), false), Err(ChangeError::InvalidState { .. })));
}

#[test]
fn crash_after_wal_before_any_write() {
    let fx = Fx::new();
    let original = fx.system_config();
    let crashing = fx.store_with(Scope::System, Arc::new(Faulty::new(fx.applier(), 0, true, false)));
    let plan = crashing.plan(
        &[set("general.dry_run", json!(true)), sysctl("vm.swappiness", "10")],
        &fx.ctx(Scope::System, Origin::Cli),
    );
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = crashing.apply(&plan, Author::new(0, Origin::Cli), "", None);
    }));
    assert!(r.is_err());
    // On disk: the write-ahead record, nothing applied.
    let wal = crashing.get(1).unwrap();
    assert_eq!(wal.status, Status::Applying);
    assert_eq!(wal.ops[1].previous, json!("60"));

    // "Restart": a fresh handle recovers.
    let store = fx.store(Scope::System);
    let rec = store.recover().unwrap();
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0].status, Status::Failed);
    assert_eq!(fx.sysctl("swappiness"), "60");
    assert_eq!(fx.system_config(), original);
    assert!(store.recover().unwrap().is_empty(), "idempotent");
    // Ids keep increasing after recovery.
    assert_eq!(fx.apply(&store, Origin::Cli, vec![sysctl("vm.swappiness", "5")], None).unwrap().id, 2);
}

#[test]
fn crash_after_partial_apply() {
    let fx = Fx::new();
    let original = fx.system_config();
    // Ops 0 and 1 are written and marked done; op 2 is written, then the process dies
    // before the record says so.
    let crashing = fx.store_with(Scope::System, Arc::new(Faulty::new(fx.applier(), 2, true, true)));
    let ops = vec![
        set("plugins.threshold.config.epoch", json!("1s")),
        sysctl("vm.swappiness", "10"),
        ChangeOp::CpuGovernor { governor: "performance".into() },
        sysctl("vm.page-cluster", "0"),
    ];
    let plan = crashing.plan(&ops, &fx.ctx(Scope::System, Origin::Cli));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = crashing.apply(&plan, Author::new(0, Origin::Cli), "", None);
    }));
    assert!(r.is_err());
    let wal = crashing.get(1).unwrap();
    assert_eq!(wal.ops.iter().map(|o| o.done).collect::<Vec<_>>(), [true, true, false, false]);
    assert_eq!(fx.sysctl("swappiness"), "10");
    assert_eq!(fx.sys("devices/system/cpu/cpufreq/policy3/scaling_governor"), "performance");

    let store = fx.store(Scope::System);
    let rec = store.recover().unwrap();
    assert_eq!(rec[0].status, Status::Failed);
    assert_eq!(fx.system_config(), original);
    assert_eq!(fx.sysctl("swappiness"), "60");
    assert_eq!(fx.sys("devices/system/cpu/cpufreq/policy3/scaling_governor"), "powersave", "in-flight op rolled back");
    assert_eq!(fx.sysctl("page-cluster"), "3", "never written");
    assert_eq!(fx.event_names().last().unwrap(), "change.failed");
}

#[test]
fn rate_limit_per_origin_with_injected_time() {
    let fx = Fx::new();
    let store = fx.store(Scope::System);
    let ctx = fx.ctx(Scope::System, Origin::Mcp);
    for i in 0..10 {
        fx.clock.advance(1_000);
        fx.apply(&store, Origin::Mcp, vec![sysctl("vm.swappiness", &format!("{}", 10 + i))], None).unwrap();
    }
    // 11th within 10 minutes: refused at plan time and at apply time.
    let plan = store.plan(&[sysctl("vm.swappiness", "50")], &ctx);
    assert_eq!(plan.rejections[0].rule, Rule::RateLimited);
    let ok_plan = store.plan(&[sysctl("vm.swappiness", "50")], &fx.ctx(Scope::System, Origin::Cli));
    let err = store.apply(&ok_plan, Author::new(1000, Origin::Mcp), "", None).unwrap_err();
    assert!(matches!(&err, ChangeError::Rejected(r) if r[0].rule == Rule::RateLimited));
    // Other origins are unaffected; reverts are never limited.
    fx.apply(&store, Origin::Cli, vec![sysctl("vm.page-cluster", "1")], None).unwrap();
    store.revert(10, Author::new(1000, Origin::Mcp), false).unwrap();
    // The window slides.
    fx.clock.advance(10 * MIN - 9_000);
    assert!(store.plan(&[sysctl("vm.swappiness", "50")], &ctx).is_ok());
    // Limits are configurable.
    let strict = fx.store(Scope::System).with_limits(Limits { rate_count: 1, ..Limits::default() });
    assert_eq!(
        strict.plan(&[sysctl("vm.swappiness", "50")], &fx.ctx(Scope::System, Origin::Cli)).rejections[0].rule,
        Rule::RateLimited
    );
}

#[test]
fn concurrent_handles_do_not_corrupt_history() {
    let fx = Fx::new();
    let limits = Limits { rate_count: 1000, ..Limits::default() };
    let a = Arc::new(fx.store(Scope::System).with_limits(limits.clone()));
    let b = Arc::new(fx.store(Scope::System).with_limits(limits));
    let ctx = fx.ctx(Scope::System, Origin::Cli);
    let threads: Vec<_> = [(a.clone(), "vm.swappiness"), (b.clone(), "vm.vfs_cache_pressure")]
        .into_iter()
        .enumerate()
        .map(|(t, (store, key))| {
            let ctx = ctx.clone();
            let key = key.to_string();
            std::thread::spawn(move || {
                for i in 0..12 {
                    let ops = vec![
                        sysctl(&key, &format!("{}", 60 + i)),
                        ChangeOp::ClassOverride {
                            scope: Scope::System,
                            unit: format!("t{t}-{i}.scope"),
                            class: QosClass::Batch,
                        },
                    ];
                    let plan = store.plan(&ops, &ctx);
                    assert!(plan.is_ok(), "{:?}", plan.rejections);
                    store.apply(&plan, Author::new(0, Origin::Cli), "", None).unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let h = a.history(100).unwrap();
    assert_eq!(h.len(), 24);
    let mut ids: Vec<u64> = h.iter().map(|c| c.id).collect();
    ids.sort();
    assert_eq!(ids, (1..=24).collect::<Vec<_>>());
    assert!(h.iter().all(|c| c.status == Status::Applied));
    assert_eq!(b.head().unwrap(), 24);
    // Parents form a chain.
    for c in &h {
        assert_eq!(c.parent, (c.id > 1).then(|| c.id - 1));
    }
    // Every config edit landed (the store lock serializes read-modify-write).
    let cfg = lindesk_core::config::Config::from_toml(&fx.system_config()).unwrap();
    assert_eq!(cfg.overrides.len(), 24);
    assert_eq!(fx.sysctl("swappiness"), "71");
    assert_eq!(fx.sysctl("vfs_cache_pressure"), "71");
}

#[test]
fn class_override_round_trip_in_user_scope() {
    let fx = Fx::new();
    let store = fx.store(Scope::User);
    // The user file doesn't exist yet.
    let cs = fx
        .apply(
            &store,
            Origin::Dbus,
            vec![ChangeOp::ClassOverride {
                scope: Scope::User,
                unit: "app-steam*.scope".into(),
                class: QosClass::Interactive,
            }],
            None,
        )
        .unwrap();
    let user = std::fs::read_to_string(fx.paths.user.as_ref().unwrap()).unwrap();
    assert_eq!(user, "[[override]]\nunit = \"app-steam*.scope\"\nclass = \"interactive\"\n");
    store.revert(cs.id, Author::new(1000, Origin::Dbus), false).unwrap();
    assert_eq!(std::fs::read_to_string(fx.paths.user.as_ref().unwrap()).unwrap(), "");
    // System-scope ops are refused by a user store.
    let plan = store.plan(&[sysctl("vm.swappiness", "10")], &fx.ctx(Scope::User, Origin::Dbus));
    assert_eq!(plan.rejections[0].rule, Rule::Scope);
}

#[test]
fn deserialized_plans_cannot_be_applied() {
    let fx = Fx::new();
    let store = fx.store(Scope::System);
    let plan = store.plan(&[sysctl("vm.swappiness", "10")], &fx.ctx(Scope::System, Origin::Cli));
    let json = serde_json::to_string(&plan).unwrap();
    let back: Plan = serde_json::from_str(&json).unwrap();
    assert!(matches!(store.apply(&back, Author::new(0, Origin::Cli), "", None), Err(ChangeError::NotPlanned)));
}

#[test]
fn changeset_json_shape() {
    let fx = Fx::new();
    let store = fx.store(Scope::System);
    let ops: Vec<ChangeOp> = serde_json::from_value(json!([
        {"op": "sysctl", "key": "vm.swappiness", "value": "10"},
        {"op": "config_set", "scope": "system", "key": "plugins.threshold.config.values.fg_epoch_uclamp_min", "value": 40.0}
    ]))
    .unwrap();
    let plan = store.plan(&ops, &fx.ctx(Scope::System, Origin::Mcp));
    let cs = store
        .apply(
            &plan,
            Author::new(1000, Origin::Mcp).with_client("claude-code").with_bus_name(":1.42"),
            "reduce swapping",
            Some(DEFAULT_MCP_TTL),
        )
        .unwrap();
    let v = serde_json::to_value(&cs).unwrap();
    println!("{}", serde_json::to_string_pretty(&v).unwrap());
    assert_eq!(v["ops"][0]["op"], "sysctl");
    assert_eq!(v["ops"][0]["previous"], "60");
    assert_eq!(v["ops"][0]["applied"], "10");
    assert_eq!(v["ops"][1]["previous"], "auto");
    assert_eq!(v["author"]["client"], "claude-code");
    assert_eq!(v["status"], "applied");
    assert_eq!(v["origin"], "mcp");
    assert_eq!(serde_json::from_value::<ChangeSet>(v).unwrap(), cs);
    match cs.notice("change.applied") {
        lindesk_plugin_api::Event::Notice { name, data, message, .. } => {
            assert_eq!(name, "change.applied");
            assert_eq!(data["id"], 1);
            assert_eq!(message, "changeset 1 applied: reduce swapping");
        }
        e => panic!("{e:?}"),
    }
}
