# lindesk

**Your Linux desktop should never make you wait for your own computer.**

[![CI](https://github.com/dipankar/lindesk/actions/workflows/ci.yml/badge.svg)](https://github.com/dipankar/lindesk/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

You know the moment. A build is running, or a big copy, or the package manager decided now
was a good time. You click on your browser and nothing happens. You type and the letters
arrive a second late. The cursor skips. The music crackles. The machine has thirty-two
cores and sixty-four gigabytes of memory, and it cannot spare a sliver of them for the one
window you are looking at.

That moment is not a law of nature. It's a missing decision. The kernel has no idea which
of the hundred things it's running is *you*.

**lindesk tells it.** It watches which app you're using and whether that app is actually
being held up, and when it is, it makes the background work wait its turn. The build
finishes a little later. Your desktop stays yours.

```text
without lindesk:  cargo build + a 40 GB copy   →  the desktop stutters, typing lags
with lindesk:     the same build, the same copy →  they slow down a little;
                                                   the window you're in stays smooth
```

The goal is a Linux desktop that stays as responsive under load as a Mac does. No kernel
patch. No reboot. Any distro. Everything it changes, it undoes.

> **Status: 0.1.0, early.** It runs daily on the author's GNOME/Wayland workstation; KDE,
> COSMIC, Hyprland and X11 adapters exist and are tested in isolation. It has not yet earned the
> trust that years of deployment give tools like system76-scheduler. Don't take our word for
> the difference it makes: [`lindesk-bench`](#measure-it-yourself) runs the same load with
> lindesk off and on, on *your* machine.

## What it feels like

```console
$ lindeskctl
● YELLOW — protecting firefox; 8 workloads yielding
  on · lindesk-session · connected · profile auto→x86-hybrid · 24×CPU · hybrid 16P+8E · 63 GiB · NVMe · AC
  pressure cpu 1.4% · io 86.1% · memory 0.1%
  yielding rustc -12% CPU · tracker-miner-fs · gnome-software · background.slice +4
```

And when you wonder why something is slow, it tells you, in a sentence:

```console
$ lindeskctl explain gnome-software
user/1000:app-gnome-org.gnome.Software-37769.scope
  class:   background ← classifier 'rules': 50-background.toml:software-updaters
  settings:
    cpu.weight=20          ← threshold: YELLOW: system io pressure 65% > 20%; background yields; was default
    io.weight=20           ← threshold: YELLOW: system io pressure 65% > 20%; background yields; was default
```

`lindeskctl watch` is the live view: pressure gauges, what's protected, what's yielding and
why, and every decision as it happens.

## Install

```bash
sudo apt install ./lindesk_*_amd64.deb     # Debian, Ubuntu, Pop!_OS
sudo dnf install ./lindesk-0*.rpm          # Fedora
makepkg -si                                # Arch, Omarchy (packaging/aur)
./packaging/install.sh                     # anything else, from source
```

Packages are on the [releases page](https://github.com/dipankar/lindesk/releases). Then:

```bash
lindeskctl doctor     # what your kernel and desktop support, and the one next step
lindeskctl            # what lindesk is doing right now
lindeskctl watch      # the live dashboard
```

It needs no configuration. On GNOME Wayland, log out and in once after the first install so
the Shell extension that reports the focused window loads; everything else works at once.
On [Omarchy](docs/src/guide/omarchy.md) and other Hyprland setups the adapter is built in.

The core package is about 15 MB. It never changes anything permanently, and you can step
away from it at any time:

```bash
lindeskctl pause --for 10m    # put everything back, keep watching; resumes by itself
lindeskctl off                # back to the machine's own behaviour (config and history kept)
sudo lindesk-uninstall        # restore everything lindesk ever changed, then remove it
```

## How it keeps the promise

Linux already has the machinery: pressure-stall information (PSI), cgroup v2 CPU, I/O and
memory control, uclamp, systemd's per-app units. What it lacks is something that connects
*"the human just touched this window"* to those knobs. lindesk is that connection.

1. **It knows which app is you.** Desktop adapters (GNOME, KDE, COSMIC, Hyprland, X11) report the
   focused window and your input. Every workload gets a class: `critical` (compositor,
   audio), `interactive`, `normal`, `background`, `batch`.
2. **It does nothing while the machine is healthy.** Only work-conserving weights, which
   cost nothing when nobody is waiting.
3. **It steps in gradually when the app in front of you is actually stalling.** Measured
   pressure moves a GREEN → YELLOW → RED controller with hysteresis. Background work yields
   in steps: lower weights, then CPU caps, then an adaptive I/O limit that tightens only
   while your foreground is still held up, and loosens as soon as it isn't.
4. **A short boost when you act.** For about half a second after you click or type, the app
   you're using gets a bounded boost. Not a permanent priority: your battery will notice.
5. **The app you just left stays ready.** For half an hour after you leave a window, its
   memory stays protected and it keeps a raised share, so switching back to your browser
   doesn't mean waiting for it to be paged in again. If it was swapped out anyway, it is
   read back in one go the moment you switch to it.
6. **It is not allowed to hurt.** No realtime classes. Every throttle has a floor, so
   nothing is starved. Critical processes can be boosted, never throttled. Every change is
   runtime-only, recorded with the value it replaced, and reverted on exit, on a crash and
   on uninstall.

It also notices builds: a terminal tab full of `rustc` and `cc1` is background work while it
compiles, even though you started it by hand. The window you're looking at is never
throttled, whatever is running in it. And it guarantees its own small share of the machine,
so it can't stall along with the load it is managing.

Every mechanism, its defaults and how to watch it work:
[How lindesk keeps the desktop responsive](docs/src/guide/responsiveness.md), including an
honest table of where this stands against macOS.

## It tells you what's wrong, including when it's not lindesk

While building lindesk, the development machine sat at 95% I/O pressure with an idle disk
and an idle CPU, for a day. The cause was not the hardware. It was a forgotten systemd
drop-in capping every terminal process to 20 MB/s, and a udev rule forcing the BFQ scheduler
onto a fast NVMe. Nothing on the system said so.

`lindeskctl doctor` does now:

```console
Tuning lindesk didn't make
Limit outside lindesk    app-org.gnome.Terminal.slice: IOWriteBandwidthMax=… 20M (~/.config/systemd/user/…/60-io-protection.conf)
                         → everything in that slice shares this cap, which shows up as pressure with an idle disk/CPU
I/O scheduler rule       ATTR{queue/scheduler}="bfq" (/etc/udev/rules.d/99-local-desktop-bfq.rules)
```

It also reports what your kernel can and can't do, which other priority daemons would fight
it, how your swap is set up, and the single most useful next step.

## Made to be extended

Every stage is a plugin, and the built-ins use the same API you do:

```text
adapters ─┐
sensors ──┼─► classifiers ─► policies ─► resolver + safety caps ─► actuators
          └─────────────── records ────────────────────────────► sinks
```

A plugin is an executable that speaks JSON-RPC on stdio, in any language:

```python
from lindesk_plugin import classifier, run

@classifier
def classify(proc):
    if proc.get("exe", "").endswith("proton"):
        return {"class": "interactive", "reason": "a game"}

run()
```

```bash
lindeskctl plugin new --kind classifier --lang python my-rule   # a working plugin and its test
lindeskctl plugin test ./my-rule                                # protocol conformance
lindeskctl simulate --policy ./my-policy --from "-1h"           # replay real history through it
```

Policies never touch the system. They return *intents*; lindesk resolves them, clamps them
to safety caps no plugin can lift, and records who asked for what. There are SDKs for
[Rust](crates/lindesk-plugin) and [Python](sdk/python), [example plugins](examples/plugins),
and the protocol as [JSON Schema](docs/src/reference/protocol.md).

## It adapts to the machine

lindesk detects the CPU (x86_64 or aarch64, hybrid P/E or big.LITTLE cores, EAS, cpufreq),
memory and swap, each disk's kind, the power source and virtualization, and scales its
thresholds to them. A **profile** is picked automatically: `battery` when unplugged,
`gaming` while a game has focus, `hdd` on spinning disks, `x86-hybrid` and `arm-eas` on
hybrid CPUs, otherwise `balanced`.

```bash
lindeskctl hardware        # what it found, and how it adapts
lindeskctl profile list    # which profile is active, and why
lindeskctl calibrate       # measure this machine: P vs E cores, wakeup latency, disk latency
lindeskctl swap            # zram, zswap and swappiness, with fixes you can apply and undo
```

## For AI agents

`lindesk-mcp` lets a local agent work out why your desktop stutters, and fix it without
being able to break it:

```bash
claude mcp add lindesk -- lindesk-mcp      # others: lindeskctl mcp config --client cursor
```

* **Read-only by default:** status, doctor, hardware, swap analysis, ranked recommendations.
* **Guardrailed changes:** allow-listed settings with ranges, safety caps that can only be
  tightened, rate limits. The MCP server itself holds no privileges.
* **Versioned and self-reverting:** every change records what it replaced and undoes itself
  after ten minutes unless you confirm it.

```bash
lindeskctl changes list         # who changed what, when, and how long until it reverts
lindeskctl changes revert 7     # undo it; or `changes confirm 7` to keep it
```

## Measure it yourself

```bash
lindesk-bench run
```

It runs a CPU, disk and memory load in the background and a latency probe in the
foreground, once with lindesk paused and once with it active, and prints the comparison:
wakeup latency, disk latency, how much your foreground stalled, and how much work the
background still got done. Bounded, cleaned up after, and safe to stop with Ctrl-C.

If lindesk doesn't make your machine better, that table will say so. We'd like to know.

## Optional: history you can query

The `lindesk-telemetry` add-on records pressure, per-app usage and every decision with its
reason into a [Vortex](https://vortex.dev) lake you can query with SQL through DuckDB. It is
never installed for you, and the core package links neither.

```bash
sudo apt install ./lindesk-telemetry_*_amd64.deb
lindeskctl query "select resource, avg(some_avg10) from pressure group by 1"
lindeskctl data delete --where "app_id = 'org.mozilla.firefox'"     # your data, your call
```

## Other priority daemons

| Tool | What it changes | With lindesk |
|---|---|---|
| system76-scheduler | nice, I/O priority and scheduling policy of most processes; a foreground boost | conflict |
| ananicy, ananicy-cpp | nice, I/O priority, scheduling policy per process name; can move processes into its own cgroups | conflict |
| uresourced | CPU and I/O weights and memory protection on the active user's units | conflict |
| gamemode | CPU governor and the game's priorities, only while a game runs | fine together |

Two daemons adjusting the same knobs undo each other's work. `lindeskctl doctor` and the
installer detect them. `sudo lindeskctl setup conflicts --disable` turns them off as a
recorded change, and turning lindesk off or uninstalling it puts them back exactly as they
were. Packages never disable anything on their own.
[Why they conflict, and when they can coexist.](docs/src/guide/conflicts.md)

## What's inside

| | |
|---|---|
| `lindeskd` | the broker (system service): sensing, policy, system units |
| `lindesk-session` | the per-user agent: which window is focused, your apps, your plugins |
| `lindeskctl` | the CLI: doctor, status, watch, explain, on/off/pause, profiles, changes, plugins |
| `lindesk-mcp` | the MCP server for local agents |
| `lindesk-bench` | the foreground latency probe and A/B harness |
| `lindesk-data`, `lindesk-telemetry-sink` | *lindesk-telemetry*: the history lake and its writer |

Documentation lives in [`docs/`](docs/src/SUMMARY.md): the user guide, the plugin guide, and
the contracts between components ([architecture](docs/ARCHITECTURE.md),
[control plane](docs/CONTROL-PLANE.md)).

## Contributing

Bug reports with a `lindeskctl doctor --json` attached are gold. So are bench results, good
or bad, and adapters for desktops we don't cover yet. See [CONTRIBUTING.md](CONTRIBUTING.md)
and the [security policy](SECURITY.md).

## License

lindesk's source is licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option.

One note on binaries: the built-in COSMIC adapter links `cosmic-protocols`, which is
GPL-3.0-only, so a `lindesk-session` binary built with it (the default, and what the
packages ship) is distributed under GPL-3.0. Everything else is unaffected. See
[THIRD-PARTY.md](THIRD-PARTY.md).
