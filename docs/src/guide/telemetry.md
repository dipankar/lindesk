# Telemetry and DuckDB

lindesk records PSI, per-unit usage, every decision with its reason, and plugin metrics into
a [Vortex](https://vortex.dev) lake that any DuckDB client can read with the `vortex`
extension. Window titles and command lines are never
stored. The tables are described in [Telemetry schema](../reference/schema.md).

| Lake | Where | Who can read it |
|---|---|---|
| yours | `~/.local/state/lindesk/telemetry` (`$XDG_STATE_HOME`) | you |
| system | `/var/lib/lindesk/telemetry` | root and the `lindesk` group (`--system`) |

`telemetry.dir` in the config moves it; `--lake DIR` (or `LINDESK_TELEMETRY_DIR`) points
any command at another lake.

## Query

```console
$ lindeskctl query "select resource, avg(some_avg10) from pressure group by 1"
$ lindeskctl query "from events where kind = 'level' order by ts desc limit 20"
$ lindeskctl query --format csv "select * from units_1h" > units.csv
$ lindeskctl query --format json "select unit, sum(io_wbytes) w from units group by 1 order by w desc limit 5"
$ lindeskctl query --format parquet --out pressure.parquet "from pressure"
$ lindeskctl query --system "select count(*) from events"
```

Queries run in a sandboxed, in-memory DuckDB that can see only the lake. lindesk reads the
`.vortex` files with DuckDB's `vortex` core extension, which it loads from (and the first
time installs into) `duckdb-ext/` next to the lake; that needs network access once. Without
it (offline, unsupported platform) the files are read by the Rust `vortex` crate instead:
everything still works, only slower, except `data catalog`. `lindesk-data install-extension`
installs it explicitly; `--backend native` forces the fallback. Only read-only
statements are accepted (SELECT, WITH, FROM, VALUES, DESCRIBE, SHOW, SUMMARIZE).
`--json` is the same as `--format json`.

Which tables and columns exist:

```console
$ lindeskctl query "show tables"
$ lindeskctl query "describe units"
```

## Any DuckDB client

```console
$ lindeskctl data catalog                     # (re)generate catalog.duckdb: views only
$ duckdb -readonly ~/.local/state/lindesk/catalog.duckdb -c "INSTALL vortex; LOAD vortex; FROM pressure LIMIT 5"
$ lindeskctl data shell                       # the same, if the duckdb CLI is installed
```

The catalog contains only views over the `read_vortex` globs, so it never goes stale as files
are added, and notebooks or BI tools can open it read-only. DuckDB does not autoload the
`vortex` extension for a view, so every client session starts with `INSTALL vortex; LOAD
vortex;` (`lindesk-data catalog` prints this; `FROM lindesk_info` shows it too).

Lakes written by lindesk before the switch to Vortex contain `.parquet` files. The views read
both formats, and the next `compact` converts every remaining Parquet partition to Vortex
(`data stats` reports how many legacy files remain).

## Maintenance

`lindesk-compact.timer` (system and user) runs `lindesk-data maintain` hourly: it merges
each sealed hour's small files, converts any legacy Parquet partitions, refreshes the
1-minute and 1-hour rollups, and applies retention. The same operations, by hand:

```console
$ lindeskctl data stats        # size, files, rows and time range per table
$ lindeskctl data compact
$ lindeskctl data prune        # [telemetry.retention] raw / rollup_1m / rollup_1h / max_size
```

## Deleting data

```console
$ lindeskctl data delete --where "app_id = 'org.mozilla.firefox'"
134 matching rows: events 134
Delete 134 rows from ~/.local/state/lindesk/telemetry? [y/N] y
✓ deleted 134 rows (3 partitions rewritten)

$ lindeskctl data delete --where "unit like 'app-steam%'" --since 2026-10-01 --dry-run
$ lindeskctl data delete --where "kind = 'focus'" --table events --yes
$ lindeskctl data purge        # everything
```

`delete` counts first, then asks on a terminal; without a terminal it refuses unless you
pass `--yes`. It rewrites only the partitions that contain matching rows, in raw tables and
rollups alike, and swaps them in atomically, so concurrent readers never see duplicates.
`--since` / `--until` take RFC 3339, `YYYY-MM-DD[ HH:MM]` or a duration ago (`-7d`).

To stop recording altogether: `[telemetry] enabled = false`.

## Replaying it: `simulate`

Your history is also a test harness. `lindeskctl simulate` replays a window of the lake
through a policy on simulated time and shows what it would have done. See
[Testing and simulation](../plugins/testing.md#simulate-replay-real-telemetry-through-your-policy).
