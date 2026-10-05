//! The installed layout, exercised the way the daemons use it: both shipped manifests
//! discovered from one plugin root next to the binary, protocol conformance, and records
//! sent through the host's stdio transport ending up as Vortex files.

use std::path::{Path, PathBuf};
use std::time::Duration;

use lindesk_core::config::Config;
use lindesk_host::conformance::{ConformanceOptions, run_conformance_with};
use lindesk_host::discovery::{self, DiscoveryOptions};
use lindesk_host::external::{ExternalOptions, ExternalPlugin};
use lindesk_plugin_api::*;
use serde_json::json;

const MANIFESTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../packaging/plugins");

/// `<tmp>/lib/lindesk/{lindesk-telemetry-sink, plugins/telemetry*/plugin.toml}`, like
/// /usr/lib/lindesk after installing lindesk-telemetry. Returns the plugin root.
fn install(tmp: &Path) -> PathBuf {
    let lib = tmp.join("lib/lindesk");
    let plugins = lib.join("plugins");
    for m in ["telemetry", "telemetry-user"] {
        std::fs::create_dir_all(plugins.join(m)).unwrap();
        std::fs::copy(Path::new(MANIFESTS).join(m).join("plugin.toml"), plugins.join(m).join("plugin.toml")).unwrap();
    }
    std::fs::copy(env!("CARGO_BIN_EXE_lindesk-telemetry-sink"), lib.join("lindesk-telemetry-sink")).unwrap();
    plugins
}

fn files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(files(&p, ext));
        } else if p.extension().is_some_and(|x| x == ext) {
            out.push(p);
        }
    }
    out
}

fn discover(root: &Path, host: Privilege, config: &Config) -> discovery::Discovery {
    let ok = |_: &Path| -> Result<(), String> { Ok(()) };
    discovery::discover(&DiscoveryOptions { host, dirs: vec![root.to_path_buf()], config, ownership: &ok })
}

#[test]
fn each_daemon_discovers_exactly_one_telemetry_plugin() {
    let tmp = tempfile::tempdir().unwrap();
    let root = install(tmp.path());
    let config = Config::default();
    for host in [Privilege::System, Privilege::User] {
        let d = discover(&root, host, &config);
        let names: Vec<_> = d.plugins.iter().map(|p| (p.name.as_str(), p.manifest.plugin.privilege)).collect();
        assert_eq!(names, [("telemetry", host)], "{host:?}: {d:#?}");
        assert!(d.rejected.iter().all(|r| r.skipped), "the other manifest is skipped, not an error: {:?}", d.rejected);
        assert!(d.plugins[0].exec.ends_with("../../lindesk-telemetry-sink"), "{:?}", d.plugins[0].exec);
        assert!(d.plugins[0].exec.is_file());
    }
}

#[test]
fn passes_conformance() {
    let tmp = tempfile::tempdir().unwrap();
    let root = install(tmp.path());
    let lake = tmp.path().join("lake");
    let report = run_conformance_with(
        &root.join("telemetry-user"),
        &ConformanceOptions { timeout_scale: 3.0, config: Some(json!({ "dir": lake, "mode": "user" })) },
    );
    println!("{report}");
    assert!(report.passed(), "{report}");
}

#[test]
fn records_sent_over_stdio_land_in_the_lake() {
    let tmp = tempfile::tempdir().unwrap();
    let root = install(tmp.path());
    let lake = tmp.path().join("lake");
    let config = Config::from_toml(&format!(
        "[plugins.telemetry.config]\ndir = {:?}\nflush = \"1h\"\nmode = \"user\"\n",
        lake.display().to_string()
    ))
    .unwrap();
    let d = discover(&root, Privilege::User, &config);
    let ctx = InitContext {
        api_version: API_VERSION,
        host: "test".into(),
        privilege: Privilege::User,
        uid: Some(1000),
        caps: HostCaps::default(),
        config: serde_json::Value::Null,
    };
    let mut p = ExternalPlugin::from_discovered(&d.plugins[0], ctx, ExternalOptions::default());
    p.start().unwrap_or_else(|e| panic!("start: {e}; logs: {:?}", p.recent_logs()));
    let now = 1_791_000_000_000;
    let batch: Vec<Record> = (0..20)
        .map(|i| {
            Record::Pressure(Sample {
                ts: now + i * 1000,
                scope: Scope::System,
                resource: Resource::Io,
                some_avg10: 12.5,
                full_avg10: Some(3.0),
                some_delta_us: 1000,
                full_delta_us: Some(10),
                interval_ms: 1000,
            })
        })
        .collect();
    Sink::record(&mut p, &batch).unwrap();
    Sink::record(&mut p, &batch[..5]).unwrap();
    Sink::flush(&mut p).unwrap_or_else(|e| panic!("flush: {e}; logs: {:?}", p.recent_logs()));
    assert_eq!(files(&lake.join("pressure"), "vortex").len(), 1, "one flush, one file");
    p.shutdown();
    assert!(p.stats().dropped == 0);
    // Nothing left behind in the plugin root (the writer only touches the lake).
    assert!(files(&root, "vortex").is_empty());
    std::thread::sleep(Duration::from_millis(50));
}
