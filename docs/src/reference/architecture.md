# Architecture and contracts

```text
           ┌──────────── plugin host (same in lindeskd and lindesk-session) ────────────┐
 adapters ─┤ sensors → classifiers → policies → resolver+safety caps → actuators         │
 (focus,   │      ╲                                   │                                 │
  input)   │       ╰──────────── events/samples ──────┴──────────→ sinks (vortex, …)   │
           └────────────────────────────────────────────────────────────────────────────┘
```

| Component | Runs as | Role |
|---|---|---|
| `lindeskd` | root, system service | The broker: probe, PSI, system plugins, policy, system units, iocost. Sends per-user desired state to session agents. |
| `lindesk-session` | you, `systemd --user` service | Foreground tracking, user units, user plugins, your telemetry. Runs the whole pipeline (your units only) when there's no broker. |
| `lindeskctl` | you (or root) | The CLI: doctor, status/watch/events, explain, run/set/restore, plugin tooling, simulate, query/data. |
| `lindesk-mcp` | you, stdio or socket | MCP server for local AI agents. |
| `lindesk-bench` | you | Latency probe and A/B harness. |
| `lindesk-telemetry-sink` | a plugin process of each daemon | *Optional lindesk-telemetry package*: the `telemetry` sink plugin that writes the lake. |
| `lindesk-data` | CLI, oneshot timer | *Optional lindesk-telemetry package*: lake query, export/import, compaction, rollups, retention (DuckDB). |

The core package (`lindesk`) links neither DuckDB nor Vortex; see "Packages and what links
what" in docs/ARCHITECTURE.md.

The source of truth for the interfaces between components is
[docs/ARCHITECTURE.md](https://github.com/dipankar/lindesk/blob/main/docs/ARCHITECTURE.md).
The pieces most people need:

* **D-Bus**: [D-Bus API](dbus.md).
* **Plugins**: [protocol](protocol.md) and [concepts](../plugins/concepts.md).
* **Telemetry**: [schema](schema.md).
* **Safety**: [invariants](safety.md).

## Filesystem layout

| What | System (`lindeskd`) | User (`lindesk-session`) |
|---|---|---|
| Config | `/etc/lindesk/lindesk.toml` | `$XDG_CONFIG_HOME/lindesk/lindesk.toml` |
| Classifier rules | `/usr/share/lindesk/rules.d/`, then `/etc/lindesk/rules.d/` | then `$XDG_CONFIG_HOME/lindesk/rules.d/` |
| Plugins | `/usr/lib/lindesk/plugins/<name>/`, `/etc/lindesk/plugins/<name>/` (root-owned) | `$XDG_DATA_HOME/lindesk/plugins/<name>/`, then the system dirs (`privilege = "user"` only) |
| Applied state | `/run/lindesk/state.json` | `$XDG_RUNTIME_DIR/lindesk/state.json` |
| Telemetry | `/var/lib/lindesk/telemetry/` (0750 root:lindesk) | `$XDG_STATE_HOME/lindesk/telemetry/` (0700) |
| DuckDB catalog | `/var/lib/lindesk/catalog.duckdb` | `$XDG_STATE_HOME/lindesk/catalog.duckdb` |

`lindeskctl plugin install` writes to `~/.local/share/lindesk/plugins/<name>` or, with
`--system` as root, `/etc/lindesk/plugins/<name>`. `lindeskctl plugin enable|disable` edits
the config file of the same scope.

## Crates

| Crate | Notes |
|---|---|
| `lindesk-plugin-api` | **Stable public API**: data model, traits, protocol, manifest. |
| `lindesk-core` | Pure: config, resolver, safety caps, reconciler state, clocks. |
| `lindesk-sys` | Probe, cgroupfs, procfs, PSI, syscalls; every function takes explicit roots. |
| `lindesk-host` | Discovery, supervision, stdio transport, the deterministic `Engine`, conformance, scaffolding. |
| `lindesk-builtins` | Built-in sensors, adapters, classifiers, policies and actuators. |
| `lindesk-store` | Telemetry writer (arrow + Vortex files, no DuckDB); lindesk-telemetry only. |
| `lindesk-query` | Telemetry reader and maintainer (DuckDB); lindesk-telemetry only. |
| `lindesk-telemetry-sink` | The `telemetry` plugin binary. |
| `lindesk-data` | The lake tool binary. |
| `lindesk-ipc` | D-Bus names, proxies, payloads and polkit helpers. |
| `lindesk-plugin` | Rust SDK for external plugins. |

## Determinism

`Engine::step(now)` takes time as an argument and the environment as a trait object, so the
same pipeline runs in the daemons (real clock, `/proc` + systemd), in tests and in
`lindeskctl simulate` (a `SimClock`, a `MemoryEnvironment` rebuilt from telemetry, a replay
sensor and a recording actuator).
