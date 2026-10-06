//! `SetPaused` / `GetPaused` on a private session bus: pause reverts what lindesk applied
//! and keeps observing, a timed pause resumes by itself, an indefinite one survives a
//! restart until resumed.

mod support;

use std::time::Duration;

use lindesk_ipc::QOS_NAME;
use serde_json::Value;
use support::*;

fn spawn_agent(tmp: &std::path::Path, bus: &Bus) -> Daemon {
    let dir = |d: &str| tmp.join(d).display().to_string();
    Daemon::spawn(
        "lindesk-session",
        &session_bin(),
        &[
            "--standalone",
            "--dry-run",
            "--no-adapters",
            "--state-dir",
            &dir("state"),
            "--telemetry-dir",
            &dir("telemetry"),
            "--config",
            &dir("none.toml"),
        ],
        &[
            ("DBUS_SESSION_BUS_ADDRESS", &bus.address),
            ("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent/lindesk-test"),
            ("XDG_CONFIG_HOME", &dir("xdg")),
            ("XDG_DATA_HOME", &dir("xdg")),
            ("XDG_STATE_HOME", &dir("xdg-state")),
        ],
    )
}

async fn json(s: zbus::Result<String>) -> Value {
    serde_json::from_str(&s.unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_resume_and_auto_resume() {
    let tmp = fast_tempdir();
    let bus = Bus::session();
    let mut agent = spawn_agent(tmp.path(), &bus);
    let conn = connect(&bus.address).await;
    wait_for_name(&conn, QOS_NAME, Duration::from_secs(60)).await;
    let qos = QoSTestProxy::new(&conn).await.unwrap();

    assert_eq!(json(qos.get_paused().await).await["paused"], false);
    let st = json(qos.status().await).await;
    assert_eq!(st["daemon"]["state"], "on");

    // Focus this process; when it lives in a user unit the threshold policy boosts it.
    let me = std::process::id() as i32;
    qos.set_foreground_process(me).await.unwrap();
    let boosted = |st: &Value| st["status"]["foreground"]["settings"].as_array().is_some_and(|a| !a.is_empty());
    let managed = eventually_opt(Duration::from_secs(10), || {
        let qos = qos.clone();
        async move { boosted(&serde_json::from_str(&qos.status().await.ok()?).ok()?).then_some(()) }
    })
    .await
    .is_some();

    // A timed pause: everything reverted, still observing, back by itself.
    let p = json(qos.set_paused(true, 6).await).await;
    assert_eq!(p["paused"], true);
    assert!(p["until_ms"].as_i64().is_some(), "{p}");
    let st = json(qos.status().await).await;
    assert_eq!(st["daemon"]["state"], "paused");
    assert!(st["text"].as_str().unwrap().contains("⏸ paused (resumes in"), "{}", st["text"]);
    let has = |name: &'static str| {
        let qos = qos.clone();
        async move {
            let v: Value = serde_json::from_str(&qos.recent_events(200).await.ok()?).ok()?;
            v.as_array()?.iter().any(|e| e["kind"] == "notice" && e["name"] == name).then_some(())
        }
    };
    eventually("lindesk.paused notice", Duration::from_secs(10), || has("lindesk.paused")).await;
    if managed {
        // What the pause put back is in its notice. (The status can't tell: a paused agent
        // keeps deciding, and like a dry run it lists what it would apply.)
        let events: Value = serde_json::from_str(&qos.recent_events(200).await.unwrap()).unwrap();
        let paused = events.as_array().unwrap().iter().find(|e| e["name"] == "lindesk.paused").unwrap();
        assert!(paused["data"]["restored"].as_u64().unwrap_or(0) > 0, "settings reverted on pause: {paused}");
    } else {
        eprintln!("note: test process is not in a user unit; skipping the settings checks");
    }

    eventually("auto-resume", Duration::from_secs(15), || {
        let qos = qos.clone();
        async move {
            let p: Value = serde_json::from_str(&qos.get_paused().await.ok()?).ok()?;
            (p["paused"] == false).then_some(())
        }
    })
    .await;
    eventually("lindesk.resumed notice", Duration::from_secs(10), || has("lindesk.resumed")).await;
    if managed {
        eventually("settings re-applied on resume", Duration::from_secs(10), || {
            let qos = qos.clone();
            async move { boosted(&serde_json::from_str(&qos.status().await.ok()?).ok()?).then_some(()) }
        })
        .await;
    }

    // An indefinite pause survives a restart until resumed.
    assert_eq!(json(qos.set_paused(true, 0).await).await["until_ms"], Value::Null);
    let file = tmp.path().join("xdg-state/lindesk/paused.json");
    assert!(file.is_file());
    assert!(agent.terminate().success());
    let mut agent = spawn_agent(tmp.path(), &bus);
    let conn = connect(&bus.address).await;
    wait_for_name(&conn, QOS_NAME, Duration::from_secs(60)).await;
    let qos = QoSTestProxy::new(&conn).await.unwrap();
    assert_eq!(json(qos.get_paused().await).await["paused"], true, "still paused after a restart");
    assert_eq!(json(qos.set_paused(false, 0).await).await["paused"], false);
    assert!(!file.exists(), "resume forgets the pause");
    assert!(agent.terminate().success());
}
