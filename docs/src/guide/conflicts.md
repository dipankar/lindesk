# Other priority daemons (conflicts)

lindesk assumes it is the only thing on the machine deciding which work gets the CPU and
the disk. A few other daemons do the same job in their own way. Running one of them next
to lindesk doesn't add up to a better desktop: the two fight over the same knobs.

| Tool | What it changes | lindesk's stance |
|---|---|---|
| system76-scheduler | nice, I/O priority and scheduling policy of most processes; a foreground boost; CFS latency tunables | conflict: stop it (or turn off its process scheduler) |
| ananicy, ananicy-cpp | nice, I/O priority, scheduling policy, `oom_score_adj` per process name; can move processes into its own cgroups | conflict: stop it |
| uresourced | `CPUWeight`, `IOWeight`, `MemoryMin`/`MemoryLow` on the active user's units; optionally a focused-app boost | conflict: stop it |
| gamemode | CPU governor, the game's nice and I/O priority, only while a game asks for it | **not** a conflict: keep it |

## What goes wrong when two of them run

* **Last writer wins.** Both daemons write the same value (a process's nice level, a unit's
  `CPUWeight`), so whichever wrote last decides, and neither knows the other changed it.
  When lindesk exits it restores the values it recorded, which may be the other daemon's.
* **Settings oscillate.** Daemons that re-apply their rules on a timer or on every focus
  change undo lindesk's adjustments, lindesk re-applies them on its next tick, and so on.
* **Processes leave their units.** A daemon that writes PIDs into its own cgroups takes
  them out of the systemd unit they were started in. lindesk attributes work to apps by
  unit, so those processes become anonymous to it; systemd's own `CPUWeight`, `IOWeight`
  and memory protection for the unit no longer cover them either.
* **Boosts double-count.** If both daemons boost the focused app, it gets the two boosts
  stacked: more than either tool's safety limits were designed for, and background work
  is starved harder than either intended.

## system76-scheduler

Pop!_OS's scheduling daemon (`com.system76.Scheduler.service`). It:

* sweeps all processes every `refresh-rate` seconds (60 by default), and on every `exec`
  when `execsnoop` is available, and assigns each one a **nice level, I/O priority and
  optionally a scheduling policy** (`batch`, `idle`, `fifo`, `rr`) from profiles in
  `config.kdl`. The default config renices system services to 12, user session services to
  9, package managers to 15 (`SCHED_BATCH`) and batch work to 19 (`SCHED_IDLE`), mostly
  with the idle I/O class;
* **boosts the foreground**: when told the focused process over D-Bus
  (`SetForegroundProcess`, used by pop-shell), that process and its children get the
  `foreground` profile and everything else the `background` profile (nice 6, idle I/O
  class). Each sweep re-applies the foreground assignment "in case it was overridden";
* gives processes connected to PipeWire their own profile, and the sound server nice -15
  with realtime I/O priority;
* switches **CFS latency tunables** (`latency_ns`, `min_granularity_ns`,
  `wakeup_granularity_ns`, `sched_cfs_bandwidth_slice_us`, `preempt`) between a
  "responsive" profile on AC and the kernel defaults on battery, and turns
  `kernel.sched_autogroup_enabled` off. Its source skips the CFS step when `latency_ns`
  doesn't exist, which is the case on kernels with the EEVDF scheduler (6.6 and later).

It doesn't move processes between cgroups; it only reads `/proc/PID/cgroup` to match
rules.

**Shared with lindesk:** per-process nice and I/O priority (lindesk's `process` actuator and
its `Nice`/`IoPrio` intents), and above all the foreground boost: lindesk raises the
focused app's unit `CPUWeight`/`IOWeight` and `uclamp.min` while system76-scheduler renices
everything that isn't focused. Both re-apply on their own schedule, so their values
oscillate, and the foreground boost is counted twice. One concrete case: lindesk never
uses realtime I/O priority, so if it ever resets a process that system76-scheduler had put
in the realtime I/O class, it restores the default class instead.

**Partial coexistence.** If you want to keep only its CFS profiles, turn off its process
scheduler. `/etc/system76-scheduler/config.kdl` replaces the distribution file
(`/usr/share/system76-scheduler/config.kdl`) entirely, so start from a copy:

```console
$ sudo mkdir -p /etc/system76-scheduler
$ sudo cp /usr/share/system76-scheduler/config.kdl /etc/system76-scheduler/config.kdl
$ sudoedit /etc/system76-scheduler/config.kdl    # process-scheduler enable=true → enable=false
$ sudo systemctl restart com.system76.Scheduler.service
```

With `process-scheduler enable=false` the daemon stops the sweep, the `execsnoop` watcher
and PipeWire monitoring, and the `foreground`/`background` profiles in `config.kdl` aren't
loaded (foreground management only runs when both are defined). This is a partial
measure: profiles in the `process-scheduler/` directories are still read, and its source
can still apply them to the focused app's process tree when pop-shell reports a focus
change. On kernels 6.6 and later the CFS profiles do nothing anyway, so stopping the
service is the simpler and complete fix. `lindeskctl doctor` checks the service, not its
config, so it keeps listing system76-scheduler as running either way.

## ananicy and ananicy-cpp

ananicy ("ANother Auto NICe daemon", Python, `ananicy.service`) and its C++ rewrite
ananicy-cpp (`ananicy-cpp.service`, a drop-in replacement that reads the same rules from
`/etc/ananicy.d`) apply a rule to every process whose name matches. A rule can set:

* `nice`, `ioclass`/`ionice`, `sched` (scheduling policy, including `batch`, `idle` and
  the realtime `fifo`/`rr`) and `oom_score_adj` (both);
* `latency_nice` and a CPU affinity (`cpuset`, with aliases such as `performance-cores`)
  (ananicy-cpp);
* `cgroup`: move the process into a named cgroup with a CPU quota. ananicy only supports
  the cgroup v1 `cpu` hierarchy (it creates `/sys/fs/cgroup/cpu/<name>` and writes the
  thread to `tasks`). ananicy-cpp also supports cgroup v2, where it creates the cgroup
  directly under the cgroup root, sets `cpu.max`, and writes the PID to its `cgroup.procs`.

ananicy scans `/proc` every `check_freq` seconds (10 in its shipped config) and applies
rules to processes it hasn't seen before; ananicy-cpp reacts to process creation events
instead (periodic full scans only with `--manual-scanning`).

**Shared with lindesk:** nice and I/O priority of the same processes (last writer wins), and
scheduling policy: a compiler that an ananicy rule puts in `SCHED_IDLE` and that lindesk
also caps as batch is throttled twice, while rules that raise games, compositors or audio
servers stack with lindesk's own boost for them. Worse, ananicy-cpp's cgroup moves take
processes **out of their systemd units** on cgroup v2: a process in a rule's cgroup (for
example `/sys/fs/cgroup/cpu80`) no longer belongs to any app or service lindesk can see,
and its unit's `CPUWeight`, `IOWeight` and memory protection stop applying to it. With
the default `cgroup_realtime_workaround=true`, ananicy-cpp on cgroup v2 also moves every
process that is already realtime when it sees it to the root cgroup, rule or no rule.

**Partial coexistence.** Both read per-feature switches from their config file
(`/etc/ananicy.d/ananicy.conf`, `/etc/ananicy.d/ananicy-cpp.conf`), for example in
ananicy-cpp:

```ini
apply_nice=false
apply_latnice=false
apply_sched=false
apply_ionice=false
apply_cgroup=false
cgroup_realtime_workaround=false
```

(ananicy has `apply_nice`, `apply_ioclass`, `apply_ionice`, `apply_sched` and
`apply_cgroup`.) What remains is `oom_score_adj` and, in ananicy-cpp, CPU affinity, which
lindesk doesn't touch. At that point the daemon does very little, so stopping it is
usually the better choice.

## uresourced

uresourced (shipped by Fedora Workstation since Fedora 33) gives the active graphical user
more resources by setting systemd unit properties at runtime:

* the system daemon (`uresourced.service`) sets `MemoryMin`, `MemoryLow`, `CPUWeight` and
  `IOWeight` on `user-UID.slice` and `user@UID.service` of the active user (by default
  250M of memory protection, capped at 10% of RAM, and weights of 500), and mirrors the
  allocation onto the user's `session.slice`;
* it installs drop-ins that move some user services (gnome-shell, pipewire, dbus-broker
  and others) into `session.slice` or `app.slice`. These are ordinary `Slice=` settings,
  so the processes stay in systemd units;
* if it was built with app management (off by default in its build options), the user
  daemon (`systemctl --user` unit `uresourced.service`) also **boosts the focused app**:
  it sets `CPUWeight`/`IOWeight` on the app's unit under `app.slice` (300 while focused,
  100 otherwise, plus 200 while the app plays audio or runs a game under gamemode),
  reading focus from an `xdg.inactive-since` extended attribute on the app's cgroup.

**Shared with lindesk:** exactly the same systemd properties (`CPUWeight`, `IOWeight`, memory
protection) on the same user units, set through the same `SetUnitProperties` call. The
focused-app boost is a second foreground boost on the very units lindesk boosts, so the
last writer wins on every focus change and the boosts double-count.

**Partial coexistence.** The app boost reads `[AppBoost]` from `~/.config/uresourced.conf`
(or `/etc/uresourced.conf`); with `ActiveCPUWeight`, `ActiveIOWeight` and the `Boost*Inc`
keys left unset, every app gets weight 100 and the boost does nothing. uresourced can
still write `CPUWeight=100` to an app's unit when its state changes, overwriting lindesk's
value, so disabling uresourced is the reliable option.

## gamemode is not a conflict

[gamemode](https://github.com/FeralInteractive/gamemode) (`gamemoded`) applies its
optimisations only while a game asks for them (games with built-in support, or
`gamemoderun ./game`) and reverts them when the last game exits. While active it can:

* set the CPU governor to `performance` (and the platform profile), restoring the original
  when it ends unless `defaultgov` is configured;
* set the game's I/O priority to best-effort 0 (the default) and renice it (off by
  default: `renice=0`);
* inhibit the screensaver and, depending on its config, pin or park CPU cores and apply
  GPU settings.

None of this fights lindesk. It is temporary and per game, it changes the game's own
processes and power settings such as the governor, while lindesk works with unit weights,
uclamp and I/O limits, and both push in the same direction: the game first. lindesk's [`gaming` profile](profiles.md) is selected
automatically while a game has focus (`steam_app_*`, gamescope, `*.exe`, Lutris), so you
get gamemode's governor and per-process settings plus lindesk's strongest foreground boost
and hard background yielding. Keep both. If you also change the governor through lindesk
(`lindeskctl changes apply --governor ...`), do it while no game is running: gamemode
restores the governor it saw when it started.

## What lindesk does about it

**Detection.** `lindeskctl doctor` and the installer's preflight look for each daemon's unit
file and for its process, and report it as running or as installed but inactive, with the
command that stops it (see [doctor](doctor.md)).

**At install time.** `./packaging/install.sh` asks on a terminal whether to disable each
conflicting daemon it finds. Without a terminal, choose up front:

```console
$ sudo ./packaging/install.sh --disable-conflicts   # disable them
$ sudo ./packaging/install.sh --keep-conflicts      # leave them alone
```

Packages (`.deb`, `.rpm`, AUR) never disable anything silently: they print the command
instead.

**Any time later**, through a [versioned change](changes.md):

```console
$ sudo lindeskctl setup conflicts --disable    # stop and disable the conflicting units
$ lindeskctl changes list                      # it shows up as a service_state changeset
$ sudo lindeskctl setup conflicts --restore    # revert it: the units come back as they were
```

The change records each unit's previous state, so `--restore` (or reverting the changeset)
brings back exactly what was enabled or running before.

**At uninstall.** The installer snapshots the state of every conflicting unit in
`/var/lib/lindesk/install-state.json` when lindesk is installed. `lindesk-uninstall` (or
`./packaging/install.sh --uninstall`) reverts all of lindesk's changesets and then offers to
restore any unit whose state differs from that snapshot, so a daemon you had before
lindesk is enabled again when lindesk goes away.

To disable one by hand instead:

```console
$ sudo systemctl disable --now com.system76.Scheduler.service
$ sudo systemctl disable --now ananicy-cpp.service      # or ananicy.service
$ sudo systemctl disable --now uresourced.service
$ systemctl --user disable --now uresourced.service     # its per-user part
```

## Sources

* system76-scheduler: <https://github.com/pop-os/system76-scheduler> (README,
  `data/config.kdl`, `daemon/src`, `config/src/parser`)
* ananicy: <https://github.com/Nefelim4ag/Ananicy> (README, `ananicy.py`,
  `ananicy.d/ananicy.conf`)
* ananicy-cpp: <https://gitlab.com/ananicy-cpp/ananicy-cpp> (README,
  `src/platform/linux/cgroups.cpp`, `src/worker.cpp`, `include/config.hpp`)
* uresourced: <https://gitlab.freedesktop.org/benzea/uresourced> (README, NEWS,
  `data/uresourced.conf.in`, `data/user/meson.build`, `src/`), and
  <https://fedoraproject.org/wiki/Changes/Reserve_resources_for_active_user_WS>
* gamemode: <https://github.com/FeralInteractive/gamemode> (README,
  `example/gamemode.ini`)
