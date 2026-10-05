# lindesk architecture and shared contracts

This file is the source of truth for the interfaces *between* lindesk components. Change it
when you change one of these contracts.

## Components

| Component | Runs as | Role |
|---|---|---|
| `lindeskd` | root, system service | Single "brain". Probe, PSI triggers, plugin host (system plugins), policy, actuation of system units, iocost. Sends per-uid desired state to session agents. |
| `lindesk-session` | user, `systemd --user` service | Foreground adapters (X11, COSMIC built in; GNOME/KWin push over D-Bus), maps pid/app_id → user unit, applies user-unit settings via user systemd, hosts user plugins, writes the user's telemetry store. Runs the whole pipeline locally (user scope only) when no broker exists. |
| `lindeskctl` | user/root CLI | doctor, status/watch, explain, run/set, plugin tooling, simulate, query/data, bench. |
| `lindesk-mcp` | user, stdio or socket | MCP server for local agents (CONTROL-PLANE.md). |
| `lindesk-bench` | user CLI | Foreground latency probe, A/B harness. |
| `lindesk-telemetry-sink` | plugin process of each daemon | *lindesk-telemetry.* The `telemetry` sink plugin: writes the Vortex lake. |
| `lindesk-data` | CLI and oneshot (timer) | *lindesk-telemetry.* The lake's reader and maintainer (DuckDB): query, export, import-bench, compaction, rollups, retention, delete. |

## Packages and what links what

Two packages. **`lindesk`** (core) is everything above except the last two rows and links
**no DuckDB, no Vortex, no Arrow**. **`lindesk-telemetry`** (optional, `Depends: lindesk`)
ships `lindesk-data`, `lindesk-telemetry-sink` with two manifests
(`/usr/lib/lindesk/plugins/telemetry/`, `privilege = "system"`, for `lindeskd`;
`.../telemetry-user/`, `privilege = "user"`, for `lindesk-session`; both run
`/usr/lib/lindesk/lindesk-telemetry-sink`), the `lindesk-compact` system and user units
and timers, their presets and tmpfiles, and `lindesk-data(1)`. The layout is defined once
in `packaging/install.sh` (default and `--without-telemetry` = core, `--with-telemetry` = both,
`--telemetry-only` = the telemetry package) and checked by `packaging/check-layout.sh` against
`expected-files.txt` / `expected-files-telemetry.txt`.

Telemetry is a plugin like any other: the daemons discover `telemetry` from the plugin
directories, and a daemon without it runs exactly the same pipeline with no sink (the engine
then builds no records at all). The core tools reach the lake only by running
`lindesk-data` (found through `$LINDESK_DATA`, next to their own executable, or on `PATH`;
`lindesk_sys::telemetry`); without it they report `telemetry isn't installed: sudo apt
install lindesk-telemetry (or ./packaging/install.sh --with-telemetry)`, and
`lindeskctl doctor`/`status` and the daemons' status (`daemon.telemetry`) say "not
installed". The contract with `lindesk-data` is its command line: `--root`, `--config`,
`--backend`, `--json` reports, `query SQL --format json`, `export --table T --from MS --to
MS` (JSON lines, `ts` in epoch ms), `import-bench RESULTS.json`, `shell`; errors are
`lindesk-data: <message>` on stderr with exit 1.

### Cargo feature hygiene

Which binary links what (normal dependencies; dev-dependencies never reach a binary). CI
fails when a core binary's `cargo tree -e normal` contains a crate of the last four columns.

| Binary (package) | lindesk-store | lindesk-query | vortex / arrow | duckdb (bundled) | Notes |
|---|---|---|---|---|---|
| `lindeskd` (core) | – | – | – | – | zbus, tokio; built-in plugins have no sink |
| `lindesk-session` (core) | – | – | – | – | + x11rb, wayland (adapters) |
| `lindeskctl` (core) | – | – | – | – | + ratatui (`watch`); `query`/`data`/`simulate` run `lindesk-data` |
| `lindesk-mcp` (core) | – | – | – | – | + rmcp, axum; `query`/`recommendations` run `lindesk-data` |
| `lindesk-bench` (core) | – | – | – | – | stores runs with `lindesk-data import-bench` |
| `lindesk-telemetry-sink` (telemetry) | ✓ | – | ✓ | – | `lindesk-plugin` SDK; writer only |
| `lindesk-data` (telemetry) | ✓ | ✓ | ✓ | ✓ | the only DuckDB user |

There is no `embedded-telemetry` feature: an in-process sink would put Vortex back into the
daemons and give telemetry two code paths; the external plugin is the only one.

## Crates

| Crate | Depends on | Notes |
|---|---|---|
| `lindesk-plugin-api` | serde, schemars | **Stable public API.** Data model, traits, protocol, manifest. |
| `lindesk-core` | plugin-api | Pure: config, resolver, safety caps, reconciler state, cgroup path parsing, clocks. |
| `lindesk-sys` | core | Probe, cgroupfs, procfs, PSI, syscalls, block devices, iocost. Every function takes explicit roots (`/sys`, `/proc`) so tests run on fixtures. |
| `lindesk-host` | core | Plugin discovery/supervision, stdio transport, the `Engine` pipeline, conformance tests, scaffolding templates. |
| `lindesk-builtins` | core, sys, host | Built-in plugins (`policy/`, `classifier/`, `sensors/`, `actuators/`, `adapters/`). No sink: telemetry is the external `telemetry` plugin. |
| `lindesk-store` | plugin-api, arrow, vortex | Telemetry *writer* only (Vortex files, async low-priority writer thread). No DuckDB. Linked by `lindesk-telemetry-sink` and `lindesk-data` only. |
| `lindesk-query` | duckdb | Telemetry *reader and maintainer*: query, catalog, compact, rollup, prune, delete. Linked by `lindesk-data` only. |
| `lindesk-telemetry-sink` | plugin (SDK), store, sys | The `telemetry` plugin binary (lindesk-telemetry package). |
| `lindesk-data` | query, store | The lake tool binary (lindesk-telemetry package). |
| `lindesk-ipc` | zbus | D-Bus interfaces (below) and polkit checks. |
| `lindesk-plugin` | plugin-api | Rust SDK for external plugins. |

## Filesystem layout

| What | System (`lindeskd`) | User (`lindesk-session`) |
|---|---|---|
| Config | `/etc/lindesk/lindesk.toml` | `$XDG_CONFIG_HOME/lindesk/lindesk.toml` |
| Classifier rules | `/usr/share/lindesk/rules.d/*.toml`, then `/etc/lindesk/rules.d/*.toml` | then `$XDG_CONFIG_HOME/lindesk/rules.d/*.toml` |
| Plugins | `/usr/lib/lindesk/plugins/<name>/plugin.toml`, `/etc/lindesk/plugins/<name>/` (must be root-owned, not group/world-writable), then `[general] plugin_dirs` | `$XDG_DATA_HOME/lindesk/plugins/<name>/`, then the system dirs (only `privilege = "user"` plugins load), then `[general] plugin_dirs` |
| Telemetry plugin | `/usr/lib/lindesk/plugins/telemetry/` | `/usr/lib/lindesk/plugins/telemetry-user/` (same binary, `/usr/lib/lindesk/lindesk-telemetry-sink`) |
| Applied state | `/run/lindesk/state.json` | `$XDG_RUNTIME_DIR/lindesk/state.json` |
| Telemetry | `/var/lib/lindesk/telemetry/` (0750 root:lindesk) | `$XDG_STATE_HOME/lindesk/telemetry/` (0700) |
| DuckDB catalog | `/var/lib/lindesk/catalog.duckdb` | `$XDG_STATE_HOME/lindesk/catalog.duckdb` |
| Versioned changes | `/var/lib/lindesk/changes/` (`<state-dir>/changes/` when not root) | `$XDG_STATE_HOME/lindesk/changes/` |
| Calibration | `/var/lib/lindesk/calibration.json` (`<state-dir>/calibration.json` when not root) | `$XDG_STATE_HOME/lindesk/calibration.json` |

## D-Bus

### Session bus: `org.lindesk.QoS1`

Owned by `lindesk-session`. Object `/org/lindesk/QoS1`, interface `org.lindesk.QoS1`.
This is the API that desktop shells and applications use. The session bus only admits the
user, so there is no polkit here; `SetClass` additionally checks that `/proc/<pid>` belongs
to the agent's uid.

| Member | Signature | Notes |
|---|---|---|
| `SetForegroundProcess(pid)` | `i` → | Focus moved to a window owned by `pid` (GNOME extension, KWin script). |
| `SetForegroundApp(app_id)` | `s` → | Focus moved to an app known only by id (COSMIC-like shells). |
| `BeginInteraction(pid)` | `i` → | Starts/extends an interaction epoch. Coalesced: at most one event per pid per 100 ms. |
| `EndInteraction(pid)` | `i` → | Ends it early (never coalesced; resets the 100 ms window). |
| `SetClass(pid, class)` | `is` → | `critical|interactive|normal|background|batch`. Own processes only (`AccessDenied` otherwise). |
| `ReportMetric(name, value, labels)` | `sda{ss}` → | Free-form metric into the `metrics` table (`plugin = "dbus"`). |
| `Status()` | → `s` | Status JSON (below). |
| `Explain(what)` | `s` → `s` | Explanation JSON (below). `InvalidArgs` when `what` doesn't parse. |
| `RecentEvents(n)` | `u` → `s` | JSON array of the last `n` events (ring of 500, oldest first). |
| `SetPaused(paused, seconds)` | `bu` → `s` | Pause (observe-only: every applied setting reverted, sensing and decisions continue) or resume; `seconds > 0` auto-resumes. Returns the pause JSON `{paused, since_ms, until_ms, by}`. Own uid only. |
| `GetPaused()` | → `s` | The pause JSON. |
| control plane | | `PlanChange`, `ApplyChange`, `ConfirmChange`, `RevertChange`, `ListChanges`, `GetChange`, `ListProfiles`, `SetProfile`, `GetHardware`, `SwapReport` as in [CONTROL-PLANE.md §5](CONTROL-PLANE.md), user scope (the user's `lindesk.toml`, store `$XDG_STATE_HOME/lindesk/changes`). Callers whose uid isn't the agent's get `AccessDenied`. |
| signal `LevelChanged(level)` | `s` | `green|yellow|red`. |
| signal `Events(json)` | `s` | JSON array of the events of one engine step (at most 100 per signal). |
| signal `ChangeEvent(json)` | `s` | ChangeSet JSON of every changeset transition (applied, confirmed, reverted, expired, failed). |

In connected mode every focus/interaction/class call (and every built-in adapter event) also
goes to the broker (`ReportForeground` with `pid = 0` for app ids, `Interaction`,
`SetClass`), fire-and-forget.

### System bus: `org.lindesk.Broker1`

Owned by `lindeskd`. Object `/org/lindesk/Broker1`, interface `org.lindesk.Broker1`.
Callers are identified by their bus credentials (uid, pid). Root passes every check. polkit
is asked non-interactively unless the call has `ALLOW_INTERACTIVE_AUTHORIZATION` set.

| Member | Signature | Authorization |
|---|---|---|
| `RegisterSession()` | → `s` | none; the caller's uid and unique name are recorded and the user's units become watched. Returns the current `SessionState` JSON. |
| `ReportForeground(pid, app_id)` | `is` → | `pid > 0`: the pid must belong to the caller (`/proc/<pid>` owner), else polkit `org.lindesk.set-class-any`. `pid <= 0`: `app_id` is resolved among the caller's own units only. |
| `Interaction(pid, end)` | `ib` → | as `ReportForeground` with a pid |
| `SetClass(pid, class)` | `is` → | own pid: polkit `org.lindesk.set-class-own`; other users' pids: `org.lindesk.set-class-any` |
| `GetStatus()` | → `s` | none |
| `Explain(what)` | `s` → `s` | none |
| `RecentEvents(n)` | `u` → `s` | none; **filtered by caller**: root gets every event, other users the system-scope events plus the events about their own units and processes |
| `Reload()` | → | polkit `org.lindesk.manage` |
| `SetPaused(paused, seconds)` | `bu` → `s` | polkit `org.lindesk.manage`; as on QoS1. A paused broker relays `"dry_run": true` to session agents, so user units are released too. |
| `GetPaused()` | → `s` | none |
| `PlanChange`, `ListChanges`, `GetChange`, `ListProfiles`, `GetHardware`, `SwapReport` | see [CONTROL-PLANE.md §5](CONTROL-PLANE.md) | none (no side effects) |
| `ApplyChange`, `ConfirmChange`, `RevertChange`, `SetProfile` | see [CONTROL-PLANE.md §5](CONTROL-PLANE.md) | polkit `org.lindesk.manage`; system scope (`/etc/lindesk/lindesk.toml` = `--config`, kernel tunables) |
| `Calibrate()` / `CalibrateWith(options_json)` | → `s` / `s` → `s` | polkit `org.lindesk.manage`. Runs `lindesk_sys::calibrate::run` on a helper thread (one at a time; `Failed` while another runs), saves it, attaches it to `HostCaps.hardware.latency` and emits `calibration.done`; returns the `LatencyMap` JSON. Options: `quick` (`CalibrateRequest`), and broker extensions `force` (default true: a busy machine gives a map flagged `noisy`) and `storage` (default true; false skips the disk phase). Also in dry-run (it measures, it changes nothing). |
| signal `UserState(uid, json)` | `us` | **Unicast** to each registered agent of `uid` (other users never receive it). JSON `SessionState` (`lindesk-ipc`): `{"uid":..., "level": ..., "desired": [{"target":..., "setting":..., "provenance":...}], "dry_run": bool}`: the complete desired set for that user's units. Sent when it changes and every 30 s as a heartbeat. |
| signal `Events(json)` | `s` | JSON array of events of one broker step (at most 100 per signal). **System-scope events are broadcast; events about one user's units or processes are unicast to that user's registered session agents only** (below). |
| signal `ChangeEvent(json)` | `s` | Broadcast. ChangeSet JSON of every system-scope changeset transition. |

`LINDESK_INSECURE_NO_POLKIT=1` makes `lindeskd` skip polkit (pid ownership checks remain).
It exists for tests on private buses and is **ignored when `lindeskd` runs as root**.

**Event privacy.** The broker sees every user's units, so its events are split by audience
(`event_audience` in `lindeskd/src/broker.rs`): an event whose target is a user unit
(`user/<uid>:…`), a cgroup under `user-<uid>.slice`, or a process of that uid (`focus`,
`interaction`, `class_set`, `action_applied`, `action_reverted`) belongs to that uid; a
process owned by root, a system unit, `level`, `plugin_error` and daemon notices
(`profile.changed`, `change.*`, `calibration.done`, …) are system scope; an event whose
owner can't be determined (unresolved app id, vanished pid) is root-only. The `Events`
signal carries one audience per emission: system scope is broadcast, a user's events go
unicast (D-Bus destination set) to each registered session agent of that uid, so neither
other users nor unregistered clients of the same user receive them. `RecentEvents` applies
the same rule to the caller's uid (root sees everything). The session agent's own `Events`
on the session bus only ever contains its user's events.

**Errors of the control-plane members** (both buses): a guardrail refusal (unknown key,
out-of-range value, rate limit, ttl required, revert conflict, invalid state) is
`org.lindesk.Error.Rejected` with the human-readable reasons as the message (lindesk-mcp
maps it to `rejected`); an unknown changeset id is `org.lindesk.Error.NotFound`; other
failures are `org.lindesk.Error.Failed`; malformed `ops_json` is
`org.freedesktop.DBus.Error.InvalidArgs`; authorization failures are
`org.freedesktop.DBus.Error.AccessDenied`. An apply whose writes failed (and were rolled
back) is not an error: it returns the changeset with `status: "failed"` and `error`.

A session agent is unregistered when its unique name leaves the bus; when it was the user's
last agent the broker stops watching that user's units and *forgets* (does not revert) the
relayed settings: the agent restores its own changes.

### JSON payloads

**Event** (each element of `Events` and `RecentEvents`): the `Event` enum of
`lindesk-plugin-api` serialised with its `"kind"` tag (`focus`, `interaction`, `class_set`,
`level`, `action_applied`, `action_reverted`, `plugin_error`, `notice`) plus `"seq"`: a number that
increases by one per event for the lifetime of the daemon process, so a client can merge
`RecentEvents` backfill with the live signal and detect gaps. Dry-run `action_applied`
events carry a provenance reason starting with `"dry run: "`. Daemon notices (`kind:
"notice"`, `name`): `requirements.warning` (one per start-up warning, `data` = the
requirement), `hardware.power` (`data: {from, to}` when AC/battery changes),
`setup.desktop_integration` (`data: {action, desktop}`), `lindesk.paused` (`data: {until_ms, by,
restored}`), `lindesk.resumed` (`data: {by, paused_since_ms}`; `by: "timer"` after `--for`), `profile.changed` (`data: {from,
to, reason, pinned}`), `calibration.done` (`data` = the `LatencyMap`), `change.applied`,
`change.confirmed`, `change.reverted`, `change.expired`, `change.failed` (`data` = the
changeset).

**Status** (`Status`, `GetStatus`):

```json
{
  "status":   { /* lindesk_host::Status: now, host, level, dry_run, foreground, protected, yielding, plugins */ },
  "daemon": {
    "name": "lindeskd" | "lindesk-session",
    "version": "0.1.0", "pid": 1234, "uid": 0,
    "mode": "broker" | "connected" | "standalone" | "starting",
    "dry_run": false,
    "state": "on" | "paused",
    "paused": {"paused": false, "since_ms": null, "until_ms": null, "by": null},
    "telemetry": {"installed": true, "enabled": true, "state": "running", "dir": "…"},
    "started": 1759560000000,            // ms since the epoch
    "uptime_s": 12.5,
    "steps": 50,
    "degraded": ["no PSI: …"],           // why the daemon runs with less than everything
    "rules": "system" | "embedded" | "configured", // where the vendor rules come from
    "profile": {"active": "x86-hybrid", "setting": "auto", "pinned": false, "reason": "x86_64 hybrid CPU",
                "matched": ["x86-hybrid"],    // profiles whose [match] holds now, best first
                "source": "local" | "broker"}, // lindesk-session only; "broker" adds "local": {…}
    // lindeskd only:
    "sessions": [1000],                  // uids with a registered agent
    "support_level": "FULL", "backend": "systemd",
    "iocost_enabled": ["259:0"],         // devices lindeskd switched iocost on for
    "conflicts": ["system76-scheduler"], // running conflicting daemons
    "relayed": {"1000": 3},              // relay-table entries per uid
    // lindesk-session only:
    "forced_standalone": false,
    "broker": {"connected": true, "level": "green", "desired": 3, "dry_run": false, "updated": 1759560000000},
    "adapters": [{"name": "x11", "connected": true}]
  },
  "pressure": {
    "system": { "cpu": P, "io": P, "memory": P },
    "foreground": null | { "target": "user/1000:app-….scope", "cgroup": "/user.slice/…", "cpu": P, "io": P, "memory": P }
  },
  "epoch": { "active": true | false | null, "source": "threshold" | "broker" | "none" },
  "hardware": { /* lindesk_plugin_api::Hardware */ },
  "requirements": [ /* warnings from the startup requirements check */ ],
  "text": "human rendering (what `lindeskctl status` prints)"
}
```

`P` = `{"some": 0.12, "full": 0.0 | null, "some_avg10": 11.9, "full_avg10": 0.0 | null,
"interval_ms": 1000, "ts": …}`: `some`/`full` are stall ratios (0..1) over the latest sample
interval, `*_avg10` the kernel's percentages. A resource without a sample is absent.
`epoch.active` is `threshold`'s interaction-epoch flag; a connected agent derives it from the
relayed reasons (`source: "broker"`); `null` means unknown.

**Explanation** (`Explain`): `lindesk_host::Explanation` (`target`, `known`, `proc`, `class`,
`foreground`, `dry_run`, `settings[{knob, value, original, provenance, since}]`, `notes`)
plus `"query"` (the `what` as given) and `"text"` (the human rendering). `what` is a pid
(`1234` or `pid:1234`), a target in `Target`'s display form (`system:x.service`,
`user/1000:app-….scope`, `cgroup:/…` or a bare `/…` path), or a bare unit name (the session
agent looks in its own manager; the broker tries the system manager, then registered users).
A pid that maps to no managed unit yields `known: false` with target `pid:N`.

## Daemons

### Threads

Each daemon runs the `Engine` on a dedicated thread (`lindeskd/src/common/engine_thread.rs`,
shared with the session agent through `#[path]`): `poll(2)` on a command eventfd plus the
sensors' wake fds (PSI triggers) with a `[general] tick` timeout → drain commands → `step` →
publish (`Arc<RwLock<Published>>`: status JSON, level, per-uid states) → notifications on a
tokio channel (level changes, `UserState`, `Events`). Steps happen every tick, on a wake fd,
and right after event-like commands, never closer than 20 ms. The blocking zbus clients of
the systemd actuator and `SysEnvironment` therefore never run inside tokio. D-Bus servers,
signal handling (`SIGTERM`/`SIGINT` stop, `SIGHUP` reloads), the watchdog and the heartbeat run
on a 2-worker tokio runtime. Every 30 s the engine thread runs daemon maintenance (power
source re-probe → `Engine::set_caps`).

### `lindeskd`

`lindeskd [--config /etc/lindesk/lindesk.toml] [--state-dir /run/lindesk] [--telemetry-dir DIR]
[--dry-run] [--log-level FILTER] [--restore] [--no-dbus]`

Startup: open the system change store and `recover()` interrupted changesets (before the
config is read), requirements check (failures exit 78), probe, warn about running
conflicting daemons, iocost (below), load a saved calibration into `HostCaps`, select the
profile, engine with `psi`, `cgroup-stat`, `rules`, `threshold`, the `relay` actuator, the
backend's actuators (normally `systemd` on the system bus, then `proc`) and discovered
system plugins (with lindesk-telemetry installed, `telemetry`; the lake directory is then
made 0750 group `lindesk`); load `state.json`; first step; `sd_notify(READY=1)` after
the bus name is owned. The engine asks the available actuators whose caps cover the
target's kind and the knob in priority order and moves on to the next one when an actuator
answers `Unsupported` (the relay for anything but user units, `systemd` for units of
another manager); `Unsupported` is not counted as a plugin error. `WATCHDOG=1` is sent at half of
`WATCHDOG_USEC` only while the engine loop is alive. On `SIGTERM`: `Engine::shutdown()`
restores everything, then iocost is restored.

**iocost**: `[io] iocost = "auto"` enables iocost on managed devices (`[io] devices`, else the
disks behind `/` and `/home`) whose scheduler is `none`; BFQ and other schedulers are left
alone. `"on"` enables it on every managed device; `"off"` never touches it. The previous
`io.cost.qos` line is written to `<state-dir>/iocost.json` *before* enabling and put back on
shutdown and `--restore` (a device that had no line is set to `enable=0`). A dry run changes
nothing (it restores leftovers of a crashed real run).

**User units**: user units are never written by the broker. Once a user's agent registers,
`SysEnvironment` also watches that user's `session.slice`, `app.slice`, `background.slice`,
`lindesk-batch.slice` and their units. The `relay` actuator accepts every knob on
`user/<uid>:…` targets and only records them (`read` = unknown); after each step the broker
builds each registered user's `SessionState` from the engine's applied entries for that uid
(authoritative also after crash recovery and in dry-run) and sends it if it changed.
`"dry_run": true` in a `SessionState` means the broker is only observing; a non-dry-run
agent then applies nothing it relays.

### `lindesk-session`

`lindesk-session [--config $XDG_CONFIG_HOME/lindesk/lindesk.toml] [--state-dir
$XDG_RUNTIME_DIR/lindesk] [--telemetry-dir DIR] [--dry-run] [--standalone] [--restore]
[--no-adapters] [--log-level FILTER]`

Modes (a pure state machine in `lindesk-session/src/mode.rs`):

| Mode | When | Active |
|---|---|---|
| connected | `RegisterSession` succeeded | `broker` relay policy (priority 50, provenance plugin `broker` with reason `<plugin>: <reason>`, shown by explain as `broker: <plugin>: <reason>`; the broker's level) |
| standalone | no broker, it vanished, registration failed, or `--standalone` | `psi` (2 s trigger windows unless configured), `threshold` |

Both modes run `cgroup-stat`, `rules`, the `systemd` actuator on the user manager and
user plugins (with lindesk-telemetry installed, `telemetry` into the user lake). `threshold` and `psi` are registered once
behind a gate (the `Registry` can't unregister); a gate that reopens re-initialises its plugin.
Switching is declarative: the engine reverts whatever the newly active policies don't ask for,
so nothing stale survives a switch. The agent follows `NameOwnerChanged` for
`org.lindesk.Broker1`: a new owner → register (stay in the current mode until it answers);
no owner → standalone. While standalone it retries every 30 s if a broker is present.
The bus name `org.lindesk.QoS1` is requested only after the engine's first step
(`Type=dbus` readiness). Built-in adapters (`adapters::detect`) are initialised with
`[plugins.<name>].config`; their events wake the engine and are forwarded to the broker when
connected. `Unsupported` adapters are skipped with an info log.

### Pause (both daemons)

`Engine::set_paused(true)` restores every applied setting and then runs the pipeline as in
dry-run (action events say `paused: …`); `set_paused(false)` forgets what was only recorded
so the next step applies for real. The engine thread owns the pause: a timed pause ends on
the first loop iteration after `until_ms`; the state is persisted (`<lib>/paused.json` for
lindeskd, `$XDG_STATE_HOME/lindesk/paused.json` for the agent) and reloaded at start-up
while it's still due (an indefinite pause until `resume`).

### On, off and the install-time state

`/var/lib/lindesk/install-state.json` (`lindesk_changes::install_state`) records how
lindesk was installed and the state of every conflicting daemon's unit before lindesk; it
is written by `install.sh` and the package scripts (`lindeskctl setup snapshot`), or by
lindeskd on its first start, and never overwritten (later installs append `upgrades`).
`lindeskctl off` sets `"lindesk_enabled": false` (with a `toggles` entry); package scripts
and `install.sh` then leave lindesk stopped until `lindeskctl on`. Conflicting daemons are
only ever changed through `service_state` changesets (allow-listed units, Medium risk, the
previous enabled/active state recorded), so `lindeskctl changes revert-all` and uninstall
restore them; `lindeskctl setup conflicts --reconcile` compares with the snapshot for
changes made outside lindesk.

### `--restore` (both daemons)

Loads `state.json` into an engine that has only the actuators of the probed backend, reverts
every entry, rewrites the file with what could not be reverted, and (lindeskd) restores the
saved iocost lines. Idempotent; a missing or empty state is "nothing to do" (exit 0); exit 1
when something could not be restored; aborts after 25 s. Works without a running daemon
(`ExecStopPost=`).

### Zero configuration

Nothing has to exist besides the binaries:

* a missing config file means the defaults (not dry-run, `threshold` on, `iocost = "auto"`,
  telemetry on with the default retention when the `telemetry` plugin is installed);
* the state directory, the telemetry lake when the plugin is installed (system: 0750,
  group `lindesk` when it exists, else root) and the runtime rule directory are created;
* when `/usr/share/lindesk/rules.d` is missing (e.g. a cargo build) the rules shipped in
  `packaging/config/rules.d` are compiled into the binaries, written to
  `<state-dir>/rules.d.builtin` and used in its place (then `/etc/lindesk/rules.d`, then the
  user's). Status `daemon.rules` says which. Profiles are embedded by `lindesk-core` and
  layered with `/usr/share`, `/etc` and the user's `profiles.d`;
* one start-up line sums it up: `lindeskd ready: level FULL, profile default, <hardware>, mode
  broker, rules system` / `lindesk-session ready: …, adapters [x11], mode standalone, …`;
* optional pieces degrade instead of failing, listed in `daemon.degraded`: no PSI, no
  systemd or no user manager, no foreground adapter, **no session bus** (the agent keeps
  running without `org.lindesk.QoS1`), `--no-dbus`, not root.

**First-run desktop integration** (`lindesk-session`, `[plugins.desktop-integration] enabled`,
default true, skipped in dry-run): on GNOME, when the `lindesk@lindesk.org` extension is
installed but disabled, `gnome-extensions enable` it; on KDE, when the `lindesk` KWin script
is installed, `kwriteconfig6 --file kwinrc --group Plugins --key lindeskEnabled true` and a KWin
`reconfigure`. Done at most once per user, recorded in `$XDG_STATE_HOME/lindesk/first-run.json`
(a user who disables it later stays disabled); nothing is recorded while the integration
isn't installed. Missing tools are not errors.

**First-run calibration** (`common/calibration.rs`): the broker (and a standalone agent,
quick mode) waits for the first idle period (global PSI `some` < 5 % for 2 min, ≥ 3 min
after start, not on battery, no interaction epoch, focus or interaction event) and then
calibrates once (`/var/lib/lindesk/calibration.json`, `$XDG_STATE_HOME/lindesk/calibration.json`)
with `lindesk_sys::calibrate::run` on a helper thread. The run can't be interrupted, so a run
during which the user became active, the machine went on battery or pressure rose is
discarded and retried at the next idle period (3 attempts per daemon run). The result goes
into `HostCaps.hardware.latency` and a `calibration.done` notice; a saved result is loaded at
start-up and suppresses the first run. Not in dry-run. It shares a "busy" flag with the
broker's `Calibrate`, so the two never run at once.

**Profiles** (`common/profiles.rs`): both daemons load a `ProfileSet` (built-ins, then
`default_dirs`: `/usr/share` and `/etc` for the broker, plus the user's for the agent) at
start-up and again on every reload. `[general] profile = "auto"` (default) re-evaluates
`ProfileSelector::update` after every step against a `ProfileContext` from
`HostCaps.hardware` and the foreground unit's app id (10 s dwell); `[general] profile =
"<name>"` pins one (no auto-selection). A switch reloads the engine with `base ⊕ profile
chain` (built-in sensors/classifiers/policies whose `[plugins.<name>] config` changed are
re-initialised) and pushes `profile.changed` into the engine's events (so it reaches
`Events`, the lake and `lindeskctl events`). A reload (SIGHUP, `Reload`, or a versioned
change to the config file, e.g. `SetProfile`) re-runs the selection immediately, so a pin or
unpin takes effect at once. Status `daemon.profile` is `{active, setting, pinned, reason,
matched}`. In connected mode the broker's profile is the one that matters: the broker sends
its `daemon.profile` in every `SessionState` (`"profile"`), the agent pauses its own
auto-selection, and its status shows the broker's (`"source": "broker"`, own under
`"local"`); `ListProfiles` marks the broker's as active.

**Versioned changes** (`common/control.rs`, [CONTROL-PLANE.md §4](CONTROL-PLANE.md)): each
daemon owns one `ChangeStore` (broker: system scope, `/var/lib/lindesk/changes`, editing
`--config`; agent: user scope, `$XDG_STATE_HOME/lindesk/changes`, editing its config file),
opened and `recover()`ed before the config is read and before D-Bus is served. Store
operations run on tokio's blocking pool (they `fsync`), never on the reactor or the engine
thread. The guardrail context has the profile names (`with_profiles`), the hardware from
`HostCaps` and a config validator (`lindesk_builtins::validate::config`: `[[override]]`
globs, and the configs of the built-in `threshold`, `psi`, `rules`, `systemd`, `x11`,
`cosmic` plugins, so an unknown plugin key or out-of-range value is rejected). After a
changeset that touched the config file (apply, revert, expiry) the daemon reloads config and
profiles. A dead-man task sleeps until `next_expiry()` (at most 60 s, re-armed after every
apply/confirm/revert) and calls `expire_due`. Every transition is sent as `ChangeEvent` and
pushed as a `change.*` notice into the engine's events. Authorship: the caller's uid and
unique bus name, plus `origin`/`client` from the ops envelope (`mcp` → `Mcp`, `cli` or
`lindeskctl` → `Cli`, anything else → `Dbus`).

**Hardware**: every 30 s the daemons re-detect the power source and, on a change, update the
engine's `HostCaps` (`Engine::set_caps`) and emit `hardware.power`.

### Extending the D-Bus APIs

Both interfaces are thin: each method maps to one engine command (`Command` in
`engine_thread.rs`; daemon-specific ones in `BrokerExt` / `SessionExt`) and awaits its reply
on a oneshot channel, or reads the `Published` snapshot. New members are one `#[interface]`
method plus, when they need the engine, one command variant handled on the engine thread.
The control-plane members (CONTROL-PLANE.md §5) are implemented once in
`common/control.rs` (`Control`, `CtlError`) and called from both interfaces after the
interface's own authorization (polkit on the system bus, same-uid on the session bus);
they use the generic `Hardware`, `Profiles` and `Calibrated` commands and `Reload`.

### Operational notes

* The state directory must be on tmpfs (the defaults `/run/lindesk`, `$XDG_RUNTIME_DIR/lindesk`
  are): the applied state is rewritten on every change, and on a saturated ext4 disk a
  rename-over can stall the engine thread for seconds.
* Sinks run on the engine thread. For an external sink (`telemetry`) `record` is a
  `sink.record` *notification*: the host serialises it into a bounded per-plugin queue
  (`external::STDIN_QUEUE` lines) drained by a writer thread that owns the child's stdin,
  so a plugin that stops reading can never block a step: requests then fail as timeouts and
  notifications are dropped (`PluginStats::dropped`). The `telemetry` plugin itself only
  parses and enqueues on its main thread; its writer thread at idle I/O priority and nice 10
  encodes, fsyncs and renames, dropping the oldest rows (counted in the
  `telemetry_dropped_rows` metric) when it cannot keep up. `sink.flush` (only at shutdown,
  manifest `call_ms = 6000`) waits at most `flush_timeout` (default 5 s). The daemons add
  no flush of their own: at shutdown the engine thread runs `Engine::shutdown` (restore,
  one `flush()` per sink, plugin shutdown). `[telemetry]` keys are applied to
  `[plugins.telemetry.config]` (`dir`, `flush`, `mode`); `[plugins.parquet-store]` (the
  former built-in sink) is folded into it; `enabled = false` keeps the plugin from starting.
  Status JSON `daemon.telemetry` is `{installed, enabled, state, dir}`. Steps or bookkeeping
  slower than 1 s are logged as `slow engine step` with per-stage timings.

### Logging

journald when `JOURNAL_STREAM` is set (syslog identifier = binary name), else stderr. Filter:
`--log-level`, else `RUST_LOG`, else `info`. Every applied/reverted action is logged at info
(target `lindesk::action`), resolver notes at debug.

Plugin log lines (external plugins' `log` notifications and stderr, re-logged by
`lindesk-host`, and built-ins logging with a `plugin` field) carry the tracing field
`plugin = <name>`. `tracing-journald` (0.3) upper-cases field names and prefixes every
field except `message` with its field prefix, which the daemons leave at the default `F`
(`common/logging.rs` sets it explicitly), so the journal field is **`F_PLUGIN=<name>`**
(also `F_SOURCE` for the stream); `TARGET`, `CODE_FILE` and `CODE_LINE` are unprefixed. Filter
with `journalctl -u lindeskd F_PLUGIN=<name>` (`--user -u lindesk-session` for user
plugins); `lindeskctl plugin logs` matches `F_PLUGIN` (and `PLUGIN`, for a build without
the prefix).

## Intent priority

An intent with `priority = 0` inherits its plugin's priority (manifest `[policy] priority`, or the
built-in registration). `[plugins.<name>] priority` in the config overrides every intent of that
plugin. Admin `[[override]]`s always win. A user host (`lindesk-session`) rejects intents that
target anything other than its own user's units.

## Plugin protocol

See `crates/lindesk-plugin-api/src/protocol.rs` and `lindeskctl plugin schema`.
Newline-delimited JSON-RPC 2.0 over stdio; the host sends `initialize` first.

## Safety invariants (enforced in `lindesk-core::safety`, never in plugins)

* No realtime CPU or I/O classes. Nothing below the configured floors (`io.max`, `memory.high`, `uclamp.max`).
* Critical workloads can be boosted but never throttled.
* All changes are runtime-only and recorded with their original value, then restored on exit, on crash recovery, or with `lindeskctl restore`.
