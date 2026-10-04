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
    for name in ["rules", "threshold", "psi", "cgroup-stat", "broker", "parquet-store"] {
        let p = plugins.iter().find(|p| p["name"] == name).unwrap_or_else(|| panic!("plugin {name} missing"));
        assert_ne!(p["state"], "disabled", "{name}: {p}");
    }
    assert!(st["daemon"]["degraded"].is_array());
    assert!(run.join("lindesk").is_dir(), "state dir created under XDG_RUNTIME_DIR");
    if !rules_installed {
        assert!(run.join("lindesk/rules.d.builtin/10-critical.toml").is_file());
    }
    assert!(agent.terminate().success());
    assert!(home.join(".local/state/lindesk/telemetry").is_dir(), "user telemetry lake created");
}
