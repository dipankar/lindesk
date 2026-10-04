# lindesk architecture and shared contracts

This file is the source of truth for the interfaces *between* lindesk components. Change it
when you change one of these contracts.

## Components

| Component | Runs as | Role |
|---|---|---|
| `lindeskd` | root, system service | Single "brain". Probe, PSI triggers, plugin host (system plugins), policy, actuation of system units, iocost. Sends per-uid desired state to session agents. |
| `lindesk-session` | user, `systemd --user` service | Foreground adapters (X11, COSMIC built in; GNOME/KWin push over D-Bus), maps pid/app_id → user unit, applies user-unit settings via user systemd, hosts user plugins, writes the user's telemetry store. Runs the whole pipeline locally (user scope only) when no broker exists. |
| `lindeskctl` | user/root CLI | doctor, status/watch, explain, run/set, plugin tooling, simulate, query/data, bench. |
| `lindesk-data` | oneshot (timer) | Telemetry compaction, rollups, retention. |

## Crates

| Crate | Depends on | Notes |
|---|---|---|
| `lindesk-plugin-api` | serde, schemars | **Stable public API.** Data model, traits, protocol, manifest. |
| `lindesk-core` | plugin-api | Pure: config, resolver, safety caps, reconciler state, cgroup path parsing, clocks. |
| `lindesk-sys` | core | Probe, cgroupfs, procfs, PSI, syscalls, block devices, iocost. Every function takes explicit roots (`/sys`, `/proc`) so tests run on fixtures. |
| `lindesk-host` | core | Plugin discovery/supervision, stdio transport, the `Engine` pipeline, conformance tests, scaffolding templates. |
| `lindesk-builtins` | core, sys, store | Built-in plugins (`policy/`, `classifier/`, `sensors/`, `actuators/`, `adapters/`, `sinks/`). |
| `lindesk-store` | plugin-api, arrow/parquet | Telemetry *writer* only. No DuckDB. |
| `lindesk-query` | duckdb | Telemetry *reader and maintainer*: query, catalog, compact, rollup, prune, delete. |
| `lindesk-ipc` | zbus | D-Bus interfaces (below) and polkit checks. |
| `lindesk-plugin` | plugin-api | Rust SDK for external plugins. |

## Filesystem layout

| What | System (`lindeskd`) | User (`lindesk-session`) |
|---|---|---|
| Config | `/etc/lindesk/lindesk.toml` | `$XDG_CONFIG_HOME/lindesk/lindesk.toml` |
| Classifier rules | `/usr/share/lindesk/rules.d/*.toml`, then `/etc/lindesk/rules.d/*.toml` | then `$XDG_CONFIG_HOME/lindesk/rules.d/*.toml` |
| Plugins | `/usr/lib/lindesk/plugins/<name>/plugin.toml`, `/etc/lindesk/plugins/<name>/` (must be root-owned, not group/world-writable) | `$XDG_DATA_HOME/lindesk/plugins/<name>/`, then the system dirs (only `privilege = "user"` plugins load) |
| Applied state | `/run/lindesk/state.json` | `$XDG_RUNTIME_DIR/lindesk/state.json` |
| Telemetry | `/var/lib/lindesk/telemetry/` (0750 root:lindesk) | `$XDG_STATE_HOME/lindesk/telemetry/` (0700) |
| DuckDB catalog | `/var/lib/lindesk/catalog.duckdb` | `$XDG_STATE_HOME/lindesk/catalog.duckdb` |

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
| signal `LevelChanged(level)` | `s` | `green|yellow|red`. |
| signal `Events(json)` | `s` | JSON array of the events of one engine step (at most 100 per signal). |

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
| `RecentEvents(n)` | `u` → `s` | none |
| `Reload()` | → | polkit `org.lindesk.manage` |
| signal `UserState(uid, json)` | `us` | **Unicast** to each registered agent of `uid` (other users never receive it). JSON `SessionState` (`lindesk-ipc`): `{"uid":..., "level": ..., "desired": [{"target":..., "setting":..., "provenance":...}], "dry_run": bool}`: the complete desired set for that user's units. Sent when it changes and every 30 s as a heartbeat. |
| signal `Events(json)` | `s` | Broadcast. JSON array of the events of one broker step, system and all users' units (at most 100 per signal). |

`LINDESK_INSECURE_NO_POLKIT=1` makes `lindeskd` skip polkit (pid ownership checks remain).
It exists for tests on private buses and is **ignored when `lindeskd` runs as root**.

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
`setup.desktop_integration` (`data: {action, desktop}`).

**Status** (`Status`, `GetStatus`):

```json
{
  "status":   { /* lindesk_host::Status: now, host, level, dry_run, foreground, protected, yielding, plugins */ },
  "daemon": {
    "name": "lindeskd" | "lindesk-session",
    "version": "0.1.0", "pid": 1234, "uid": 0,
    "mode": "broker" | "connected" | "standalone" | "starting",
    "dry_run": false,
    "started": 1759560000000,            // ms since the epoch
    "uptime_s": 12.5,
    "steps": 50,
    "degraded": ["no PSI: …"],           // why the daemon runs with less than everything
    "rules": "system" | "embedded" | "configured", // where the vendor rules come from
    "profile": {"active": "x86-hybrid", "setting": "auto", "pinned": false, "reason": "x86_64 hybrid CPU"},
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

Startup: requirements check (failures exit 78), probe, warn about running conflicting
daemons, iocost (below), engine with `psi`, `cgroup-stat`, `rules`, `threshold`, the `relay`
actuator (wrapping the backend's unit actuator, normally `systemd`, as its fallback for
system units; the engine picks actuators by target kind only), the other backend actuators,
`parquet-store` and discovered system plugins; load `state.json`; first step;
`sd_notify(READY=1)` after the bus name is owned. `WATCHDOG=1` is sent at half of
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
| connected | `RegisterSession` succeeded | `broker` relay policy (priority 50, reason `broker: <plugin>: <reason>`, the broker's level) |
| standalone | no broker, it vanished, registration failed, or `--standalone` | `psi` (2 s trigger windows unless configured), `threshold` |

Both modes run `cgroup-stat`, `rules`, the `systemd` actuator on the user manager,
`parquet-store` into the user lake and user plugins. `threshold` and `psi` are registered once
behind a gate (the `Registry` can't unregister); a gate that reopens re-initialises its plugin.
Switching is declarative: the engine reverts whatever the newly active policies don't ask for,
so nothing stale survives a switch. The agent follows `NameOwnerChanged` for
`org.lindesk.Broker1`: a new owner → register (stay in the current mode until it answers);
no owner → standalone. While standalone it retries every 30 s if a broker is present.
The bus name `org.lindesk.QoS1` is requested only after the engine's first step
(`Type=dbus` readiness). Built-in adapters (`adapters::detect`) are initialised with
`[plugins.<name>].config`; their events wake the engine and are forwarded to the broker when
connected. `Unsupported` adapters are skipped with an info log.

### `--restore` (both daemons)

Loads `state.json` into an engine that has only the actuators of the probed backend, reverts
every entry, rewrites the file with what could not be reverted, and (lindeskd) restores the
saved iocost lines. Idempotent; a missing or empty state is "nothing to do" (exit 0); exit 1
when something could not be restored; aborts after 25 s. Works without a running daemon
(`ExecStopPost=`).

### Zero configuration

Nothing has to exist besides the binaries:

* a missing config file means the defaults (not dry-run, `threshold` on, `iocost = "auto"`,
  telemetry on with the default retention);
* the state directory, the telemetry lake (system: 0750, group `lindesk` when it exists,
  else root) and the runtime rule directory are created;
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

**First-run calibration**: the broker (and a standalone agent, quick mode) waits for the first
idle period (global PSI `some` < 5 % for 2 min, ≥ 3 min after start, on AC, no interaction
or focus change) and then calibrates once (`/var/lib/lindesk/calibration.json`,
`$XDG_STATE_HOME/lindesk/calibration.json`) with `lindesk_sys::calibrate::run` on a helper
thread. A run during which the user became active or pressure rose is discarded and retried
at the next idle period (3 attempts per daemon run). The result goes into
`HostCaps.hardware.latency` and a `calibration.done` notice; a saved result is loaded at
start-up. Not in dry-run.

**Profiles**: `[general] profile = "auto"` (default) re-evaluates `lindesk_core::profile` on
every step (power, hardware, foreground app id, 10 s dwell); a switch reloads the engine with
`base ⊕ profile chain` (built-in sensors/classifiers/policies whose `[plugins.<name>] config`
changed are re-initialised) and emits `profile.changed`. Status `daemon.profile` shows it.

**Hardware**: every 30 s the daemons re-detect the power source and, on a change, update the
engine's `HostCaps` (`Engine::set_caps`) and emit `hardware.power`.

### Extending the D-Bus APIs

Both interfaces are thin: each method maps to one engine command (`Command` in
`engine_thread.rs`; daemon-specific ones in `BrokerExt` / `SessionExt`) and awaits its reply
on a oneshot channel, or reads the `Published` snapshot. New members (the control-plane
methods in CONTROL-PLANE.md §5) are one `#[interface]` method plus, when they need the engine,
one command variant handled on the engine thread.

### Operational notes

* The state directory must be on tmpfs (the defaults `/run/lindesk`, `$XDG_RUNTIME_DIR/lindesk`
  are): the applied state is rewritten on every change, and on a saturated ext4 disk a
  rename-over can stall the engine thread for seconds.
* Sinks run on the engine thread; the `parquet-store` flush (every `[telemetry] flush`) and the
  final flush at shutdown are synchronous. Steps or bookkeeping slower than 1 s are logged as
  `slow engine step` with per-stage timings.

### Logging

journald when `JOURNAL_STREAM` is set (syslog identifier = binary name), else stderr. Filter:
`--log-level`, else `RUST_LOG`, else `info`. Every applied/reverted action is logged at info
(target `lindesk::action`), resolver notes at debug.

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
