# Telemetry schema

lindesk records telemetry into a hive-partitioned [Vortex](https://vortex.dev) lake:
`<root>/<table>/date=YYYY-MM-DD/hour=HH/part-<ulid>.vortex` (rollups are partitioned by
day only), with `<root>/_format.json` naming the format and the `vortex` crate version.
Files are immutable; compaction swaps whole partitions atomically. Every table starts with
`schema_version INTEGER` and `ts TIMESTAMP WITH TIME ZONE` (UTC, microsecond precision).
All files of a table share exactly one schema (DuckDB's `read_vortex` has no
`union_by_name`), so a schema change is migrated by compaction rather than mixed. Vortex has
no MAP type: `labels` columns hold a JSON object, queried as `labels->>'host'`.

The definitions live in `lindesk-store/src/schema.rs`; ask the lake itself with
`lindeskctl query "describe units"`.

| Lake | Location | Access |
|---|---|---|
| user | `$XDG_STATE_HOME/lindesk/telemetry` (`~/.local/state/…`) | you (0700) |
| system | `/var/lib/lindesk/telemetry` | root, and the `lindesk` group (read) |

## `pressure`: PSI samples

| Column | Type | |
|---|---|---|
| `scope` | VARCHAR | `system` or a cgroup path |
| `resource` | VARCHAR | `cpu`, `io`, `memory` |
| `some_avg10` | DOUBLE | PSI some avg10, percent |
| `full_avg10` | DOUBLE | PSI full avg10 (NULL for system cpu) |
| `some_delta_us` | BIGINT | some stall time during the interval |
| `full_delta_us` | BIGINT | full stall time during the interval |
| `interval_ms` | BIGINT | wall time covered by the deltas |

## `units`: per-target usage

| Column | Type | |
|---|---|---|
| `manager` | VARCHAR | `system`, `user`, or NULL for non-unit targets |
| `uid` | BIGINT | owner of user-manager units |
| `unit` | VARCHAR | unit name, or `cgroup:<path>` / `pid:<pid>` |
| `class` | VARCHAR | QoS class |
| `foreground` | BOOLEAN | held input focus |
| `cpu_usage_us`, `io_rbytes`, `io_wbytes`, `io_rios`, `io_wios` | BIGINT | used during the interval |
| `mem_current` | BIGINT | memory.current at sample time |
| `interval_ms` | BIGINT | |
| `applied` | VARCHAR | JSON array of settings lindesk had applied |

## `events`

| Column | Type | |
|---|---|---|
| `kind` | VARCHAR | `focus`, `interaction`, `class_set`, `level`, `action_applied`, `action_reverted`, `plugin_error` |
| `unit`, `uid` | VARCHAR, BIGINT | target, flattened like `units` |
| `app_id` | VARCHAR | desktop app id |
| `pid` | INTEGER | |
| `plugin` | VARCHAR | the plugin, policy or adapter responsible |
| `reason` | VARCHAR | short human-readable reason |
| `payload` | VARCHAR | the complete event as JSON (same as the D-Bus `Event` signal) |

## `metrics`

`plugin`, `name`, `value DOUBLE`, `labels VARCHAR` (a JSON object; `labels->>'key'`): whatever plugins report
(`lp.metric(...)`, D-Bus `ReportMetric`).

## Benchmarks: `bench_runs`, `bench_samples`

Written by `lindesk-bench`; never expired by retention, only by `data delete` / `purge`.

## Rollups

Compaction aggregates sealed hours into `pressure_1m`, `pressure_1h`, `units_1m` and
`units_1h` (one row per group per bucket, `ts` = bucket start):

* `pressure_*`: `samples`, `some_avg10_{min,avg,max,p95}`, `full_avg10_{min,avg,max,p95}`,
  summed deltas and `interval_ms`.
* `units_*`: `class` (latest in the bucket), `foreground` (any), `samples`, summed usage
  counters, `mem_current_{min,avg,max,p95}`.

## Retention

`[telemetry.retention]`: `raw` (7 days), `rollup_1m` (90 days), `rollup_1h` (365 days),
and `max_size` (2 GiB, oldest partitions first). See [Telemetry and DuckDB](../guide/telemetry.md).
