# Plugin protocol

External plugins speak **JSON-RPC 2.0, one JSON object per line, over stdio**. The host
writes requests and notifications to the plugin's stdin and reads responses and
notifications from its stdout. Anything on stderr is captured as log lines. This is API
version **1**.

Machine-readable reference: [`protocol.schema.json`](protocol.schema.json), JSON Schema
draft 2020-12, generated from the Rust types in `lindesk-plugin-api`. Get the same document
from `lindeskctl plugin schema`, or regenerate it in a checkout:

```sh
cargo run -q --manifest-path examples/gen-schema/Cargo.toml > docs/src/reference/protocol.schema.json
cargo run -q --manifest-path examples/gen-schema/Cargo.toml -- --check docs/src/reference/protocol.schema.json
```

## Framing

* UTF-8. Exactly one message per line, terminated by `\n`. Never pretty-print, and never
  send batch arrays.
* **Requests** have an integer `id` and expect exactly one response with the same `id`.
  **Notifications** have no `id` and must never be answered.
* The plugin never sends requests to the host. It answers requests and sends
  notifications.
* stdout carries protocol messages only. Anything else there counts as a protocol error.
  Log to stderr or with the `log` notification.
* Key order is not significant. The envelope comes as `jsonrpc`, `id`, `method`, `params`,
  but keys inside `params` arrive in no particular order (in practice, sorted).
* Within API v1, new *optional* fields may appear in any object. Ignore fields you don't
  know. Breaking changes bump `api_version`.

## Lifecycle

```text
host                                         plugin
 │ spawn (cwd = plugin dir, minimal env)       │
 │── initialize {api_version, host, …} ───────►│  check version, read config
 │◄──────── {name, version, kinds, …} ─────────│  (an error = refuse to start)
 │── adapter.start ───────────────────────────►│  adapters only, once per process
 │                                             │
 │── classifier.classify / policy.evaluate ───►│  per kind, each with a timeout
 │◄─────────────────────────────── result ─────│
 │── sink.record (notification) ──────────────►│  no reply
 │◄───────── event / log / metric / samples ───│  notifications, any time, any thread
 │                                             │
 │── sink.flush ──────────────────────────────►│  sinks, before shutdown
 │── shutdown (notification), close stdin ────►│  exit 0 within 500 ms
 │                     (then SIGTERM, then SIGKILL)
```

The process starts in its plugin directory, in its own process group. Its environment is
cleared except for `PATH`, `HOME`, `LANG`, `XDG_*` and `LINDESK_SDK_PATH`, plus
`LINDESK_API_VERSION=1`. After a crash or timeout the host restarts it lazily, with
exponential backoff, and runs the handshake (and `adapter.start`) again.

## Host → plugin

| Method | Type | Params | Result | Default timeout |
|---|---|---|---|---|
| `initialize` | request | `InitializeParams` | `InitializeResult` | 2 s (`[timeouts] init_ms`) |
| `shutdown` | notification | none | none | 500 ms to exit |
| `classifier.classify` | request | `{proc: ProcInfo}` | `Classification` or `null` | 50 ms |
| `policy.evaluate` | request | `{snapshot: Snapshot}` | `PolicyOutput` | 100 ms |
| `sensor.poll` | request | `SensorInput` `{now, targets}` | `SensorOutput` | 200 ms |
| `sink.record` | **notification** | `{records: [Record]}` | none | none |
| `sink.flush` | request | none | `null` | 1 s |
| `adapter.start` | request | none | `null` (answer promptly; emit later) | 1 s |
| `actuator.caps` | request | none | `ActuatorCaps` | 1 s |
| `actuator.read` | request | `{target, knob}` | `Setting` or `null` | 1 s |
| `actuator.apply` | request | `{target, setting}` | `null` | 1 s |
| `actuator.reset` | request | `{target, knob, original}` | `null` | 1 s |

`[timeouts] call_ms` in plugin.toml replaces the default for every method except
`initialize`.

### `initialize`

```json
{"api_version": 1,
 "host": "lindesk-session",            // "lindeskd", "lindesk-session", "simulate", "conformance"
 "privilege": "user",
 "uid": 1000,                          // the session user (user hosts)
 "caps": {"cgroup_v2": true, "psi": true, "uclamp": true, "io_max": true, "devices": ["259:0"], "...": "..."},
 "config": {"webhelper_class": "background"}}   // plugin.toml [config] + [plugins.<name>] config
```

The answer is `InitializeResult`:

| Field | |
|---|---|
| `name` | must equal the manifest's name |
| `version` | |
| `api_version` | `1` |
| `kinds` | must be a subset of the manifest's `kinds` |
| `tables` | sinks: the tables to receive. When non-empty it replaces `[sink] tables`; empty means the manifest's list, or all |
| `actuator` | actuators: their `ActuatorCaps` |

If `api_version` is one you don't speak, or your config is invalid, reply with an error
and the host won't use the plugin.

## Plugin → host notifications

| Method | Params | |
|---|---|---|
| `log` | `{level, message}` | `level`: `trace`, `debug`, `info`, `warn`, `error`. Goes to the journal, tagged with the plugin |
| `metric` | a `Metric`: `{ts, name, value, labels?}` | into the `metrics` table. The host fills in `plugin` |
| `event` | an `Event` | adapters: `focus`, `interaction` |
| `samples` | `{samples: [Sample]}` | pressure samples outside a poll (a bare array is also accepted) |

Each host queue holds 16 384 items. A plugin that floods past that loses the excess, and
the drops are counted.

## Errors

| Code | Name | Host treats it as |
|---|---|---|
| -32700 | parse error | protocol error: **failure** |
| -32600 | invalid request | protocol error: **failure** |
| -32601 | method not found | "unsupported", a plain error |
| -32602 | invalid params | protocol error: **failure** |
| -32603 | internal error | protocol error: **failure** |
| -32000 | failed | a plain error: "couldn't do it this time" |
| -32001 | unsupported | a plain error: "not for me" |

**Failures** move the plugin into backoff (1 s, 2 s, … up to 60 s), and five within five
minutes disable it. So do timeouts, crashes and unparseable output. Plain errors are only
counted. Unknown methods must get `-32601`, which is how new methods can be added without
breaking old plugins.

## Data types

The schema has every field. These are the shapes you meet first:

| Type | Example |
|---|---|
| `Target` | `{"type":"unit","manager":{"kind":"user","uid":1000},"name":"app-cargo-9.scope"}`, `{"type":"unit","manager":{"kind":"system"},"name":"packagekit.service"}`, `{"type":"cgroup","path":"/background.slice"}`, `{"type":"process","pid":42}` |
| `Setting` | `{"knob":"cpu_weight","value":50}`, `{"knob":"cpu_idle","value":true}`, `{"knob":"uclamp_max","percent":40.0}`, `{"knob":"io_max","device":"259:0","limit":{"wbps":16777216}}`, `{"knob":"memory_high","bytes":null}`, `{"knob":"io_prio","class":"idle","level":7}` |
| `Knob` | `{"knob":"cpu_weight"}`, `{"knob":"io_max","device":"259:0"}` |
| `QosClass` | `critical`, `interactive`, `normal`, `background`, `batch` |
| `Level` | `green`, `yellow`, `red` |
| `Classification` | `{"class":"batch","confidence":0.9,"reason":"compiler"}` (`confidence` defaults to 1) |
| `Intent` | `{"target":…,"setting":…,"priority":0,"reason":"…"}` (`priority: 0` = the plugin's priority) |
| `PolicyOutput` | `{"intents":[…],"level":"yellow","metrics":[…]}` (`level` and `metrics` optional) |
| `Sample` | `{"ts":…,"scope":{"type":"system"},"resource":"io","some_avg10":12.5,"full_avg10":4.0,"some_delta_us":125000,"full_delta_us":40000,"interval_ms":1000}` |

**Events** are tagged by `kind`:

* `focus` `{ts, pid?, app_id?, target?, source}`
* `interaction` `{ts, pid?, target?, end, source}`
* `class_set` `{ts, target, class, source}`
* `level_changed` `{ts, from, to, policy}`
* `action_applied` `{ts, target, setting, provenance}`
* `action_reverted` `{ts, target, knob, reason}`
* `plugin_error` `{ts, plugin, message}`

> The level event's `kind` on the wire is **`level_changed`**. The `events` telemetry
> table stores it as `level`, so match both if you read either.

**Records** (sinks) are tagged by `table`, with the payload's fields flattened in:

* `{"table":"pressure", …Sample}`
* `{"table":"units","ts","target","class","foreground","usage","applied"}`
* `{"table":"events","kind":…, …Event}`
* `{"table":"metrics", …Metric}`

## Annotated session: a classifier

The [steam-games](https://github.com/dipankar/lindesk/tree/main/examples/plugins/python/steam-games)
example, as `lindesk-session` drives it. `→` is the host writing to the plugin's stdin and
`←` is the plugin writing to stdout. These are real lines; the comments are not part of
the protocol.

```text
→ {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"api_version":1,"caps":{"bpf":false,"cgroup_v2":true,"devices":["259:0"],"io_cost":false,"io_latency":false,"io_max":true,"io_weight_effective":false,"psi":true,"sched_ext":false,"systemd":true,"uclamp":true},"config":{"game_units":[],"webhelper_class":"background"},"host":"lindesk-session","privilege":"user","uid":1000}}
```
The handshake. `config` is plugin.toml's `[config]` with the admin's
`[plugins.steam-games] config` merged over it. The SDK checks `api_version` and runs the
`@lp.on_init` hooks; raising there would refuse to start.

```text
← {"jsonrpc":"2.0","id":1,"result":{"name":"steam-games","version":"0.1.0","api_version":1,"kinds":["classifier"]}}
```
Name and kinds must match plugin.toml. From now on, the host checks each new unit's
representative process against the manifest's `prefilter` (`exe` globs) and calls the
plugin only on a match. Firefox, your editor and the compositor never reach it.

```text
→ {"jsonrpc":"2.0","id":2,"method":"classifier.classify","params":{"proc":{"cgroup":"/user.slice/user-1000.slice/user@1000.service/app.slice/steam_app_1245620.scope","comm":"reaper","exe":"/home/me/.steam/debian-installation/ubuntu12_32/reaper","pid":48211,"slice":"app.slice","uid":1000,"unit":"steam_app_1245620.scope"}}}
← {"jsonrpc":"2.0","id":2,"result":{"class":"interactive","confidence":1.0,"reason":"Steam game scope steam_app_1245620.scope"}}
```
A game launched in its own scope. The answer arrived within 50 ms and its confidence is
at least `min_confidence` (0.5), so the unit becomes `interactive`. The host emits
`class_set` and `explain` shows "via classifier:steam-games".

```text
→ {"jsonrpc":"2.0","id":3,"method":"classifier.classify","params":{"proc":{"cgroup":"/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-steam-3020.scope","comm":"steamwebhelper","exe":"/home/me/.steam/debian-installation/ubuntu12_64/steamwebhelper","pid":3311,"slice":"app.slice","uid":1000,"unit":"app-gnome-steam-3020.scope"}}}
← {"jsonrpc":"2.0","id":3,"result":{"class":"background","confidence":1.0,"reason":"Steam's embedded browser"}}
```
Answering `null` would mean "no opinion": the next classifier by priority (the built-in
`rules`) would decide, and then the default `normal`.

```text
→ {"jsonrpc":"2.0","id":4,"method":"classifier.classify","params":{"bogus":true}}
← {"jsonrpc":"2.0","id":4,"error":{"code":-32602,"message":"classifier.classify: KeyError: 'proc'"}}
```
What conformance sends to check robustness. The plugin answers with an error and keeps
running. In production a `-32602` counts as a failure. A `KeyError` from *your* code looks
the same, which is why handlers should use `.get()` for optional fields.

```text
→ {"jsonrpc":"2.0","id":5,"method":"lindesk.conformance/no-such-method","params":null}
← {"jsonrpc":"2.0","id":5,"error":{"code":-32601,"message":"method not found: lindesk.conformance/no-such-method"}}
→ {"jsonrpc":"2.0","method":"shutdown","params":null}
```
Unknown methods get `-32601`. On `shutdown` (then EOF on stdin) the plugin exits with
status 0. The host waits 500 ms before sending SIGTERM.

## Manifest

`plugin.toml` is part of the contract too (`Manifest` in the schema). Unknown keys are
rejected.

```toml
[plugin]
name = "steam-games"          # letters, digits, - _ . ; must match `initialize`
version = "0.1.0"
api = 1                       # plugin API version
kinds = ["classifier"]        # sensor adapter classifier policy actuator sink
exec = "steam_games.py"       # relative to this directory, or absolute
args = []                     # optional
privilege = "user"            # user | system
description = "..."
enabled = true

[classifier]                  # only with kinds = [..."classifier"...]
priority = 50                 # default 10
min_confidence = 0.5
prefilter = { comm = [], exe = ["*wine*"], unit = [], app_id = [] }

[policy]
priority = 10

[sensor]
interval_ms = 1000

[sink]
tables = ["events"]           # pressure units events metrics; empty = all

[timeouts]
init_ms = 2000
call_ms = 50

[config]                      # free-form, handed over in `initialize`
```
