# D-Bus API

lindesk has two D-Bus services. Desktop shells, applications and `lindeskctl` use the
session one; session agents and `lindeskctl --system` use the broker. The authoritative
contract is [docs/ARCHITECTURE.md](https://github.com/dipankar/lindesk/blob/main/docs/ARCHITECTURE.md),
and the Rust client proxies are `lindesk_ipc::QoSProxy` / `BrokerProxy` (plus the
`…ProxyBlocking` variants).

## Session bus: `org.lindesk.QoS1`

Owned by `lindesk-session`. Object `/org/lindesk/QoS1`, interface `org.lindesk.QoS1`.

| Member | Signature | What it does |
|---|---|---|
| `SetForegroundProcess(pid)` | `i` → | Focus moved to a window owned by `pid` (GNOME extension, KWin script). |
| `SetForegroundApp(app_id)` | `s` → | Focus moved to an app known only by its id. |
| `BeginInteraction(pid)` | `i` → | Start or extend an interaction epoch (input, app-initiated work). |
| `EndInteraction(pid)` | `i` → | End it early. |
| `SetClass(pid, class)` | `is` → | `critical`, `interactive`, `normal`, `background` or `batch`. Own processes only. |
| `ReportMetric(name, value, labels)` | `sda{ss}` → | A free-form metric for the `metrics` table. |
| `Status()` | → `s` | JSON [`Status`](#status-json). |
| `Explain(what)` | `s` → `s` | `what` is a pid or a target string; JSON [`Explanation`](#explain-json). |
| `RecentEvents(n)` | `u` → `s` | JSON array of the last `n` [events](#events), oldest first. |
| signal `LevelChanged(level)` | `s` | `green`, `yellow` or `red`. |
| signal `Events(json)` | `s` | The events of one engine step, as a JSON array (at most 100). |

## System bus: `org.lindesk.Broker1`

Owned by `lindeskd`. Object `/org/lindesk/Broker1`, interface `org.lindesk.Broker1`.
Callers are identified by their bus credentials; polkit decides.

| Member | Signature | polkit action |
|---|---|---|
| `RegisterSession()` | → `s` | none; returns the caller's `SessionState` JSON |
| `ReportForeground(pid, app_id)` | `is` → | `org.lindesk.set-class-own` (the pid must be the caller's) |
| `Interaction(pid, end)` | `ib` → | same |
| `SetClass(pid, class)` | `is` → | own pid: `org.lindesk.set-class-own`; others: `org.lindesk.set-class-any` |
| `GetStatus()` | → `s` | none |
| `Explain(what)` | `s` → `s` | none |
| `RecentEvents(n)` | `u` → `s` | none |
| `Reload()` | → | `org.lindesk.manage` |
| signal `UserState(uid, json)` | `us` | `SessionState`: the settings one user's session agent should apply |
| signal `Events(json)` | `s` | Broadcast: the events of one broker step, as a JSON array. |

`RecentEvents` and `Events` are newer than the rest of the API. `lindeskctl watch` and
`lindeskctl events` detect daemons without them and fall back to inferring events from
consecutive `Status` replies (or, for `events` without `--follow`, from the telemetry lake).

## Status JSON

`Status()` / `GetStatus()` return an envelope around a serialised `lindesk_host::Status`:

```json
{
  "status":   { … lindesk_host::Status, below … },
  "daemon":   {"name": "lindesk-session", "version": "0.1.0", "pid": 1234, "mode": "connected",
               "dry_run": false, "started": 1791099000000, "uptime_s": 7980.5, …},
  "pressure": {"system": {"cpu": P, "io": P, "memory": P},
               "foreground": {"target": "user/1000:app-….scope", "cpu": P, "io": P, "memory": P}},
  "epoch":    {"active": true, "source": "threshold"},
  "hardware": { … lindesk_plugin_api::Hardware … },
  "requirements": [ … warnings from the startup requirements check … ],
  "text": "the human rendering"
}
```

`P` is `{"some": 0.12, "full": 0.0, "some_avg10": 11.9, "full_avg10": 0.0, "interval_ms": 1000, "ts": …}`.
`mode` is `broker` (lindeskd), `connected` or `standalone` (lindesk-session), or `starting`.
The inner `status`:

```json
{
  "now": 1791100000000, "host": "lindesk-session", "level": "yellow", "dry_run": false,
  "foreground": {"target": {"type": "unit", "manager": {"kind": "user", "uid": 1000},
                            "name": "app-gnome-org.mozilla.firefox-4242.scope"},
                 "comm": "firefox", "class": "interactive", "foreground": true,
                 "settings": [{"knob": "cpu_weight", "value": 500}]},
  "protected": [ … same shape … ],
  "yielding": [{"target": {…}, "comm": "rustc", "class": "batch",
                "settings": [{"knob": "io_weight", "value": 1}],
                "since": 1791099870000, "cpu_delta_pct": -12.0, "io_delta_pct": -31.0}],
  "plugins": [{"name": "threshold", "kinds": ["policy"], "origin": {"type": "builtin"},
               "state": "running", "stats": {"calls": 14211, "errors": 0, "timeouts": 0,
               "failures": 0, "restarts": 0, "latency_ewma_us": 210.0, "latency_p99_us": 310}}]
}
```

`lindeskctl status --json` prints the whole envelope as the daemon sent it. `lindeskctl`
also accepts a bare `Status` object from older daemons.

## Explain JSON

`Explain(what)` returns a serialised `lindesk_host::Explanation`: the `target`, whether it
is `known`, the representative `proc`, the `class` with its `source` (`explicit`,
`override`, `classifier` or `default`) and `reason`, `foreground`, `dry_run`, the
`settings` lindesk holds (each with `value`, `original`, `provenance` and `since`) and
recent `notes` (clamps, rejections, plugin and actuator errors).

## Events

Each event is a serialised `lindesk_plugin_api::Event`, tagged by `kind`: `focus`,
`interaction`, `class_set`, `level`, `action_applied`, `action_reverted`,
`plugin_error`, plus `seq`, which increases by one per event for the life of the daemon, so
a client can merge the `RecentEvents` backfill with the live `Events` signal without
duplicates (that's what `lindeskctl events -f` does). The same JSON (without `seq`) is
stored in the `payload` column of the `events` table.

## From the command line

```console
$ busctl --user call org.lindesk.QoS1 /org/lindesk/QoS1 org.lindesk.QoS1 Status | head -c 200
$ gdbus call --session -d org.lindesk.QoS1 -o /org/lindesk/QoS1 -m org.lindesk.QoS1.SetClass 4242 batch
$ busctl --user monitor org.lindesk.QoS1          # watch Events signals go by
```
