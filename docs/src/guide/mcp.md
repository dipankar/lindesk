# AI agents: the lindesk MCP server

`lindesk-mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server. A local
AI agent such as Claude Code uses it to look at your machine, explain why it stutters, and
apply fixes that are **guardrailed, versioned and revertible**.

```text
 agent (Claude Code, …) ── MCP (stdio | unix socket | loopback HTTP) ──► lindesk-mcp
                                                                          │ read-only: lindesk-sys probes, telemetry lake
                                                                          │ changes: D-Bus only (no privileges here)
                                       org.lindesk.QoS1 (user scope) ◄────┤
                                    org.lindesk.Broker1 (system scope) ◄──┘  guardrails → plan → apply → confirm/revert
```

## Register it

```sh
# Claude Code (stdio; started on demand)
claude mcp add lindesk -- lindesk-mcp

# Print the snippet for other clients (or: lindeskctl mcp config --client …)
lindesk-mcp config --client claude-desktop   # JSON for ~/.config/Claude/claude_desktop_config.json
lindesk-mcp config --client cursor           # JSON for ~/.cursor/mcp.json
lindesk-mcp config --client generic

# Is it installed, is the optional socket running, are the daemons up?
lindeskctl mcp status
```

The stdio mode needs no service: the client starts `lindesk-mcp` when it needs it. The
packages install `lindesk-mcp.service` and `lindesk-mcp.socket` as **user** units but do not
enable them; enable the socket only for clients that connect to a running endpoint (see
below). Everything an agent changes shows up in `lindeskctl changes list` with its client
name (`mcp:claude-code`), and `lindeskctl doctor` lists changes still waiting for
confirmation.

Then ask the agent something like *"my desktop lags while I compile, can you look?"*, or
use the `assess-responsiveness` prompt (`/mcp__lindesk__assess-responsiveness` in Claude
Code).

## Safety model

* **lindesk-mcp holds no privileges.** Every change goes to a daemon over D-Bus:
  `lindesk-session` for the user scope, `lindeskd` for the system scope. The daemons enforce
  the guardrails, polkit and versioning (see the
  [control plane](https://github.com/dipankar/lindesk/blob/main/docs/CONTROL-PLANE.md)).
* **Plan first.** `plan_change` is a dry run: risk, a diff of previous → new values, and any
  rejections. Nothing changes.
* **Everything auto-reverts unless confirmed.** `apply_change` always sends a ttl (default
  10 minutes, 1–1440). Unless the agent calls `confirm_change`, the daemon restores the
  previous values when it expires.
* **Everything is versioned.** `list_changes`/`get_change` show the history and
  `revert_change` undoes any changeset (as a new changeset; history is append-only).
* **Prefer the user scope.** `scope=user` only touches your own lindesk config and session.
  `scope=system` (sysctls, I/O schedulers, zswap, cpufreq EPP/governor, `/etc/lindesk`)
  needs polkit `org.lindesk.manage`, so you may see an admin authentication dialog.
* Guardrail rejections come back as normal tool results with `isError: true`, e.g.
  `rejected: vm.swappiness=500 outside 0–200`.
* Changes are tagged with their origin: the daemons record
  `{"origin": "mcp", "client": "<the MCP client's name>"}` with your uid.

Without a running daemon the read-only tools still work (they probe the machine locally);
the mutating tools return an error that says how to start the daemon.

## Tools

| Tool | Read-only | What it does |
|---|---|---|
| `status` | yes | Level (green/yellow/red), PSI pressure, protected/yielding units, daemon mode. |
| `doctor` | yes | What the kernel/session supports, with remedies (always local). |
| `hardware` | yes | CPU (hybrid P/E, EPP/governors), memory, storage kinds and schedulers, power, latency map. |
| `latency_map` | yes | The calibrated baseline: wakeup latency, core speed, memory, disk 4 KiB p50/p99, swap page-in. |
| `swap_analysis` | yes | Swap devices, zswap, vm.* tunables, MGLRU, swap rates, findings with proposed ops. |
| `explain {target}` | yes | How lindesk treats a pid/unit: class, foreground, applied settings and why. |
| `query {sql, limit}` | yes | One SELECT/WITH over the telemetry lake (≤ 1000 rows), through `lindesk-data`. Without the optional lindesk-telemetry package: a tool error `telemetry_not_installed`. |
| `list_plugins` | yes | Live pipeline health plus installed external plugins. |
| `list_profiles` | yes | Profiles, which is active, which match this machine now. |
| `list_changes {limit}`, `get_change {id}` | yes | The versioned change history. |
| `recent_events {n, kind}` | yes | Focus, level, action and plugin events. |
| `recommendations` | yes | Ranked fixes from doctor, swap, requirements, latency anomalies and 24 h of telemetry (skipped, `sources.telemetry = "not installed"`, without lindesk-telemetry), each with `proposed_ops`, `risk` and `scope`. |
| `plan_change {scope, ops, description}` | yes (dry run) | Guardrail validation, risk and diff. |
| `apply_change {scope, ops, description, ttl_minutes}` | no | Apply with a dead-man switch. |
| `confirm_change {scope, id}` | no | Keep a change. |
| `revert_change {scope, id}` | no | Undo a change now. |
| `set_profile {scope, name}` | no | A `profile_use` changeset (also auto-reverts unless confirmed). |
| `calibrate {quick}` | no | Measure the latency baseline (lindeskd runs it, ≈15 s). |

`ops` are `ChangeOp` objects, for example:

```json
[{"op": "sysctl", "key": "vm.swappiness", "value": "10"},
 {"op": "profile_use", "scope": "user", "name": "latency"},
 {"op": "class_override", "scope": "user", "unit": "app-*steam*.scope", "class": "interactive"},
 {"op": "io_scheduler", "device": "8:0", "scheduler": "bfq"}]
```

## Resources and prompts

* `lindesk://status`, `lindesk://hardware` (JSON), `lindesk://config` (the effective user
  config as TOML), `lindesk://changes/{id}` and `lindesk://changes/system/{id}`,
  `lindesk://docs/{topic}` (`control-plane`, `architecture`, `plugin-concepts`,
  `telemetry-schema`, `safety`, `mcp`).
* Prompts: `assess-responsiveness {symptom?}` (status + hardware + latency + swap →
  recommendations → explain → plan → apply with a ttl → observe → confirm or revert) and
  `explain-slowdown {app, when?}`.

## Running it as a service (HTTP)

For clients that connect to a running endpoint instead of spawning a process:

```sh
systemctl --user enable --now lindesk-mcp.socket     # unix socket $XDG_RUNTIME_DIR/lindesk/mcp.sock (0600)
lindeskctl mcp serve --listen 127.0.0.1:7457         # same as lindesk-mcp --listen …
lindesk-mcp --listen 127.0.0.1:7457                  # loopback TCP, bearer token required
lindesk-mcp config --client claude-code --listen 127.0.0.1:7457
```

* The endpoint is `/mcp` (MCP streamable HTTP).
* **Unix socket**: created mode 0600 in a 0700 directory; only your user can connect. No
  token.
* **TCP**: only loopback addresses are accepted (`0.0.0.0` and LAN addresses are refused).
  Every request needs `Authorization: Bearer <token>`; the token is generated on first start
  into `$XDG_RUNTIME_DIR/lindesk/mcp.token` (0600). Requests carrying a browser `Origin`
  header are rejected.
* `lindesk-mcp.socket` uses socket activation (`LISTEN_FDS`); the service also works alone.

Logs go to stderr only (never stdout, which carries the stdio protocol); set the level with
`--log-level debug` or `RUST_LOG`.

## For daemon implementers

The D-Bus members lindesk-mcp calls are in `lindesk-ipc` (`QoSProxy`, `BrokerProxy`) and
[CONTROL-PLANE.md §5](https://github.com/dipankar/lindesk/blob/main/docs/CONTROL-PLANE.md):
`PlanChange`, `ApplyChange`, `ConfirmChange`, `RevertChange`, `ListChanges`, `GetChange`,
`ListProfiles`, `SetProfile`, `GetHardware`, `SwapReport`, `RecentEvents`, `Calibrate` (broker),
the `ChangeEvent` signal, plus the optional broker member `CalibrateWith(options_json)`
(`{"quick": true}`), which lindesk-mcp tries before falling back to `Calibrate()`.
`ops_json` is a `ChangeEnvelope` (`{"origin", "client", "ops"}`; a bare array is also valid).
Reply errors are mapped as follows: `InvalidArgs` or an error name ending in `.Rejected`
(or a message starting with `rejected`) → a guardrail rejection; `AccessDenied`/`AuthFailed`
→ not authorized; `UnknownMethod` → daemon too old.
