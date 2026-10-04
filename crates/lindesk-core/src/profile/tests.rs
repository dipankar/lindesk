use std::path::{Path, PathBuf};
use std::time::Duration;

use lindesk_plugin_api::{CpuFeatures, CpuVendor, Hardware, PowerSource, QosClass, StorageDevice, StorageKind};
use serde_json::json;

use super::*;
use crate::config::{Config, Override};

const GIB: u64 = 1 << 30;

fn shipped_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/config/profiles.d")
}

fn threshold(c: &Config) -> Value {
    c.plugin("threshold").config
}

fn disk(kind: StorageKind) -> StorageDevice {
    StorageDevice { dev: "8:0".into(), name: "sda".into(), kind, scheduler: "none".into(), nominal_bps: 0 }
}

/// Intel Alder Lake laptop, unplugged.
fn intel_hybrid_laptop_on_battery() -> Hardware {
    Hardware {
        cpus: 16,
        cores: 12,
        smt: true,
        performance_cpus: (0..8).collect(),
        efficiency_cpus: (8..16).collect(),
        cpu_model: "12th Gen Intel(R) Core(TM) i7-1260P".into(),
        mem_total: 16 * GIB,
        storage: vec![disk(StorageKind::Nvme)],
        power: PowerSource::Battery,
        portable: true,
        arch: "x86_64".into(),
        vendor: CpuVendor::Intel,
        features: CpuFeatures { itmt: true, ..Default::default() },
        ..Default::default()
    }
}

/// RK3588-style board: 4×A76 + 4×A55, EAS on, eMMC, mains powered.
fn arm_eas_board() -> Hardware {
    Hardware {
        cpus: 8,
        cores: 8,
        performance_cpus: (4..8).collect(),
        efficiency_cpus: (0..4).collect(),
        mem_total: 8 * GIB,
        storage: vec![disk(StorageKind::Emmc)],
        power: PowerSource::Ac,
        arch: "aarch64".into(),
        vendor: CpuVendor::Arm,
        features: CpuFeatures { eas: true, ..Default::default() },
        ..Default::default()
    }
}

/// Old desktop with a spinning disk behind /home.
fn hdd_desktop() -> Hardware {
    Hardware {
        cpus: 4,
        cores: 4,
        mem_total: 8 * GIB,
        storage: vec![disk(StorageKind::Ssd), disk(StorageKind::Hdd)],
        power: PowerSource::Ac,
        arch: "x86_64".into(),
        vendor: CpuVendor::Intel,
        ..Default::default()
    }
}

/// Ryzen desktop on AC with NVMe: nothing special.
fn amd_desktop() -> Hardware {
    Hardware {
        cpus: 16,
        cores: 8,
        smt: true,
        mem_total: 32 * GIB,
        storage: vec![disk(StorageKind::Nvme)],
        power: PowerSource::Ac,
        arch: "x86_64".into(),
        vendor: CpuVendor::Amd,
        ..Default::default()
    }
}

fn ctx(hw: &Hardware, app: Option<&str>) -> ProfileContext {
    ProfileContext::from_hardware(hw, app)
}

fn write(dir: &Path, file: &str, text: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(file), text).unwrap();
}

// ── shipped profiles ──────────────────────────────────────────────────────────────────

/// Keys of the threshold policy's config (crates/lindesk-builtins/src/policy/threshold/config.rs).
/// lindesk-core can't depend on lindesk-builtins, so the shipped profiles are checked
/// against this list; lindesk-changes' integration tests validate through the real parser.
const THRESHOLD_KEYS: &[&str] = &[
    "epoch",
    "ewma_half_life",
    "ewma_alpha",
    "enter",
    "exit_ratio",
    "foreground_yellow",
    "foreground_red",
    "red_after",
    "memory_full_red",
    "mem_red_after",
    "recover_after",
    "yellow_min_dwell",
    "red_min_dwell",
    "critical_memory_low",
    "fg_memory_low_max",
    "aimd_floor",
    "aimd_step",
    "aimd_decrease",
    "aimd_interval",
    "memory_high_ratio",
    "priority",
    "values",
];
const THRESHOLD_ENTER_KEYS: &[&str] = &["cpu", "io", "memory"];
const THRESHOLD_VALUE_KEYS: &[&str] = &[
    "critical_weight",
    "fg_epoch_weight",
    "fg_epoch_uclamp_min",
    "fg_weight",
    "background_weight",
    "batch_io_weight",
    "yellow_background_weight",
    "yellow_background_uclamp_max",
    "yellow_batch_io_weight",
    "yellow_batch_uclamp_max",
];

#[test]
fn every_shipped_profile_parses_and_validates() {
    let builtins = builtin_profiles();
    let names: Vec<_> = builtins.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        ["balanced", "latency", "battery", "throughput", "gaming", "arm-eas", "x86-hybrid", "hdd"],
        "every file in packaging/config/profiles.d must be embedded and parse"
    );
    let on_disk = std::fs::read_dir(shipped_dir()).unwrap().count();
    assert_eq!(on_disk, builtins.len(), "a profile file is missing from BUILTIN_FILES");

    let set = ProfileSet::builtin();
    let caps = crate::safety::SafetyCaps::default();
    for p in &builtins {
        assert!(p.builtin && !p.description.is_empty(), "{}", p.name);
        let eff = set.effective_config(&Config::default(), &p.name).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(eff.general.profile, "auto");
        // Only documented threshold tunables, within the safety caps.
        let t = &p.config["plugins"]["threshold"]["config"];
        if let Some(obj) = t.as_object() {
            for k in obj.keys() {
                assert!(THRESHOLD_KEYS.contains(&k.as_str()), "{}: unknown threshold key {k}", p.name);
            }
            for k in t["enter"].as_object().into_iter().flat_map(|o| o.keys()) {
                assert!(THRESHOLD_ENTER_KEYS.contains(&k.as_str()), "{}: enter.{k}", p.name);
            }
            for k in t["values"].as_object().into_iter().flat_map(|o| o.keys()) {
                assert!(THRESHOLD_VALUE_KEYS.contains(&k.as_str()), "{}: values.{k}", p.name);
            }
        }
        let v = &threshold(&eff)["values"];
        if let Some(u) = v["fg_epoch_uclamp_min"].as_f64() {
            assert!(u <= caps.uclamp_min_max as f64, "{}", p.name);
        }
        for k in ["yellow_background_uclamp_max", "yellow_batch_uclamp_max"] {
            if let Some(u) = v[k].as_f64() {
                assert!(u >= caps.uclamp_max_floor as f64, "{}: {k}", p.name);
            }
        }
        for k in ["fg_epoch_weight", "fg_weight", "background_weight", "yellow_background_weight"] {
            if let Some(w) = v[k].as_u64() {
                assert!((caps.cpu_weight_min..=caps.cpu_weight_max).contains(&w), "{}: {k}", p.name);
            }
        }
    }
    // balanced is the reference: it changes nothing.
    assert_eq!(set.effective_config(&Config::default(), "balanced").unwrap(), Config::default());
    // manual-only profiles
    assert!(!set.get("latency").unwrap().is_auto());
    assert!(!set.get("throughput").unwrap().is_auto());
    assert!(!set.get("balanced").unwrap().is_auto());
}

#[test]
fn shipped_dir_loads_like_the_builtins() {
    let (set, errors) = ProfileSet::load(&[shipped_dir()]);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(set.names().len(), 8);
    let g = set.get("gaming").unwrap();
    assert!(!g.builtin && g.source.ends_with("40-gaming.toml"));
    assert_eq!(g.config, builtin_profiles().into_iter().find(|p| p.name == "gaming").unwrap().config);
}

#[test]
fn priorities_rank_as_documented() {
    let set = ProfileSet::builtin();
    let pr = |n: &str| set.get(n).unwrap().priority().unwrap();
    assert!(pr("gaming") > pr("battery"));
    assert!(pr("battery") > pr("hdd"));
    assert!(pr("hdd") > pr("arm-eas"));
    assert_eq!(pr("arm-eas"), pr("x86-hybrid"));
}

// ── selection ─────────────────────────────────────────────────────────────────────────

#[test]
fn out_of_the_box_selection_on_fixture_hardware() {
    // No directories at all: only the embedded profiles.
    let (set, errors) = ProfileSet::load(&[]);
    assert!(errors.is_empty());
    let cases = [
        (intel_hybrid_laptop_on_battery(), "battery", "battery power"),
        (arm_eas_board(), "arm-eas", "aarch64 with EAS"),
        (hdd_desktop(), "hdd", "HDD storage"),
    ];
    for (hw, want, reason) in cases {
        let s = set.select(&ctx(&hw, None));
        assert_eq!(s.name, want, "{s:?}");
        assert_eq!(s.reason, reason);
        assert_eq!(s.matched[0], want);
    }
    let s = set.select(&ctx(&amd_desktop(), None));
    assert_eq!(s.name, "balanced");
    assert!(s.matched.is_empty());
    assert_eq!(s.reason, "default: no other profile matches this machine (AC power; x86_64 AMD, 16 CPUs; NVMe)");
    // And every selection yields a valid effective config.
    for hw in [intel_hybrid_laptop_on_battery(), arm_eas_board(), hdd_desktop(), amd_desktop()] {
        let s = set.select(&ctx(&hw, None));
        set.effective_config(&Config::default(), &s.name).unwrap();
    }
}

#[test]
fn selection_matrix() {
    let set = ProfileSet::builtin();
    let pick = |hw: &Hardware, app: Option<&str>| set.select(&ctx(hw, app));

    // The hybrid laptop plugged in: x86-hybrid.
    let mut ac = intel_hybrid_laptop_on_battery();
    ac.power = PowerSource::Ac;
    let s = pick(&ac, None);
    assert_eq!((s.name.as_str(), s.reason.as_str()), ("x86-hybrid", "x86_64 hybrid CPU"));
    // ... on battery, battery wins but x86-hybrid still matches.
    let s = pick(&intel_hybrid_laptop_on_battery(), None);
    assert_eq!(s.matched, ["battery", "x86-hybrid"]);
    // A game beats everything.
    for app in ["steam_app_570", "gamescope", "Cyberpunk2077.exe", "net.lutris.Lutris", "STEAM_APP_1"] {
        let s = pick(&intel_hybrid_laptop_on_battery(), Some(app));
        assert_eq!(s.name, "gaming", "{app}");
        assert_eq!(s.reason, format!("foreground app {app}"));
        assert_eq!(s.matched, ["gaming", "battery", "x86-hybrid"]);
    }
    assert_eq!(pick(&amd_desktop(), Some("org.gnome.Nautilus")).name, "balanced");
    assert_eq!(pick(&amd_desktop(), Some("firefox")).name, "balanced");
    // ARM on battery: battery; ARM without EAS: balanced; EAS on x86: not arm-eas.
    let mut arm = arm_eas_board();
    arm.power = PowerSource::Battery;
    assert_eq!(pick(&arm, None).matched, ["battery", "arm-eas"]);
    let mut arm = arm_eas_board();
    arm.features.eas = false;
    assert_eq!(pick(&arm, None).name, "balanced");
    let mut x = amd_desktop();
    x.features.eas = true;
    assert_eq!(pick(&x, None).name, "balanced");
    // HDD + hybrid on AC: hdd outranks x86-hybrid.
    let mut hy = ac.clone();
    hy.storage.push(disk(StorageKind::Hdd));
    assert_eq!(pick(&hy, None).matched, ["hdd", "x86-hybrid"]);
    // Unknown power never matches power = "battery".
    let mut unk = intel_hybrid_laptop_on_battery();
    unk.power = PowerSource::Unknown;
    assert_eq!(pick(&unk, None).name, "x86-hybrid");
    // Unknown hardware: balanced with a readable reason.
    let s = set.select(&ProfileContext::default());
    assert_eq!(s.name, "balanced");
    assert!(s.reason.contains("unknown hardware"), "{}", s.reason);
}

#[test]
fn ties_break_by_name_and_reasons_compose() {
    let mut set = ProfileSet::default();
    let p = |n: &str, m: &str| Profile::parse(&format!("[profile]\nname = \"{n}\"\n[match]\n{m}"), n).unwrap();
    set.insert(p("zeta", "power = \"battery\"\npriority = 5"));
    set.insert(p("alpha", "power = \"battery\"\npriority = 5"));
    set.insert(p(
        "combo",
        "power = \"battery\"\narch = [\"aarch64\"]\nfeatures = [\"eas\"]\nmin_cpus = 4\nmax_mem = \"8GiB\"\npriority = 1",
    ));
    let mut hw = arm_eas_board();
    hw.power = PowerSource::Battery;
    let s = set.select(&ctx(&hw, None));
    assert_eq!(s.name, "alpha");
    assert_eq!(s.matched, ["alpha", "zeta", "combo"]);
    let info = set.list(&ctx(&hw, None), Some("alpha"));
    let combo = info.iter().find(|i| i.name == "combo").unwrap();
    assert_eq!(combo.reason.as_deref(), Some("battery power; aarch64 with EAS; 8 CPUs (≥ 4); 8GiB RAM (≤ 8GiB)"));
    assert!(info.iter().find(|i| i.name == "alpha").unwrap().active);
    assert!(!combo.active && combo.matches_now && combo.auto);
    // min_mem / max_mem exclude
    hw.mem_total = 16 * GIB;
    assert_eq!(set.select(&ctx(&hw, None)).matched, ["alpha", "zeta"]);
}

#[test]
fn list_reports_sources_and_matches() {
    let set = ProfileSet::builtin();
    let rows = set.list(&ctx(&hdd_desktop(), None), Some("hdd"));
    assert_eq!(rows.len(), 8);
    let hdd = rows.iter().find(|r| r.name == "hdd").unwrap();
    assert!(hdd.matches_now && hdd.active && hdd.builtin && hdd.error.is_none());
    assert_eq!(hdd.source, "<builtin>/60-hdd.toml");
    assert_eq!(hdd.priority, Some(40));
    let g = rows.iter().find(|r| r.name == "gaming").unwrap();
    assert_eq!(g.inherits.as_deref(), Some("latency"));
    assert!(!g.matches_now && g.reason.is_none());
    let t = rows.iter().find(|r| r.name == "throughput").unwrap();
    assert!(!t.auto && t.priority.is_none());
}

// ── inheritance and overlays ──────────────────────────────────────────────────────────

#[test]
fn inheritance_overlays_and_appends_overrides() {
    let set = ProfileSet::builtin();
    let mut base = Config::default();
    base.overrides.push(Override {
        unit: Some("docker-*.scope".into()),
        comm: None,
        class: Some(QosClass::Batch),
        settings: vec![],
    });
    base.plugins.entry("threshold".into()).or_default().config =
        json!({"exit_ratio": 0.4, "epoch": "600ms", "values": {"critical_weight": 300}});

    let eff = set.effective_config(&base, "gaming").unwrap();
    let t = threshold(&eff);
    assert_eq!(t["epoch"], "2s", "child beats parent and base");
    assert_eq!(t["foreground_yellow"], 0.03, "inherited from latency");
    assert_eq!(t["recover_after"], "8s", "inherited from latency");
    assert_eq!(t["exit_ratio"], 0.4, "base keys not in any overlay survive");
    assert_eq!(t["values"]["critical_weight"], 300, "nested objects merge");
    assert_eq!(t["values"]["fg_epoch_weight"], 1000);
    assert_eq!(t["values"]["fg_weight"], 500, "child beats parent");
    assert_eq!(t["values"]["yellow_background_weight"], 5);
    // [[override]]: base first, then the profile's, appended.
    assert_eq!(eff.overrides.len(), 2);
    assert_eq!(eff.overrides[0].unit.as_deref(), Some("docker-*.scope"));
    assert_eq!(eff.overrides[1].comm.as_deref(), Some("gamescope*"));
    assert_eq!(eff.overrides[1].class, Some(QosClass::Interactive));
    // Everything else is the base.
    assert_eq!(eff.safety, base.safety);
    assert_eq!(eff.telemetry, base.telemetry);

    // Chain order.
    let chain: Vec<_> = set.chain("gaming").unwrap().iter().map(|p| p.name.clone()).collect();
    assert_eq!(chain, ["latency", "gaming"]);

    // Overrides accumulate along the chain too.
    let mut set = set.clone();
    let child = Profile::parse(
        "[profile]\nname = \"mygame\"\ninherits = \"gaming\"\n[[config.override]]\nunit = \"steam*.scope\"\nclass = \"interactive\"\n",
        "x",
    )
    .unwrap();
    set.insert(child);
    let eff = set.effective_config(&base, "mygame").unwrap();
    let units: Vec<_> = eff.overrides.iter().map(|o| o.unit.clone().or(o.comm.clone()).unwrap()).collect();
    assert_eq!(units, ["docker-*.scope", "gamescope*", "steam*.scope"]);
}

#[test]
fn effective_config_revalidates() {
    // A user base config value that a profile pushes out of type is caught.
    let mut set = ProfileSet::builtin();
    set.insert(Profile::parse("[profile]\nname = \"fast\"\n[config.general]\ntick = \"100ms\"\n", "x").unwrap());
    assert_eq!(set.effective_config(&Config::default(), "fast").unwrap().general.tick, Duration::from_millis(100));
    // Typos are rejected when the profile is parsed.
    for bad in [
        "[profile]\nname = \"t\"\n[config.generl]\ntick = \"1s\"\n",
        "[profile]\nname = \"t\"\n[config.general]\ntick = 5\n",
        "[profile]\nname = \"t\"\n[config.safety]\nuclamp_min_max = 10.0\n",
        "[profile]\nname = \"t\"\n[config.general]\nprofile = \"x\"\n",
        "[profile]\nname = \"t\"\n[match]\npriority = 3\n",
        "[profile]\nname = \"t\"\n[match]\npower = \"mains\"\n",
        "[profile]\nname = \"t\"\n[match]\nfeatures = [\"turbo\"]\n",
        "[profile]\nname = \"t\"\n[match]\nstorage = [\"floppy\"]\n",
        "[profile]\nname = \"t\"\n[mach]\npower = \"ac\"\n",
        "[profile]\nname = \"auto\"\n",
        "[profile]\nname = \"Has Space\"\n",
        "[profile]\nname = \"t\"\ninherits = \"t\"\n",
    ] {
        assert!(Profile::parse(bad, "bad.toml").is_err(), "accepted: {bad}");
    }
    assert!(matches!(set.effective_config(&Config::default(), "nope"), Err(ProfileError::Unknown(_))));
}

#[test]
fn cycles_and_missing_parents_are_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("p");
    write(&d, "a.toml", "[profile]\nname = \"a\"\ninherits = \"b\"\n");
    write(&d, "b.toml", "[profile]\nname = \"b\"\ninherits = \"c\"\n");
    write(&d, "c.toml", "[profile]\nname = \"c\"\ninherits = \"a\"\n[match]\npower = \"ac\"\npriority = 99\n");
    write(&d, "orphan.toml", "[profile]\nname = \"orphan\"\ninherits = \"ghost\"\n");
    let (set, errors) = ProfileSet::load(&[d]);
    assert!(errors.iter().any(|e| matches!(e, ProfileError::Cycle(c) if c.len() == 4)), "{errors:?}");
    assert!(errors.iter().any(|e| matches!(e, ProfileError::MissingParent { parent, .. } if parent == "ghost")));
    assert!(matches!(set.effective_config(&Config::default(), "a"), Err(ProfileError::Cycle(_))));
    // A broken profile is never auto-selected, even with the highest priority.
    assert_eq!(set.select(&ctx(&amd_desktop(), None)).name, "balanced");
    let rows = set.list(&ProfileContext::default(), None);
    assert!(rows.iter().find(|r| r.name == "orphan").unwrap().error.is_some());
}

// ── loader precedence ─────────────────────────────────────────────────────────────────

#[test]
fn later_dirs_override_earlier_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let vendor = tmp.path().join("usr/share/lindesk/profiles.d");
    let etc = tmp.path().join("etc/lindesk/profiles.d");
    let user = tmp.path().join("home/.config/lindesk/profiles.d");
    write(
        &vendor,
        "40-gaming.toml",
        "[profile]\nname = \"gaming\"\ndescription = \"vendor\"\n[match]\nforeground_app_id = [\"steam_app_*\"]\npriority = 60\n",
    );
    write(&vendor, "70-quiet.toml", "[profile]\nname = \"quiet\"\ndescription = \"vendor quiet\"\n");
    write(&etc, "quiet.toml", "[profile]\nname = \"quiet\"\ndescription = \"admin quiet\"\n");
    // The user disables auto-selection of gaming by redefining it without [match] ...
    write(&user, "my-gaming.toml", "[profile]\nname = \"gaming\"\ndescription = \"mine\"\ninherits = \"latency\"\n");
    // ... and a broken user file leaves the admin's version in place.
    write(&user, "quiet.toml", "[profile]\nname = \"quiet\"\n[config.nope]\nx = 1\n");
    write(&user, "notes.txt", "not a profile");

    let (set, errors) = ProfileSet::load(&[vendor.clone(), etc.clone(), user.clone()]);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(matches!(&errors[0], ProfileError::Invalid { path, .. } if path.ends_with("quiet.toml")));
    let g = set.get("gaming").unwrap();
    assert_eq!(g.description, "mine");
    assert!(g.source.starts_with(&user));
    assert!(!g.is_auto());
    assert_eq!(set.get("quiet").unwrap().description, "admin quiet");
    assert_eq!(set.select(&ctx(&amd_desktop(), Some("steam_app_1"))).name, "balanced");
    // Built-ins not redefined anywhere are still there.
    assert!(set.get("battery").unwrap().builtin);

    // Duplicate names in one directory: reported, the later file wins.
    write(&etc, "zz-quiet.toml", "[profile]\nname = \"quiet\"\ndescription = \"admin quiet 2\"\n");
    let (set, errors) = ProfileSet::load(&[vendor, etc]);
    assert!(errors.iter().any(|e| matches!(e, ProfileError::Duplicate { name, .. } if name == "quiet")));
    assert_eq!(set.get("quiet").unwrap().description, "admin quiet 2");

    let dirs = default_dirs(Some(Path::new("/home/u/.config")));
    assert_eq!(dirs.last().unwrap(), Path::new("/home/u/.config/lindesk/profiles.d"));
    assert_eq!(default_dirs(None).len(), 2);
}

// ── selector dwell ────────────────────────────────────────────────────────────────────

#[test]
fn selector_dwell_and_pinning() {
    let set = ProfileSet::builtin();
    let mut sel = ProfileSelector::default();
    let hw = amd_desktop();
    let idle = ctx(&hw, Some("firefox"));
    let game = ctx(&hw, Some("steam_app_570"));

    let sw = sel.update(&set, "auto", &idle, 0).expect("first update always switches");
    assert_eq!((sw.from, sw.to.as_str()), (None, "balanced"));
    assert_eq!(sel.update(&set, "auto", &idle, 1_000), None);

    // 3s after the first switch a game gets focus: wait for the 10s dwell.
    assert_eq!(sel.update(&set, "auto", &game, 3_000), None);
    assert_eq!(sel.active(), Some("balanced"));
    assert_eq!(sel.update(&set, "auto", &game, 9_999), None);
    let sw = sel.update(&set, "auto", &game, 10_000).unwrap();
    assert_eq!((sw.from.as_deref(), sw.to.as_str()), (Some("balanced"), "gaming"));
    assert_eq!(sw.reason, "foreground app steam_app_570");
    match sw.notice(10_000) {
        Event::Notice { name, data, .. } => {
            assert_eq!(name, "profile.changed");
            assert_eq!(data["from"], "balanced");
            assert_eq!(data["to"], "gaming");
        }
        e => panic!("{e:?}"),
    }

    // A quick alt-tab and back within the dwell changes nothing.
    assert_eq!(sel.update(&set, "auto", &idle, 12_000), None);
    assert_eq!(sel.update(&set, "auto", &game, 13_000), None);
    assert_eq!(sel.active(), Some("gaming"));
    // Staying away past the dwell switches back.
    assert_eq!(sel.update(&set, "auto", &idle, 20_000).unwrap().to, "balanced");

    // Pinning switches immediately and ignores [match].
    let sw = sel.update(&set, "throughput", &game, 20_500).unwrap();
    assert!(sw.pinned);
    assert_eq!(sw.to, "throughput");
    assert_eq!(sel.update(&set, "throughput", &game, 40_000), None);
    assert!(sel.is_pinned());
    // Unpinning re-evaluates immediately.
    assert_eq!(sel.update(&set, "auto", &game, 20_600).unwrap().to, "gaming");
    // Pinning a missing profile falls back to balanced, with the reason.
    let sw = sel.update(&set, "nope", &game, 20_700).unwrap();
    assert_eq!(sw.to, "balanced");
    assert!(sw.reason.contains("'nope'"), "{}", sw.reason);

    // Configurable dwell.
    let mut fast = ProfileSelector::new(Duration::from_secs(1));
    fast.update(&set, "auto", &idle, 0);
    assert_eq!(fast.update(&set, "auto", &game, 1_000).unwrap().to, "gaming");
}

#[test]
fn power_flapping_is_damped() {
    let set = ProfileSet::builtin();
    let mut sel = ProfileSelector::default();
    let mut hw = amd_desktop();
    sel.update(&set, "auto", &ctx(&hw, None), 0);
    let mut switches = 0;
    for i in 1..=40 {
        hw.power = if i % 2 == 0 { PowerSource::Ac } else { PowerSource::Battery };
        if sel.update(&set, "auto", &ctx(&hw, None), i * 1_000).is_some() {
            switches += 1;
        }
    }
    assert!(switches <= 4, "{switches} switches in 40s");
}

#[test]
fn globs() {
    assert!(glob_match("steam_app_*", "steam_app_570"));
    assert!(glob_match("*.exe", "Game.EXE"));
    assert!(glob_match("a?c*", "abcdef"));
    assert!(glob_match("*", ""));
    assert!(!glob_match("steam_app_*", "steam"));
    assert!(!glob_match("*.exe", "exe"));
    assert!(glob_match("*a*b*", "xxaxxbxx"));
}

#[test]
fn context_from_hardware() {
    let mut hw = intel_hybrid_laptop_on_battery();
    hw.features.numa_nodes = 2;
    hw.compressed_swap = true;
    let c = ctx(&hw, Some("x"));
    assert!(c.hybrid);
    assert_eq!(
        c.features,
        [Feature::Itmt, Feature::Smt, Feature::Numa, Feature::CompressedSwap, Feature::Portable].into_iter().collect()
    );
    assert_eq!(c.storage, [StorageKind::Nvme]);
    assert_eq!(c.foreground_app_id.as_deref(), Some("x"));
    // Round-trips (it is sent over D-Bus for `explain`).
    let j = serde_json::to_string(&c).unwrap();
    assert_eq!(serde_json::from_str::<ProfileContext>(&j).unwrap(), c);
}
