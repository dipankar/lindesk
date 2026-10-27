//! `lindesk-session --standalone --dry-run` on a private session bus: the QoS1 API end to end.

mod support;

use std::time::Duration;

use futures_util::StreamExt;
use lindesk_ipc::QOS_NAME;
use serde_json::Value;
use support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn qos_api_standalone_dry_run() {
    let tmp = fast_tempdir();
    let bus = Bus::session();
    let dir = |d: &str| tmp.path().join(d).display().to_string();
    let (state, tel, cfg, xdg, xdg_state) =
        (dir("state"), dir("telemetry"), dir("none.toml"), dir("xdg"), dir("xdg-state"));
    let mut agent = Daemon::spawn(
        "lindesk-session",
        &session_bin(),
        &[
            "--standalone",
            "--dry-run",
            "--no-adapters",
            "--state-dir",
            &state,
            "--telemetry-dir",
            &tel,
            "--config",
            &cfg,
        ],
        &[
            ("DBUS_SESSION_BUS_ADDRESS", &bus.address),
            ("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent/lindesk-test"),
            ("XDG_CONFIG_HOME", &xdg),
            ("XDG_DATA_HOME", &xdg),
            ("XDG_STATE_HOME", &xdg_state),
        ],
    );
    let conn = connect(&bus.address).await;
    wait_for_name(&conn, QOS_NAME, Duration::from_secs(60)).await;
    let qos = QoSTestProxy::new(&conn).await.unwrap();
    let mut events = qos.receive_events().await.unwrap();

    // Status before anything happened.
    let st: Value = serde_json::from_str(&qos.status().await.unwrap()).unwrap();
    assert_eq!(st["status"]["host"], "lindesk-session");
    assert_eq!(st["status"]["dry_run"], true);
    assert_eq!(st["daemon"]["mode"], "standalone");
    assert_eq!(st["daemon"]["forced_standalone"], true);
    assert!(st["daemon"]["uptime_s"].as_f64().is_some());
    assert!(st["pressure"]["system"].is_object());
    assert!(st["text"].as_str().unwrap().contains("lindesk-session: level"));

    // Focus this test process (it lives in a user unit when run from a desktop terminal).
    let me = std::process::id() as i32;
    qos.set_foreground_process(me).await.unwrap();
    let batch = tokio::time::timeout(Duration::from_secs(10), events.next()).await.expect("an Events signal").unwrap();
    let args = batch.args().unwrap();
    let arr: Value = serde_json::from_str(args.json()).unwrap();
    let focus = arr.as_array().unwrap().iter().find(|e| e["kind"] == "focus").expect("focus event streamed");
    assert_eq!(focus["pid"], me);
    assert!(focus["seq"].as_u64().unwrap() >= 1);

    let recent: Value = serde_json::from_str(&qos.recent_events(50).await.unwrap()).unwrap();
    assert!(recent.as_array().unwrap().iter().any(|e| e["kind"] == "focus" && e["pid"] == me));
    assert!(qos.recent_events(0).await.unwrap() == "[]");

    let ex: Value = serde_json::from_str(&qos.explain(&me.to_string()).await.unwrap()).unwrap();
    assert_eq!(ex["query"], me.to_string());
    assert!(ex["text"].is_string());
    let managed = ex["known"] == true;
    println!("explain({me}):\n{}", ex["text"].as_str().unwrap());

    if managed {
        // Our own process: SetClass works and shows up in explain.
        qos.set_class(me, "batch").await.unwrap();
        // The class is decided on the next step.
        let ex = eventually("explicit class", Duration::from_secs(10), || {
            let qos = qos.clone();
            async move {
                let ex: Value = serde_json::from_str(&qos.explain(&me.to_string()).await.ok()?).ok()?;
                (ex["class"]["class"] == "batch").then_some(ex)
            }
        })
        .await;
        assert_eq!(ex["class"]["source"]["by"], "explicit");
        let st: Value = serde_json::from_str(&qos.status().await.unwrap()).unwrap();
        assert_eq!(st["status"]["foreground"]["target"], ex["target"]);
        // The standalone pipeline ran the threshold policy: its epoch is visible.
        let st = eventually("epoch flag", Duration::from_secs(5), || {
            let qos = qos.clone();
            async move {
                let st: Value = serde_json::from_str(&qos.status().await.ok()?).ok()?;
                st["epoch"]["active"].is_boolean().then_some(st)
            }
        })
        .await;
        assert_eq!(st["epoch"]["source"], "threshold");
    } else {
        eprintln!("note: test process is not in a user unit; skipping SetClass checks");
    }

    // Errors.
    assert!(qos.set_class(1, "batch").await.is_err(), "pid 1 belongs to root");
    assert!(qos.set_class(me, "realtime").await.is_err(), "unknown class");
    assert!(qos.explain("user/abc:x").await.is_err());
    let unknown: Value = serde_json::from_str(&qos.explain("system:nope.service").await.unwrap()).unwrap();
    assert_eq!(unknown["known"], false, "system units are not this host's");
    qos.begin_interaction(me).await.unwrap();
    qos.end_interaction(me).await.unwrap();
    qos.report_metric("frames_dropped", 3.0, Default::default()).await.unwrap();
    let api = lindesk_ipc::QoSProxy::new(&conn).await.unwrap();
    api.report_metric("desktop.frame_ms", 19.5, Default::default()).await.unwrap();
    assert!(api.report_metric("desktop.frame_ms", f64::NAN, Default::default()).await.is_err());
    let latency: Value = serde_json::from_str(&api.latency_report().await.unwrap()).unwrap();
    assert_eq!(latency["summary"]["desktop.frame_ms"]["p99"], 19.5);
    assert!(latency["context"].as_array().is_some_and(|v| !v.is_empty()));
    let server = agent.child.as_ref().unwrap().id() as i32;
    api.report_dependency(me, server, 1000).await.unwrap();
    api.report_dependency(me, server, 0).await.unwrap();
    assert!(api.report_dependency(me, me, 1000).await.is_err());
    assert!(api.report_dependency(me, 1, 1000).await.is_err());
    assert!(api.report_dependency(me, 1, 30_001).await.is_err());
    qos.set_foreground_app("org.example.Nope").await.unwrap();

    let status = agent.terminate();
    assert!(status.success(), "clean exit on SIGTERM: {status:?}");
    let left = std::fs::read_to_string(tmp.path().join("state/state.json")).unwrap_or_else(|_| "[]".into());
    assert_eq!(left.trim(), "[]", "dry run leaves no applied state");
}
