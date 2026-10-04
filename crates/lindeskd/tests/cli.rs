//! The `lindeskd` binary from the outside: CLI and `--restore` with nothing to do. (Also
//! makes cargo build the binary, which the broker ↔ session test in lindesk-session runs.)

use std::process::Command;
use std::time::{Duration, Instant};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lindeskd"))
}

#[test]
fn version_and_help() {
    let out = bin().arg("--version").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("lindeskd "));
    let out = bin().arg("--help").output().unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    for flag in ["--config", "--state-dir", "--telemetry-dir", "--dry-run", "--log-level", "--restore", "--no-dbus"] {
        assert!(help.contains(flag), "{flag} missing from --help");
    }
    assert!(!bin().arg("--bogus").output().unwrap().status.success());
}

#[test]
fn restore_with_nothing_to_do_is_quick_and_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let st = bin()
        .args(["--restore", "--state-dir"])
        .arg(tmp.path())
        .args(["--config", "/nonexistent/lindesk.toml"])
        .env_remove("JOURNAL_STREAM")
        .status()
        .unwrap();
    assert!(st.success(), "{st:?}");
    assert!(start.elapsed() < Duration::from_secs(30));
    // Twice is fine too, and nothing was created.
    assert!(bin().args(["--restore", "--state-dir"]).arg(tmp.path()).status().unwrap().success());
    assert!(!tmp.path().join("state.json").exists() && !tmp.path().join("iocost.json").exists());
}
