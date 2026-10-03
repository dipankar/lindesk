//! `lindeskctl plugin test` for every example plugin: each one is spawned exactly as the
//! host would spawn it and must pass `lindesk_host::conformance`.
//!
//! Run with `cargo test -p on-battery --test conformance`. Needs `python3` and `sh` on PATH.
//! On a slow or heavily loaded machine, relax the per-call timeouts with
//! `LINDESK_CONFORMANCE_TIMEOUT_SCALE=3`.

use std::path::{Path, PathBuf};

use lindesk_host::conformance::{ConformanceOptions, ConformanceReport, run_conformance_with};
use lindesk_plugin::serde_json::{Value, json};

/// `examples/plugins/`.
fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn options() -> ConformanceOptions {
    let timeout_scale =
        std::env::var("LINDESK_CONFORMANCE_TIMEOUT_SCALE").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0);
    ConformanceOptions { timeout_scale, config: None }
}

fn check(dir: &Path) -> ConformanceReport {
    let report = run_conformance_with(dir, &options());
    println!("{report}");
    assert!(report.passed(), "{} failed conformance:\n{report}", dir.display());
    report
}

/// Copy `src/plugin.toml` into `tmp` with `exec` made absolute (optionally replaced) and
/// `config` merged over its `[config]`.
///
/// (`ConformanceOptions::config` would be the natural way to pass test config, but the
/// host currently ignores it: `ExternalPlugin::from_discovered` overwrites it with the
/// manifest's config.)
fn patched(src: &Path, tmp: &Path, exec: Option<&str>, config: Value) -> PathBuf {
    let text = std::fs::read_to_string(src.join("plugin.toml")).unwrap();
    let mut manifest: toml::Table = toml::from_str(&text).unwrap();
    let plugin = manifest["plugin"].as_table_mut().unwrap();
    let exec = exec.map(PathBuf::from).unwrap_or_else(|| src.join(plugin["exec"].as_str().unwrap()));
    plugin.insert("exec".into(), exec.display().to_string().into());
    let table = manifest.entry("config").or_insert_with(|| toml::Table::new().into());
    let overrides: toml::Table = toml::Table::try_from(config).unwrap();
    table.as_table_mut().unwrap().extend(overrides);
    let dir = tmp.join(src.file_name().unwrap());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.toml"), toml::to_string(&manifest).unwrap()).unwrap();
    dir
}

#[test]
fn rust_on_battery() {
    // The manifest says `exec = "on-battery"` (the installed layout: the binary next to
    // plugin.toml); use the binary cargo just built.
    let src = Path::new(env!("CARGO_MANIFEST_DIR"));
    let bin = Some(env!("CARGO_BIN_EXE_on-battery"));
    let tmp = tempfile::tempdir().unwrap();
    // With the manifest's config: whatever this machine's /sys says.
    let r = check(&patched(src, &tmp.path().join("sys"), bin, json!({})));
    assert_eq!(r.plugin.as_deref(), Some("on-battery"));
    // And on a (fixture) laptop running on battery, so the intent path runs too.
    let fixture = src.join("fixtures/laptop-on-battery");
    let config = json!({ "sysfs_root": fixture, "poll_ms": 0 });
    check(&patched(src, &tmp.path().join("fixture"), bin, config));
}

#[test]
fn python_steam_games() {
    check(&examples().join("python/steam-games"));
}

#[test]
fn python_prometheus_exporter() {
    // Port 0: let the kernel pick, so a running exporter on :9469 doesn't matter.
    let tmp = tempfile::tempdir().unwrap();
    let src = examples().join("python/prometheus-exporter");
    check(&patched(&src, tmp.path(), None, json!({ "listen": "127.0.0.1:0" })));
}

#[test]
fn python_focus_logger() {
    let tmp = tempfile::tempdir().unwrap();
    let fifo = tmp.path().join("focus.fifo");
    let src = examples().join("python/focus-logger");
    check(&patched(&src, tmp.path(), None, json!({ "fifo": fifo })));
}

#[test]
fn shell_notify_on_red() {
    // Conformance sends a green → red level change; don't pop up a real notification.
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("last");
    let src = examples().join("shell/notify-on-red");
    let r = check(&patched(&src, tmp.path(), None, json!({ "dry_run": true, "state_file": state })));
    assert!(
        r.logs.iter().any(|l| l.message.contains("would run: notify-send")),
        "the red level event should have triggered a (dry-run) notification: {:?}",
        r.logs
    );
    assert!(state.exists(), "the rate-limit state file should have been written");
}

#[test]
fn every_example_is_covered() {
    let covered = [
        "python/focus-logger",
        "python/prometheus-exporter",
        "python/steam-games",
        "rust/on-battery",
        "shell/notify-on-red",
    ];
    let mut found = Vec::new();
    for lang in std::fs::read_dir(examples()).unwrap() {
        let lang = lang.unwrap().path();
        if !lang.is_dir() {
            continue;
        }
        for plugin in std::fs::read_dir(&lang).unwrap() {
            let plugin = plugin.unwrap().path();
            if plugin.join("plugin.toml").is_file() {
                found.push(plugin.strip_prefix(examples()).unwrap().display().to_string());
            }
        }
    }
    found.sort();
    assert_eq!(found, covered, "add a conformance test for each new example plugin");
}

/// The examples' own test suites (`python3 test_*.py`, `./test.sh`), so that
/// `cargo test --workspace` covers them too.
#[test]
fn example_unit_tests() {
    for (dir, cmd, arg) in [
        ("python/steam-games", "python3", "test_steam_games.py"),
        ("python/prometheus-exporter", "python3", "test_prometheus_exporter.py"),
        ("python/focus-logger", "python3", "test_focus_logger.py"),
        ("shell/notify-on-red", "sh", "test.sh"),
    ] {
        let out = std::process::Command::new(cmd)
            .arg(arg)
            .current_dir(examples().join(dir))
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output()
            .unwrap_or_else(|e| panic!("running {cmd}: {e}"));
        assert!(
            out.status.success(),
            "{dir}: {cmd} {arg} failed\n--- stdout\n{}\n--- stderr\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
