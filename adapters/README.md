# lindesk foreground adapters

lindesk protects the application you are using. To do that it needs two facts from the desktop:

* **focus**: which application has the focused window, as a pid or an app id
* **interaction**: when you are actively using it (typing, clicking, touching, scrolling)

Only the compositor or window manager knows these facts reliably, so each desktop needs an
adapter. Some adapters are built into `lindesk-session` and turned on automatically from
`XDG_CURRENT_DESKTOP`, `XDG_SESSION_TYPE`, `WAYLAND_DISPLAY` and `DISPLAY`
(`lindesk_builtins::adapters::detect`). The rest run inside the shell and push to lindesk
over D-Bus.

| Desktop | Adapter | Mechanism | Focus | Interaction | Extra |
|---|---|---|---|---|---|
| GNOME 45 to 49 (Wayland, X11) | [GNOME Shell extension](gnome-shell-extension/) | `notify::focus-window` and stage input/idle monitor → D-Bus `org.lindesk.QoS1` | pid | yes, ≤ 1 per 250 ms | optional frame-overrun metrics |
| KDE Plasma 5/6 (Wayland, X11) | [KWin script](kwin-script/) | `workspace.windowActivated` → `callDBus` | pid | focus changes only (KWin scripts cannot see input) | |
| COSMIC | built-in `cosmic` | Wayland `ext_foreign_toplevel_list_v1` + `zcosmic_toplevel_info_v1` (v2+) | app_id | no (not exposed to clients) | |
| any X11 session (Xfce, MATE, Cinnamon, i3, Openbox, GNOME/Plasma on Xorg, ...) | built-in `x11` | `_NET_ACTIVE_WINDOW` → `_NET_WM_PID` (+ `WM_CLIENT_LEADER`, children), `WM_CLASS`; XInput2 raw events | pid + WM_CLASS | yes, ≤ 1 per 250 ms | |
| wlroots (sway, Hyprland, river, labwc, Wayfire, ...) | none built in for v1 | use the D-Bus API below with the compositor's IPC | | | `wlr-foreign-toplevel-management` adapter planned |

Built-in adapters are plugins, so they are configured in `lindesk.toml` like any other plugin:

```toml
[plugins.x11]
enabled = true
config = { display = ":0", input = true }   # defaults: $DISPLAY; input on when XInput2 is available

[plugins.cosmic]
config = { display = "wayland-1" }          # default: $WAYLAND_DISPLAY
```

On a GNOME or Plasma X11 session both the built-in `x11` adapter and the shell script may
run. They report the same focus, which is harmless.

## The D-Bus API (`org.lindesk.QoS1`)

`lindesk-session` owns the name `org.lindesk.QoS1` on the **session bus**, object
`/org/lindesk/QoS1`, interface `org.lindesk.QoS1`. Any process in your session may call it.
The full contract is in [`docs/ARCHITECTURE.md`](../docs/ARCHITECTURE.md). Adapters use these
members:

| Member | Signature | Call it when |
|---|---|---|
| `SetForegroundProcess(pid)` | `i` | focus moved to a window owned by `pid` |
| `SetForegroundApp(app_id)` | `s` | focus moved to an app known only by its id (e.g. a Wayland `app_id` / desktop file id) |
| `BeginInteraction(pid)` | `i` | the user is actively interacting; starts or extends a short interaction epoch (about 500 ms). Throttle to ≤ 4 calls per second. |
| `EndInteraction(pid)` | `i` | optional: the interaction ended early |
| `ReportMetric(name, value, labels)` | `sda{ss}` | optional: any numeric metric to store in the `metrics` table |

Calls are fire-and-forget. Don't wait for replies on a hot path, and ignore errors so your
integration keeps working when lindesk is stopped. Prefer the pid when you have it. lindesk
maps a pid to its systemd unit (`app-…​.scope`) through `/proc/<pid>/cgroup`, and an app_id
through the unit name and desktop files.

Try it from a shell:

```sh
busctl --user call org.lindesk.QoS1 /org/lindesk/QoS1 org.lindesk.QoS1 SetForegroundProcess i 1234
busctl --user call org.lindesk.QoS1 /org/lindesk/QoS1 org.lindesk.QoS1 SetForegroundApp s org.mozilla.firefox
busctl --user call org.lindesk.QoS1 /org/lindesk/QoS1 org.lindesk.QoS1 BeginInteraction i 1234
busctl --user introspect org.lindesk.QoS1 /org/lindesk/QoS1
```

## Integrating another shell in about 10 lines

**Hyprland**, through its event socket (`activewindowv2` gives the window address, and
`hyprctl` maps that to a pid):

```sh
#!/bin/sh
sock="$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket2.sock"
socat -U - UNIX-CONNECT:"$sock" | while IFS='>' read -r event _ data; do
  [ "$event" = activewindowv2 ] || continue
  pid=$(hyprctl -j clients | jq -r --arg a "0x$data" '.[] | select(.address == $a) | .pid')
  [ -n "$pid" ] && [ "$pid" -gt 0 ] && busctl --user call org.lindesk.QoS1 /org/lindesk/QoS1 \
      org.lindesk.QoS1 SetForegroundProcess i "$pid" >/dev/null 2>&1 &
done
```

**sway / i3**, through window focus events from the IPC:

```sh
#!/bin/sh
swaymsg -m -t subscribe '["window"]' | jq --unbuffered -r 'select(.change == "focus") | .container.pid' |
while read -r pid; do
  [ "$pid" -gt 0 ] 2>/dev/null && busctl --user call org.lindesk.QoS1 /org/lindesk/QoS1 \
      org.lindesk.QoS1 SetForegroundProcess i "$pid" >/dev/null 2>&1 &
done
```

**Any language with D-Bus bindings.** For example, Python with `gi`:

```python
from gi.repository import Gio, GLib
bus = Gio.bus_get_sync(Gio.BusType.SESSION)
def set_foreground(pid):
    bus.call("org.lindesk.QoS1", "/org/lindesk/QoS1", "org.lindesk.QoS1",
             "SetForegroundProcess", GLib.Variant("(i)", (pid,)), None,
             Gio.DBusCallFlags.NO_AUTO_START, -1, None, None)  # async, errors ignored
```

Run such a script from your compositor's autostart (`exec` in sway, `exec-once` in Hyprland).
For input activity, call `BeginInteraction` from a key or button hook if your compositor
offers one, at most once every 250 ms.

## Writing a built-in adapter

Built-in adapters implement `lindesk_plugin_api::Adapter`. `start(sink)` spawns a thread
that calls `sink(Event::Focus { .. })` / `sink(Event::Interaction { .. })`. `stop()` joins
it, and `connected()` reports the link state. If the desktop isn't present, `start` returns
`PluginError::Unsupported` so the host skips the adapter. See
[`crates/lindesk-builtins/src/adapters/`](../crates/lindesk-builtins/src/adapters/): `x11.rs` and
`cosmic.rs` share the worker/backoff plumbing in `worker.rs` and the `Debouncer`/`Throttle`
helpers in `mod.rs`. External adapters can also be stdio plugins of kind `adapter` (see the
plugin guide).
