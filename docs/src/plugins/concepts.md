# Plugin concepts

Every stage of lindesk's pipeline is a plugin. This page covers the model you need before
writing one. It takes about ten minutes, and the tutorials link back here.

## The six kinds

| Kind | Called with | Returns | Built-ins |
|---|---|---|---|
| **sensor** | `sensor.poll {now, targets}` every `interval_ms` | pressure samples, per-unit usage, metrics | `psi`, `cgroup-stat` |
| **adapter** | `adapter.start`, once | nothing; pushes `focus` / `interaction` events whenever they happen | `x11`, `cosmic` (GNOME and KDE push over [D-Bus](adapters.md)) |
| **classifier** | `classifier.classify {proc}` per new or changed unit | a class with a confidence and a reason, or `null` (no opinion) | `rules` (TOML globs) |
| **policy** | `policy.evaluate {snapshot}` every tick | intents, optionally a level, metrics | `threshold` |
| **actuator** | `actuator.read/apply/reset` | the current value / nothing | systemd, cgroup v2, cgroup v1, per-process |
| **sink** | `sink.record {records}`, a notification | nothing | `telemetry` (external, from lindesk-telemetry: the Vortex lake writer) |

One process may serve several kinds. The [focus-logger](https://github.com/dipankar/lindesk/tree/main/examples/plugins/python/focus-logger)
example is both an adapter and a sink.

**Built-in or external.** Built-ins are Rust types that implement the traits in
`lindesk-plugin-api`, running in-process. External plugins are executables that speak the
[stdio protocol](../reference/protocol.md). The host wraps each one in a proxy that
implements the *same* traits, so the pipeline can't tell them apart. External plugins are
crash-isolated: a bug in yours can't take lindesk down.

## Classification

lindesk classifies **units**: systemd scopes, services and slices. Each unit is
represented by its main (or oldest) process, which is what the classifier sees:

```json
{"pid": 4242, "uid": 1000, "comm": "rustc", "exe": "/usr/bin/rustc",
 "cgroup": "/user.slice/user-1000.slice/user@1000.service/app.slice/app-cargo-4242.scope",
 "unit": "app-cargo-4242.scope", "slice": "app.slice"}
```

A slice with no processes of its own arrives as a pseudo-process with `pid: 0`, an empty
`comm` and `unit: "<name>.slice"`. Your classifier must cope with that.

A unit's class is decided in this order, and the first answer wins:

1. an explicit class: `lindeskctl set`, or the `SetClass` D-Bus call;
2. an admin `[[override]]` in `lindesk.toml` with a `class`;
3. classifiers, highest `priority` first, skipping any whose **prefilter** doesn't match
   and any answer below its `min_confidence`;
4. `normal`.

The representative process is re-read every few seconds. If it changes (for example
after an `exec`), the unit is classified again.

### Prefilters

A classifier's `plugin.toml` can declare globs that the host checks before calling it, so
most processes cost your plugin nothing:

```toml
[classifier]
priority = 50
prefilter = { exe = ["*wine*", "*/gamescope*"], unit = ["app-*.scope"] }
```

The lists are `comm`, `exe`, `unit` and `app_id`. A process passes when it matches at
least one glob in **every non-empty list**, so lists narrow the filter (AND) and globs
within a list widen it (OR). `*` also matches `/`. Remember that `comm` is truncated to
15 characters by the kernel.

## Policies and intents

A policy receives a **snapshot** on every tick (250 ms by default):

* `now`;
* system-wide `pressure`;
* `units`, each with its class, a `foreground` flag, its own pressure, its usage and the
  settings currently applied;
* the `events` since the last evaluation (focus, interaction, `level_changed`,
  `class_set`, ...);
* `foreground`;
* `caps`, describing what this machine supports.

It returns **intents**:

```json
{"target": {"type": "unit", "manager": {"kind": "user", "uid": 1000}, "name": "app-cargo-99.scope"},
 "setting": {"knob": "uclamp_max", "percent": 50.0},
 "priority": 0,
 "reason": "on battery: batch runs at reduced clocks"}
```

### Return your complete intent set, every time

Policies are declarative. Each evaluation returns **everything** that should be in force
right now. Anything a policy stops returning is reverted by the host, unless another
policy still asks for it. So you never write "undo" code: when the condition goes away,
stop emitting the intent. A policy may keep its own state between calls (hysteresis,
timers). Take time from `snapshot.now`, never from the wall clock, so that
[`lindeskctl simulate`](testing.md#simulate-replay-real-telemetry-through-your-policy)
can replay it.

### How intents are resolved

All policies run every tick. The resolver then picks **one value per (target, knob)**:

1. **Admin overrides** (`[[override]] settings = [...]` in `lindesk.toml`) always win.
2. Then the **highest priority**. An intent with `priority: 0` gets its plugin's priority:
   `[policy] priority` in plugin.toml, 10 by default, 0 for built-ins. The admin can force
   a plugin's priority with `[plugins.<name>] priority = N`, which then applies to all of
   its intents.
3. Ties go to the plugin name, alphabetically, so the result is deterministic.

Because resolution is per knob, a plugin can act as a **modifier**: it runs next to
`threshold` with a higher priority and emits only the knobs it cares about. The
[on-battery](rust.md) example does exactly that. Its values must be at least as strict as
the policy it overrides, since it wins outright.

### Safety caps

The winning value then passes through the safety caps (`[safety]` in `lindesk.toml`).
These are enforced by the host, not trusted to plugins:

* no realtime CPU or I/O classes (they cannot even be expressed);
* `cpu.weight` / `io.weight` within 1..1000;
* `uclamp.min` ≤ 50%, `uclamp.max` ≥ 20%;
* `io.max` never below 8 MiB/s and 100 IOPS;
* `memory.high` never below 256 MiB or half the current usage;
* `memory.low` ≤ 25% of RAM.

**Critical workloads can be boosted but never throttled**: such an intent is rejected.

A clamped value is applied with `clamped_from` in its provenance. Clamps and rejections
are both recorded as `plugin_error` events and show up in `lindeskctl explain`.

### Provenance

Every applied value remembers the plugin, the reason and the priority that won it:

```console
$ lindeskctl explain rustc
app-cargo-4242.scope (user 1000)   class batch   via classifier:rules (rule "compilers")
  cpu.idle=1           threshold   batch: idle CPU, low I/O share (static, work-conserving)
  io.weight=1          threshold   YELLOW: system io pressure 31% > 20%; batch yields
  cpu.uclamp.max=30    on-battery  on battery (BAT0 64%, under pressure): batch runs at reduced clocks
    overruled: threshold wanted cpu.uclamp.max=40
```

(The exact layout is in [Everyday use](../guide/usage.md#why-explain); `--json` gives the full `Explanation`.)

## Privilege

| | `privilege = "user"` (default) | `privilege = "system"` |
|---|---|---|
| Runs in | `lindesk-session`, as you | `lindeskd`, as root |
| Installed in | `~/.local/share/lindesk/plugins/<name>/`, or the system dirs | `/etc/lindesk/plugins/<name>/` or `/usr/lib/lindesk/plugins/<name>/` |
| May target | only your own user's units (other intents are dropped, with a note) | anything |
| Kinds | all except `actuator` | all |

System plugins load only if the plugin directory, the manifest and the executable are
owned by root and not group- or world-writable. If two directories provide the same
plugin name, the first one wins and the other is reported as shadowed.

## Lifecycle

1. **Discovery**: `<plugin dir>/<name>/plugin.toml` is validated. The `api` version must
   match, the kinds must be known, the sections must fit the kinds, and `exec` must exist
   and be executable. The config file's `[plugins.<name>] config = {...}` is merged over
   the manifest's `[config]`.
2. **Spawn**: in its own directory, as its own process group, with a minimal environment
   (`PATH`, `HOME`, `LANG`, `XDG_*`, `LINDESK_SDK_PATH`, plus `LINDESK_API_VERSION`).
   stdout is the protocol. stderr lines become log lines.
3. **Handshake**: `initialize` with the API version, the host (`lindeskd`,
   `lindesk-session`, `simulate`), the privilege, the uid, the machine's caps and your
   config. You answer with your name, version and kinds. An error here means "refuse to
   start", which is the place to reject bad config.
4. **Calls** for your kinds, and notifications from you at any time: `log`, `metric`,
   `event`, `samples`.
5. **Shutdown**: a `shutdown` notification, then stdin closes. You get 500 ms to exit
   with status 0, then SIGTERM, then SIGKILL. Sinks get a final `sink.flush` first.

## Supervision

Each call has a timeout:

| Method | Default |
|---|---|
| `initialize` | 2 s |
| `classifier.*` | 50 ms |
| `policy.*` | 100 ms |
| `sensor.*` | 200 ms |
| everything else | 1 s |

Override them in plugin.toml with `[timeouts] init_ms = …, call_ms = …`. `call_ms`
applies to every method except `initialize`.

* **Failures** (a timeout, invalid output, a crash, or an error reply other than
  `-32000 failed` / `-32001 unsupported`) put the plugin into exponential **backoff**:
  1 s, 2 s, 4 s, … up to 60 s. The process is restarted lazily on the next call.
* **Five failures within five minutes disable the plugin** until `lindeskctl plugin
  reload`.
* A **plain error** (`-32000`, `-32001`) is counted but doesn't back off. Use it for "I
  can't answer this one".
* Nothing else waits for a broken plugin. A failing classifier means the next classifier
  decides. A failing policy means the others still run, and when it recovers, its intents
  come back.

`lindeskctl plugin list` shows each plugin's health (`running`, `backoff`, `disabled`),
and `lindeskctl plugin logs <name>` shows its recent stderr, `log` notifications and the
host's messages about it (restarts, timeouts, protocol errors).

## Configuration

```toml
# /etc/lindesk/lindesk.toml or ~/.config/lindesk/lindesk.toml
[plugins.steam-games]
enabled = true                 # false: discovered but never started
priority = 60                  # overrides plugin.toml for every intent / classification
config = { webhelper_class = "normal" }   # merged over plugin.toml's [config]
```

Built-in plugins (`threshold`, `rules`, `x11`, …) are configured the same way.
