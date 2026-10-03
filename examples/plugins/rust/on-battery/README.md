# on-battery (Rust policy modifier)

On battery, background work yields harder and foreground boosts are shorter. On AC power
it does nothing at all.

It is a **modifier**: it runs next to the built-in `threshold` policy rather than
replacing it. lindesk resolves one value per (unit, knob), and the higher priority wins.
on-battery has priority 20 against threshold's 0, and returns intents for only a few
knobs, so it wins exactly those knobs and threshold keeps everything else.

| On battery | threshold says | on-battery says |
|---|---|---|
| background `cpu.weight` | 50 (yellow/red: 20) | 25 (yellow/red: 10) |
| batch `cpu.uclamp.max` | none (yellow/red: 40%) | 50% (yellow/red: 30%) |
| foreground boost | 500 ms after input | 200 ms: from then until threshold's 500 ms, the foreground is put back on threshold's no-input values (`cpu.weight` / `io.weight` 200, no `uclamp.min`) |

A modifier wins every knob it emits, so its values must be at least as strict as
threshold's at every level. on-battery has no level of its own: it follows threshold's
through the `level_changed` events in each snapshot.

Power detection reads `<sysfs_root>/class/power_supply/*`. The machine is on battery when
no AC adapter (`Mains`/`USB`) is `online` and a system battery reports `Discharging`.
Peripheral batteries such as a wireless mouse (`scope = Device`) are ignored.

## Build, test, install

```sh
cargo test -p on-battery                 # unit tests (testing::Harness) + conformance of every example
cargo build --release -p on-battery
lindeskctl plugin test examples/plugins/rust/on-battery

dest=~/.local/share/lindesk/plugins/on-battery
mkdir -p "$dest"
cp examples/plugins/rust/on-battery/plugin.toml "$dest"/
cp target/release/on-battery "$dest"/     # plugin.toml says exec = "on-battery"
lindeskctl plugin reload                  # or: systemctl --user restart lindesk-session
lindeskctl plugin list
```

In the checkout, `exec = "on-battery"` is a small launcher script that runs the newest
`target/{debug,release}/on-battery`. The install above copies the real binary over it.

Installed like this, it is a user plugin, so it tunes your own apps. To also cover system
services (packagekit, fstrim, ...), set `privilege = "system"` and install it, owned by
root, into `/etc/lindesk/plugins/on-battery/`.

## Try it on a desktop

`fixtures/laptop-on-battery` is a fake `/sys` with an unplugged AC adapter, `BAT0` at 64%
and a mouse battery. The plugin runs in its own directory, so a relative path works:

```toml
# ~/.config/lindesk/lindesk.toml
[plugins.on-battery]
config = { sysfs_root = "fixtures/laptop-on-battery" }
```

(and copy `fixtures/` next to the installed plugin.toml). Or talk to the binary directly:

```console
$ U='{"type":"unit","manager":{"kind":"user","uid":1000},"name":'
$ printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"api_version":1,"host":"cli","privilege":"user","uid":1000,"caps":{"uclamp":true},"config":{"sysfs_root":"fixtures/laptop-on-battery"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"policy.evaluate","params":{"snapshot":{"now":1700000000000,"pressure":[],"events":[],"caps":{"uclamp":true},"units":[{"target":'"$U"'"app-tracker-miner-fs-3-12.scope"},"class":"background"},{"target":'"$U"'"app-cargo-99.scope"},"class":"batch"}]}}}' \
  | ./on-battery
{"jsonrpc":"2.0","id":1,"result":{"api_version":1,"kinds":["policy"],"name":"on-battery","tables":[],"version":"0.1.0"}}
{"jsonrpc":"2.0","method":"log","params":{"level":"info","message":"power: BAT0 64% (background work now yields harder)"}}
{"jsonrpc":"2.0","id":2,"result":{"intents":[
  {"priority":0,"reason":"on battery (BAT0 64%): background yields harder","setting":{"knob":"cpu_weight","value":25},
   "target":{"manager":{"kind":"user","uid":1000},"name":"app-tracker-miner-fs-3-12.scope","type":"unit"}},
  {"priority":0,"reason":"on battery (BAT0 64%): batch runs at reduced clocks","setting":{"knob":"uclamp_max","percent":50.0},
   "target":{"manager":{"kind":"user","uid":1000},"name":"app-cargo-99.scope","type":"unit"}}],
 "metrics":[{"name":"on_battery.active","plugin":"","ts":1700000000000,"value":1.0},
            {"name":"on_battery.capacity","plugin":"","ts":1700000000000,"value":64.0}]}}
```

(Line breaks added.) `priority: 0` means "use the `[policy] priority` from plugin.toml",
which is 20 here. Once installed, `lindeskctl explain <pid>` shows which settings came from
on-battery and why.

## Configuration

All keys are in `plugin.toml` under `[config]`, with their defaults. Override them in
`lindesk.toml` with `[plugins.on-battery] config = { ... }`. Unknown keys are rejected at
startup, so typos show up in `lindeskctl plugin list` instead of being silently ignored.

## Code tour

* `src/power.rs` reads the power state from any sysfs root, so tests use fixtures.
* `src/main.rs` contains the plugin, then its unit tests. It uses
  `Server::new(..).policy().with_notifier(..)` rather than `main!` so it can log power
  changes; under `testing::Harness` the same notifier records into the harness.
* `tests/conformance.rs` runs `lindesk_host::conformance` against **every** example plugin
  in `examples/plugins/`.
