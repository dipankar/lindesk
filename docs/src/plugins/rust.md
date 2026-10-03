# Rust plugins

The [`lindesk-plugin`](https://github.com/dipankar/lindesk/tree/main/crates/lindesk-plugin)
crate is the whole runtime for an out-of-process Rust plugin. You implement the same
traits the built-in plugins implement (from `lindesk-plugin-api`), and the SDK runs the
stdio loop:

* the handshake, with an API version check;
* routing calls to your trait methods;
* mapping errors to the right codes;
* thread-safe notifications;
* clean shutdown.

The running example is
[`examples/plugins/rust/on-battery`](https://github.com/dipankar/lindesk/tree/main/examples/plugins/rust/on-battery):
a policy that, on battery only, makes background work yield harder and shortens
foreground boosts.

## Scaffold

```console
$ lindeskctl plugin new --kind policy --lang rust on-battery
$ cd on-battery && cargo test && cargo build --release
$ lindeskctl plugin test .
```

The generated `Cargo.toml` depends on `lindesk-plugin` and has an empty `[workspace]`
table, so it builds standalone. Its `plugin.toml` points at `target/release/on-battery`.

## The traits

A plugin is a type that implements `Plugin` plus one trait per kind:

```rust
use lindesk_plugin::prelude::*;   // API types and traits, Server, Notifier, LogLevel

#[derive(Default)]
struct OnBattery { /* config and state */ }

impl Plugin for OnBattery {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: "on-battery".into(),               // must match plugin.toml
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Policy],
            description: env!("CARGO_PKG_DESCRIPTION").into(),
        }
    }

    /// Called once with the merged [config]. An Err refuses to start.
    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> { Ok(()) }
}

impl Policy for OnBattery {
    fn evaluate(&mut self, s: &Snapshot) -> PluginResult<PolicyOutput> {
        // ... return the complete set of intents that should be in force now
        Ok(PolicyOutput::default())
    }
}
```

| Kind | Trait method(s) |
|---|---|
| classifier | `classify(&mut self, &ProcInfo) -> PluginResult<Option<Classification>>` |
| policy | `evaluate(&mut self, &Snapshot) -> PluginResult<PolicyOutput>` |
| sensor | `poll(&mut self, &SensorInput) -> PluginResult<SensorOutput>` |
| sink | `record(&mut self, &[Record])`, optionally `tables()` and `flush()` |
| adapter | `start(&mut self, EventSink)`: spawn a thread and call the sink per event |
| actuator | `caps`, `read`, `apply`, `reset` (system privilege only) |

Errors:

* `PluginError::Failed` is a plain error that doesn't back off;
* `PluginError::Unsupported` means "not for me";
* invalid params are reported as protocol errors for you.

## Serving: `main!` or `Server`

For a `Default` type, one line generates `fn main`:

```rust
lindesk_plugin::main!(OnBattery, policy);            // or (MyPlugin, classifier, sink)
```

The builder does the same, and lets you add more:

```rust
fn main() {
    lindesk_plugin::run(
        Server::new(OnBattery::default())
            .policy()                                       // one call per kind served
            .with_notifier(|p, n| p.notifier = Some(n)),    // keep a Notifier for logs/metrics
    );
}
```

A `Notifier` sends `log`, `metric`, `event` and `samples` notifications. It is cheap to
clone and safe from any thread:

```rust
if let Some(n) = &self.notifier {
    n.info(format!("power: {} (background work now yields harder)", p.describe()));
    n.metric("capacity", 64.0, [("battery", "BAT0")]);
}
```

`lindesk_plugin::notifier()` returns one from anywhere. The notifier from `with_notifier`
is better, because the test harness captures it.

## Config

`ctx.config` is the plugin.toml `[config]` table with the admin's
`[plugins.<name>] config` merged over it, as a `serde_json::Value`. Deserialize it into a
struct and reject unknown keys, so typos fail at startup:

```rust
#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Config { sysfs_root: PathBuf, poll_ms: i64, batch_uclamp_max: f32, /* ... */ }

fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
    let cfg: Config = lindesk_plugin::serde_json::from_value(ctx.config.clone())
        .map_err(|e| PluginError::Failed(format!("[config]: {e}")))?;
    // ...
}
```

`lindesk_plugin::serde_json` is re-exported. You need `serde` as a dependency for the
derive.

## Writing a modifier

on-battery doesn't replace `threshold`. It runs next to it with a higher priority
(`[policy] priority = 20`, against threshold's 0) and only emits the knobs it cares about.
The resolver picks one value per (unit, knob), so on-battery wins those and threshold
keeps the rest. Two rules make a modifier safe:

1. **Never loosen.** You win outright, so your value must be at least as strict as what
   you override, at every level. on-battery follows the level through `level_changed`
   events and uses 10/30% under pressure, where threshold uses 20/40%.
2. **Return nothing when inactive.** On AC power, `evaluate` returns no intents, and the
   host reverts everything on-battery had asked for.

```rust
fn evaluate(&mut self, s: &Snapshot) -> PluginResult<PolicyOutput> {
    let power = self.refresh_power(s.now);           // time from the snapshot, not the clock
    if !power.on_battery() {
        return Ok(PolicyOutput::default());          // complete set: nothing
    }
    let mut intents = Vec::new();
    for u in s.units.iter().filter(|u| !u.foreground) {
        if u.class == QosClass::Batch && s.caps.uclamp {   // skip knobs the host lacks
            intents.push(Intent {
                target: u.target.clone(),
                setting: Setting::UclampMax { percent: 50.0 },
                priority: 0,                         // 0 = [policy] priority from plugin.toml
                reason: format!("on battery ({}): batch runs at reduced clocks", power.describe()),
            });
        }
    }
    Ok(PolicyOutput { intents, ..Default::default() })
}
```

## Testing with `Harness`

`lindesk_plugin::testing::Harness` drives your server in-process through the real wire
path. Every call is serialized to a JSON line, dispatched, and the reply is parsed back. So
the tests catch serde mistakes, wrong error codes and missing kind registrations, not
just logic bugs. Notifications are captured.

```rust
use lindesk_plugin::testing::{Harness, snapshot, test_context, proc_info};

#[test]
fn on_battery_background_and_batch_yield_harder() {
    let tmp = tempfile::tempdir().unwrap();
    laptop(tmp.path(), false);                       // write a fake /sys/class/power_supply
    let mut h = Harness::new(Server::new(OnBattery::default()).policy()
        .with_notifier(|p, n| p.notifier = Some(n)));
    h.initialize_with(test_context(json!({ "sysfs_root": tmp.path(), "poll_ms": 0 }))).unwrap();

    let mut s = snapshot(1_000);                     // empty snapshot at t = 1000 ms
    s.caps.uclamp = true;
    s.units.push(/* a batch UnitState */);
    let out = h.evaluate(&s).unwrap();

    assert_eq!(out.intents[0].setting, Setting::UclampMax { percent: 50.0 });
    assert!(h.logs().iter().any(|l| l.message.contains("BAT0 64%")));
}
```

| Harness method | Does |
|---|---|
| `initialize()` / `initialize_with(ctx)` | the handshake (`test_context(config)` builds a ctx) |
| `classify`, `evaluate`, `poll`, `record`, `flush`, `start_adapter` | one call each |
| `request_raw(method, params)` | any method; returns the raw `Response` (check error codes) |
| `notifications()`, `logs()` | drain what the plugin sent |
| `plugin()`, `plugin_mut()`, `shutdown()` | inspect state; `shutdown` returns the plugin |

`testing::snapshot(now)`, `testing::proc_info(comm)` and `testing::assert_unsupported`
cover the common fixtures.

**Make I/O roots configurable.** on-battery reads `<sysfs_root>/class/power_supply`, with
`sysfs_root = "/sys"` by default. Tests point it at a temporary directory, and the example
ships a checked-in fixture, `fixtures/laptop-on-battery`, which you can also point a real
lindesk at to try the battery path on a desktop.

Finally, conformance-test the binary. In the lindesk repository, one test covers every
example:

```console
$ cargo test -p on-battery
     Running unittests src/main.rs
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
     Running tests/conformance.rs
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

`tests/conformance.rs` calls `lindesk_host::conformance::run_conformance_with` on each
example directory, which is the same check as `lindeskctl plugin test`. It also runs the
Python and shell examples' own test scripts.

## Install

```console
$ cargo build --release
$ dest=~/.local/share/lindesk/plugins/on-battery
$ mkdir -p "$dest" && cp plugin.toml "$dest"/ && cp target/release/on-battery "$dest"/
$ lindeskctl plugin reload
```

Make `exec` in the installed plugin.toml point at the binary. The example uses
`exec = "on-battery"`, next to the manifest. For a system plugin (`privilege = "system"`),
install into `/etc/lindesk/plugins/<name>/`, owned by root and not group- or
world-writable.
