//! `lindeskctl events`: recent events, and a live stream with `--follow`.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

use crate::bus::{self, Daemon};
use crate::live::{self, FeedItem};
use crate::ui::{self, Ui, out};

pub const KINDS: &[&str] =
    &["focus", "interaction", "class_set", "level", "action_applied", "action_reverted", "plugin_error", "notice"];

fn wanted(kinds: &[String], v: &Value) -> bool {
    if kinds.is_empty() {
        return true;
    }
    let k = v.get("kind").and_then(Value::as_str).unwrap_or("");
    kinds.iter().any(|w| w == k || (w == "action" && k.starts_with("action_")))
}

/// One printed line.
pub fn line(ui: &Ui, e: &FeedItem) -> String {
    let text = match e.severity {
        2 => ui.red(&e.text),
        1 => ui.yellow(&e.text),
        _ => e.text.clone(),
    };
    format!("{}  {}  {text}", ui.dim(ui::clock(e.ts)), ui.cyan(format!("{:<15}", e.kind)))
}

fn emit(ui: &Ui, kinds: &[String], v: &Value) {
    if !wanted(kinds, v) {
        return;
    }
    if ui.json {
        crate::ui::out_raw(&v.to_string());
    } else {
        out(&line(ui, &live::describe_any(v)));
    }
}

/// Last events from the telemetry lake (daemons without `RecentEvents`).
fn from_lake(ui: &Ui, n: usize) -> Result<Vec<Value>> {
    let config = crate::paths::load_config(&crate::paths::config_path(ui.system)).ok();
    let root = crate::paths::lake_root(None, ui.system, config.as_ref());
    let lake = lindesk_query::Lake::open(&root)?;
    let sql = format!("select payload from events order by ts desc limit {n}");
    let json = lake.query(&sql, &lindesk_query::Format::Json)?;
    let rows: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
    let mut v: Vec<Value> = rows
        .iter()
        .filter_map(|r| r.get("payload").and_then(Value::as_str).and_then(|p| serde_json::from_str(p).ok()))
        .collect();
    v.reverse();
    Ok(v)
}

pub fn run(ui: &Ui, follow: bool, kinds: &[String], n: usize) -> Result<()> {
    for k in kinds {
        if k != "action" && !KINDS.contains(&k.as_str()) {
            return Err(
                ui::Usage(format!("unknown event kind '{k}' (expected one of: action, {})", KINDS.join(", "))).into()
            );
        }
    }
    let order: &[Daemon] = if ui.system { &[Daemon::Broker] } else { &[Daemon::Session, Daemon::Broker] };
    let mut conn = None;
    let mut first_err = None;
    for &d in order {
        match bus::running(d) {
            Ok(c) => {
                conn = Some((d, c));
                break;
            }
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    let Some((d, conn)) = conn else {
        if !follow {
            ui.info(ui.dim("lindesk isn't running; showing the last events recorded in telemetry"));
            for v in from_lake(ui, n)? {
                emit(ui, kinds, &v);
            }
            return Ok(());
        }
        return Err(first_err.unwrap_or_else(|| Daemon::Session.not_running()));
    };
    // Subscribe before backfilling so nothing falls between the two.
    let rx = if follow { live::subscribe(&conn, d).ok() } else { None };
    match live::recent_events(&conn, d, n as u32)? {
        Some(recent) => {
            // Events carry a per-daemon `seq`: skip what the backfill already showed.
            let mut seen: HashSet<String> = HashSet::new();
            let mut last_seq: Option<u64> = None;
            for v in &recent {
                seen.insert(v.to_string());
                if let Some(s) = v.get("seq").and_then(Value::as_u64) {
                    last_seq = Some(last_seq.map_or(s, |l| l.max(s)));
                }
                emit(ui, kinds, v);
            }
            let Some(rx) = rx else { return Ok(()) };
            ui.info(ui.dim(format!("following {} events (Ctrl-C to stop)…", d.name())));
            for v in rx {
                let dup = match (v.get("seq").and_then(Value::as_u64), last_seq) {
                    (Some(s), Some(l)) => s <= l,
                    _ => !seen.is_empty() && seen.remove(&v.to_string()),
                };
                if !dup {
                    emit(ui, kinds, &v);
                }
            }
            Err(anyhow::anyhow!("{} closed the connection", d.name()))
        }
        None => {
            // An older daemon: no Event signal or RecentEvents.
            if !follow {
                ui.info(
                    ui.dim(format!(
                        "{} doesn't serve RecentEvents yet; showing events recorded in telemetry",
                        d.name()
                    )),
                );
                for v in from_lake(ui, n)? {
                    emit(ui, kinds, &v);
                }
                return Ok(());
            }
            ui.info(ui.dim(format!(
                "{} has no Event signal yet; inferring events from status changes (Ctrl-C to stop)…",
                d.name()
            )));
            let mut client = live::Client::new(ui.system);
            loop {
                let snap = client.poll(0);
                for v in &snap.events {
                    emit(ui, kinds, v);
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kind_filter() {
        let v = json!({"kind": "action_applied"});
        assert!(wanted(&[], &v));
        assert!(wanted(&["action".into()], &v));
        assert!(wanted(&["action_applied".into()], &v));
        assert!(!wanted(&["level".into()], &v));
    }

    #[test]
    fn formats_a_line() {
        let e =
            live::describe(&json!({"kind": "level", "ts": 0, "from": "yellow", "to": "red", "policy": "threshold"}));
        let l = line(&Ui::plain(), &e);
        assert!(l.contains("level            level YELLOW → RED  (threshold)"), "{l}");
    }
}
