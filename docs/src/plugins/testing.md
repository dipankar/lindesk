# Testing and simulation

There are three layers, from fastest to most realistic:

| Layer | Tool | Catches |
|---|---|---|
| Unit tests | `testing::Harness` (Rust), the generated `test_*.py` (Python), `test.sh` (shell) | logic bugs, wire-format mistakes |
| Protocol conformance | `lindeskctl plugin test <dir>` | anything that would get the plugin restarted or disabled in production |
| Replay | `lindeskctl simulate --policy <dir> --from -1h` | bad decisions on *your* machine's real history |

## Unit tests

**Rust**: `lindesk_plugin::testing::Harness` runs your `Server` in-process, but every call
goes through the JSON protocol. Requests are serialized to a line, dispatched, and the
reply is parsed back. Notifications are captured. See [Rust plugins](rust.md#testing-with-harness).

```rust
let mut h = Harness::new(Server::new(MyPolicy::default()).policy());
h.initialize_with(test_context(json!({ "io_threshold": 30 }))).unwrap();
let out = h.evaluate(&snapshot(1_000)).unwrap();
assert!(out.intents.is_empty());
assert_unsupported(h.classify(&proc_info("rustc")));   // not a classifier
```

**Python**: `lindeskctl plugin new` generates `test_<name>.py`, plain `unittest` with no
pytest needed. Its `Plugin` helper starts your plugin as a subprocess and talks JSON-RPC to
it, exactly like the host:

```python
p = Plugin(config={"batch": ["blender"]})
self.assertEqual(p.result("classifier.classify", {"proc": proc})["class"], "batch")
p.notify("sink.record", {"records": [...]})        # sinks: then p.result("sink.flush")
```

**Shell**: pipe lines through the script and grep the output. See
[notify-on-red's `test.sh`](https://github.com/dipankar/lindesk/blob/main/examples/plugins/shell/notify-on-red/test.sh).

**Make I/O roots configurable.** If your plugin reads `/sys` or `/proc`, take the root
from `[config]` (on-battery's `sysfs_root`) so tests can point it at a fixture directory.
lindesk's own `lindesk-sys` crate works the same way.

## Conformance: `lindeskctl plugin test`

```console
$ lindeskctl plugin test ./on-battery
plugin conformance: on-battery 0.1.0 (policy)
  ✓ plugin.toml is valid (0.2ms)
  ✓ initialize handshake (≤2000ms) (1.3ms)
  ✓ initialize reports the manifest's name and kinds (0.0ms)
  ✓ policy.evaluate answers an empty snapshot (≤100ms) (0.4ms)
  ✓ policy.evaluate answers a busy snapshot (≤100ms) (1.0ms)
  ✓ unknown methods get METHOD_NOT_FOUND (-32601) (0.1ms)
  ✓ survives invalid params to policy.evaluate (0.2ms)
  ✓ stdout carries only JSON-RPC messages (0.0ms)
  ✓ exits by itself on shutdown (≤500ms, status 0) (5.1ms)
9 passed, 0 failed
```

The plugin is spawned exactly as the host would spawn it: same environment, same working
directory, and your manifest's `[config]`. Then:

* **manifest**: parses, `api` matches, sections fit `kinds`, `exec` exists and is
  executable, and prefilter globs compile;
* **handshake**: `initialize` answers within `init_ms`, and the reported name and kinds
  match the manifest;
* **per kind**, canned requests whose answers must parse into the protocol types within
  the call timeout:
  * classifiers get a regular process and a slice pseudo-process (`pid: 0`);
  * policies get an empty and a busy snapshot;
  * sensors are polled;
  * sinks get one record of each table, then `sink.flush`;
  * adapters get `adapter.start`;
  * actuators are asked for `caps` and a `read`;
* **robustness**: an unknown method gets `-32601`, and invalid params get an error reply
  without killing the process;
* **hygiene**: nothing but JSON-RPC on stdout;
* **shutdown**: exits by itself with status 0 within 500 ms of `shutdown`.

Nothing is ever applied to the system. Actuators get only `caps` and `read`. When a check
fails, the report shows the last lines of the plugin's stderr and log notifications.

From Rust, the same suite is `lindesk_host::conformance::run_conformance(dir)`, which
returns a `ConformanceReport` (`passed()`, `failures()`, `Display`). The example plugins
use it in one integration test,
[`examples/plugins/rust/on-battery/tests/conformance.rs`](https://github.com/dipankar/lindesk/blob/main/examples/plugins/rust/on-battery/tests/conformance.rs).
`ConformanceOptions::timeout_scale` relaxes the timeouts on slow CI machines.

> Shell plugins: the 500 ms shutdown check is the one to watch. Each `jq`, `sed` or `rm`
> you spawn per message costs time on a loaded machine. Parse the method with parameter
> expansion, and do nothing expensive on `shutdown`.

## Simulate: replay real telemetry through your policy

lindesk records pressure, per-unit usage and classes, focus and every decision into its
telemetry lake. `simulate` turns that history into a test harness for your policy:

```console
$ lindeskctl simulate --policy ./batch-saver --from -1h
replaying 2026-10-03 04:59:59 → 2026-10-03 05:02:10  (120 ticks, 2 units, 360 samples, 1 focus/interaction events)
candidate  ./batch-saver (with threshold)   baseline  threshold

LEVEL
  candidate green 1m00s    yellow 1m10s    red -      
  baseline  green 1m01s    yellow 1m09s    red -      
  recorded  green 34s      yellow 1m37s    red -      

LEVEL CHANGES  candidate
  05:00:30.000  ● GREEN → ● YELLOW  batch-saver
  05:01:40.000  ● YELLOW → ● GREEN  threshold

DIFF  candidate vs baseline (nothing is applied)
  05:00:30.000  lindesk-run-make-1.scope  uclamp.max 30%  was nothing  [batch-saver: io pressure 60%]
  05:00:30.000  lindesk-run-make-1.scope  cpu.weight 5  was nothing  [batch-saver: io pressure 60%]
  05:00:31.000  lindesk-run-make-1.scope  (no cpu.uclamp.max change)  was uclamp.max 40%
  05:01:30.000  lindesk-run-make-1.scope  uclamp.max 40%  was nothing  [threshold: YELLOW: system io pressure 43% > 20%; batch yields; uclamp.max keeps it on E-cores]
  05:01:30.000  lindesk-run-make-1.scope  revert cpu.weight  was nothing

SUMMARY
  actions    candidate 16, baseline 13; 4 only in candidate, 1 only in baseline, 0 with different values
  would have throttled lindesk-run-make-1.scope for 2m10s (baseline 2m10s)

NOTES
  [overruled] threshold: user/1000:lindesk-run-make-1.scope cpu.uclamp.max: cpu.uclamp.max=40.00 overruled by batch-saver
```

How it works:

1. The recorded `pressure`, `units` and `events` rows of the window are read through the
   same query layer as `lindeskctl query`.
2. A `MemoryEnvironment` is rebuilt from the recorded units (with their cgroups, from the
   per-cgroup pressure rows), and each unit's recorded class is pinned.
3. A replay sensor feeds the recorded samples and usage back in timestamp order, the
   recorded focus and interaction events are pushed at their times, and a `SimClock` steps
   the real `Engine` once per recorded timestamp. A recording actuator accepts every
   action: nothing touches the system.
4. Your policy is spawned like any external plugin, with `host = "simulate"` in
   `initialize`, and runs together with the baseline (the built-in `threshold`), exactly as
   in production. The same window is then replayed with the baseline alone, and the two
   runs are diffed per (time, unit, knob). Levels are compared with the recorded ones.

Options:

| Option | Default | |
|---|---|---|
| `--policy` | (required) | a plugin directory, or `builtin:threshold` |
| `--from`, `--to` | `-1h`, `now` | RFC 3339, `YYYY-MM-DD[ HH:MM[:SS]]`, `now`, or `-1h` / `-30m` |
| `--baseline` | `builtin:threshold` | or `none` to see the candidate alone |
| `--config` | your lindesk.toml | `[plugins.*]` settings to use, e.g. tuned thresholds |
| `--system` | | replay the system lake (`privilege = "system"` policies) |
| `--lake` | from the config | any lake directory |
| `--limit` | 40 | action lines to print (`--json` has everything) |
| `--json` | | the whole result: levels, actions, diff, throttle time per unit |

For a policy to replay faithfully:

* **Take time only from `snapshot.now`** and event timestamps, never from the wall clock.
* **Don't read live state** when `context.host == "simulate"`, or make its root
  configurable. on-battery, for instance, reads today's battery, not the battery at the
  replayed time. Point its `sysfs_root` at a fixture to replay "what if I had been on
  battery".
* **No side effects.** Sinks are not fed during a simulation.
