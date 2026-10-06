# lindesk

**macOS-style QoS for the Linux desktop.** lindesk keeps the app you are using responsive
while compiles, copies, indexers and package upgrades run in the background. It needs no
kernel patch and works on any distro.

**The goal: a Linux desktop that stays as responsive under load as a Mac does.**
[How lindesk keeps the desktop responsive](guide/responsiveness.md) lists every mechanism
that serves it, with its defaults, and where lindesk still falls short of macOS.

```text
before:  cargo build -j32 + 40 GB copy  →  desktop stutters, typing lags
after:   same build, same copy          →  build and copy slow down a little,
                                           the desktop stays smooth
```

## The thesis

Linux already has the mechanisms: pressure stall information (PSI), cgroup v2 weights and
limits (`cpu.weight`, `io.weight`, `io.max`, `memory.low`, `memory.high`), uclamp, iocost,
and systemd's `app.slice` / `background.slice` layout. What it lacks is a **control plane**
that connects *"the human just interacted with this app"* to those knobs. macOS has one in
its QoS classes. lindesk is that control plane for Linux.

The principle: **don't make interactive work dangerous; make non-interactive work yield.**
lindesk never gives anything a realtime class or an unbounded boost. It protects the
foreground by asking everything else to step back, within floors that keep background
work progressing.

## How it works

Every workload (a systemd scope or service) gets one of five classes:

| Class | Examples | Under pressure |
|---|---|---|
| `critical` | compositor, PipeWire, the shell | protected, never throttled |
| `interactive` | the app you are using | boosted briefly after each interaction |
| `normal` | other open apps | left alone |
| `background` | sync, indexers | lower weights, then CPU caps |
| `batch` | builds, package upgrades, backups | idle CPU, then I/O admission control |

A pipeline of plugins does the rest:

```text
adapters ─┐   (focus, input)
sensors ──┼─► classifiers ─► policies ─► resolver + safety caps ─► actuators
          │   (PSI, usage)   (class)     (intents)  (one value per knob)  (cgroups, systemd)
          └──────────────────────── records ──────────────────────────► sinks
                                                                (Vortex telemetry, Prometheus, …)
```

1. **Adapters** report which app has focus and when you type or click (GNOME, KDE, COSMIC
   and X11 are covered).
2. **Sensors** measure pressure: how much time tasks spend waiting for CPU, I/O and memory.
3. **Classifiers** give each workload its class (TOML rules ship by default).
4. **Policies** look at a snapshot of all this and return **intents**: "this unit should
   have `cpu.weight = 20`, because...". They never touch the system themselves.
5. The **resolver** picks one value per (unit, knob). Admin overrides win, then the higher
   priority. **Safety caps** clamp the result, and every applied value keeps its
   provenance for `lindeskctl explain`.
6. **Actuators** apply the values as runtime-only changes, remember the originals, and
   restore them on exit or crash.
7. **Sinks** receive everything for telemetry. The built-in one writes a Vortex lake you
   can query with DuckDB.

## Do nothing while healthy

The default policy, `threshold`, is a GREEN → YELLOW → RED controller with hysteresis:

* **GREEN** (almost always): only work-conserving weights, which change nothing until
  there is contention. Batch work gets `cpu.idle` and a low I/O weight. After you click or
  type, the focused app gets a short boost (about 500 ms) that is not held after that, so
  it costs no battery.
* **YELLOW** (system pressure rising, or the foreground starting to stall): background and
  batch yield harder, with lower weights and `uclamp.max` caps.
* **RED** (the foreground itself is stalling): `io.max` admission control on background
  and batch I/O, which halves while you suffer and recovers additively. Batch gets
  `memory.high` under sustained memory pressure.

It steps back down one level at a time, only after the pressure has stayed low for a
while, so levels don't flap.

## Everything is a plugin

The built-ins use the same API as third-party plugins. A plugin is any executable that
speaks newline-delimited JSON-RPC on stdin/stdout, so a classifier can be 20 lines of
Python and a sink can be a shell script:

```python
import lindesk_plugin as lp

@lp.classifier
def classify(proc):
    if proc["comm"].endswith(".exe"):
        return lp.classification("interactive", reason="Proton game")

lp.run(name="steam-games")
```

Start with [Plugin concepts](plugins/concepts.md), or go straight to [your first plugin](plugins/python.md).
