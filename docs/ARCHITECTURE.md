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
This is the API that desktop shells and applications use.

| Member | Signature | Notes |
|---|---|---|
| `SetForegroundProcess(pid)` | `i` → | Focus moved to a window owned by `pid` (GNOME extension, KWin script). |
| `SetForegroundApp(app_id)` | `s` → | Focus moved to an app known only by id (COSMIC-like shells). |
| `BeginInteraction(pid)` | `i` → | Starts/extends an interaction epoch (input events, app-initiated). |
| `EndInteraction(pid)` | `i` → | Ends it early. |
| `SetClass(pid, class)` | `is` → | `critical|interactive|normal|background|batch`. Own processes only. |
| `ReportMetric(name, value, labels)` | `sda{ss}` → | Free-form metric into the `metrics` table (e.g. GNOME frame overruns). |
| `Status()` | → `s` | JSON status. |
| `Explain(what)` | `s` → `s` | `what` = pid or target string; JSON explanation. |
| signal `LevelChanged(level)` | `s` | `green|yellow|red`. |

### System bus: `org.lindesk.Broker1`

Owned by `lindeskd`. Object `/org/lindesk/Broker1`, interface `org.lindesk.Broker1`.
Callers are identified by their bus credentials (uid, pid).

| Member | Signature | polkit action |
|---|---|---|
| `RegisterSession()` | → `s` | none (caller uid recorded); returns current user state JSON |
| `ReportForeground(pid, app_id)` | `is` → | `org.lindesk.set-class-own` (pid must belong to caller) |
| `Interaction(pid, end)` | `ib` → | same |
| `SetClass(pid, class)` | `is` → | own pid: `org.lindesk.set-class-own`; other users: `org.lindesk.set-class-any` |
| `GetStatus()` | → `s` | none |
| `Explain(what)` | `s` → `s` | none |
| `Reload()` | → | `org.lindesk.manage` |
| signal `UserState(uid, json)` | `us` | JSON `SessionState` (`lindesk-ipc`): `{"uid":..., "level": ..., "desired": [{"target":..., "setting":..., "provenance":...}]}`: settings for that user's units, to be applied by their session agent. |

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
