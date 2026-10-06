# Safety invariants

lindesk's principle is **don't make interactive work dangerous; make non-interactive work
yield**. These rules are enforced by the host (`lindesk-core::safety` and the resolver),
never by plugins, so no plugin can break them.

## The invariants

1. **No realtime.** No realtime CPU scheduling class and no realtime I/O priority. `Setting`
   cannot even express `ioprio` RT.
2. **Bounded boosts.** `cpu.uclamp.min` is capped (`uclamp_min_max`, default 50%), weights
   stay within `[cpu|io]_weight_min..max` (default 1..1000), and niceness never goes below
   `nice_min` (default -5).
3. **Floors on every throttle.** `io.max` never goes below `io_bps_floor` (8 MiB/s) or
   `io_iops_floor` (100) per direction; `cpu.uclamp.max` never below `uclamp_max_floor`
   (20%); `memory.high` never below `memory_high_floor` (256 MiB) nor below
   `memory_high_ratio_floor` (50%) of the target's current usage. Nothing is starved.
4. **Critical workloads, lindesk's own services and the focused app can be boosted but never throttled.** An intent that would throttle
   a `critical` target is rejected.
5. **`memory.low` is bounded** to `memory_low_max_ratio` (25%) of RAM.
6. **Runtime-only, always reversible.** Every change is made with runtime semantics
   (systemd `SetUnitProperties(runtime=true)` or direct cgroup writes) and recorded with its
   original value in the applied-state file (`/run/lindesk/state.json`,
   `$XDG_RUNTIME_DIR/lindesk/state.json`). It is restored on exit, after a crash
   (`ExecStopPost=… --restore`), and with `lindeskctl restore`.
7. **Users can only touch their own units.** `lindesk-session` drops intents for anything
   but its user's units, and the broker checks the caller's uid against the pid's owner
   (polkit `set-class-own` vs `set-class-any`).
8. **System plugins must be root-owned.** `lindeskd` loads plugins only from root-owned,
   non-group/world-writable directories.

## What a violation looks like

A clamped value is applied with `clamped_from` in its provenance; a rejected one is not
applied. Both become `plugin_error` events and show up in:

```console
$ lindeskctl explain rustc
user/1000:lindesk-run-cargo-77.scope
  class:   batch ← set explicitly by lindeskctl run
  settings:
    cpu.uclamp.max=20.00   ← my-policy: battery saver (clamped from cpu.uclamp.max=5.00); was default
$ lindeskctl events --kind plugin_error
```

## Tuning the caps

All caps live in `[safety]` of `lindesk.toml` (see [Configuration](../guide/configuration.md)).
Check edits with `lindeskctl config check`.

## Escape hatches

* `lindeskctl restore --stop` stops the session agent and restores everything it changed
  (`sudo lindeskctl restore --system --stop` for the broker).
* `[general] dry_run = true` computes, records and explains intents without applying any.
* `lindeskctl plugin disable <name>` turns a plugin off without uninstalling it.
