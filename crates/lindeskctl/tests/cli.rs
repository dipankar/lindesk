//! End-to-end tests of the built `lindeskctl` binary.

use std::path::Path;
use std::process::{Command, Output};

fn ctl(args: &[&str], env: &[(&str, &Path)]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_lindeskctl"));
    c.args(args).env("NO_COLOR", "1").env("LANG", "C.UTF-8").env_remove("LC_ALL").env_remove("LINDESK_TELEMETRY_DIR");
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().expect("run lindeskctl")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn help_lists_commands_with_examples() {
    let o = ctl(&["--help"], &[]);
    assert!(o.status.success());
    let s = stdout(&o);
    for c in [
        "doctor",
        "status",
        "watch",
        "events",
        "explain",
        "run",
        "plugin",
        "simulate",
        "query",
        "data",
        "completions",
        "man",
    ] {
        assert!(s.contains(c), "--help lacks {c}:\n{s}");
    }
    assert!(s.contains("Examples:"));
    let o = ctl(&["plugin", "new", "--help"], &[]);
    assert!(o.status.success() && stdout(&o).contains("lindeskctl plugin new --kind classifier"));
}

#[test]
fn bare_invocation_is_a_friendly_status() {
    let o = ctl(&[], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let s = stdout(&o);
    assert!(s.contains("lindeskctl watch for live view"), "{s}");
    assert!(s.contains("pressure"), "{s}");
}

#[test]
fn ascii_fallback_without_utf8_locale() {
    let d = tempfile::tempdir().unwrap();
    let o = ctl(&["config", "check", d.path().join("none.toml").to_str().unwrap()], &[]);
    assert!(o.status.success());
    let mut c = Command::new(env!("CARGO_BIN_EXE_lindeskctl"));
    let o = c
        .args(["doctor", "--preflight"])
        .env("LANG", "C")
        .env_remove("LC_ALL")
        .env_remove("LC_CTYPE")
        .output()
        .unwrap();
    let s = stdout(&o);
    assert!(!s.is_empty() && s.is_ascii(), "non-ASCII output in the C locale:\n{s}");
}

#[test]
fn usage_errors_exit_2_and_suggest() {
    let o = ctl(&["stauts"], &[]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(stderr(&o).contains("Did you mean 'status'?"), "{}", stderr(&o));
    let o = ctl(&["run", "--batch", "--background", "true"], &[]);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn doctor_json_is_valid() {
    let o = ctl(&["doctor", "--json"], &[]);
    assert!(matches!(o.status.code(), Some(0 | 1)), "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).expect("doctor --json prints JSON");
    assert!(v["level"].is_string());
    assert!(v["findings"].is_array());
    assert!(v["services"].as_array().unwrap().iter().any(|s| s["check"] == "lindeskd"));
    assert!(v["needs_action"].is_boolean());
    assert_eq!(o.status.code() == Some(1), v["needs_action"].as_bool().unwrap());
}

#[test]
fn plugin_new_then_test_python_classifier() {
    let d = tempfile::tempdir().unwrap();
    let o = ctl(&["plugin", "new", "--kind", "classifier", "--lang", "python", "steam-games", "--dir"], &[]);
    assert_eq!(o.status.code(), Some(2), "--dir needs a value");
    let dir = d.path().to_str().unwrap();
    let o = ctl(&["plugin", "new", "--kind", "classifier", "--lang", "python", "steam-games", "--dir", dir], &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    let s = stdout(&o);
    assert!(s.contains("created") && s.contains("Next steps") && s.contains("lindeskctl plugin test ."), "{s}");
    let plugin = d.path().join("steam-games");
    assert!(plugin.join("plugin.toml").is_file());
    let o = ctl(&["plugin", "test", plugin.to_str().unwrap()], &[]);
    let s = stdout(&o);
    assert!(o.status.success(), "{s}{}", stderr(&o));
    assert!(s.contains("✓ plugin.toml is valid") && s.contains("0 failed"), "{s}");
    // A relative path from the parent directory works too.
    let o = Command::new(env!("CARGO_BIN_EXE_lindeskctl"))
        .args(["plugin", "test", "steam-games"])
        .current_dir(d.path())
        .env("NO_COLOR", "1")
        .env("LANG", "C.UTF-8")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}{}", stdout(&o), stderr(&o));
    let o = ctl(&["plugin", "test", "--json", plugin.to_str().unwrap()], &[]);
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(v["checks"].as_array().unwrap().iter().all(|c| c["ok"] == true));
    // A broken plugin fails with exit 1.
    std::fs::write(plugin.join("plugin.toml"), "[plugin]\nname = \"x\"\n").unwrap();
    let o = ctl(&["plugin", "test", plugin.to_str().unwrap()], &[]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stdout(&o).contains("✗"));
}

#[test]
fn plugin_schema_is_json_schema() {
    let o = ctl(&["plugin", "schema"], &[]);
    assert!(o.status.success());
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(v["$defs"]["Snapshot"].is_object());
}

#[test]
fn completions_and_man() {
    let o = ctl(&["completions", "bash"], &[]);
    assert!(o.status.success());
    let s = stdout(&o);
    assert!(s.contains("_lindeskctl") && s.contains("simulate"));
    let d = tempfile::tempdir().unwrap();
    let o = ctl(&["man", d.path().to_str().unwrap()], &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    for p in ["lindeskctl.1", "lindeskctl-plugin.1", "lindeskd.8", "lindesk-session.1", "lindesk-data.1"] {
        assert!(d.path().join(p).is_file(), "missing {p}");
    }
}

#[test]
fn query_and_data_on_a_temp_lake() {
    let d = tempfile::tempdir().unwrap();
    let lake = d.path().join("telemetry");
    std::fs::create_dir_all(&lake).unwrap();
    // Write a small lake through lindesk-store.
    {
        use lindesk_plugin_api::{Record, Resource, Sample, Scope};
        let mut store = lindesk_store::Store::open(&lake, lindesk_store::StoreOptions::user()).unwrap();
        let t0 = 1_791_000_000_000i64;
        let recs: Vec<Record> = (0..10)
            .flat_map(|i| {
                Resource::ALL.into_iter().map(move |r| {
                    Record::Pressure(Sample {
                        ts: t0 + i * 1000,
                        scope: Scope::System,
                        resource: r,
                        some_avg10: if r == Resource::Io { 20.0 } else { 2.0 },
                        full_avg10: None,
                        some_delta_us: 1000,
                        full_delta_us: None,
                        interval_ms: 1000,
                    })
                })
            })
            .collect();
        store.append(&recs);
        store.flush(t0 + 100_000).unwrap();
    }
    let l = lake.to_str().unwrap();
    let o =
        ctl(&["query", "--lake", l, "select resource, avg(some_avg10) as a from pressure group by 1 order by 1"], &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    let s = stdout(&o);
    assert!(s.contains("io") && s.contains("20"), "{s}");
    let o = ctl(&["query", "--json", "--lake", l, "select count(*) as n from pressure"], &[]);
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v[0]["n"], 30);
    let o = ctl(&["query", "--lake", l, "--format", "csv", "select resource from pressure group by 1 order by 1"], &[]);
    assert_eq!(stdout(&o).lines().next(), Some("resource"));
    let o = ctl(&["query", "--lake", l, "delete from pressure"], &[]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("read-only"), "{}", stderr(&o));
    let o = ctl(&["data", "stats", "--lake", l], &[]);
    assert!(o.status.success() && stdout(&o).contains("pressure"), "{}", stderr(&o));
    // Destructive commands refuse without a terminal unless --yes.
    let o = ctl(&["data", "delete", "--lake", l, "--where", "resource = 'cpu'"], &[]);
    assert_eq!(o.status.code(), Some(1), "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("--yes"));
    let o = ctl(&["data", "delete", "--lake", l, "--where", "resource = 'cpu'", "--yes"], &[]);
    assert!(o.status.success(), "{}", stderr(&o));
    let o = ctl(&["query", "--json", "--lake", l, "select count(*) as n from pressure"], &[]);
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v[0]["n"], 20);
}

#[test]
fn config_check_and_plugin_enable_round_trip() {
    let d = tempfile::tempdir().unwrap();
    let cfg = d.path().join("lindesk");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("lindesk.toml"), "# mine\n[plugins.threshold]\nenabled = true # keep\n").unwrap();
    let env = [("XDG_CONFIG_HOME", d.path()), ("XDG_DATA_HOME", d.path()), ("HOME", d.path())];
    let o = ctl(&["config", "check"], &env);
    assert!(o.status.success(), "{}{}", stdout(&o), stderr(&o));
    let o = ctl(&["plugin", "disable", "threshold"], &env);
    assert!(o.status.success(), "{}", stderr(&o));
    let text = std::fs::read_to_string(cfg.join("lindesk.toml")).unwrap();
    assert_eq!(text, "# mine\n[plugins.threshold]\nenabled = false # keep\n");
    std::fs::write(cfg.join("lindesk.toml"), "[generl]\n").unwrap();
    let o = ctl(&["config", "check"], &env);
    assert_eq!(o.status.code(), Some(1));
}
