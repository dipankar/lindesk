# Changes: versioned, guardrailed, reversible

Everything that outlives a policy tick, whether a config key, the active profile, a class
override, a VM sysctl, an I/O scheduler, zswap, or the cpufreq governor/EPP, is changed through a
**changeset**. That holds whether the change comes from you (`lindeskctl`), another D-Bus
client, or a local AI agent through the MCP server. A changeset is:

* **checked by guardrails** before anything is touched,
* **planned**: you see a diff (a real TOML diff for config edits),
* **applied write-ahead**: the previous value of every op is recorded first, so a crash
  or a failed write is rolled back,
* **versioned**: monotonic ids, append-only history, config snapshots,
* **reversible**: `revert` restores the recorded previous values in a new changeset,
* and, for agents, **temporary until confirmed**: a dead-man ttl auto-reverts it.

```console
$ lindeskctl change plan sysctl vm.swappiness=10 --set plugins.threshold.config.epoch=800ms
risk: medium
--- a/etc/lindesk/lindesk.toml
+++ b/etc/lindesk/lindesk.toml
@@ -92,7 +92,7 @@
 # auto [500ms]: 300ms on battery, 500ms on AC.
-epoch = "auto"
+epoch = "800ms"
--- a/proc/sys/vm/swappiness
+++ b/proc/sys/vm/swappiness
@@ sysctl vm.swappiness = 10 @@
-60
+10
$ sudo lindeskctl change apply ... --ttl 10m -m "less swapping"
changeset 7 applied (auto-reverts at 14:32 unless confirmed)
$ lindeskctl change confirm 7
$ lindeskctl change list
$ lindeskctl change revert 7
$ lindeskctl change revert-to 4         # undo everything after version 4
$ lindeskctl change diff 4 7            # config file between two versions
```

## Ops

Ops are JSON objects tagged by `"op"`:

| op | fields | scope | risk |
|---|---|---|---|
| `config_set` | `scope`, `key` (dotted, e.g. `plugins.threshold.config.epoch`), `value` | user/system | low |
| `config_unset` | `scope`, `key` | user/system | low |
| `profile_use` | `scope`, `name` (`"auto"` or a profile) | user/system | low |
| `class_override` | `scope`, `unit` (glob), `class` | user/system | low |
| `sysctl` | `key`, `value`, `persist?` | system | medium |
| `zswap` | `enabled?`, `compressor?`, `max_pool_percent?` | system | medium |
| `io_scheduler` | `device` (`MAJ:MIN` or name), `scheduler`, `persist?` | system | high |
| `cpu_epp` | `preference` (all cpufreq policies) | system | high |
| `cpu_governor` | `governor` (all cpufreq policies) | system | high |

User-scope ops go to lindesk-session and only touch your `~/.config/lindesk/lindesk.toml`;
system-scope ops go to lindeskd and need polkit `org.lindesk.manage`.

Runtime ops are runtime-only unless `persist: true`, which also writes
`/etc/sysctl.d/90-lindesk.conf` (sysctl) or `/etc/udev/rules.d/90-lindesk-iosched.rules`
(scheduler). Those files are part of the recorded previous/applied values, so reverting
removes the lines again (and the file when it becomes empty).

## Guardrails

| Rule | What is refused |
|---|---|
| `too_many_ops` / `empty` | more than 16 ops in one changeset, or none |
| `rate_limited` | more than 10 changesets in 10 minutes from the same origin (cli, mcp, dbus, daemon). Reverts never count. |
| `scope` | a system op sent to the user agent and vice versa |
| `config_key` | unknown keys and sections (the same check as `lindeskctl config check`); `[[override]]` is edited with `class_override` |
| `config_invalid` | values that don't parse (`tick = 5`), a config file that is already broken, or a failure in the daemon's plugin/override validation |
| `safety_loosened` | any `[safety]` change that loosens a cap (see below), including unsetting a tightened value |
| `profile_unknown` | a profile that isn't installed |
| `glob` | an empty, whitespace-containing, unbalanced or match-everything unit glob |
| `sysctl_not_allowed` / `out_of_range` | sysctls outside the allow-list, or outside its range |
| `device_unknown` / `scheduler_unavailable` | devices that don't exist, schedulers the device's `queue/scheduler` doesn't offer |
| `epp_unavailable` / `governor_unavailable` | values the CPU doesn't advertise (`energy_performance_available_preferences`, `scaling_available_governors`) |
| `zswap` | compressors outside `lzo lzo-rle lz4 lz4hc zstd 842 deflate` or missing from `/proc/crypto`; `max_pool_percent` outside 1-50 |
| `not_available` | a knob this kernel doesn't have |
| `ttl_required` | a high-risk changeset from MCP without a ttl |

Allow-listed sysctls: `vm.swappiness` 0-200, `vm.page-cluster` 0-3,
`vm.watermark_scale_factor` 10-1000, `vm.vfs_cache_pressure` 50-200.

### `[safety]` may only be tightened

Through changes, each safety cap can move only in its safe direction:

| field | tighter is | because |
|---|---|---|
| `cpu_weight_min`, `io_weight_min` | higher | throttled work keeps a bigger share |
| `cpu_weight_max`, `io_weight_max` | lower | boosts are smaller |
| `uclamp_min_max` | lower | smaller frequency/placement boosts |
| `uclamp_max_floor` | higher | capped work keeps more capacity |
| `io_bps_floor`, `io_iops_floor` | higher | io.max throttles starve less |
| `memory_high_floor`, `memory_high_ratio_floor` | higher | memory.high reclaims less |
| `memory_low_max_ratio` | lower | less RAM can be reserved for protected units |
| `nice_min` | higher | less negative niceness |

The result must also stay coherent (min ≤ max, percentages and ratios in range). Reverting a
tightening is always allowed: it restores a value that was there before. To loosen safety,
edit `lindesk.toml` by hand.

## Dead-man switch

A changeset applied with a ttl is **auto-reverted** when the ttl passes unless you
`confirm` it. The MCP server always passes a ttl (10 minutes by default), and a high-risk
changeset from MCP is refused without one. So an agent can try a change, watch `status` and
the telemetry, and confirm it only if it helped. If the agent disappears, the machine goes
back to how it was. Ttls are capped at 24 hours. Values that a *later* changeset changed
again are left alone by the auto-revert; those belong to the later change.

## History and versions

Changesets live in `/var/lib/lindesk/changes/` (system) and `~/.local/state/lindesk/changes/`
(user):

```text
000007.json          {id, parent, ts, author, origin, description, risk, ops: [{op, …, previous, applied}],
                      status, ttl_until, reverts, reverted_by, config_snapshot}
config/000007.toml   the config file after changeset 7 (000000.toml: before the first one)
HEAD                 7
```

Statuses: `applying → applied → confirmed | reverted`, or `failed`. History is append-only:

* `revert <id>` appends a changeset that writes back the recorded previous values, newest op
  first, and marks the original `reverted`. If a **later** changeset touched the same key,
  the revert is refused (it would silently undo the later change too) with a hint: revert
  the later one first, use `revert-to`, or `--force`.
* `revert-to <version>` unwinds everything after that version in one changeset (including
  reverts and earlier revert-tos), newest first, restoring exactly the values lindesk
  recorded at that version.
* A crash during apply leaves the changeset `applying`; on startup the daemon rolls back
  every op that was (or may have been) written and marks it `failed`.

Every transition is a `Notice` event (`change.applied`, `change.confirmed`,
`change.reverted`, `change.expired`, `change.failed`) with the changeset as data. It lands in
the telemetry lake and in the D-Bus `ChangeEvent` signal:

```console
$ lindeskctl query "select ts, name, data->>'description' from events
                    where name like 'change.%' order by ts desc limit 20"
```

## Example changeset

```json
{
  "id": 1,
  "parent": null,
  "ts": 1800000000000,
  "updated": 1800000000000,
  "author": { "uid": 1000, "origin": "mcp", "client": "claude-code", "bus_name": ":1.42" },
  "origin": "mcp",
  "scope": "system",
  "description": "reduce swapping",
  "risk": "medium",
  "ops": [
    { "op": "sysctl", "key": "vm.swappiness", "value": "10",
      "previous": "60", "applied": "10", "done": true },
    { "op": "config_set", "scope": "system",
      "key": "plugins.threshold.config.values.fg_epoch_uclamp_min", "value": 40.0,
      "previous": "auto", "applied": 40.0, "done": true }
  ],
  "status": "applied",
  "ttl_until": 1800000600000,
  "reverts": null,
  "reverted_by": null,
  "error": null,
  "config_snapshot": "config/000001.toml"
}
```
