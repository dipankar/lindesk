# lindesk: a plugin-based QoS control plane for the Linux desktop

## Context

Under heavy background load (compiles, big copies, indexers, package upgrades) a Linux desktop stutters, while macOS stays responsive because of its QoS classes. The kernel already has the pieces: PSI, cgroup v2 cpu/io/memory, uclamp, iocost, systemd's `session.slice`/`app.slice`/`background.slice`, and sched_ext. What's missing is a control plane that connects "the human just interacted with this app" to those controls. **lindesk** is that control plane. It needs no kernel patch and works on any distro. It probes what the host supports and changes nothing while the machine is healthy. When foreground latency gets worse it steps in gradually, with hysteresis. Principle: **don't make interactive work dangerous; make non-interactive work yield.**

Two product requirements shape the design:
1. **Plugin-based.** Every stage of the pipeline is an extension point: sensing, desktop adapters, classification, policy, actuation and sinks. Third parties can extend it in any language, and the built-ins use the same API. Adoption depends on the size of that surface and how good the developer experience is.
2. **Telemetry you can query with DuckDB.** Samples and events are stored as a partitioned Parquet lake with proper compaction, rollups, retention, targeted deletion and access control.

Greenfield: `/home/dipankar/Github/lindesk` is empty and not yet a git repo.

### What this dev host actually has (probed)
- Pop!_OS 24.04, kernel 7.0.11, systemd 255, GNOME 46 Wayland. COSMIC 1.0 is also installed. Root is ext4, so cgroup writeback works.
- cgroup v2. The user manager delegates `cpu io memory pids`. PSI is on. `CONFIG_UCLAMP_TASK_GROUP=y`, `CONFIG_SCHED_CLASS_EXT=y` (sched_ext is currently disabled), BTF is present.
- **`CONFIG_BLK_CGROUP_IOLATENCY` is not set.** There is no `io.latency`, so the I/O fallback chain is mandatory.
- The I/O scheduler is `none` on every disk, so **ioprio and io.weight do nothing until iocost is enabled.** `io.cost.qos` is present.
- `system76-scheduler` is installed but inactive. It's a potential conflict that `doctor` must report.
- Global I/O PSI is currently around 93% `some`, which makes this machine a good real-world test bed.

### Decisions
- Confirmed with you:
  - All four foreground adapters: GNOME, KDE, COSMIC, X11.
  - A root broker plus a per-user agent.
  - .deb, .rpm, an AUR PKGBUILD and `install.sh`.
  - Plugins, and DuckDB-accessible telemetry.
- My defaults, each easy to change:
  - Rust 1.95 with zbus 5, tokio, clap, serde/toml, schemars, tracing, rustix, x11rb, wayland-client, arrow/parquet and duckdb.
  - License MIT OR Apache-2.0.
  - D-Bus names `org.lindesk.Broker1` and `org.lindesk.QoS1`.

## Architecture

```
           ┌──────────── plugin host (same in lindeskd and lindesk-session) ────────────┐
 adapters ─┤ sensors → classifiers → policies → resolver+safety caps → actuators         │
 (focus,   │      ╲                                   │                                 │
  input)   │       ╰──────────── events/samples ──────┴──────────→ sinks (parquet, …)   │
           └────────────────────────────────────────────────────────────────────────────┘
 built-ins: in-process Rust trait impls   external: out-of-process JSON-RPC over stdio

GNOME ext / KWin script (push adapters over D-Bus), COSMIC + X11 (built-in adapters)
        ▼
lindesk-session (per user, unprivileged, session bus org.lindesk.QoS1)
  • maps pid/app_id → systemd user unit; applies user-side settings via user-systemd
    SetUnitProperties(runtime=true); hosts user plugins; writes the user's telemetry store
  • standalone fallback: runs the pipeline locally if there's no broker
        ▼ system bus org.lindesk.Broker1 (polkit)
lindeskd (root broker, the single "brain")
  • probe → capabilities; PSI triggers; controller; system.slice policy; iocost
  • hosts system plugins (root-owned dirs only); sends per-uid desired state to agents
```

## Plugin system (the core DX surface)

**Extension kinds.** Each kind is a Rust trait in `lindesk-plugin-api`, with a matching JSON-RPC method set.

| Kind | Contract | Built-ins that use it |
|---|---|---|
| `sensor` | emits `Sample{resource, scope, some, full, stall_delta…}` (push) or answers `poll` | `psi`, `cgroup-stat`; eBPF is phase 2 |
| `adapter` | emits `Foreground{pid\|app_id}`, `Interaction{pid}` | `x11`, `cosmic`; GNOME and KWin push over D-Bus |
| `classifier` | `classify(ProcInfo{pid,comm,exe,unit,app_id,cgroup,uid}) → Option<{class, confidence}>`, chained by priority | `rules` (TOML globs) |
| `policy` | `evaluate(Snapshot{now, level, pressure, units, events}) → Vec<Intent{target, knob, value, priority, reason}>` | `threshold` (GREEN/YELLOW/RED + epochs + io.max AIMD) |
| `actuator` | `capabilities()`, `snapshot(target)`, `apply(target, Settings)`, `restore(target)`. **System-privileged only** | `systemd`, `cgroup-v2-raw`, `cgroup-v1`, `proc` (nice/ioprio/uclamp) |
| `sink` | `record(table, RecordBatch)` | `parquet-store`; Prometheus and OTLP are example plugins |

**Transports.**
1. **In-process.** Built-ins implement the traits directly in `lindesk-builtins`.
2. **Out-of-process.** Newline-delimited JSON-RPC 2.0 over stdio, so plugins can be written in any language and are crash-isolated. There's a versioned handshake (`initialize {api_version, host_caps}` → `{kinds, subscriptions}`). A JSON Schema is generated from the Rust types with schemars and published in the docs.
3. **D-Bus push** for desktop-shell adapters (GNOME extension, KWin script).

The trait is transport-agnostic, so a later WASM transport is purely additive.

**Manifest** (`plugin.toml`):
```toml
[plugin]
name = "steam-games"   version = "0.1.0"   api = "1"
kinds = ["classifier"]  exec = "steam_games.py"  privilege = "user"   # user|system
[classifier]
priority = 50
prefilter = { comm = ["*.exe", "steam*"] }   # host-side filter → no RPC per process
```

**Discovery.**
- System plugins load only from root-owned `/usr/lib/lindesk/plugins` and `/etc/lindesk/plugins`.
- User plugins load from `~/.local/share/lindesk/plugins`. They run in the session agent and can only target the user's own units.

**Supervision.**
- Per-call timeouts: classifier 50 ms, policy 100 ms.
- Restart with exponential backoff; a plugin is auto-disabled after N crashes.
- Any plugin failure falls back to the built-in for that kind.
- Per-plugin logs go to the journal.

**Composition and safety.** Policies emit *intents*, never direct writes. The resolver picks one value per (target, knob):
- precedence is admin config, then plugin priority, then built-in
- then `SafetyCaps` clamp the result

The caps:
- no RT CPU or RT I/O classes
- `uclamp.min ≤ 512`
- a `CPUWeight` range limit
- an `io.max` floor, so nothing is ever starved
- `memory.high` never below a working-set floor

Every applied value carries provenance (plugin + reason), which is what `lindeskctl explain` shows.

**Developer experience.**
- **Plugin tooling:**
  - `lindeskctl plugin new --kind classifier --lang python|rust` scaffolds a working plugin plus a test.
  - `plugin list | info | enable | disable | logs | reload`
  - `plugin test <dir>` runs protocol conformance checks.
  - **`lindeskctl simulate --policy <plugin> --from … --to …`** replays recorded telemetry through a policy with simulated time and prints the actions it *would* have taken, with a diff against the built-in. Recorded data becomes a test harness.
- **Inspection:** `lindeskctl explain <pid|unit>` shows class, which classifier decided it, the effective settings, and which plugin or intent set each one.
- **CLI conventions:** git-style external subcommands (`lindeskctl foo` → `lindeskctl-foo` on PATH); `--json` on every command; shell completions and man pages (clap_complete/clap_mangen); `lindeskctl config check`.
- **SDKs and docs:**
  - Rust `lindesk-plugin` crate: `lindesk_plugin::main!(MyClassifier)` runs the stdio loop.
  - Python SDK: a single file, stdlib only, with decorators (`@classifier`, `@policy`).
  - An mdBook docs site: concepts, plugin guide, protocol reference, cookbook.
- **Example plugins:** `python/steam-games` (classifier), `rust/on-battery` (policy modifier), `python/prometheus-exporter` (sink), `shell/notify-on-red` (sink → notify-send).

## Telemetry store (Parquet lake, accessed through DuckDB)

**Layout (hive-partitioned, immutable files).**
- System data goes in `/var/lib/lindesk/telemetry/` (0750 root:lindesk) and holds global PSI, levels, and system.slice actions.
- Per-user data goes in `~/.local/state/lindesk/telemetry/` (0700) and holds the user's apps, focus and actions.

That split makes privacy a property of the layout. Window titles and cmdline arguments are never stored. Files are named `<table>/date=YYYY-MM-DD/hour=HH/part-<ulid>.parquet`.

**Tables.**
- `pressure` (ts, scope, resource, some/full avg10, stall deltas in µs)
- `units` (ts, unit, class, cpu_usage_delta, io r/w bytes/ios, mem_current, applied settings)
- `events` (ts, kind = focus | interaction | level | class_set | action_applied | action_reverted | plugin_error, unit, app_id, pid, plugin, reason, payload JSON)
- `metrics` (ts, plugin, name, labels MAP, value), for plugin-defined data
- `bench_runs` and `bench_samples`
- rollups `pressure_1m/_1h` and `units_1m/_1h`

**Write path.** It lives in daemon-side `lindesk-store` and uses arrow-rs/parquet only; the daemons never link DuckDB.
- Default sampling is 1 s. Data goes into an in-memory Arrow buffer, which is flushed every 60 s to a new file in the current hour partition (write to `.tmp`, fsync, rename).
- On shutdown the buffer is flushed. If the daemon crashes, at most one flush interval is lost.

**Compaction and retention.** `lindesk-data` is a oneshot binary that links duckdb (bundled). It runs from `lindesk-compact.timer`, both as a system timer and as a user timer.
- It only touches *sealed* partitions, meaning older than the current hour, under a lock file.
- **Compact:** merge an hour's small files into one ZSTD Parquet file sorted by (unit, ts), then aggregate into the 1m and 1h rollups.
- **Swap:** write a staging dir, then `renameat2(RENAME_EXCHANGE)` it with the partition. Readers see either old or new, never duplicates. `query` retries once on the rare "file vanished" race.
- **Retention:** defaults are raw 7d, 1m 90d, 1h 1y, plus a `max_size` cap (default 2 GiB) that drops the oldest partitions first. Expiring a whole partition is a cheap directory delete.

**Deletion.**
- `lindeskctl data delete --where "app_id = 'org.mozilla.firefox'" [--since …]` rewrites only the affected partitions with `COPY (SELECT … WHERE NOT (…))` and the same exchange swap, including rollups.
- `data purge` removes everything.
- `telemetry.enabled = false` turns it all off.

**Access.**
- `lindeskctl query "<SQL>" [--format table|csv|json|parquet] [--system]` uses embedded DuckDB, read-only.
- `lindeskctl data catalog` (re)generates `catalog.duckdb`, which contains only `VIEW`s over the Parquet globs (`read_parquet(..., hive_partitioning=true, union_by_name=true)`). Users can then run `duckdb -readonly ~/.local/state/lindesk/catalog.duckdb` from any DuckDB client, notebook or BI tool.
- `lindeskctl data shell` opens the duckdb CLI with the catalog attached, if the CLI is installed.
- `lindeskctl data stats` reports size per table and partition and the retention status.
- Schema versioning: a `schema_version` column plus `union_by_name` lets old and new files coexist.
- `simulate`, `bench compare` and `watch` all read through the same query layer.

Config:
```toml
[telemetry]
enabled = true   interval = "1s"   flush = "60s"
[telemetry.retention]
raw = "7d"   rollup_1m = "90d"   rollup_1h = "1y"   max_size = "2GiB"
```

## Control mechanisms (implemented as built-in plugins)

**Probe and backend choice** (`lindesk-sys/probe.rs`; every probe takes a sysfs/procfs root, so tests run on fixtures). It detects:
- kernel version, cgroup mode and delegated controllers
- PSI, uclamp, `io.latency`, iocost, `io.weight`/BFQ, `io.max`, and the scheduler per device
- cgroup writeback support for the filesystems behind `/` and `/home`
- sched_ext state and ops, BPF/BTF
- desktop and session type, and adapter status
- conflicting daemons: system76-scheduler, ananicy-cpp, uresourced, gamemode

The overall level is one of FULL / IO-PARTIAL / CPU-ONLY / MINIMAL. The doctor shows the result, and the probe picks which actuator plugin is active.

**I/O chain.**
- `io.latency` target, when the kernel has it.
- Otherwise iocost `io.weight`. The broker enables iocost on `none`-scheduler devices when `io.iocost = "auto"` and restores the original setting on exit.
- `io.max` AIMD admission control, in RED only. It starts from the measured `io.stat` rate, halves while the foreground is stalling, and raises additively during recovery.
- ioprio idle for batch, only with BFQ or mq-deadline. Never RT.

**`threshold` policy.**

| Situation | Action |
|---|---|
| Static (work-conserving, so free) | batch gets `CPUWeight=idle` and low `IOWeight`; `MemoryLow` on `session.slice` |
| Interaction epoch (500 ms, extended by input) | foreground `CPUWeight↑`, `uclamp.min↑`, `IOWeight↑`, `MemoryLow` |
| YELLOW | background/batch `CPUWeight↓`, `IOWeight↓`, `uclamp.max↓` |
| RED (the foreground cgroup is stalling) | `io.max` AIMD, batch `cpu.idle`, ioprio idle |
| Sustained severe memory pressure | `memory.high` on batch only |

Recovery steps down with enter/exit thresholds and minimum dwell times.

**Reconciler.** It diffs desired against applied state, makes runtime-only changes, and keeps the original values in `/run/lindesk/state.json`. That file is used to restore on crash or restart and by `ExecStopPost=lindeskd --restore`. The broker runs `Type=notify` with `WatchdogSec`.

**Classification.** The `rules` classifier reads `/usr/share/lindesk/rules.d`, then `/etc`, then the user's directory.
- Critical: compositors, pipewire/wireplumber, portals, plasmashell.
- Background: tracker, baloo, EDS, `background.slice`.
- Batch: packagekitd, apt/dnf/unattended-upgrades, updatedb, man-db, fwupd, snapd refresh.
- Docker and containerd scopes count as background.
- Explicit overrides: `lindeskctl run --batch …` (a transient unit in `lindesk-batch.slice`) and `set <pid> <class>`.

**Adapters.**
- GNOME extension: `notify::focus-window` → `get_pid()`. Stage input is throttled to one Interaction call every 250 ms. It optionally reports frame overruns as a sensor metric.
- KWin script: `workspace.windowActivated` → `window.pid`, sent with `callDBus`.
- COSMIC: `ext-foreign-toplevel-list` + `zcosmic_toplevel_info_v1`. The app_id is mapped to its `app-*-<app_id>-*.scope`, with a `/proc` and desktop-file fallback.
- X11: x11rb on `_NET_ACTIVE_WINDOW` and `_NET_WM_PID`.

**D-Bus.**
- `org.lindesk.QoS1`: `SetClass`, `BeginInteraction`, `EndInteraction`, `SetForegroundProcess`, `SetForegroundApp`, `Status`.
- `org.lindesk.Broker1`: `RegisterSession`, `ReportForeground`, `Interaction`, `SetClass`, `GetStatus`, plus the `UserState` signal.
- Polkit actions: `set-class-own` (allow_active), and `set-class-any` / `manage` (auth_admin). Caller uid is checked against the pid's owner.

## Repository layout

```
crates/
  lindesk-plugin-api/   stable public API: traits, protocol types, JSON Schema, api_version
  lindesk-plugin/       Rust SDK (stdio runtime, main! macro, test helpers)
  lindesk-core/         pure: classes, Settings, Intent, resolver, SafetyCaps, sim clock, config
  lindesk-host/         plugin discovery, manifests, supervision, routing, transports
  lindesk-builtins/     psi, cgroup-stat, rules, threshold, systemd/v2raw/v1/proc actuators,
                        x11 + cosmic adapters, parquet sink
  lindesk-sys/          probe, cgroupfs, PSI, syscalls (ioprio, sched_setattr), blockdev, iocost
  lindesk-store/        Arrow schemas, buffered Parquet writer, partition layout (no DuckDB)
  lindesk-query/        DuckDB: query, catalog, compact, rollup, retention, delete
  lindesk-ipc/          zbus interfaces + polkit helper
  lindeskd/  lindesk-session/  lindeskctl/  lindesk-data/  lindesk-bench/
sdk/python/lindesk_plugin.py
adapters/{gnome-shell-extension, kwin-script}
examples/plugins/{python/steam-games, rust/on-battery, python/prometheus-exporter, shell/notify-on-red}
docs/ (mdBook)   bench/   packaging/{systemd, dbus, polkit, config, debian, rpm, aur, install.sh}
.github/workflows/ci.yml
```

## Milestones

1. **Scaffold.** git init, workspace, CI (fmt, clippy, test), license, README with the thesis, mdBook skeleton.
2. **lindesk-plugin-api + lindesk-core.** Traits, protocol types, schema generation; intents, resolver, safety caps. Exhaustive unit tests.
3. **lindesk-sys probe + `lindeskctl doctor`.** Fixture tests: v1, no-PSI, ZFS, no-iolatency (this host).
4. **lindesk-host.** In-process and stdio transports, manifests, supervision, timeouts; a conformance test suite; the Rust SDK and Python SDK.
5. **Built-ins.**
   - Sensors (psi, cgroup-stat), the rules classifier, the threshold policy with simulated-time tests (no oscillation, recovery ramp, epoch expiry, nothing applied in GREEN beyond static weights).
   - Actuators: systemd, v2raw, v1, proc. iocost enable and restore. The reconciler.
6. **lindesk-store + lindesk-query + lindesk-data.**
   - Writer, compaction, rollups, retention, delete.
   - catalog.duckdb views; `query`, `data …`.
   - Concurrency test: a reader runs queries in a loop while compaction runs, with no duplicates and no errors after retry.
7. **lindeskd.** PSI trigger loop, D-Bus, polkit, state file, watchdog, system plugins.
8. **lindesk-session.** Unit mapping, user actuation, standalone fallback, X11 and COSMIC adapters, user plugins, user store.
9. **GNOME extension and KWin script.**
10. **DX layer.** `plugin new/test/…`, `simulate`, `explain`, external subcommands, completions, man pages, example plugins, docs.
11. **lindesk-bench + `bench/torture.sh`.**
    - The foreground probe runs in an interactive scope. It measures wakeup overshoot (p50/p99/max), 4k O_DIRECT read latency, scope PSI, and GNOME frame overruns.
    - The load is `stress-ng` CPU and VM plus `fio` buffered and direct writes, with an optional build.
    - Runs are A/B. Results go into the store, and `bench compare` renders a markdown table.
12. **Packaging.** cargo-deb, RPM spec (built in a Fedora container), AUR PKGBUILD (Arch container + namcap), `install.sh` (`--prefix`, `--uninstall`), an XDG autostart fallback, compact timers.

**Phase 2, explicitly out of v1:** eBPF sensor, a sched_ext policy (v1 only detects and reports `scx_lavd`), a wlroots adapter, a WASM plugin transport, application QoS SDKs (`qos::user_initiated`), an applet UI, process freezing, and Tao integration.

## Verification

- `cargo fmt --check && cargo clippy -- -D warnings && cargo test`. Core, probe, host and store suites run on fixtures without root.
- Plugin conformance:
  - `lindeskctl plugin test` passes for every example plugin.
  - Deliberately crashing or timing-out test plugins fall back to the built-in and get auto-disabled.
  - A malicious policy that asks for RT or `uclamp.min=1024` is clamped, and the event is recorded.
- `lindeskctl doctor` on this host should report: io.latency not built, iocost available but disabled (→ IO-PARTIAL; FULL once enabled), ext4 writeback supported, sched_ext available, GNOME Wayland, system76-scheduler inactive.
- Live, after running `install.sh` here and `dpkg -i` on the .deb:
  - both services are healthy
  - `lindeskctl watch` follows focus changes
  - `systemctl --user show <scope> -p CPUWeight` changes and then reverts
  - `explain` shows the provenance of each setting
  - after `systemctl stop lindeskd`, every property is restored
- Telemetry:
  - After an hour, `lindeskctl query "select resource, avg(some_avg10) from pressure group by 1"` works.
  - `duckdb -readonly catalog.duckdb` works from outside lindesk.
  - `lindesk-data compact` reduces the file count, and the rollups match the raw aggregates.
  - `data delete --where app_id=…` removes the matching rows from raw and rollup tables.
  - Retention and `max_size` pruning work with a shortened TTL.
- Benchmark: `bench/torture.sh` with lindesk off and on, on GNOME Wayland. Success means foreground p99 wakeup and I/O latency drop sharply, foreground I/O PSI stall drops, and background jobs still progress. Then `simulate` replays the run consistently.
- Adapters:
  - COSMIC: log into a COSMIC session on this machine.
  - X11: Xvfb/Xephyr with a window manager (an automated test).
  - KDE: nested `kwin_wayland --virtual` in a container or VM.
- Packages: the .rpm installs in a Fedora container and the AUR package builds in an Arch container, each followed by a doctor smoke test.
