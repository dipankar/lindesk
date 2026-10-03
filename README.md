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

## Telemetry you can query with DuckDB

Pressure, per-unit usage, every decision and its reason are written to a hive-partitioned
Parquet lake. The lake is compacted hourly, rolled up into 1m and 1h tables, and pruned by
retention. Data can be deleted on demand.

```bash
lindeskctl query "select resource, avg(some_avg10) from pressure group by 1"
lindeskctl data delete --where "app_id = 'org.mozilla.firefox'"
duckdb -readonly ~/.local/state/lindesk/catalog.duckdb   # any DuckDB client works
```

## Quick start

```bash
# Debian/Ubuntu/Pop!_OS
sudo apt install ./lindesk_*.deb
# Fedora:  sudo dnf install ./lindesk-*.rpm      Arch:  makepkg -si (packaging/aur)
# Anything else:
./packaging/install.sh

lindeskctl doctor        # what your kernel/desktop supports
lindeskctl watch         # live view: level, protected and yielding workloads
lindeskctl run --batch make -j32
lindeskctl explain $(pidof firefox)
```

## Components

| | |
|---|---|
| `lindeskd` | privileged broker (system service): probe, PSI, policy, system units, iocost |
| `lindesk-session` | per-user agent: foreground tracking, user units, user plugins |
| `lindeskctl` | CLI: doctor, status/watch, explain, run/set, plugin tooling, simulate, query/data, bench |
| `lindesk-data` | telemetry maintenance (timer) |

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the contracts between them.

## License

MIT OR Apache-2.0.
