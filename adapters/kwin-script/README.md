# lindesk KWin script

Reports the focused window's pid to lindesk on KDE Plasma (Wayland and X11). On every active
window change it calls `org.lindesk.QoS1.SetForegroundProcess(i pid)` on the session bus with
KWin's `callDBus`. The call is asynchronous; if lindesk is not running it fails silently.

* KWin 6 (Plasma 6): `workspace.windowActivated` and `window.pid`.
* KWin 5 (Plasma 5): falls back to `workspace.clientActivated` and `client.pid`.

**Only focus is reported on KDE.** KWin scripts cannot see input events, so the script cannot
send `BeginInteraction`. lindesk starts an interaction epoch on each focus change, and nothing
extends it while you keep typing in the same window. Applications can still call
`BeginInteraction` themselves.

## Install

```sh
# Plasma 6
kpackagetool6 --type=KWin/Script -i adapters/kwin-script/lindesk
# upgrade after changes:
kpackagetool6 --type=KWin/Script -u adapters/kwin-script/lindesk

# Plasma 5
kpackagetool5 --type=KWin/Script -i adapters/kwin-script/lindesk
```

This installs the script to `~/.local/share/kwin/scripts/lindesk/`.

## Enable

Use System Settings → Window Management → KWin Scripts → "lindesk", or the command line:

```sh
kwriteconfig6 --file kwinrc --group Plugins --key lindeskEnabled true
qdbus org.kde.KWin /KWin reconfigure          # qdbus6 on some distributions
```

To disable it, set the key to `false` and reconfigure again. To uninstall it:
`kpackagetool6 --type=KWin/Script -r lindesk`.

## Check that it works

```sh
# KWin's script output (print() and errors):
journalctl --user -f -o cat | grep -i kwin
# The calls themselves:
dbus-monitor --session "interface='org.lindesk.QoS1'"
```

`callDBus` converts JavaScript numbers to D-Bus types itself. `window.pid` is an integer
property, so it is sent as `i`. If `dbus-monitor` shows the argument as `double`, your KWin
build converts numbers differently. Please report it.
