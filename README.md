# lindesk

**macOS-style QoS for the Linux desktop.** lindesk keeps the app you are using responsive
while compiles, copies, indexers and package upgrades run in the background. It needs no
kernel patch and works on any distro.

```text
before:  cargo build -j32 + 40 GB copy  →  desktop stutters, typing lags
after:   same build, same copy          →  build and copy slow down a little,
                                           the desktop stays smooth
```

Linux already has the mechanisms: PSI, cgroup v2 `io.latency` / iocost / `io.max`,
`memory.low`, uclamp, systemd's `session.slice` / `app.slice` / `background.slice`,
sched_ext. What it lacks is a **control plane** that connects *"the human just interacted
with this app"* to those knobs. lindesk is that control plane.

## How it works

1. **It knows what you're doing.** Desktop adapters (GNOME, KDE, COSMIC, X11) report the
   focused app and input activity. Every workload gets a semantic class: `critical`,
   `interactive`, `normal`, `background` or `batch`.
2. **It does nothing while the machine is healthy.** It applies only work-conserving
   weights, which cost nothing when there's no contention.
3. **It steps in gradually when the foreground suffers.** PSI pressure moves a
   GREEN → YELLOW → RED controller, with hysteresis. Background work yields step by step:
   lower weights, then uclamp caps, then adaptive `io.max` admission control (AIMD).
4. **Interaction epochs.** For about 500 ms after you click or type, the app you're using
   gets a short, bounded boost. That boost is not permanent, which saves battery.
5. **It never makes interactive work dangerous.** No realtime classes. Every throttle has a
   floor. Critical processes (compositor, audio) can be boosted but never throttled. Every
   change is runtime-only and reverted on exit or crash.

## Profiles & hardware awareness

Great defaults out of the box: lindesk detects the CPU (x86_64/aarch64, hybrid P/E or
big.LITTLE cores, EAS, cpufreq/EPP), RAM and swap, each disk's kind, the power source and
virtualization, and scales its thresholds to them. A **profile** (an overlay on the
config) is picked automatically: `battery` when unplugged, `gaming` while a game has focus,
`hdd` on spinning disks, `x86-hybrid` / `arm-eas` on hybrid CPUs, else `balanced`.

```bash
lindeskctl profile list        # which profile is active, which match, and why
lindeskctl profile use gaming  # pin one (a versioned change; `use auto` to undo)
lindeskctl hardware            # what lindesk detected and how it adapts
lindeskctl calibrate           # measure the latency map: P vs E speed, wakeup p99, disk 4K p99
lindeskctl swap                # zram/zswap/swappiness analysis with ready-to-run fixes
```

## Everything is a plugin

```text
adapters ─┐
sensors ──┼─► classifiers ─► policies ─► resolver + safety caps ─► actuators
          └─────────────── records ────────────────────────────► sinks
```

The built-ins use the same API that you do. You can write a plugin in **any language**:
it's an executable that speaks newline-delimited JSON-RPC on stdio.

```bash
lindeskctl plugin new --kind classifier --lang python steam-games
lindeskctl plugin test ./steam-games          # protocol conformance
lindeskctl simulate --policy ./my-policy --from "-1h"   # replay real telemetry through it
```

```python
from lindesk_plugin import classifier, run

@classifier
def classify(proc):
    if proc["comm"].endswith(".exe"):
        return {"class": "interactive", "reason": "proton game"}

run()
```

There are SDKs for [Rust](crates/lindesk-plugin) and [Python](sdk/python), and
[example plugins](examples/plugins). The protocol is specified as
[JSON Schema](docs/src/reference/protocol.md) (`lindeskctl plugin schema`).

## Optional: telemetry & DuckDB queries

With the optional `lindesk-telemetry` package, pressure, per-unit usage, every decision and
its reason are written to a hive-partitioned [Vortex](https://vortex.dev) lake, read through
DuckDB's `vortex` extension (with a pure-Rust fallback when it is unavailable). The lake is
compacted hourly, rolled up into 1m and 1h tables, and pruned by retention. Data can be
deleted on demand. The writer is just another plugin (`telemetry`), so the core package
links neither DuckDB nor Vortex and stays small; without telemetry lindesk works the same,
minus the history.

```bash
lindeskctl query "select resource, avg(some_avg10) from pressure group by 1"
lindeskctl data delete --where "app_id = 'org.mozilla.firefox'"
duckdb -readonly ~/.local/state/lindesk/catalog.duckdb -c "INSTALL vortex; LOAD vortex; FROM lindesk_tables"
```

## For AI agents (MCP)

`lindesk-mcp` lets a local agent assess why your desktop stutters and fix it, safely:

```bash
claude mcp add lindesk -- lindesk-mcp      # Claude Code; others: lindeskctl mcp config --client cursor
```

* **Read-only by default**: status, doctor, hardware, latency map, swap analysis, telemetry
  SQL, ranked recommendations.
* **Every change is guardrailed**: allow-listed sysctls with ranges, safety caps can only
  tighten, rate limits, polkit for system scope. lindesk-mcp itself has no privileges.
* **Versioned and auto-reverting**: each change records the previous values and reverts
  itself after 10 minutes unless confirmed. You see and undo everything:

```bash
lindeskctl changes list                    # who changed what, when; ttl left
lindeskctl changes revert 7                # undo (or: changes confirm 7 to keep it)
lindeskctl changes apply --sysctl vm.swappiness=10 --ttl 10m -m "less swapping"   # same, by hand
```

## Quick start

Install **`lindesk`**: the daemons, CLI, MCP server and bench, a few MB each.

```bash
sudo apt install ./lindesk_*_amd64.deb        # Debian/Ubuntu/Pop!_OS
# Fedora:  sudo dnf install ./lindesk-0*.rpm      Arch:  makepkg -si (packaging/aur)
# Anything else:
./packaging/install.sh

lindeskctl doctor        # what your kernel/desktop supports, and the one next step
lindeskctl               # what lindesk is doing right now
lindeskctl watch         # live dashboard: level, PSI, protected and yielding work, events
lindeskctl run --batch make -j32
lindeskctl explain firefox
lindeskctl calibrate     # optional: measure this machine's latency baseline (≈15 s)
```

On, off, or a quick A/B:

```bash
lindeskctl on | off              # off = back to the machine's default behaviour (config and history kept)
lindeskctl pause --for 10m       # revert everything, keep observing; resumes by itself
lindeskctl resume
sudo lindesk-uninstall [--purge] # restore everything lindesk changed, then remove it
```

Optional add-on: **`lindesk-telemetry`** records history you can query with SQL (DuckDB +
Vortex; much larger). It is never pulled in automatically, and it is picked up without
configuration:

```bash
sudo apt install ./lindesk-telemetry_*_amd64.deb   # or: ./packaging/install.sh --with-telemetry
```

## Other priority daemons (conflicts)

| Tool | What it changes | lindesk's stance |
|---|---|---|
| system76-scheduler | nice, I/O priority and scheduling policy of most processes; foreground boost; CFS tunables | conflict: stop it |
| ananicy, ananicy-cpp | nice, I/O priority, scheduling policy, `oom_score_adj` per process name; can move processes out of their units into its own cgroups | conflict: stop it |
| uresourced | `CPUWeight`, `IOWeight`, memory protection on the active user's units; optionally a focused-app boost | conflict: stop it |
| gamemode | CPU governor, the game's nice and I/O priority, only while a game runs | not a conflict: keep it |

Two daemons tuning the same knobs undo each other's work and stack their foreground boosts.
`lindeskctl doctor` and the installer detect them; `sudo lindeskctl setup conflicts --disable`
turns them off as a versioned change that `--restore` (or uninstalling lindesk) reverts.
Packages never disable anything silently. Details and partial-coexistence options:
[docs/src/guide/conflicts.md](docs/src/guide/conflicts.md).

## Components

| | |
|---|---|
| `lindeskd` | privileged broker (system service): probe, PSI, policy, system units, iocost |
| `lindesk-session` | per-user agent: foreground tracking, user units, user plugins |
| `lindeskctl` | CLI: doctor, status/watch/events, explain, run/set/restore, profile, changes, hardware/swap/calibrate, mcp, plugin tooling, simulate, query/data, config, bench |
| `lindesk-mcp` | MCP server for local AI agents (stdio; optional user socket unit) |
| `lindesk-bench` | foreground latency probe and A/B harness |
| `lindesk-data` | *lindesk-telemetry*: the lake's query, export/import and maintenance tool (DuckDB; timer) |
| `lindesk-telemetry-sink` | *lindesk-telemetry*: the `telemetry` plugin that writes the lake |

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the contracts between them and
[docs/CONTROL-PLANE.md](docs/CONTROL-PLANE.md) for profiles, changes and MCP.

## License

MIT OR Apache-2.0.
