//! `lindeskd --dry-run` (unprivileged, on a private system-like bus, reading the real /sys
//! and /proc) with a connected `lindesk-session --dry-run`: registration, focus forwarding,
//! the unicast `UserState`, the relay policy, and falling back to standalone when the
//! broker goes away.
//!
//! Needs the `lindeskd` binary next to `lindesk-session`: run with
//! `cargo test -p lindeskd -p lindesk-session`.

mod support;

use std::time::Duration;

use futures_util::StreamExt;
use lindesk_ipc::{BROKER_NAME, QOS_NAME};
use serde_json::Value;
use support::*;

fn json(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_relays_to_connected_session() {
    let Some(lindeskd) = lindeskd_bin() else {
        eprintln!(
            "skipping: lindeskd is not built next to lindesk-session (cargo test -p lindeskd -p lindesk-session)"
        );
        return;
    };
    let tmp = fast_tempdir();
    let dir = |d: &str| tmp.path().join(d).display().to_string();
    let system = Bus::system_like(tmp.path());
    let session = Bus::session();
    let me = std::process::id() as i32;
    let uid = rustix::process::getuid().as_raw();
    let common_env = [
        ("DBUS_SYSTEM_BUS_ADDRESS", system.address.as_str()),
        ("LINDESK_INSECURE_NO_POLKIT", "1"),
        ("LINDESK_ALLOW_UNPRIVILEGED", "1"),
    ];

    let (bstate, btel, cfg) = (dir("broker-state"), dir("broker-telemetry"), dir("none.toml"));
    let mut broker = Daemon::spawn(
        "lindeskd",
        &lindeskd,
        &["--dry-run", "--state-dir", &bstate, "--telemetry-dir", &btel, "--config", &cfg],
        &common_env,
    );
    let sys = connect(&system.address).await;
    wait_for_name(&sys, BROKER_NAME, Duration::from_secs(60)).await;
    let bproxy = BrokerTestProxy::new(&sys).await.unwrap();
    let mut broker_events = bproxy.receive_events().await.unwrap();

    let (sstate, stel, xdg) = (dir("session-state"), dir("session-telemetry"), dir("xdg"));
    let mut env = common_env.to_vec();
    env.extend([
        ("DBUS_SESSION_BUS_ADDRESS", session.address.as_str()),
        ("XDG_CONFIG_HOME", &xdg),
        ("XDG_DATA_HOME", &xdg),
    ]);
    let mut agent = Daemon::spawn(
        "lindesk-session",
        &session_bin(),
        &["--dry-run", "--no-adapters", "--state-dir", &sstate, "--telemetry-dir", &stel, "--config", &cfg],
        &env,
    );
    let ses = connect(&session.address).await;
    wait_for_name(&ses, QOS_NAME, Duration::from_secs(60)).await;
    let qos = QoSTestProxy::new(&ses).await.unwrap();

    // Registered: the session says connected, the broker lists our uid.
    let st = json(&qos.status().await.unwrap());
    assert_eq!(st["daemon"]["mode"], "connected", "{st}");
    let bst = json(&bproxy.get_status().await.unwrap());
    assert_eq!(bst["daemon"]["mode"], "broker");
    assert!(bst["daemon"]["sessions"].as_array().unwrap().contains(&Value::from(uid)), "{}", bst["daemon"]);
    assert_eq!(bst["status"]["host"], "lindeskd");

    // Focus goes to the local engine and through ReportForeground to the broker.
    qos.set_foreground_process(me).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let focus = loop {
        let sig = tokio::time::timeout_at(deadline, broker_events.next())
            .await
            .expect("a focus event from the broker within 10 s")
            .expect("signal stream open");
        let args = sig.args().unwrap();
        if let Some(f) = json(args.json()).as_array().unwrap().iter().find(|e| e["kind"] == "focus") {
            break f.clone();
        }
    };
    assert_eq!(focus["pid"], me);
    let target = focus["target"].clone();
    assert_eq!(target["manager"]["uid"], uid, "the broker maps our pid to one of our user units: {focus}");

    // The broker's threshold boosts the foreground; the UserState reaches the agent, whose
    // relay policy turns it into (dry-run) session-side settings with broker provenance.
    let ex = eventually("relayed settings", Duration::from_secs(15), || {
        let qos = qos.clone();
        async move {
            let ex = json(&qos.explain(&me.to_string()).await.ok()?);
            let s = ex["settings"].as_array()?;
            (!s.is_empty()).then_some(ex)
        }
    })
    .await;
    println!("session explain:\n{}", ex["text"].as_str().unwrap());
    for s in ex["settings"].as_array().unwrap() {
        assert_eq!(s["provenance"]["plugin"], "broker");
        assert!(s["provenance"]["reason"].as_str().unwrap().starts_with("broker: threshold: "), "{s}");
    }
    let st = json(&qos.status().await.unwrap());
    assert!(st["daemon"]["broker"]["desired"].as_u64().unwrap() >= 1, "{}", st["daemon"]);
    assert_eq!(st["daemon"]["broker"]["dry_run"], true);
    assert_eq!(st["epoch"]["source"], "broker");
    let bex = json(&bproxy.explain(&me.to_string()).await.unwrap());
    assert_eq!(bex["known"], true);
    assert!(!bex["settings"].as_array().unwrap().is_empty(), "broker side: {}", bex["text"]);
    let recent = json(&bproxy.recent_events(500).await.unwrap());
    assert!(recent.as_array().unwrap().iter().any(|e| e["kind"] == "action_applied"));

    // The broker goes away: the agent falls back to standalone and drops relayed settings.
    assert!(broker.terminate().success());
    let st = eventually("standalone after broker exit", Duration::from_secs(15), || {
        let qos = qos.clone();
        async move {
            let st = json(&qos.status().await.ok()?);
            (st["daemon"]["mode"] == "standalone").then_some(st)
        }
    })
    .await;
    assert_eq!(st["daemon"]["broker"]["connected"], false);
    let ex = eventually("no broker provenance", Duration::from_secs(10), || {
        let qos = qos.clone();
        async move {
            let ex = json(&qos.explain(&me.to_string()).await.ok()?);
            ex["settings"].as_array()?.iter().all(|s| s["provenance"]["plugin"] != "broker").then_some(ex)
        }
    })
    .await;
    println!("after the broker left:\n{}", ex["text"].as_str().unwrap());

    assert!(agent.terminate().success());
}
