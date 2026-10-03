# prometheus-exporter (Python sink)

Serves lindesk's state on `http://127.0.0.1:9469/metrics` for Prometheus, Grafana Agent,
VictoriaMetrics or a plain `curl`. It uses the standard library only: `http.server` runs
in a background thread while the main thread handles the plugin protocol.

| Metric | Type | Labels | From |
|---|---|---|---|
| `lindesk_pressure_some_avg10` | gauge | `resource`, `scope` | `pressure` records (PSI) |
| `lindesk_pressure_full_avg10` | gauge | `resource`, `scope` | `pressure` records |
| `lindesk_level` | gauge | | `level_changed` events: 0 green, 1 yellow, 2 red |
| `lindesk_units` | gauge | `class` | `units` records |
| `lindesk_applied_settings` | gauge | `class` | `units` records: settings lindesk has applied right now |
| `lindesk_events_total` | counter | `kind` | `events` records (focus, interaction, class_set, level_changed, action_applied, ...) |
| `lindesk_exporter_records_total` | counter | | everything received |

Only system-wide pressure is exported by default. `cgroup_pressure = true` adds one
series per tracked unit.

## Sample output

```console
$ curl -s localhost:9469/metrics
# HELP lindesk_pressure_some_avg10 PSI some avg10: percent of time at least one task stalled on the resource.
# TYPE lindesk_pressure_some_avg10 gauge
lindesk_pressure_some_avg10{resource="cpu",scope="system"} 31.2
lindesk_pressure_some_avg10{resource="io",scope="system"} 48.9
lindesk_pressure_some_avg10{resource="memory",scope="system"} 0.4
# HELP lindesk_pressure_full_avg10 PSI full avg10: percent of time all non-idle tasks stalled on the resource.
# TYPE lindesk_pressure_full_avg10 gauge
lindesk_pressure_full_avg10{resource="io",scope="system"} 22.1
lindesk_pressure_full_avg10{resource="memory",scope="system"} 0.0
# HELP lindesk_level Health level from the policies: 0 green (hands off), 1 yellow, 2 red.
# TYPE lindesk_level gauge
lindesk_level 2
# HELP lindesk_units Workloads lindesk tracks, by QoS class.
# TYPE lindesk_units gauge
lindesk_units{class="background"} 1
lindesk_units{class="batch"} 1
lindesk_units{class="critical"} 1
lindesk_units{class="interactive"} 1
lindesk_units{class="normal"} 0
# HELP lindesk_applied_settings Settings lindesk currently has applied, by the QoS class of the workload.
# TYPE lindesk_applied_settings gauge
lindesk_applied_settings{class="background"} 3
lindesk_applied_settings{class="batch"} 3
lindesk_applied_settings{class="critical"} 3
lindesk_applied_settings{class="interactive"} 4
lindesk_applied_settings{class="normal"} 0
# HELP lindesk_events_total Events seen since the exporter started, by kind.
# TYPE lindesk_events_total counter
lindesk_events_total{kind="action_applied"} 37
lindesk_events_total{kind="focus"} 14
lindesk_events_total{kind="level_changed"} 1
# HELP lindesk_exporter_records_total Records received from lindesk.
# TYPE lindesk_exporter_records_total counter
lindesk_exporter_records_total 59
```

A starter alert: `max_over_time(lindesk_level[5m]) >= 2` means "the desktop needed rescuing
in the last five minutes".

## Install

```sh
mkdir -p ~/.local/share/lindesk/plugins
cp -rL examples/plugins/python/prometheus-exporter ~/.local/share/lindesk/plugins/
lindeskctl plugin reload        # or: systemctl --user restart lindesk-session
lindeskctl plugin logs prometheus-exporter   # "serving http://127.0.0.1:9469/metrics"
```

(`lindesk_plugin.py` is a symlink to `sdk/python/`, hence `cp -L`.) Installed as a user
plugin, it sees your session's pipeline. For the system broker's view (system services,
global PSI), set `privilege = "system"` and install it, owned by root, into
`/etc/lindesk/plugins/`.

```yaml
# prometheus.yml
scrape_configs:
  - job_name: lindesk
    static_configs: [{ targets: ["127.0.0.1:9469"] }]
```

## Configuration

```toml
# ~/.config/lindesk/lindesk.toml
[plugins.prometheus-exporter]
config = { listen = "127.0.0.1:9470", cgroup_pressure = true }
```

If the port is taken, the plugin logs `cannot listen on ...` and keeps running without
HTTP rather than crash-looping.

## Test

```sh
python3 test_prometheus_exporter.py   # runs the plugin on an ephemeral port and scrapes it
lindeskctl plugin test .
```

Design note: the subscribed tables are declared once, in `plugin.toml`'s `[sink] tables`.
A plugin can also announce them in its `initialize` reply (`record.tables = [...]` in the
Python SDK), and that list wins.
