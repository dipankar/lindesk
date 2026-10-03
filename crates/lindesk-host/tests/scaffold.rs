//! Every generated plugin must pass protocol conformance and its own generated test.

mod common;

use std::process::Command;

use common::write_plugin;
use lindesk_host::conformance::{ConformanceOptions, run_conformance_with};
use lindesk_host::{Lang, run_conformance, scaffold};
use lindesk_plugin_api::PluginKind;

/// Generous timeouts: these tests run many interpreters in parallel on loaded CI machines.
fn opts() -> ConformanceOptions {
    ConformanceOptions { timeout_scale: 10.0, ..Default::default() }
}

fn check_lang(lang: Lang) {
    let tmp = tempfile::tempdir().unwrap();
    for kind in lang.kinds() {
        let name = format!("my-{kind}");
        let dir = tmp.path().join(&name);
        let files = scaffold(*kind, lang, &name, &dir).unwrap();
        assert!(files.iter().any(|f| f.ends_with("README.md")));

        let report = run_conformance_with(&dir, &opts());
        assert!(report.passed(), "{lang} {kind} scaffold failed conformance:\n{report}");
        assert!(report.checks.len() >= 6, "{report}");

        let test = match lang {
            Lang::Python => Command::new("python3").arg(dir.join(format!("test_my_{kind}.py"))).output(),
            Lang::Shell => Command::new(dir.join("test.sh")).output(),
            Lang::Rust => unreachable!(),
        }
        .unwrap();
        assert!(
            test.status.success(),
            "{lang} {kind} generated test failed:\n{}\n{}",
            String::from_utf8_lossy(&test.stdout),
            String::from_utf8_lossy(&test.stderr)
        );
    }
}

#[test]
fn python_scaffolds_pass_conformance_and_their_tests() {
    check_lang(Lang::Python);
}

#[test]
fn shell_scaffolds_pass_conformance_and_their_tests() {
    check_lang(Lang::Shell);
}

#[test]
fn default_timeouts_hold_for_a_python_scaffold() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("quick");
    scaffold(PluginKind::Classifier, Lang::Python, "quick", &dir).unwrap();
    // Real budgets (50ms per classify); allow one retry for a noisy machine.
    let report = run_conformance(&dir);
    let report = if report.passed() { report } else { run_conformance(&dir) };
    assert!(report.passed(), "{report}");
    let text = report.to_string();
    assert!(text.contains("✓ classifier.classify answers a regular process (≤50ms)"), "{text}");
    assert!(text.contains("0 failed"), "{text}");
}

#[test]
fn conformance_reports_broken_plugins() {
    let tmp = tempfile::tempdir().unwrap();
    // Answers every request with a result (even unknown methods), prints junk, and ignores
    // shutdown until stdin closes.
    let script = r#"#!/bin/bash
while IFS= read -r line; do
  [[ $line =~ \"id\":([0-9]+) ]] || continue
  id=${BASH_REMATCH[1]}
  if [[ $line == *'"initialize"'* ]]; then
    echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"name\":\"other-name\",\"version\":\"1\",\"api_version\":1,\"kinds\":[\"policy\"]}}"
  else
    echo "starting evaluation"
    echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"intents\":[]}}"
  fi
done
exit 3
"#;
    let dir = write_plugin(tmp.path(), "broken", &["policy"], script, "");
    let report = run_conformance_with(&dir, &opts());
    assert!(!report.passed());
    let failed: Vec<&str> = report.failures().map(|c| c.name.as_str()).collect();
    assert!(failed.contains(&"initialize reports the manifest's name and kinds"), "{report}");
    assert!(failed.contains(&"unknown methods get METHOD_NOT_FOUND (-32601)"), "{report}");
    assert!(failed.contains(&"stdout carries only JSON-RPC messages"), "{report}");
    assert!(failed.iter().any(|f| f.starts_with("exits by itself on shutdown")), "{report}");
    let text = report.to_string();
    assert!(text.contains("✗ unknown methods get METHOD_NOT_FOUND"), "{text}");
    assert!(text.contains("✓ policy.evaluate answers a busy snapshot"), "{text}");
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["plugin"], "broken");

    // A manifest error stops early with one clear failure.
    let dir = tmp.path().join("no-manifest-fields");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.toml"), "[plugin]\nname = \"broken\"\n").unwrap();
    let report = run_conformance(&dir);
    assert_eq!(report.checks.len(), 1);
    assert!(report.checks[0].detail.as_deref().unwrap().contains("missing field"), "{report}");
}

/// Builds every Rust scaffold against this checkout and runs its `cargo test` (slow; needs
/// the crates.io index cached). Run with `cargo test -- --ignored`.
#[test]
#[ignore]
fn rust_scaffolds_build_and_pass_their_tests() {
    let tmp = tempfile::tempdir().unwrap();
    let sdk = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../lindesk-plugin").canonicalize().unwrap();
    let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target-host/scaffolds");
    for kind in Lang::Rust.kinds() {
        let name = format!("rs-{kind}");
        let dir = tmp.path().join(&name);
        scaffold(*kind, Lang::Rust, &name, &dir).unwrap();
        let cargo_toml = dir.join("Cargo.toml");
        let text = std::fs::read_to_string(&cargo_toml).unwrap().replace(
            "lindesk-plugin = \"0.1\"",
            &format!("lindesk-plugin = {{ path = {:?} }}", sdk.display().to_string()),
        );
        std::fs::write(&cargo_toml, text).unwrap();
        let out = Command::new(env!("CARGO"))
            .args(["test", "--offline", "--quiet"])
            .env("CARGO_TARGET_DIR", &target)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{kind}: cargo test failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let out = Command::new(env!("CARGO"))
            .args(["build", "--offline", "--quiet", "--release"])
            .env("CARGO_TARGET_DIR", dir.join("target"))
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "{kind}: build failed: {}", String::from_utf8_lossy(&out.stderr));
        let report = run_conformance_with(&dir, &opts());
        assert!(report.passed(), "{report}");
    }
}
