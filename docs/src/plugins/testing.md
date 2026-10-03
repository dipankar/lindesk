# Testing and simulation

There are three layers, from fastest to most realistic:

| Layer | Tool | Catches |
|---|---|---|
| Unit tests | `testing::Harness` (Rust), the generated `test_*.py` (Python), `test.sh` (shell) | logic bugs, wire-format mistakes |
| Protocol conformance | `lindeskctl plugin test <dir>` | anything that would get the plugin restarted or disabled in production |
| Replay | `lindeskctl simulate --policy <dir> --from -1h` (planned) | bad decisions on *your* machine's real history |

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

> **Planned.** `lindeskctl simulate` is being written. This section describes the design,
> and the engine side already exists. `Engine::step(now)` is deterministic, and
> `SimClock` and `MemoryEnvironment` exist for this purpose.

lindesk records pressure, per-unit usage and classes, focus and every decision into its
telemetry lake. `simulate` turns that history into a test harness for your policy:

```console
$ lindeskctl simulate --policy ./on-battery --from -1h
replaying 2026-10-03 14:02:11 → 15:02:11 (14 400 ticks, 37 units, 412 focus/interaction events)

LEVEL   green 51m12s   yellow 7m40s   red 1m19s          (same as recorded)

ACTIONS (would apply; nothing is changed)
 14:17:03.250  user/1000:app-cargo-99.scope               cpu.uclamp.max=50   on-battery  on battery (BAT0 64%): batch runs at reduced clocks
 14:17:03.250  user/1000:app-tracker-miner-fs-3-12.scope  cpu.weight=25       on-battery  on battery (BAT0 64%): background yields harder
 ...

DIFF vs threshold alone
 knob             units  ticks changed  example
 cpu.weight           3          9 210  app-tracker-miner-fs-3-12.scope: 50 → 25
 cpu.uclamp.max       1          9 210  app-cargo-99.scope: none → 50%
 cpu.weight (fg)      1            214  app-gnome-org.gnome.TextEditor-77.scope: 500 → 200 (boost cut at 200ms)
```

How it works (planned):

1. The recorded `pressure`, `units` and `events` tables for the window are read through the
   same query layer as `lindeskctl query`.
2. A `MemoryEnvironment` is rebuilt from the recorded units and classes, and a `SimClock`
   steps through the window at the recorded tick rate.
3. Your policy is spawned like any external plugin, with `host = "simulate"` in
   `initialize`. It runs together with the built-in policies, exactly as it would in
   production, in dry-run mode, so nothing is applied.
4. The same window is replayed with the built-in policies alone. The two runs are diffed
   per (unit, knob), and the levels are compared.

Options (planned): `--to` (default now), `--policy` repeated for several plugins,
`--config <lindesk.toml>` to try different thresholds, `--system` for the system lake,
and `--json`.

For a policy to replay faithfully:

* **Take time only from `snapshot.now`** and event timestamps, never from the wall clock.
* **Don't read live state** when `context.host == "simulate"`, or make its root
  configurable. on-battery, for instance, reads today's battery, not the battery at the
  replayed time. Point its `sysfs_root` at a fixture to replay "what if I had been on
  battery".
* **No side effects.** Sinks are not fed during a simulation.
