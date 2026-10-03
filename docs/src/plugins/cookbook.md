# Cookbook

Short recipes, cheapest first. The first two need no code at all.

## Pin Docker builds to batch (config only)

Container workloads run in scopes such as `docker-<id>.scope` under `system.slice`, owned
by the system manager. That makes this a job for the broker's config,
`/etc/lindesk/lindesk.toml`:

```toml
[[override]]
unit = "docker-*.scope"
class = "batch"

[[override]]
unit = "libpod-*.scope"      # podman
class = "batch"
```

Batch work gets `cpu.idle` and a low I/O weight at all times. Under pressure it also gets
`io.max` admission control and, if needed, `memory.high`. Overrides beat every classifier.
Check the result with `lindeskctl explain <container pid>`.

`lindeskctl run --batch docker build .` does **not** help. It only puts the `docker` CLI
in batch, while the build runs inside the daemon's scopes. For rules you'd rather ship as a
file, drop the same match into `/etc/lindesk/rules.d/70-containers.toml`:

```toml
[[rule]]
name = "containers"
class = "batch"
unit = ["docker-*.scope", "libpod-*.scope", "cri-containerd-*.scope"]
```

## Keep a video call smooth while it's in the background (config only)

You are screen-sharing your editor, so the call app isn't focused, but its audio and
video must not stutter when a build starts. Give it a permanent, work-conserving share:

```toml
# ~/.config/lindesk/lindesk.toml (your own apps: the session agent applies it)
[[override]]
comm = "zoom*"                  # or unit = "app-flatpak-us.zoom.Zoom-*.scope"
settings = [
  { knob = "cpu_weight", value = 400 },
  { knob = "io_weight", value = 400 },
  { knob = "memory_low", bytes = 1073741824 },   # 1 GiB of its memory stays resident
]
```

Weights cost nothing while the machine is idle. Avoid `uclamp_min` here: it holds the CPU
clock up for the whole call and drains the battery. Admin settings always win, and the
safety caps still apply (`cpu_weight` ≤ 1000, `memory_low` ≤ 25% of RAM).

## An "on battery" modifier

Run next to `threshold`, win only the knobs you care about, and return nothing on AC. In
Python:

```python
import glob
import lindesk_plugin as lp

def on_battery():
    def read(path):
        try:
            with open(path) as f:
                return f.read().strip()
        except OSError:
            return ""
    ac = any(read(p) == "1" for p in glob.glob("/sys/class/power_supply/*/online"))
    bat = any(read(p) == "Discharging" for p in glob.glob("/sys/class/power_supply/BAT*/status"))
    return bat and not ac

@lp.policy
def evaluate(s):
    if not on_battery():
        return []                    # complete set: nothing, so the host reverts our knobs
    return [lp.intent(u["target"], uclamp_max=50, reason="on battery: batch at reduced clocks")
            for u in s["units"] if u["class"] == "batch" and not u.get("foreground")]

if __name__ == "__main__":
    lp.run(name="battery-saver")
```

```toml
[policy]
priority = 20      # > threshold's 0: our uclamp.max wins
```

The Rust [on-battery](rust.md#writing-a-modifier) example adds caching, peripheral-battery
handling, level tracking (so it never loosens threshold's YELLOW values) and shorter
foreground boosts.

## Export to Prometheus

```sh
cp -rL examples/plugins/python/prometheus-exporter ~/.local/share/lindesk/plugins/
lindeskctl plugin reload
curl -s localhost:9469/metrics | grep lindesk_level
```

It exports gauges for PSI per resource and for the level, settings applied per class, and
event counters by kind. See the
[example's README](https://github.com/dipankar/lindesk/tree/main/examples/plugins/python/prometheus-exporter).
To ship to an OTLP collector instead, the shape is the same: a sink subscribed to
`pressure`, `units` and `events`, with a background thread for the network.

## A notification sink

A sink is a `read` loop. This is the jq-only core of
[notify-on-red](https://github.com/dipankar/lindesk/tree/main/examples/plugins/shell/notify-on-red):

```sh
#!/bin/sh
while IFS= read -r line; do
    method=$(printf '%s' "$line" | jq -r .method) id=$(printf '%s' "$line" | jq -r '.id // empty')
    case $method in
    initialize) printf '{"jsonrpc":"2.0","id":%s,"result":{"name":"notify-on-red","version":"0.1.0","api_version":1,"kinds":["sink"]}}\n' "$id" ;;
    sink.record) printf '%s' "$line" | jq -e '.params.records[] | select(.kind == "level_changed" and .to == "red")' >/dev/null &&
        notify-send lindesk "Background work is yielding to keep your app responsive" ;;
    sink.flush) printf '{"jsonrpc":"2.0","id":%s,"result":null}\n' "$id" ;;
    shutdown) exit 0 ;;
    *) [ -n "$id" ] && printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"no"}}\n' "$id" ;;
    esac
done
```

With `[sink] tables = ["events"]` in plugin.toml. The full example adds rate limiting,
names the focused app, and finds the session bus despite the minimal environment.

## A sensor: GPU utilisation from sysfs

amdgpu exposes `gpu_busy_percent`. A sensor can record it as a metric every two seconds:

```python
import glob
import lindesk_plugin as lp

@lp.sensor
def poll(inp):
    for path in glob.glob("/sys/class/drm/card*/device/gpu_busy_percent"):
        card = path.split("/")[4]                      # "card0"
        try:
            with open(path) as f:
                lp.metric("gpu.busy_percent", int(f.read()), card=card)
        except (OSError, ValueError):
            pass                                       # card went away / not ready
    return []                                          # no PSI samples from this sensor

if __name__ == "__main__":
    lp.run(name="gpu-busy")
```

```toml
[plugin]
kinds = ["sensor"]
[sensor]
interval_ms = 2000
```

Metrics land in the `metrics` table:
`lindeskctl query "select ts, labels['card'], value from metrics where name = 'gpu.busy_percent'"`.
Policies don't see plugin metrics yet. The snapshot carries PSI and per-unit usage only.

## Protect an app's memory, but only what it uses

`memory.low` keeps an app's pages resident when a big build or copy floods the page cache.
A static override reserves memory whether the app uses it or not. A small policy can
follow the app's real usage instead:

```python
import lindesk_plugin as lp

@lp.policy
def evaluate(s):
    caps = lp.config.get("apps", {"code": 2 << 30, "java": 3 << 30})   # comm -> max bytes
    out = []
    for u in s["units"]:
        cap = caps.get(u.get("top_comm"))
        used = (u.get("usage") or {}).get("mem_current", 0)
        if cap and used:
            out.append(lp.intent(u["target"], memory_low=min(cap, used),
                                 reason="keep %s resident (%d MiB)" % (u["top_comm"], min(cap, used) >> 20)))
    return out

if __name__ == "__main__":
    lp.run(name="keep-resident")
```

The safety caps limit `memory.low` to 25% of RAM per target. Changing the value
every tick is cheap, but if you see churn in `explain`, round it to 64 MiB steps.

## A classifier that reads `/proc/<pid>/cmdline`, carefully

`comm` is `java` for both IntelliJ and the Gradle daemon. The command line tells them
apart, but read it with care:

```python
import lindesk_plugin as lp

BATCH_MAINS = ("org.gradle.launcher.daemon.bootstrap.GradleDaemon",
               "org.jetbrains.kotlin.daemon.KotlinCompileDaemon")

@lp.classifier
def classify(proc):
    if proc["pid"] <= 0:                    # slice pseudo-process
        return None
    try:
        with open("/proc/%d/cmdline" % proc["pid"], "rb") as f:
            argv = f.read(8192).split(b"\0")  # bounded read: cmdlines can be huge
    except (FileNotFoundError, ProcessLookupError, PermissionError):
        return None                          # exited already, or hidepid: no opinion
    for main in BATCH_MAINS:
        if main.encode() in argv:
            # The reason ends up in telemetry and `explain`: name the program, never the args.
            return lp.classification("batch", reason="JVM build daemon (%s)" % main.rsplit(".", 1)[-1])
    return None

if __name__ == "__main__":
    lp.run(name="jvm-daemons")
```

```toml
[classifier]
priority = 40
prefilter = { comm = ["java"] }    # only JVMs ever reach the plugin
```

The rules of thumb:

* **Prefilter first**, so you read `/proc` for a handful of processes, not all of them.
* **The process may be gone.** Every `/proc` read can fail: return `None`, don't raise. An
  exception counts as a plugin failure.
* **Bound the read** and stay well inside the 50 ms budget.
* **cmdline is self-reported.** A process can rewrite it, which is fine for a QoS hint and
  wrong for anything security-sensitive.
* **Privacy.** lindesk never stores command lines. Keep it that way: reasons and logs
  should name the program, not its arguments.
