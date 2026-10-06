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
| You switch apps | The app you switched to gets that boost for 3 s | `lindeskctl events -f` |
| You leave an app | It stays *warm* for 30 minutes: memory protected, raised share | `lindeskctl explain <app>` → `warm:` |
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
| COSMIC | built into `lindesk-session` (toplevel-info protocol) |
| any X11 session | built into `lindesk-session` (EWMH), with XInput2 for input |

The window's process is mapped to its systemd unit (the app's scope), and that unit is what
gets tuned. An app isn't always one unit: Chromium-based browsers (Chrome, Wavebox, Edge,
Electron apps) put the window's process in one scope and their renderer and GPU processes
in another. lindesk follows the focused process's descendants, so every unit the app runs
in is foreground: all of them get the boost, none is throttled, and a stall in any of them
counts as you waiting. The set is refreshed every 5 s as tabs open and close. On GNOME Wayland the extension loads at the next login after the first install;
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
battery and hide problems; a burst when you act is what makes the app *feel* immediate. The
switch boost is longer because an app you return to has to repaint and fault pages back in.

On hybrid CPUs (Intel P/E cores, ARM big.LITTLE with EAS) `uclamp.min` also steers the
foreground toward the fast cores, and `uclamp.max` on background work keeps it on the
efficient ones.

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

```toml
[plugins.threshold.config]
switch_epoch = "3s"
warm_for = "30m"            # "0s" turns warm apps off
warm_weight = 150
warm_memory_low_max = "1GiB"
warm_memory_budget = 0.25
memory_low_step = "64MiB"
```

## 5. Doing nothing until it matters

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
calm. A sawtooth load doesn't make it flap.

**The adaptive I/O limit (RED only).** Each background and batch unit gets an `io.max` limit
per managed disk, starting at half of what it was just doing. Every second the foreground is
still stalling, the limit halves; once it stops stalling, the limit grows again; leaving RED
removes it. Floors scale with the disk (about 2 % of its speed, never below the safety
floor), so nothing is starved. This is admission control: the disk queue stays short enough
for your app's reads to get through.

`io.weight` needs a controller behind it. On disks using `none`, `mq-deadline` or `kyber`,
`lindeskd` enables the kernel's iocost controller (`[io] iocost = "auto"`) and puts the
previous setting back when it stops; on BFQ, weights already work.

## 6. Staying responsive itself

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
* no plugin or policy can throttle lindesk's own units;
* telemetry, when installed, is written by a separate low-priority process and is dropped
  rather than allowed to block.

## 7. Never making things worse

* No realtime CPU or I/O classes, ever.
* Every throttle has a floor (`[safety]` in the config): I/O limits, `memory.high`,
  `uclamp.max`. Boosts have ceilings.
* Critical units, lindesk's own units and the focused app cannot be throttled.
* Plugins return *intents*; lindesk resolves and clamps them. A plugin can't lift a cap.
* Every setting is runtime-only and recorded with the value it replaced. It is restored when
  the daemon stops, after a crash (`ExecStopPost=… --restore`), by `lindeskctl off`, and by
  the uninstaller.
* `lindeskctl pause --for 10m` puts everything back and keeps watching, for a quick
  before/after.

## 8. Adapting to the machine

Thresholds and limits scale with what lindesk detects (`lindeskctl hardware`): disk kind and
measured speed set the I/O floors, RAM sets the memory protections, a small CPU count
relaxes the CPU threshold, battery shortens boosts. A **profile** is selected automatically
(`battery`, `gaming`, `hdd`, `x86-hybrid`, `arm-eas`, else `balanced`); see
[CPU profiles](profiles.md) and [Hardware awareness](hardware.md).

## 9. Telling you what it can't fix

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
| QoS classes on every thread and queue (user-interactive … background) | five classes per app unit, set by rules, by what's running, and by focus | per unit, not per thread; apps can't yet declare their own (an SDK is planned) |
| The focused app and the window server are favoured | focus tracking, interaction and switch boosts, critical compositor | on GNOME Wayland it needs the Shell extension |
| Background QoS throttles CPU and I/O | weights, `cpu.idle`, uclamp, adaptive `io.max` | driven by measured stalls, so it reacts within about a second rather than pre-emptively |
| Memory compression and keeping recent apps ready | warm apps (`memory.low`), works with zram and zswap | no proactive page-in of a swapped app when you focus it (planned) |
| App Nap, timer coalescing | — | not attempted |
| Priority inheritance across IPC | — | a kernel matter; out of reach from userspace |
| One vendor tuned the whole stack for known hardware | hardware detection, profiles, calibration | every machine is different; that's why the bench is there |

We'd rather show the gap than hide it. The parts that matter most for "it doesn't stutter
when I'm building" are in place; the list above is the road from there.

## Check it on your machine

```bash
lindeskctl watch        # levels, pressure, who is protected, who yields, every decision
lindeskctl explain firefox
lindesk-bench run       # the same load with lindesk paused and active, side by side
```
