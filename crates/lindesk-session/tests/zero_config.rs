//! Out of the box: `lindesk-session --standalone` with an empty HOME/XDG environment, no
//! config file and no rules on disk comes up healthy with the embedded defaults.

mod support;

use std::time::Duration;

use lindesk_ipc::QOS_NAME;
use serde_json::Value;
use support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_with_nothing_configured() {
    let tmp = fast_tempdir();
    let home = tmp.path().join("home");
    let run = tmp.path().join("run");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    let bus = Bus::session();
    let (home_s, run_s) = (home.display().to_string(), run.display().to_string());
    // Only the bus, HOME and the runtime dir: every other XDG variable is unset. Dry run so
    // the test never touches the real user manager.
    let mut agent = Daemon::spawn_clean(
        "lindesk-session",
        &session_bin(),
        &["--standalone", "--dry-run"],
        &[
            ("DBUS_SESSION_BUS_ADDRESS", bus.address.as_str()),
            ("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent/lindesk-test"),
            ("HOME", &home_s),
            ("XDG_RUNTIME_DIR", &run_s),
            ("PATH", "/usr/bin:/bin"),
        ],
    );
    let conn = connect(&bus.address).await;
    wait_for_name(&conn, QOS_NAME, Duration::from_secs(60)).await;
    let qos = QoSTestProxy::new(&conn).await.unwrap();
    let st: Value = serde_json::from_str(&qos.status().await.unwrap()).unwrap();
    println!("{}", st["text"].as_str().unwrap());

    assert_eq!(st["daemon"]["mode"], "standalone");
    let rules_installed = std::path::Path::new("/usr/share/lindesk/rules.d").is_dir();
    assert_eq!(st["daemon"]["rules"], if rules_installed { "system" } else { "embedded" });
    let plugins = st["status"]["plugins"].as_array().unwrap();
    for name in ["rules", "threshold", "psi", "cgroup-stat", "broker"] {
        let p = plugins.iter().find(|p| p["name"] == name).unwrap_or_else(|| panic!("plugin {name} missing"));
        assert_ne!(p["state"], "disabled", "{name}: {p}");
    }
    assert!(st["daemon"]["degraded"].is_array());
    assert!(run.join("lindesk").is_dir(), "state dir created under XDG_RUNTIME_DIR");
    if !rules_installed {
        assert!(run.join("lindesk/rules.d.builtin/10-critical.toml").is_file());
    }
    // No telemetry plugin unless lindesk-telemetry is installed on this host.
    let telemetry_installed = std::path::Path::new("/usr/lib/lindesk/plugins/telemetry-user/plugin.toml").is_file();
    assert_eq!(st["daemon"]["telemetry"]["installed"], telemetry_installed, "{}", st["daemon"]["telemetry"]);
    if !telemetry_installed {
        assert!(st["text"].as_str().unwrap().contains("telemetry: not installed"));
        assert!(plugins.iter().all(|p| p["name"] != "telemetry"));
    }
    assert!(agent.terminate().success());
    if !telemetry_installed {
        assert!(!home.join(".local/state/lindesk/telemetry").exists(), "no lake without the plugin");
    }
}

/// "Installing" lindesk-telemetry: the `telemetry-user` manifest and the sink binary in a
/// plugin directory (here via `[general] plugin_dirs`) is all it takes; the agent records
/// into the user lake with no other configuration.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_with_the_telemetry_plugin() {
    let sink = session_bin().with_file_name("lindesk-telemetry-sink");
    if !sink.is_file() {
        eprintln!(
            "lindesk-telemetry-sink not built next to lindesk-session (cargo test --workspace builds it); skipping"
        );
        return;
    }
    let tmp = fast_tempdir();
    let home = tmp.path().join("home");
    let run = tmp.path().join("run");
    let lib = tmp.path().join("lib/lindesk");
    let manifests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/plugins/telemetry-user");
    std::fs::create_dir_all(lib.join("plugins/telemetry-user")).unwrap();
    std::fs::copy(manifests.join("plugin.toml"), lib.join("plugins/telemetry-user/plugin.toml")).unwrap();
    std::fs::copy(&sink, lib.join("lindesk-telemetry-sink")).unwrap();
    std::fs::create_dir_all(home.join(".config/lindesk")).unwrap();
    std::fs::write(
        home.join(".config/lindesk/lindesk.toml"),
        format!("[general]\nplugin_dirs = [{:?}]\n", lib.join("plugins").display().to_string()),
    )
    .unwrap();
    std::fs::create_dir_all(&run).unwrap();
    let bus = Bus::session();
    let (home_s, run_s) = (home.display().to_string(), run.display().to_string());
    let mut agent = Daemon::spawn_clean(
        "lindesk-session",
        &session_bin(),
        &["--standalone", "--dry-run", "--no-adapters"],
        &[
            ("DBUS_SESSION_BUS_ADDRESS", bus.address.as_str()),
            ("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent/lindesk-test"),
            ("HOME", &home_s),
            ("XDG_RUNTIME_DIR", &run_s),
            ("PATH", "/usr/bin:/bin"),
        ],
    );
    let conn = connect(&bus.address).await;
    wait_for_name(&conn, QOS_NAME, Duration::from_secs(60)).await;
    let qos = QoSTestProxy::new(&conn).await.unwrap();
    let st: Value = serde_json::from_str(&qos.status().await.unwrap()).unwrap();
    let t = &st["daemon"]["telemetry"];
    assert_eq!((t["installed"].as_bool(), t["enabled"].as_bool()), (Some(true), Some(true)), "{t}");
    let lake = home.join(".local/state/lindesk/telemetry");
    assert_eq!(t["dir"].as_str(), Some(lake.to_str().unwrap()));
    let plugins = st["status"]["plugins"].as_array().unwrap();
    let p = plugins.iter().find(|p| p["name"] == "telemetry").expect("telemetry plugin loaded");
    assert_eq!(p["state"], "running", "{p}");
    // A few steps' worth of records, then shutdown flushes them.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(agent.terminate().success());
    fn vortex(d: &std::path::Path) -> usize {
        std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| {
                let p = e.path();
                if p.is_dir() { vortex(&p) } else { usize::from(p.extension().is_some_and(|x| x == "vortex")) }
            })
            .sum()
    }
    assert!(vortex(&lake) > 0, "records reached the lake");
}
