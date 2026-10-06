# Desktop adapters

lindesk protects the app you are using, so it needs two facts that only the desktop knows:

* **focus**: which app owns the focused window, as a pid or an app id;
* **interaction**: when you are actively typing, clicking or scrolling in it.

Adapters turn these into events. The host maps each event's pid or app_id to a systemd
unit through `/proc/<pid>/cgroup` (for an app_id, through the unit name and desktop
files), marks that unit as the foreground, and hands the event to every policy in the next
snapshot. `threshold` starts a ~500 ms interaction epoch on each focus change or input,
which is the short boost behind lindesk's responsiveness.

## What's covered

| Desktop | Adapter | Focus | Interaction |
|---|---|---|---|
| GNOME 45–49 | Shell extension (`adapters/gnome-shell-extension`), pushes over D-Bus | pid | yes, ≤ 1 per 250 ms |
| KDE Plasma 5/6 | KWin script (`adapters/kwin-script`), pushes over D-Bus | pid | focus changes only |
| COSMIC | built-in `cosmic` (Wayland toplevel protocols) + `wayland-input` | app_id | yes, ≤ 1 per 250 ms |
| Hyprland (Omarchy, …) | built-in `hyprland` (its event and request sockets) + `wayland-input` | pid + class | yes, ≤ 1 per 250 ms |
| any X11 session | built-in `x11` (`_NET_ACTIVE_WINDOW`, XInput2) | pid | yes |
| other Wayland compositors (sway, niri, river, …) | none built in yet (planned); a 10-line script using the D-Bus API works today | | |

`wayland-input` reports input activity on any compositor with `ext-idle-notify-v1`: it asks
to be told after 250 ms without input and when input resumes, and reports an interaction
for as long as you keep typing or moving the pointer. It never sees what you type.

The details per desktop, and the script for sway, are in
[`adapters/README.md`](https://github.com/dipankar/lindesk/blob/main/adapters/README.md).

## Three ways to write an adapter

### 1. Push over D-Bus (shell extensions, scripts)

When the code that knows about focus lives *inside* the shell or compositor (a GNOME
extension, a KWin script, a sway IPC loop), call `lindesk-session`'s session-bus API. You
don't need a plugin at all.

| Member | Signature | Call it when |
|---|---|---|
| `SetForegroundProcess(pid)` | `i` | focus moved to a window owned by `pid` |
| `SetForegroundApp(app_id)` | `s` | focus moved to an app known only by its id |
| `BeginInteraction(pid)` | `i` | the user is interacting. Throttle to ≤ 4 calls/s |
| `EndInteraction(pid)` | `i` | optional: the interaction ended early |
| `ReportMetric(name, value, labels)` | `sda{ss}` | optional: any number for the `metrics` table |

These are on bus name `org.lindesk.QoS1`, object `/org/lindesk/QoS1`, interface
`org.lindesk.QoS1`.

```sh
busctl --user call org.lindesk.QoS1 /org/lindesk/QoS1 org.lindesk.QoS1 SetForegroundProcess i 1234
busctl --user call org.lindesk.QoS1 /org/lindesk/QoS1 org.lindesk.QoS1 BeginInteraction i 1234
```

Make the calls fire-and-forget: don't wait for replies on a hot path, and ignore errors so
the desktop keeps working when lindesk is stopped. Prefer the pid when you have one. See
the [D-Bus reference](../reference/dbus.md) for the full interface.

### 2. A stdio plugin of kind `adapter`

When focus can be observed from *outside* the compositor (a Wayland protocol, an IPC
socket, X11), write an adapter plugin. The host sends `adapter.start` once after the
handshake (and again after a restart). Answer it promptly, then emit events from a
background thread whenever they happen:

```python
import json, subprocess
import lindesk_plugin as lp

@lp.adapter
def watch(emit):
    # sway: `swaymsg -r -m` prints one compact JSON object per line per window event.
    # (SWAYSOCK is not passed to plugins; take it from [config] or find it in $XDG_RUNTIME_DIR.)
    cmd = ["swaymsg", "-s", lp.config["swaysock"], "-r", "-m", "-t", "subscribe", '["window"]']
    with subprocess.Popen(cmd, stdout=subprocess.PIPE, text=True) as p:
        for line in p.stdout:
            e = json.loads(line)
            if e.get("change") == "focus" and e["container"].get("pid", 0) > 0:
                emit(lp.focus(pid=e["container"]["pid"], source="sway"))

if __name__ == "__main__":
    lp.run(name="sway-focus")
```

On the wire, each `emit` is an `event` notification:

```json
{"jsonrpc":"2.0","method":"event","params":{"kind":"focus","ts":1700000000123,"pid":4242,"source":"sway"}}
```

Rules of thumb:

* **Throttle interactions** to at most one per 250 ms per pid. The epoch lasts ~500 ms
  anyway.
* **Send `source`** (your adapter's name). It shows up in `explain` and in telemetry.
* **Expect a minimal environment.** Plugins get `PATH`, `HOME`, `LANG` and `XDG_*`,
  including `XDG_RUNTIME_DIR`, but not `WAYLAND_DISPLAY`, `DISPLAY`, `SWAYSOCK` or
  `DBUS_SESSION_BUS_ADDRESS`. Derive what you need: sockets usually live in
  `$XDG_RUNTIME_DIR`, and the session bus is at `$XDG_RUNTIME_DIR/bus`. You can also take
  the path from `[config]`.
* **Reconnect** with a backoff when the compositor restarts, rather than exiting. A
  plugin that exits is restarted, but repeated crashes get it disabled.

In Rust, implement `Adapter::start(&mut self, sink: EventSink)`: spawn a thread, call
`sink(Event::Focus { .. })`, and return. Return `PluginError::Unsupported` when the desktop
isn't present, and the host skips you.

### 3. A built-in adapter

Built-in adapters live in `crates/lindesk-builtins/src/adapters/`. `x11.rs` and
`cosmic.rs` share reconnect/backoff plumbing (`worker.rs`) and `Debouncer`/`Throttle`
helpers. They are picked automatically from `XDG_CURRENT_DESKTOP`, `XDG_SESSION_TYPE`,
`WAYLAND_DISPLAY` and `DISPLAY`. Choose this route for a widely used desktop that deserves
zero-setup support.

## Debugging an adapter

Install the [focus-logger](https://github.com/dipankar/lindesk/tree/main/examples/plugins/python/focus-logger)
example while you work. It logs every focus and interaction event together with the unit
lindesk resolved it to:

```text
focus       pid=48211 app_id=org.gnome.TextEditor     -> user/1000:app-gnome-org.gnome.TextEditor-48211.scope  [source=gnome-shell, 2ms ago]
focus       pid=3141                                  -> NO UNIT (pid/app_id did not map to a managed scope; check /proc/<pid>/cgroup)  [source=my-adapter, 0ms ago]
```

`NO UNIT` usually means one of these:

* the pid lives in a container or in another user's session;
* the pid had already exited;
* you sent the pid of a helper process rather than the window's owner.

focus-logger also reads a FIFO, so you can inject events by hand
(`echo "focus 1234" > $XDG_RUNTIME_DIR/lindesk/focus-logger.fifo`) and drive the pipeline
without a desktop. Then check the result with `lindeskctl explain <pid>`.
