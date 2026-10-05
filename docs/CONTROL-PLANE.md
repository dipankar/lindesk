# Control plane: profiles, versioned changes, calibration, MCP

This is the contract for the components that let people **and local AI agents** assess a
machine and tune lindesk safely. It extends [ARCHITECTURE.md](ARCHITECTURE.md).

```text
 local agent (Claude Code, …)        lindeskctl
        │ MCP (stdio | unix socket)      │
        ▼                                 │
   lindesk-mcp  ── read-only ─► lake (runs lindesk-data, if installed), lindesk-sys (hardware, swap, doctor)
        │ mutations (D-Bus only; lindesk-mcp itself holds no privileges)
        ▼                                 ▼
 org.lindesk.QoS1 (user scope)   org.lindesk.Broker1 (system scope, polkit)
        │                                 │
        └──────► lindesk-changes: guardrails → plan → versioned apply → confirm/revert
```

## 1. Hardware profile (contract: `lindesk_plugin_api::Hardware`)

* CPU: arch (x86_64/aarch64/…), vendor, cores/threads/SMT, hybrid P/E (or big.LITTLE)
  CPU lists, features (EAS, ITMT, cpufreq driver, governor, EPP, NUMA nodes).
* Memory: RAM, swap, compressed swap.
* Storage: per managed device kind (NVMe/SSD/HDD/eMMC/SD/virtual/network), scheduler,
  nominal bandwidth.
* Power: AC/battery, portable chassis. Virtualization.
* `latency: Option<LatencyMap>`: measured baseline (§3).

Detected by `lindesk_sys::hardware::detect` and carried in `HostCaps.hardware`, so every policy
and plugin sees it.

## 2. CPU profiles (`lindesk_core::profile`)

A profile is a named **overlay on `lindesk.toml`** plus optional auto-selection rules. Files:
`/usr/share/lindesk/profiles.d/*.toml` → `/etc/lindesk/profiles.d/` → `$XDG_CONFIG_HOME/lindesk/profiles.d/`
(a later file with the same profile name replaces earlier ones).

```toml
[profile]
name = "gaming"
description = "Max foreground protection while a game is focused"
inherits = "latency"          # optional; overlays apply base → parent → child

[match]                       # optional auto-selection; all given fields must match
foreground_app_id = ["steam_app_*", "*.exe"]
power = "ac"                  # ac | battery
arch = ["x86_64"]             # x86_64 | aarch64 | riscv64 | …
vendor = ["amd", "intel"]
hybrid = true
storage = ["hdd"]             # any managed device of this kind
min_cpus = 8
max_mem = "8GiB"
priority = 60                 # highest matching priority wins; ties → name

[config]                      # same schema as lindesk.toml, merged as an overlay
[config.plugins.threshold.config]
epoch = "800ms"
[[config.override]]
unit = "steam*.scope"
class = "interactive"
```

Built-ins shipped in `packaging/config/profiles.d/`: `balanced` (default), `latency`,
`battery`, `throughput` (build servers: background yields less), `gaming`, and architecture
tuned ones: `arm-eas` (aarch64 + EAS: rely on uclamp steering), `x86-hybrid` (Intel hybrid:
P-core boosts, E-core batch caps), `hdd` (rotational storage: earlier I/O intervention).

Selection: `[general] profile = "auto"` (default) evaluates `[match]` rules every tick against
a `ProfileContext` (power, arch, vendor, hybrid, storage kinds, cpus, mem, foreground app id),
with a 10 s minimum dwell; `profile = "<name>"` pins one. A switch recomputes the effective
config (`base ⊕ profile chain`) and calls `Engine::reload_config`, emitting
`Event::Notice { name: "profile.changed", data: {from, to, reason} }`.

API: `ProfileSet::load(dirs) -> (ProfileSet, Vec<ProfileError>)`, `ProfileSet::select(&ProfileContext) -> Selection { name, reason }`,
`ProfileSet::effective_config(&Config, name) -> Result<Config, ProfileError>`, `ProfileSet::list()`.

## 3. Latency map and swap analysis (`lindesk_sys::{calibrate, swap}`)

* `calibrate::run(&Roots, &CalibrateOptions) -> LatencyMap`: ≈15 s bounded run on an idle
  machine (refuses/flags `noisy` when PSI is high): per-CPU timer-wakeup p50/p99 and relative
  single-thread throughput (pinned via `sched_setaffinity`, empirically confirming P/E cores),
  memory pointer-chase latency, copy bandwidth and first-touch fault cost, per managed device
  4 KiB random-read p50/p99 and sequential read/write bandwidth (bounded file in a scratch
  dir on that filesystem, `O_DIRECT` when possible), swap page-in estimate per swap device.
  Stored at `/var/lib/lindesk/calibration.json` (system) or `$XDG_STATE_HOME/lindesk/calibration.json`,
  and in the lake table `calibration`. The broker runs it once after install (when idle) and
  on `lindeskctl calibrate`. Policies prefer measured bandwidth over nominal.
* `swap::analyze(&Roots, &Hardware) -> SwapReport`: swap devices/files (zram, partition, file;
  backing storage kind), zswap state, `vm.swappiness`, `vm.page-cluster`, `vm.watermark_scale_factor`,
  `min_free_kbytes`, MGLRU (`/sys/kernel/mm/lru_gen/enabled`), swap-in/out rates (`/proc/vmstat`),
  top units by `memory.swap.current`, and `findings` with **proposed change ops** (§4), e.g.
  swap on HDD → lower swappiness or add zram; `page-cluster 3` on zram/SSD → 0; no swap with
  ≤ 8 GiB RAM → enable zram (advice only; lindesk doesn't create swap).

## 4. Versioned, guardrailed changes (`lindesk-changes` crate)

Every mutation that outlives a policy tick (config, profile, sysctl, I/O scheduler, cpufreq)
goes through a **changeset**: validated by guardrails, applied with previous values recorded,
versioned, and reversible.

```rust
#[serde(tag = "op", rename_all = "snake_case")]
enum ChangeOp {
    ConfigSet   { scope: Scope, key: String /* dotted path */, value: serde_json::Value },
    ConfigUnset { scope: Scope, key: String },
    ProfileUse  { scope: Scope, name: String },          // sets general.profile
    ClassOverride { scope: Scope, unit: String /* glob */, class: QosClass },
    Sysctl      { key: String, value: String },          // system scope, allow-listed
    IoScheduler { device: String /* MAJ:MIN or name */, scheduler: String },
    Zswap       { enabled: Option<bool>, compressor: Option<String>, max_pool_percent: Option<u8> },
    CpuEpp      { preference: String },                  // all cpufreq policies
    CpuGovernor { governor: String },
}
enum Scope { User, System }
```

**Guardrails** (`guard::validate(op, &GuardContext) -> Result<Risk, Rejection>`):
* Allow-lists with ranges: `vm.swappiness` 0–200, `vm.page-cluster` 0–3,
  `vm.watermark_scale_factor` 10–1000, `vm.vfs_cache_pressure` 50–200; schedulers only from
  the device's `queue/scheduler`; EPP/governor only from the advertised available values.
* Config edits must leave a config that parses (`Config::from_toml`) and resolves; `[safety]`
  may only be **tightened** through this path (loosening is rejected); unknown keys rejected.
* At most 16 ops per changeset; rate limit 10 changesets / 10 min per origin.
* `Risk::{Low, Medium, High}`: config/profile/class = Low; sysctl/zswap = Medium;
  scheduler/governor/EPP = High.
* **Dead-man switch:** changesets from MCP (and any with `ttl`) auto-revert after `ttl`
  (default 10 min for MCP, required for High risk) unless confirmed with `confirm(id)`.
* System-scope ops need polkit `org.lindesk.manage`; user-scope ops only touch the caller's
  own config.

**Versioning** (`ChangeStore`, `/var/lib/lindesk/changes/` and `$XDG_STATE_HOME/lindesk/changes/`):
* Monotonic version ids. `NNNNNN.json` per changeset `{id, parent, ts, author, origin, description, ops: [{op, previous, applied}], status, ttl_until, reverts}`,
  `config/NNNNNN.toml` snapshots of the config file after each version, `HEAD`.
* Status: `planned → applying → applied → confirmed | reverted | failed`. Write-ahead: the
  record is written as `applying` before touching the system; on startup any `applying`
  changeset is rolled back from its recorded `previous` values.
* `revert(id)` creates a **new** changeset that restores the previous values (history is
  append-only). `revert_to(version)` reverts every later applied changeset, newest first.
* Runtime ops (sysctl, scheduler, zswap, EPP, governor) are runtime-only by default; `persist:
  true` additionally writes `/etc/sysctl.d/90-lindesk.conf` / a udev rule, also versioned.
* Every transition emits `Event::Notice { name: "change.applied" | "change.confirmed" | "change.reverted" | "change.failed", data: changeset }`.

API: `ChangeStore::open(dir, Scope)`, `plan(&[ChangeOp], &GuardContext) -> Plan { ops, risk, diff, rejections }`,
`apply(plan, Author, ttl) -> ChangeSet`, `confirm(id)`, `revert(id, Author) -> ChangeSet`, `revert_to(id)`,
`history(limit)`, `get(id)`, `expire_due(now) -> Vec<ChangeSet>`, `recover()`. Appliers sit
behind `trait Applier { fn read(&self, &ChangeOp) -> Result<Value>; fn write(&self, &ChangeOp, &Value) -> Result<()> }`
with roots for tests.

## 5. D-Bus additions (both `org.lindesk.QoS1` → user scope and `org.lindesk.Broker1` → system scope)

| Member | Signature | Notes |
|---|---|---|
| `PlanChange(ops_json)` | `s` → `s` | Plan JSON (risk, diff, rejections). No side effects. |
| `ApplyChange(ops_json, description, ttl_secs)` | `ssu` → `s` | ChangeSet JSON. `ttl_secs = 0` → no auto-revert (not allowed for High risk from MCP). |
| `ConfirmChange(id)` | `t` → | |
| `RevertChange(id)` | `t` → `s` | New ChangeSet JSON. |
| `ListChanges(limit)` | `u` → `s` | |
| `GetChange(id)` | `t` → `s` | |
| `ListProfiles()` | → `s` | name, description, active, matches-now, reason |
| `SetProfile(name)` | `s` → `s` | Shortcut for a `ProfileUse` changeset ("auto" allowed). |
| `GetHardware()` | → `s` | `Hardware` JSON incl. latency map |
| `SwapReport()` | → `s` | |
| `Calibrate()` | → `s` | Broker only; runs calibration, returns `LatencyMap`. |
| `CalibrateWith(options_json)` | `s` → `s` | Broker only; `{"quick": bool}` (`CalibrateRequest`) plus broker extensions `force` (default true) and `storage` (default true). |
| signal `ChangeEvent(json)` | `s` | Any changeset transition. |

Callers' authorship is recorded (`uid`, bus name, and an `origin` hint passed in the ops
envelope: `{"origin": "mcp", "client": "claude-code", "ops": [...]}`; `mcp` → `Mcp`, `cli`/
`lindeskctl` → `Cli`, anything else → `Dbus`). Authorization: on the system bus the mutating
members (`ApplyChange`, `ConfirmChange`, `RevertChange`, `SetProfile`, `Calibrate*`) need
polkit `org.lindesk.manage`; the session agent serves only callers with its own uid.

Errors: a guardrail refusal is the D-Bus error `org.lindesk.Error.Rejected` whose message is
the human-readable list of reasons; an unknown id is `org.lindesk.Error.NotFound`; malformed
`ops_json` is `org.freedesktop.DBus.Error.InvalidArgs`; other failures
`org.lindesk.Error.Failed`. An apply whose writes failed and were rolled back returns the
changeset with `status: "failed"`. Where the stores live and how the daemons wire them (recovery,
dead-man timer, reloads) is in [ARCHITECTURE.md](ARCHITECTURE.md) ("Versioned changes").

## 6. MCP server (`lindesk-mcp`, rmcp)

* Binary `lindesk-mcp`: `--stdio` (default; for `claude mcp add lindesk -- lindesk-mcp`) or
  `--listen unix:$XDG_RUNTIME_DIR/lindesk/mcp.sock` / `--listen 127.0.0.1:7457` (streamable
  HTTP; TCP requires the bearer token in `$XDG_RUNTIME_DIR/lindesk/mcp.token`, 0600).
  User unit `lindesk-mcp.service` (+ `lindesk-mcp.socket`) for a running local endpoint.
  `lindeskctl mcp [serve|install --client claude-code|config]`.
* Read-only tools (`readOnlyHint`): `status`, `doctor`, `hardware`, `latency_map`,
  `swap_analysis`, `explain`, `query` (read-only SQL, row cap), `list_plugins`,
  `list_profiles`, `recommendations` (aggregates doctor/swap/latency/telemetry findings into
  ready-to-plan op lists with rationale), `list_changes`, `get_change`, `recent_events`.
* Mutating tools (guardrailed, `destructiveHint` false where reversible): `plan_change`,
  `apply_change` (always with a ttl; default 10 min), `confirm_change`, `revert_change`,
  `set_profile`, `calibrate`.
* Resources: `lindesk://status`, `lindesk://hardware`, `lindesk://config`,
  `lindesk://changes/{id}`, `lindesk://docs/{topic}`.
* Prompts: `assess-responsiveness` (step-by-step assessment → recommendations → plan →
  apply with ttl → observe via `status`/`query` → confirm or revert).
* lindesk-mcp has no privileges: mutations go to the daemons over D-Bus, which enforce
  guardrails, polkit and versioning. When no daemon runs, mutating tools return a clear error
  and read-only tools fall back to local probing.
