# lindesk GNOME Shell extension

`lindesk@lindesk.org` tells lindesk which application you are using and when you are
interacting with it. It is a thin client for `org.lindesk.QoS1` on the session bus, which
`lindesk-session` owns. When lindesk is not running it does nothing: every call is
asynchronous and errors are ignored.

Supports GNOME Shell 45 to 49 (ESM extensions), on Wayland and X11.

| Shell signal | D-Bus call | Notes |
|---|---|---|
| `global.display` `notify::focus-window` | `SetForegroundProcess(i pid)` | `MetaWindow.get_pid()`; windows without a pid (≤ 0) are skipped. Re-sent when `lindesk-session` (re)appears on the bus. |
| input: `global.stage` `captured-event` (key press, button press, touch begin, scroll) **and** the core idle monitor | `BeginInteraction(i pid)` of the focused window | At most once every 250 ms. The handler is a clock read and a compare, always returns `Clutter.EVENT_PROPAGATE`, and never blocks. |
| `global.stage` `after-paint` (optional) | `ReportMetric("gnome.frame_overruns", n, {"monitor": …})`, `ReportMetric("gnome.frame_max_ms", ms, {"monitor": …})` every 5 s | Off by default: set `FRAME_METRICS = true` in `extension.js`. An overrun is a frame interval above 1.5× the view's refresh interval; gaps longer than 8 refresh intervals count as idle, not dropped frames. |

Why two input sources: on Wayland, mutter delivers input that goes to application windows
straight to the client, and that input never reaches the stage's `captured-event` signal.
So `captured-event` only sees input to shell UI (overview, panel, dialogs). The idle monitor
sees all input: a one-shot "user active" watch fires on the first event after a quiet period,
and then the idle time is sampled every 250 ms until the user goes quiet again. Nothing runs
while the user is idle.

## Install

From the repository root:

```sh
cd adapters/gnome-shell-extension
gnome-extensions pack --force lindesk@lindesk.org        # -> lindesk@lindesk.org.shell-extension.zip
gnome-extensions install --force lindesk@lindesk.org.shell-extension.zip
```

Or copy the directory by hand:

```sh
mkdir -p ~/.local/share/gnome-shell/extensions
cp -r adapters/gnome-shell-extension/lindesk@lindesk.org ~/.local/share/gnome-shell/extensions/
```

GNOME Shell only scans for new extensions at startup. On Wayland, log out and back in; on
X11, press Alt+F2 and run `r`. Then enable the extension:

```sh
gnome-extensions enable lindesk@lindesk.org
gnome-extensions info lindesk@lindesk.org     # State: ACTIVE
```

System-wide installs (packages) use `/usr/share/gnome-shell/extensions/lindesk@lindesk.org/`.

To remove it: `gnome-extensions uninstall lindesk@lindesk.org`.

## Check that it works

```sh
# Shell log (extension errors, if any):
journalctl -f -o cat /usr/bin/gnome-shell

# Watch the calls arrive (lindesk-session running):
busctl --user monitor org.lindesk.QoS1
# or, without lindesk running, just watch the method calls being attempted:
dbus-monitor --session "interface='org.lindesk.QoS1'"

# What lindesk thinks is in the foreground:
lindeskctl status
```

To develop safely without touching your session, run a nested shell:
`dbus-run-session -- gnome-shell --nested --wayland` (GNOME 45 and 46) or
`dbus-run-session -- gnome-shell --devkit` (GNOME 47+).
