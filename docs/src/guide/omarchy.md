# Omarchy and Hyprland

lindesk is meant to drop into [Omarchy](https://omarchy.org) with one command and no
configuration. This page says what it relies on there, what it deliberately leaves alone,
and what has not been measured yet.

> **Status.** The Hyprland adapter is tested against a stand-in for Hyprland's sockets, and
> everything below about Omarchy was checked against its source (v4.0.4, Hyprland 0.56,
> uwsm 0.26, `linux-omarchy` 7.2). It has **not yet been run on an Omarchy machine.** The
> [open questions](#not-measured-yet) at the end are the ones a real install has to answer.
> If you try it, `lindeskctl doctor --json` in an issue is the most useful thing you can
> send.

## Install

```bash
omarchy pkg aur add lindesk        # or: yay -S lindesk
lindeskctl doctor
```

(The AUR package is published with the first public release; until then use the source
install below.)

The package enables the broker (`lindeskd.service`) and installs a systemd user unit for the
per-user agent. Omarchy starts Hyprland through uwsm, which reaches
`graphical-session.target`, so the agent starts with your session: there is nothing to add
to `~/.config/hypr/autostart.lua`, and nothing an Omarchy update or a config reset can
remove. `omarchy update` keeps AUR packages current.

Building from the AUR compiles for a few minutes. Without the AUR:

```bash
git clone https://github.com/dipankar/lindesk && cd lindesk
./packaging/install.sh
```

Then check that it sees you. `lindeskctl doctor` should list
`Desktop adapter   Hyprland (built into lindesk-session)`, and `lindeskctl` should name the
app in front of you as the one it is protecting; `lindeskctl events -f` shows a `focus`
line each time you switch windows.

## Why it fits

| lindesk needs | Omarchy has |
|---|---|
| The focused window's process | Hyprland's sockets give the window's pid; the built-in `hyprland` adapter reads them |
| When you are typing or clicking | `ext-idle-notify-v1` (version 2), read by the built-in `wayland-input` adapter |
| One systemd unit per app | every launch goes through `uwsm-app`, so every app is its own scope under `app-graphical.slice` |
| PSI, cgroup v2, uclamp groups, iocost, `io.max` | all built into `linux-omarchy` |
| A session target to start with | uwsm reaches `graphical-session.target` and exports the session environment |
| A protected compositor | Hyprland runs as `wayland-wm@hyprland.desktop.service` in `session.slice`, which is `critical` |
| No competing priority daemon | none is installed (no ananicy, system76-scheduler, gamemode or scx scheduler) |

Scopes on Omarchy are often named after the launcher rather than the app
(`app-Hyprland-gtk\x2dlaunch-….scope`, `app-Hyprland-xdg\x2dterminal\x2dexec-….scope`).
That doesn't matter: lindesk finds the unit from the focused window's process, not from the
unit's name.

The Omarchy shell (bar, launcher, notifications, lock screen: one Quickshell process) is
started by Hyprland itself and so shares the compositor's unit. It is protected with it.

## What lindesk leaves alone

* **The I/O scheduler.** Omarchy is moving to Kyber on every disk through a udev rule.
  lindesk never changes a scheduler by itself; it turns on the kernel's iocost controller
  underneath, which is what makes `io.weight` work on Kyber, and puts the previous setting
  back when it stops. `lindeskctl doctor` lists the udev rule under "Tuning lindesk didn't
  make" so you know it is there; it is not a problem.
* **power-profiles-daemon, thermald, intel-lpmd.** They manage frequency, heat and core
  parking. lindesk decides who waits when the machine is busy. They don't touch the same
  settings.
* **zram and Omarchy's VM settings** (`swappiness=150`, zram the size of RAM).
  `lindeskctl swap` reads them and will tell you they are sensible.
* **Your Hyprland and shell configuration.** Nothing is written to `~/.config`.

## Things to know

**systemd-oomd watches `app.slice`.** Omarchy kills the largest app when `app.slice` has
been above 50 % memory pressure for 20 s. lindesk's memory work mostly pushes the other
way: `memory.low` on the app you are using and the ones you just left makes reclaim take
from background work first. The one throttle it has, `memory.high` on `batch` work under
sustained memory pressure, applies to package managers and `lindeskctl run --batch`, which
live outside `app.slice`. If you class something inside `app.slice` as `batch` yourself,
its reclaim stalls count toward oomd's limit.

**Full-disk encryption changes what `io.weight` can do.** Omarchy encrypts by default.
dm-crypt hands encrypted *writes* to its own kernel workers, so on the physical disk they
are no longer attributed to the app that made them; *reads* keep their attribution.
`io.weight` therefore protects your app's reads (launching, paging back in, loading a tab)
from other apps' reads, and is weaker against a heavy writer. The adaptive `io.max` limit
that lindesk applies when your app actually stalls is enforced before encryption, on the
mapped device, where writes still belong to their app. This is read from the kernel source,
not measured; see below.

**tmux.** A tmux server lives in the scope of the terminal that first started it. A build
inside tmux is found by what it runs (the `workload` classifier looks for compilers), so it
still yields, but the terminal you are typing in may not be the unit doing the work.

**Snapshots.** `omarchy update` snapshots the root subvolume. Rolling back restores an
older lindesk and its state under `/var/lib/lindesk`; nothing lindesk sets survives a
reboot anyway, so there is nothing to clean up. Your home directory, and any plugins in it,
are not rolled back.

## Plain Hyprland, without Omarchy or uwsm

The adapter works with any Hyprland that keeps its sockets under `$XDG_RUNTIME_DIR/hypr`
(every release of the last couple of years). Two things differ when Hyprland is started
directly instead of through uwsm:

* `graphical-session.target` is never reached, so start the agent from your config:
  `exec-once = systemctl --user start lindesk-session.service` (or
  `hl.exec_cmd("systemctl --user start lindesk-session.service")` in a Lua config).
* Apps started with a plain `exec` all share the compositor's cgroup, and lindesk can only
  treat them as one. Launch them through `uwsm app --` or
  `systemd-run --user --scope --slice=app.slice` and each becomes its own unit.

The agent finds Hyprland even if `HYPRLAND_INSTANCE_SIGNATURE` never reached the systemd
user environment (it looks under `$XDG_RUNTIME_DIR/hypr`), and follows Hyprland across
restarts.

```toml
# /etc/lindesk/lindesk.toml — only if you need to override the defaults
[plugins.hyprland]
config = { instance = "…" }          # default: $HYPRLAND_INSTANCE_SIGNATURE, else the newest instance

[plugins.wayland-input]
enabled = true                        # input activity through ext-idle-notify-v1
```

## Not measured yet

1. The cgroup paths apps really get from the launcher, each keybinding, the browser and
   web apps (`systemd-cgls --user`), and which controllers reach `app-graphical.slice`.
2. Whether `io.weight` isolates writers through dm-crypt on `linux-omarchy`, and whether
   `io.max` on the mapped device throttles as expected.
3. Whether lindesk's `memory.low` protection changes how often oomd's 50 %/20 s limit trips.
4. `ext-idle-notify-v1` with a 250 ms timeout on Hyprland 0.56, and the round trip of the
   focused-window request while the compositor is busy.
5. intel-lpmd on hybrid Intel laptops: whether it restricts CPUs through cgroups.
6. A before/after from `lindesk-bench run` on Omarchy's defaults.
