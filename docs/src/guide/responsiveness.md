# How lindesk keeps the desktop responsive

**The goal is simple to state: a Linux desktop that stays as responsive under load as a Mac
does.** You should be able to start a build, a backup and a container stack, switch to your
browser, and not be able to tell from the way it scrolls.

macOS gets there because every piece of work carries a quality-of-service class, and the
whole system (CPU scheduler, I/O, memory, timers) honours it. Linux has the same machinery
in the kernel, but nothing connects it to *what the person is doing*. lindesk is that
connection. This page lists everything it does to keep the promise, in the order you'd
notice it, with the defaults and where to see each one working.

## The short version

| When | What lindesk does | See it |
|---|---|---|
| Always | Compositor, audio and the session stay protected; background services and builds get a smaller share | `lindeskctl explain <app>` |
| You click or type | The focused app gets a strong, bounded boost for 500 ms | `lindeskctl watch` (⚡) |
| You switch apps | The app you switched to gets that boost for 3 s, and whatever of it is in swap is read back in | `lindeskctl events -f` |
| You leave an app | It stays *warm* for 30 minutes: memory protected, raised share | `lindeskctl explain <app>` → `warm:` |
| You put an app away | Off screen and silent for 2 minutes, it *naps*: timers coalesced, efficient cores | `lindeskctl explain <app>` → `nap:` |
| The machine gets busy | Background and batch work yield: lower weights, CPU caps | level **YELLOW** |
| Your app actually stalls | Background I/O is limited, tightening until the stall stops | level **RED** |
| Pressure passes | Everything is released, one step at a time | level back to **GREEN** |
| lindesk exits, crashes or is removed | Every setting goes back to what it was | `lindeskctl off`, `lindesk-uninstall` |

## 1. Knowing which app is you

Nothing else works without this. A **foreground adapter** reports the window you're using
and your input:

| Desktop | How |
|---|---|
| GNOME (Wayland, X11) | a Shell extension (`lindesk@lindesk.org`); also reports key, pointer and touch input |
| KDE Plasma | a KWin script |
| COSMIC | built into `lindesk-session` (toplevel-info protocol; input from `ext-idle-notify`) |
| Hyprland (Omarchy) | built into `lindesk-session` (Hyprland's sockets give the window's pid; input from `ext-idle-notify`) |
| any X11 session | built into `lindesk-session` (EWMH), with XInput2 for input |

The window's process is mapped to its systemd unit (the app's scope), and that unit is what
gets tuned. An app isn't always one unit: Chromium-based browsers (Chrome, Wavebox, Edge,
Electron apps) put the window's process in one scope and their renderer and GPU processes
in another. lindesk follows the focused process's descendants that run the same program,
so every unit the app runs in is foreground: all of them get the boost, none is throttled,
and a stall in any of them counts as you waiting. The set is refreshed every 5 s as tabs
open and close. Descendants running a *different* program are not the app: every tab of a
terminal is a child of the terminal, and a build in one of them is still a build. On GNOME Wayland the extension loads at the next login after the first install;
`lindeskctl status` says `foreground tracking: waiting for GNOME extension` until then.

## 2. Classes

Every unit has one of five classes. Rules decide (`/usr/share/lindesk/rules.d`, overridable
in `/etc/lindesk/rules.d` and `~/.config/lindesk/rules.d`), then the `workload` classifier,
then any classifier plugin you add; `lindeskctl set` and `[[override]]` win over all of them.

| Class | What it is | Standing treatment (work-conserving, costs nothing when idle) |
|---|---|---|
| `critical` | compositor, Xwayland, PipeWire, portals, input methods, `session.slice` | `cpu.weight` and `io.weight` 200; `memory.low` 512 MiB; **can never be throttled** |
| `interactive` | set explicitly, or by a plugin | like normal; the *focused* app is handled by focus, not class |
| `normal` | your apps | defaults |
| `background` | indexers, sync, software updaters, containers, **running builds** | `cpu.weight` and `io.weight` 50 |
| `batch` | package managers, backups, `lindeskctl run --batch` | `cpu.idle` (SCHED_IDLE for the group), `io.weight` 10 |

**Builds are found automatically.** A build started in a terminal runs in the terminal
tab's scope, next to your shell, so by name it looks like any other app. The `workload`
classifier looks inside: a user scope holding two or more compiler or linker processes
(`rustc`, `cc1`, `clang`, `ld`, `rust-lld`, `mold`, `javac`, `go`, `tsc`, `ninja`, `make`, …)
is `background` while it builds and for 60 s after the last one exits. Classes are
re-evaluated every 15 s.

```toml
[plugins.workload.config]
min_processes = 2
hold = "60s"
class = "background"     # or "batch"
extra_comms = ["zig"]
```

## 3. The app in front of you

The focused app is treated by what you're doing with it, whatever its class, and is **never
throttled**:

| State | Settings | Default length |
|---|---|---|
| Just clicked or typed (an *interaction epoch*) | `cpu.weight` 500, `io.weight` 500, `cpu.uclamp.min` 30 %, `memory.low` = its usage (≤ 2 GiB) | 500 ms after each input |
| Just switched to it | the same boost | **3 s** (1 s on battery) |
| Focused, idle | `cpu.weight` 200, `io.weight` 200, same `memory.low` | while focused |

The boost is deliberately short. A permanent high priority for the focused app would cost
battery and hide problems; a burst when you act is what makes the app *feel* immediate.
While you keep typing or clicking (each input within 2 s of the last) it holds through the
pauses instead of dropping between words and coming back with the next key
(`epoch_linger`, not on battery). The
switch boost is longer because an app you return to has to repaint and fault pages back in.

On hybrid CPUs (Intel P/E cores, ARM big.LITTLE with EAS) `uclamp.min` also steers the
foreground toward the fast cores, and `uclamp.max` on background work keeps it on the
efficient ones.

**Inside the app: the threads you are waiting on go first.** A browser is not one thing.
It has a compositor and an I/O thread that decide whether scrolling is smooth, and a pool
of twenty-odd workers running scripts and decoding images. When the pool is busy the
kernel gives the compositor one share in twenty-odd, and the page you are looking at
stutters because of work on the same page. macOS avoids this because apps tag their
threads with a QoS class. Nothing tags them on Linux, but browser engines *name* them.
lindesk recognises Chromium (Chrome, Edge, Brave, Wavebox, every Electron app) and Firefox
by those names, and while such an app has focus it raises its UI threads (the main thread,
the compositor, the I/O thread, raster: nice −4) and lowers the ones that only keep house.
Niceness competes inside the app's own cgroup, so this moves CPU between the app's threads
and takes nothing from anyone else. It is undone when focus moves on, and a thread whose
priority the app set itself is left alone.

```toml
[plugins.thread-qos]
enabled = true
config = { ui_nice = -4, background_nice = 5, refresh = "5s" }

# Another program that names its threads:
[[plugins.thread-qos.config.engines]]
name = "my-toolkit"
markers = ["MyIOThread"]            # a process with a thread of this name is one
ui = ["MyRenderThread", "MyIO*"]
background = ["MyIndexer*"]
```

## 4. Apps you just left stay warm

This is what makes switching back to a browser instant. Without it, an app is only
protected while it has focus: the moment you look away under memory or I/O pressure, the
kernel pages it out, and coming back means waiting for it to be read in again.

An app that had focus within the last **30 minutes** is *warm*:

* `cpu.weight` and `io.weight` 150, so its background tabs, timers and network keep moving;
* `memory.low` = its current usage (rounded up to 64 MiB, at most 1 GiB per app), so reclaim
  takes from background and batch work first;
* all warm apps together may protect at most 25 % of RAM, most recently used first.

Only `normal` and `interactive` apps warm up; a build tab that once had focus still yields.

**And when an app did end up in swap.** Protection has limits: leave an app alone for a
few hours on a busy machine and part of it is in swap anyway. Normally it then comes back
one page fault at a time, each one a stalled thread, which is the "window is there but
nothing answers" feeling. When you switch to an app, lindesk asks the kernel to read that
app's swapped-out memory back in one go, in the background, while the switch boost gives
it the CPU and I/O share to repaint. On the development machine, touching 4,096 pages that
had been swapped out cost 4,096 blocking faults; after paging in, none.

It is held back by what the machine can afford: at most 1 GiB per switch, never into the
last 5 % of memory (bringing one app back must not push another out), and not again for
the same app within 30 s. `lindeskctl events` shows each one
(`bringing 244 MiB of … back from swap`). It needs Linux 5.10 and runs in the broker.

```toml
[plugins.pagein]
enabled = true
config = { budget = "1GiB", cooldown = "30s", min = "4MiB", reserve = 0.05 }
```

```toml
[plugins.threshold.config]
switch_epoch = "3s"
warm_for = "30m"            # "0s" turns warm apps off
warm_weight = 150
warm_memory_low_max = "1GiB"
warm_memory_budget = 0.25
memory_low_step = "64MiB"
```

## 5. Apps you put away nap

An app on another workspace, or minimized, still wakes up dozens of times a second to
poll, blink a cursor nobody sees and tick its timers. On macOS this is what App Nap stops.
lindesk's version is deliberately gentle. An app whose windows are all off screen, and that
is silent, is put in an efficiency mode after two minutes:

* its **timers are coalesced**: the kernel may delay them by up to 50 ms so they expire
  together instead of each waking a CPU;
* it is **steered to the efficient cores and lower frequencies** (`cpu.uclamp.max` 50 %).

Nothing is frozen and nothing is capped. A napping app still gets all the CPU time it asks
for, its network connections stay up and its notifications arrive; it just stops costing
what an app in front of you is worth. It wakes the moment one of its windows is on screen
again, it gets focus, or it starts playing or recording sound.

What never naps: the app in use, anything critical, anything playing or recording, and
anything that never had a window: a service or a terminal tab is not an app you put away.

It needs to know two things, and where it can't know them it does nothing:

| What | From | Where |
|---|---|---|
| which windows are on screen | the desktop adapter | GNOME (the Shell extension), Hyprland. Not yet KDE, COSMIC or plain X11: no naps there |
| which apps are making sound | PipeWire (`pw-dump`) | any session with PipeWire; without it nothing naps |

```toml
[plugins.threshold.config]
nap = true
nap_after = "2m"
nap_timer_slack = "50ms"     # at most 100ms
nap_uclamp_max = 50.0
```

`lindeskctl explain <app>` shows `nap: no window on screen for over 120s, silent` on a
napping app.

## 6. Doing nothing until it matters

lindesk reads **pressure-stall information** (PSI): the share of time tasks were stalled
waiting for CPU, I/O or memory, for the whole system and *inside the focused app's own
cgroup*. That second number is the one that matters: it is a direct measurement of you
waiting.

| Level | Entered when | What yields |
|---|---|---|
| **GREEN** | — | nothing beyond the standing weights |
| **YELLOW** | system pressure above 30 % CPU, 20 % I/O or 10 % memory, or the foreground stalled more than 5 % | background: weights 20, `cpu.uclamp.max` 60 %; batch: `io.weight` 1, `cpu.uclamp.max` 40 % |
| **RED** | the foreground stalled more than 10 % for 1 s, or memory fully stalled above 20 % | plus an adaptive `io.max` limit on background and batch; `memory.high` on batch under sustained memory pressure |

Levels have hysteresis: exit thresholds are half the entry ones, a level is held for a
minimum time (3 s YELLOW, 2 s RED), and recovery steps down one level at a time after 5 s of
calm. A sawtooth load doesn't make it flap. Nor does a bursty one: when the level rises
again within a minute of stepping down, the calm it needs before the next step down
doubles (10 s, 20 s, … up to a minute), so a disk that stalls for a second every few
seconds holds YELLOW instead of re-tuning every unit each time. After a calm minute it
starts over at 5 s.

**The adaptive I/O limit (RED only).** Each background and batch unit gets an `io.max` limit
per managed disk, starting at half of what it was just doing. So does anything else that is
doing real I/O (more than 1 MiB/s) next to your stalling app, whatever its class: a test
run in another terminal tab is "normal" by every rule and still the reason you are waiting
(`limit_heavy_neighbours = false` turns that off). Critical units and the app in use are
never limited. Every second the foreground is still stalling, the limit halves; once it
stops stalling, the limit grows again; leaving RED removes it. Floors scale with the disk
(about 2 % of its speed, shared between the units being limited, never below the safety
floor), so nothing is starved. This is admission control: the disk queue stays short enough
for your app's reads to get through.

**Weights that mean something.** `io.weight` needs a controller behind it. On disks using
`none`, `mq-deadline` or `kyber`, `lindeskd` enables the kernel's iocost controller
(`[io] iocost = "auto"`); on BFQ, weights already work. Enabling iocost is not enough,
though: with the kernel's own parameters it has no latency target, never slows anyone
down, and lets requests queue in the disk for seconds while every unit, whatever its
weight, gets all it asks for. So lindesk also gives it targets
(`iocost_latency_targets = "auto"`): the 95th percentile of completions should stay under
10 ms for reads and 40 ms for writes on NVMe (20/60 ms on SATA SSDs, 150/300 ms on
spinning disks). When the disk can't keep that up, the kernel slows issuance until it can,
and what the disk manages within the target is shared by weight: your app first. The cost
is throughput for background work while the disk is saturated; an idle or healthy disk is
not slowed. Both the QoS line and the cost model are put back when lindesk stops.

This matters most on consumer SSDs that collapse under sustained writes (a hot or nearly
full QLC drive can drop from gigabytes to a few megabytes per second). Nothing makes such
a disk fast; the targets decide who waits for it.

**What your app is waiting on is not held back.** macOS lends a thread's priority to
whoever it is waiting on, even in another process. Linux can't, and nothing outside the
kernel can add that. What lindesk can do is not make it worse. A browser showing a page
from a dev server that is rebuilding in a terminal tab is the everyday case: the tab is a
build, it yields, and the page you are looking at loads slowly *because of lindesk*. So
while the level is raised, lindesk looks at the local connections of the app in front and
finds which throttled units hold the other ends. Those are left alone for as long as the
connection is there (`lindeskctl events` says `… is connected to …: not throttling it`).

It sees TCP on the loopback interface: a dev server, a local database, a language server
on a port. It does not see Unix sockets, D-Bus, or containers with their own network.

```toml
[plugins.serving]
enabled = true
config = { refresh = "5s" }
```

## 7. Staying responsive itself

A governor that stalls with the workload it governs is no use. lindesk needs little, and
guarantees itself that little:

* both daemons lock their memory in RAM (`mlockall`), so a page fault can't park them behind
  the disk queue they're trying to relieve; their cgroups have `MemoryMin` and
  `MemorySwapMax=0`;
* high CPU and I/O weights (1000 for the broker, 500 for the session agent), a small nice
  boost, a very low OOM score. Never a realtime class;
* a step spends at most about 400 ms applying settings; the rest is applied on the next
  step, so sensing never waits on a slow systemd. The watchdog restarts a broker that is
  really stuck, not one that is busy;
* its logging never waits for the journal. When journald is stuck behind a saturated disk,
  a daemon that logs synchronously stops with it; lindesk drops the record and counts it;
* it is cheap: only the system and the focused app's cgroups are sampled on every step,
  everything else every two seconds, and the loop drops to one step a second while the
  machine is calm (input, a focus change or rising pressure wakes it at once);
* no plugin or policy can throttle lindesk's own units;
* telemetry, when installed, is written by a separate low-priority process and is dropped
  rather than allowed to block.

## 8. Never making things worse

* No realtime CPU or I/O classes, ever.
* Every throttle has a floor (`[safety]` in the config): I/O limits, `memory.high`,
  `uclamp.max`. Boosts have ceilings.
* Critical units, lindesk's own units, the focused app and any slice it runs inside
  (`app.slice`, `user.slice`) cannot be throttled: limiting a slice limits everything in it.
* Plugins return *intents*; lindesk resolves and clamps them. A plugin can't lift a cap.
* Every setting is runtime-only and recorded with the value it replaced. It is restored when
  the daemon stops, after a crash (`ExecStopPost=… --restore`), by `lindeskctl off`, and by
  the uninstaller.
* `lindeskctl pause --for 10m` puts everything back and keeps watching, for a quick
  before/after.

## 9. Adapting to the machine

Thresholds and limits scale with what lindesk detects (`lindeskctl hardware`): disk kind and
measured speed set the I/O floors, RAM sets the memory protections, a small CPU count
relaxes the CPU threshold, battery shortens boosts. A **profile** is selected automatically
(`battery`, `gaming`, `hdd`, `x86-hybrid`, `arm-eas`, else `balanced`); see
[CPU profiles](profiles.md) and [Hardware awareness](hardware.md).

## 10. Telling you what it can't fix

Some stalls aren't lindesk's to fix, and it says so instead of quietly failing:

* `lindeskctl doctor` lists **tuning lindesk didn't make**: persistent systemd resource
  limits and udev I/O-scheduler rules left by guides or other tools. A slice capped at
  20 MB/s looks exactly like a slow disk: 95 % I/O pressure, idle device, idle CPU.
* It detects other priority daemons that would fight it ([conflicts](conflicts.md)).
* `lindeskctl swap` analyses swap and proposes changes you can apply and revert.
* `lindeskctl calibrate` measures the machine's own latency baseline.

lindesk decides who waits. It can't make a full, hot or worn disk faster, or create memory
that isn't there.

## Where this stands against macOS

| macOS | lindesk today | Gap |
|---|---|---|
| QoS classes on every thread and queue (user-interactive … background) | five classes per app unit, set by rules, by what's running, and by focus; inside the app in use, the UI threads of browser engines (Chromium, Firefox) go ahead of their workers | threads are told apart by name, so only engines that name them are covered, and with two levels rather than five. Apps can't yet declare their own threads' classes (an API is the missing half) |
| The focused app and the window server are favoured | focus tracking, interaction and switch boosts, critical compositor | on GNOME Wayland it needs the Shell extension |
| Background QoS throttles CPU and I/O | weights enforced by latency targets, `cpu.idle`, uclamp, adaptive `io.max` | the limits are driven by measured stalls, so they react within about a second rather than pre-emptively |
| Memory compression and keeping recent apps ready | warm apps (`memory.low`), and an app that did get swapped out is paged back in when you switch to it; works with zram and zswap | macOS also compresses and decompresses per app ahead of time; here it starts at the moment of the switch |
| App Nap, timer coalescing | apps that are off screen and silent nap: timers coalesced, efficient cores | gentler than macOS (nothing is throttled or frozen), it doesn't look at power assertions, and it needs the desktop to say what is on screen (GNOME and Hyprland so far) |
| Priority inheritance across IPC | critical services (compositor, audio, portals, D-Bus, the journal) are always protected, and a unit the app in use has an open local TCP connection to is not throttled | not inheritance: nothing is boosted, lindesk only stops holding back what you are waiting on, and only where it can see the connection. Unix sockets, D-Bus and containers are not seen. Closer would need tracing wake-ups in the kernel (eBPF) or a sched_ext scheduler that follows wake-up chains |
| One vendor tuned the whole stack for known hardware | hardware detection, profiles, calibration | every machine is different; that's why the bench is there |

We'd rather show the gap than hide it. The parts that matter most for "it doesn't stutter
when I'm building" are in place; the list above is the road from there.

## Check it on your machine

```bash
lindeskctl watch        # levels, pressure, who is protected, who yields, every decision
lindeskctl explain firefox
lindesk-bench run       # the same load with lindesk paused and active, side by side
```
