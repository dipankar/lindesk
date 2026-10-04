//! `lindeskd --dry-run` (unprivileged, on a private system-like bus, reading the real /sys
//! and /proc) with a connected `lindesk-session --dry-run`: registration, focus forwarding,
//! the unicast `UserState`, the relay policy, the broker's profile in the agent, falling
//! back to standalone when the broker goes away; and the broker's control plane (system-scope
//! changes, `Calibrate`, `GetHardware`, `SwapReport`) with the privacy of its `Events`.
//!
//! Needs the `lindeskd` binary next to `lindesk-session`: run with
//! `cargo test -p lindeskd -p lindesk-session`.

mod support;

use std::time::Duration;

use futures_util::StreamExt;
use lindesk_ipc::{BROKER_NAME, BrokerProxy, QOS_NAME};
use serde_json::{Value, json};
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
    // Events about our units are unicast to our registered agents only: register this
    // connection too so it receives them.
    bproxy.register_session().await.unwrap();
    let mut broker_events = bproxy.receive_events().await.unwrap();

    let (sstate, stel, xdg, xdg_state) = (dir("session-state"), dir("session-telemetry"), dir("xdg"), dir("xdg-state"));
    let mut env = common_env.to_vec();
    env.extend([
        ("DBUS_SESSION_BUS_ADDRESS", session.address.as_str()),
        ("XDG_CONFIG_HOME", &xdg),
        ("XDG_DATA_HOME", &xdg),
        ("XDG_STATE_HOME", &xdg_state),
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
        assert!(s["provenance"]["reason"].as_str().unwrap().starts_with("threshold: "), "{s}");
    }
    let text = ex["text"].as_str().unwrap();
    assert!(text.contains("broker: threshold: ") && !text.contains("broker: broker:"), "one prefix: {text}");
    let st = json(&qos.status().await.unwrap());
    assert!(st["daemon"]["broker"]["desired"].as_u64().unwrap() >= 1, "{}", st["daemon"]);
    assert_eq!(st["daemon"]["broker"]["dry_run"], true);
    assert_eq!(st["epoch"]["source"], "broker");
    let bex = json(&bproxy.explain(&me.to_string()).await.unwrap());
    assert_eq!(bex["known"], true);
    assert!(!bex["settings"].as_array().unwrap().is_empty(), "broker side: {}", bex["text"]);
    let recent = json(&bproxy.recent_events(500).await.unwrap());
    assert!(recent.as_array().unwrap().iter().any(|e| e["kind"] == "action_applied"));

    // Connected, the broker's profile is the one in effect and the agent shows it.
    let st = eventually("the broker's profile in the agent", Duration::from_secs(10), || {
        let qos = qos.clone();
        async move {
            let st = json(&qos.status().await.ok()?);
            (st["daemon"]["profile"]["source"] == "broker").then_some(st)
        }
    })
    .await;
    assert_eq!(st["daemon"]["profile"]["active"], bst["daemon"]["profile"]["active"], "{}", st["daemon"]["profile"]);

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

/// A D-Bus error's name and message.
fn error_of(e: &zbus::Error) -> (String, String) {
    match e {
        zbus::Error::MethodError(name, msg, _) => (name.to_string(), msg.clone().unwrap_or_default()),
        e => (String::new(), e.to_string()),
    }
}

/// Broker alone: system-scope versioned changes, `SetProfile`, `Calibrate`, `GetHardware`,
/// `SwapReport`, and who receives which `Events`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_control_plane_and_event_privacy() {
    let Some(lindeskd) = lindeskd_bin() else {
        eprintln!(
            "skipping: lindeskd is not built next to lindesk-session (cargo test -p lindeskd -p lindesk-session)"
        );
        return;
    };
    let tmp = fast_tempdir();
    let dir = |d: &str| tmp.path().join(d).display().to_string();
    let system = Bus::system_like(tmp.path());
    let original = "# system config\n[general]\ntick = \"250ms\"\n";
    let cfg = tmp.path().join("lindesk.toml");
    std::fs::write(&cfg, original).unwrap();
    let (bstate, btel, cfg_s) = (dir("broker-state"), dir("broker-telemetry"), cfg.display().to_string());
    let mut broker = Daemon::spawn(
        "lindeskd",
        &lindeskd,
        &["--dry-run", "--state-dir", &bstate, "--telemetry-dir", &btel, "--config", &cfg_s],
        &[
            ("DBUS_SYSTEM_BUS_ADDRESS", system.address.as_str()),
            ("LINDESK_INSECURE_NO_POLKIT", "1"),
            ("LINDESK_ALLOW_UNPRIVILEGED", "1"),
        ],
    );
    // Two clients of the same user: `plain` only listens, `agent` registered as a session agent.
    let plain_conn = connect(&system.address).await;
    wait_for_name(&plain_conn, BROKER_NAME, Duration::from_secs(60)).await;
    let plain = BrokerProxy::new(&plain_conn).await.unwrap();
    let agent_conn = connect(&system.address).await;
    let agent = BrokerTestProxy::new(&agent_conn).await.unwrap();
    let mut plain_events = plain.receive_events().await.unwrap();
    let mut agent_events = agent.receive_events().await.unwrap();
    let mut changes = plain.receive_change_event().await.unwrap();
    agent.register_session().await.unwrap();

    // ---- Events privacy: a focus on one of our processes is about our units ---------------
    let me = std::process::id() as i32;
    plain.report_foreground(me, "").await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let sig = tokio::time::timeout_at(deadline, agent_events.next())
            .await
            .expect("the registered agent gets the user's focus event")
            .unwrap();
        if json(sig.args().unwrap().json()).as_array().unwrap().iter().any(|e| e["kind"] == "focus" && e["pid"] == me) {
            break;
        }
    }
    // The unregistered client saw only system-scope events (if any) meanwhile.
    let quiet = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Some(sig)) = tokio::time::timeout_at(quiet, plain_events.next()).await {
        for e in json(sig.args().unwrap().json()).as_array().unwrap() {
            assert_ne!(e["kind"], "focus", "user events must not be broadcast: {e}");
            assert!(e["target"]["manager"]["uid"].is_null(), "user unit event broadcast: {e}");
        }
    }
    // RecentEvents: our own events are visible to us.
    let recent = json(&plain.recent_events(500).await.unwrap());
    assert!(recent.as_array().unwrap().iter().any(|e| e["kind"] == "focus" && e["pid"] == me));

    // ---- system-scope changes ----------------------------------------------------------------
    let set_epoch = json!({"origin": "test", "ops": [
        {"op": "config_set", "scope": "system", "key": "plugins.threshold.config.epoch", "value": "400ms"}
    ]})
    .to_string();
    let plan = json(&plain.plan_change(&set_epoch).await.unwrap());
    assert_eq!(plan["rejections"], json!([]), "{plan}");
    let cs = json(&plain.apply_change(&set_epoch, "shorter epoch", 0).await.unwrap());
    let id = cs["id"].as_u64().unwrap();
    assert_eq!((cs["status"].as_str(), cs["scope"].as_str()), (Some("applied"), Some("system")), "{cs}");
    assert!(cs["ttl_until"].is_null());
    let ev = tokio::time::timeout(Duration::from_secs(10), changes.next()).await.unwrap().unwrap();
    assert_eq!(json(ev.args().unwrap().json())["id"].as_u64(), Some(id));
    assert!(std::fs::read_to_string(&cfg).unwrap().contains("epoch = \"400ms\""));
    let r = json(&plain.revert_change(id).await.unwrap());
    assert_eq!(r["reverts"].as_u64(), Some(id));
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
    assert!(std::path::Path::new(&bstate).join("changes").join(format!("{id:06}.json")).is_file());
    let bad = json!([{"op": "config_set", "scope": "system", "key": "safety.nope", "value": 1}]).to_string();
    let (name, msg) = error_of(&plain.apply_change(&bad, "bad", 0).await.unwrap_err());
    assert_eq!(name, "org.lindesk.Error.Rejected", "{msg}");
    let user_op = json!([{"op": "profile_use", "scope": "user", "name": "battery"}]).to_string();
    let (name, _) = error_of(&plain.apply_change(&user_op, "wrong scope", 0).await.unwrap_err());
    assert_eq!(name, "org.lindesk.Error.Rejected");

    // ---- SetProfile on the broker ----------------------------------------------------------
    let p = json(&plain.set_profile("latency").await.unwrap());
    let st = eventually("broker profile latency", Duration::from_secs(10), || {
        let plain = plain.clone();
        async move {
            let st = json(&plain.get_status().await.ok()?);
            (st["daemon"]["profile"]["active"] == "latency").then_some(st)
        }
    })
    .await;
    assert_eq!(st["daemon"]["profile"]["pinned"], true);
    let profiles = json(&plain.list_profiles().await.unwrap());
    assert!(profiles.as_array().unwrap().iter().any(|p| p["name"] == "latency" && p["active"] == true));
    plain.revert_change(p["id"].as_u64().unwrap()).await.unwrap();
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);

    // ---- GetHardware, SwapReport, Calibrate --------------------------------------------------
    let hw = json(&plain.get_hardware().await.unwrap());
    assert!(hw["cpus"].as_u64().unwrap() >= 1);
    assert!(json(&plain.swap_report().await.unwrap()).is_object());
    // Quick, forced (a busy test machine gives a `noisy` map) and without the disk phase.
    let map = json(&plain.calibrate_with(r#"{"quick": true, "storage": false}"#).await.unwrap());
    assert!(!map["cpus"].as_array().unwrap().is_empty(), "{map}");
    assert!(std::path::Path::new(&bstate).join("calibration.json").is_file(), "saved under the state dir");
    let hw = eventually("latency map in the caps", Duration::from_secs(10), || {
        let plain = plain.clone();
        async move {
            let hw = json(&plain.get_hardware().await.ok()?);
            hw["latency"].is_object().then_some(hw)
        }
    })
    .await;
    assert_eq!(hw["latency"]["cpus"], map["cpus"]);
    let recent = json(&plain.recent_events(500).await.unwrap());
    assert!(recent.as_array().unwrap().iter().any(|e| e["name"] == "calibration.done"));

    assert!(broker.terminate().success());
}
