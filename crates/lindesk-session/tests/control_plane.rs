//! The user-scope control plane of `lindesk-session --standalone --dry-run` on a private
//! session bus (docs/CONTROL-PLANE.md §5): plan, apply with a dead-man ttl, `ChangeEvent`,
//! expiry, confirm, revert, guardrail rejections, `SetProfile`, `GetHardware`, `SwapReport`.
//! Everything happens in a temporary config file and state directory.

mod support;

use std::path::Path;
use std::time::Duration;

use futures_util::StreamExt;
use lindesk_ipc::{QOS_NAME, QoSProxy};
use serde_json::{Value, json};
use support::*;

const ORIGINAL: &str = "# my lindesk config (comments must survive)\n[general]\ntick = \"250ms\"\n";

fn parse(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|e| panic!("invalid JSON ({e}): {s}"))
}

fn envelope(ops: Value) -> String {
    json!({ "origin": "test", "client": "control-plane-test", "ops": ops }).to_string()
}

fn set_epoch(v: &str) -> Value {
    json!([{ "op": "config_set", "scope": "user", "key": "plugins.threshold.config.epoch", "value": v }])
}

/// The D-Bus error name and message of a failed call.
fn error_of(e: &zbus::Error) -> (String, String) {
    match e {
        zbus::Error::MethodError(name, msg, _) => (name.to_string(), msg.clone().unwrap_or_default()),
        e => (String::new(), e.to_string()),
    }
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

async fn next_change(changes: &mut lindesk_ipc::ChangeEventStream) -> Value {
    let sig = tokio::time::timeout(Duration::from_secs(15), changes.next())
        .await
        .expect("a ChangeEvent within 15 s")
        .expect("signal stream open");
    parse(sig.args().unwrap().json())
}

async fn change_status(qos: &QoSProxy<'_>, id: u64) -> String {
    parse(&qos.get_change(id).await.unwrap())["status"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_scope_control_plane() {
    let tmp = fast_tempdir();
    let bus = Bus::session();
    let dir = |d: &str| tmp.path().join(d).display().to_string();
    let cfg = tmp.path().join("config/lindesk/lindesk.toml");
    std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    std::fs::write(&cfg, ORIGINAL).unwrap();
    let cfg_s = cfg.display().to_string();
    let (state, tel, xdg_config, xdg_state) = (dir("run"), dir("telemetry"), dir("config"), dir("state"));
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
            &cfg_s,
        ],
        &[
            ("DBUS_SESSION_BUS_ADDRESS", &bus.address),
            ("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent/lindesk-test"),
            ("XDG_CONFIG_HOME", &xdg_config),
            ("XDG_DATA_HOME", &xdg_config),
            ("XDG_STATE_HOME", &xdg_state),
        ],
    );
    let conn = connect(&bus.address).await;
    wait_for_name(&conn, QOS_NAME, Duration::from_secs(60)).await;
    let qos = QoSProxy::new(&conn).await.unwrap();
    let mut changes = qos.receive_change_event().await.unwrap();
    let me = rustix::process::getuid().as_raw();

    // ---- plan: a diff, no side effects ----------------------------------------------------
    let plan = parse(&qos.plan_change(&envelope(set_epoch("400ms"))).await.unwrap());
    assert_eq!(plan["rejections"], json!([]), "{plan}");
    assert_eq!(plan["risk"], "low");
    assert!(plan["diff"].as_str().unwrap().contains("epoch = \"400ms\""), "{}", plan["diff"]);
    assert_eq!(read(&cfg), ORIGINAL, "planning changes nothing");

    // ---- apply with a 2 s ttl → ChangeEvent → the dead-man switch reverts it ---------------
    let cs = parse(&qos.apply_change(&envelope(set_epoch("400ms")), "try a shorter epoch", 2).await.unwrap());
    let id = cs["id"].as_u64().unwrap();
    assert_eq!(cs["status"], "applied", "{cs}");
    assert!(cs["ttl_until"].is_i64());
    assert_eq!(cs["author"]["uid"], me);
    assert_eq!(cs["author"]["client"], "control-plane-test");
    assert!(cs["author"]["bus_name"].as_str().unwrap().starts_with(':'));
    let ev = next_change(&mut changes).await;
    assert_eq!((ev["id"].as_u64(), ev["status"].as_str()), (Some(id), Some("applied")), "{ev}");
    let applied = read(&cfg);
    assert!(applied.starts_with("# my lindesk config") && applied.contains("epoch = \"400ms\""), "{applied}");
    // The engine reloaded the new config.
    let st = parse(&qos.status().await.unwrap());
    assert!(st["status"]["plugins"].as_array().unwrap().iter().any(|p| p["name"] == "threshold"));

    let mut saw_revert = false;
    let mut saw_expired = false;
    while !(saw_revert && saw_expired) {
        let ev = next_change(&mut changes).await;
        saw_revert |= ev["reverts"].as_u64() == Some(id);
        saw_expired |= ev["id"].as_u64() == Some(id) && ev["status"] == "reverted";
    }
    assert_eq!(change_status(&qos, id).await, "reverted");
    assert_eq!(read(&cfg), ORIGINAL, "expiry restored the file byte for byte");

    // ---- confirm prevents expiry -----------------------------------------------------------
    let cs = parse(&qos.apply_change(&envelope(set_epoch("500ms")), "keep this one", 2).await.unwrap());
    let kept = cs["id"].as_u64().unwrap();
    qos.confirm_change(kept).await.unwrap();
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(change_status(&qos, kept).await, "confirmed");
    assert!(read(&cfg).contains("epoch = \"500ms\""));

    // ---- revert creates a new version ------------------------------------------------------
    let r = parse(&qos.revert_change(kept).await.unwrap());
    let rid = r["id"].as_u64().unwrap();
    assert!(rid > kept, "a new changeset: {r}");
    assert_eq!(r["reverts"].as_u64(), Some(kept));
    assert_eq!(change_status(&qos, kept).await, "reverted");
    assert_eq!(read(&cfg), ORIGINAL);
    let list = parse(&qos.list_changes(50).await.unwrap());
    let ids: Vec<u64> = list.as_array().unwrap().iter().map(|c| c["id"].as_u64().unwrap()).collect();
    assert_eq!(ids.first(), Some(&rid), "newest first: {ids:?}");
    assert!(ids.contains(&id) && ids.contains(&kept));
    // Reverting twice is a refusal, not a crash.
    let (name, msg) = error_of(&qos.revert_change(kept).await.unwrap_err());
    assert_eq!(name, "org.lindesk.Error.Rejected", "{msg}");
    let (name, _) = error_of(&qos.get_change(9999).await.unwrap_err());
    assert_eq!(name, "org.lindesk.Error.NotFound");

    // ---- guardrail rejections → org.lindesk.Error.Rejected ---------------------------------
    let bad_key =
        json!([{ "op": "config_set", "scope": "user", "key": "plugins.threshold.config.bogus_knob", "value": 1 }]);
    let plan = parse(&qos.plan_change(&envelope(bad_key.clone())).await.unwrap());
    assert!(!plan["rejections"].as_array().unwrap().is_empty(), "{plan}");
    let (name, msg) = error_of(&qos.apply_change(&envelope(bad_key), "bad", 60).await.unwrap_err());
    assert_eq!(name, "org.lindesk.Error.Rejected", "{msg}");
    assert!(msg.contains("bogus_knob"), "the human message says why: {msg}");
    let sysctl = json!([{ "op": "sysctl", "key": "vm.swappiness", "value": "10" }]);
    let (name, msg) = error_of(&qos.apply_change(&envelope(sysctl), "wrong scope", 60).await.unwrap_err());
    assert_eq!(name, "org.lindesk.Error.Rejected", "{msg}");
    let (name, _) = error_of(&qos.apply_change("[{\"op\":\"nope\"}]", "malformed", 60).await.unwrap_err());
    assert_eq!(name, "org.freedesktop.DBus.Error.InvalidArgs");
    let (name, msg) = error_of(&qos.set_profile("no-such-profile").await.unwrap_err());
    assert_eq!(name, "org.lindesk.Error.Rejected", "{msg}");
    assert_eq!(read(&cfg), ORIGINAL, "nothing rejected was written");

    // ---- SetProfile("battery") reloads the engine; status shows it -------------------------
    let cs = parse(&qos.set_profile("battery").await.unwrap());
    let battery = cs["id"].as_u64().unwrap();
    assert_eq!(cs["ops"][0]["op"], "profile_use");
    let st = eventually("profile battery", Duration::from_secs(10), || {
        let qos = qos.clone();
        async move {
            let st = parse(&qos.status().await.ok()?);
            (st["daemon"]["profile"]["active"] == "battery").then_some(st)
        }
    })
    .await;
    let p = &st["daemon"]["profile"];
    assert_eq!((p["pinned"].as_bool(), p["setting"].as_str()), (Some(true), Some("battery")), "{p}");
    assert!(p["matched"].is_array() && p["reason"].is_string());
    let profiles = parse(&qos.list_profiles().await.unwrap());
    let active: Vec<&str> = profiles
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["active"] == true)
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(active, ["battery"]);
    // The notice is emitted by the engine thread on its next step, after the reply.
    let mut seen = false;
    for _ in 0..50 {
        let events = parse(&qos.recent_events(500).await.unwrap());
        if events.as_array().unwrap().iter().any(|e| e["name"] == "profile.changed" && e["data"]["to"] == "battery") {
            seen = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(seen, "profile.changed notice in the events");
    let events = parse(&qos.recent_events(500).await.unwrap());
    assert!(events.as_array().unwrap().iter().any(|e| e["name"] == "change.applied"));
    // Back to auto, then unwind both so the file is the original again.
    let auto = parse(&qos.set_profile("auto").await.unwrap())["id"].as_u64().unwrap();
    eventually("auto selection", Duration::from_secs(10), || {
        let qos = qos.clone();
        async move { (parse(&qos.status().await.ok()?)["daemon"]["profile"]["pinned"] == false).then_some(()) }
    })
    .await;
    qos.revert_change(auto).await.unwrap();
    qos.revert_change(battery).await.unwrap();
    assert_eq!(read(&cfg), ORIGINAL);

    // ---- GetHardware, SwapReport -----------------------------------------------------------
    let hw = parse(&qos.get_hardware().await.unwrap());
    assert!(hw["cpus"].as_u64().unwrap() >= 1, "{hw}");
    let swap = parse(&qos.swap_report().await.unwrap());
    assert!(swap.is_object(), "{swap}");

    assert!(agent.terminate().success());
    assert_eq!(read(&cfg), ORIGINAL);
}
