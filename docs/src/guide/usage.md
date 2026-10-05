# Everyday use

`lindeskctl` talks to `lindesk-session` on your session bus by default, falling back to the
`lindeskd` broker; `--system` talks to the broker (and the system lake) only. Every command
accepts `--json`, `--color auto|always|never` (and honours `NO_COLOR`) and `-q`. Exit
status is 0 on success, 1 on failure and 2 on a usage error. `lindeskctl <cmd> --help`
shows examples for every command.

## Just type `lindeskctl`

With no command, `lindeskctl` prints a compact status and where to go next:

```console
$ lindeskctl
● YELLOW — protecting firefox; 2 workloads yielding
  lindesk-session · connected · 32×CPU · 62 GiB · NVMe · AC
  pressure cpu 8.1% · io 31.2% · memory 0.0%
  yielding rustc -12% CPU -31% IO · tracker-miner-f -8% IO
lindeskctl watch for live view · lindeskctl doctor · lindeskctl --help
```

When something is installed but not running or not enabled (the services, the GNOME
extension, the KWin script), `lindeskctl`, `status` and `watch` add exactly one line with
the command that fixes it, once per login session.

## What is lindesk doing? `status`

```console
$ lindeskctl status
● YELLOW — protecting firefox; 2 workloads yielding
  lindesk-session

PRESSURE  system, % of time stalled (avg10)
  cpu     ██▏░░░░░░░░░░░░░░░░░░░░░   8.1%
  io      ███████▌░░░░░░░░░░░░░░░░  31.2%   full 12.0%
  memory  ░░░░░░░░░░░░░░░░░░░░░░░░   0.0%   full  0.0%

FOREGROUND
  firefox             user/1000:app-gnome-org.mozilla.firefox-4242.scope
                      cpu.weight 500 · uclamp.min 30%

PROTECTED  2
  session.slice       critical     cpu.weight 200 · memory.low 512M
  pipewire            critical

YIELDING  2
  WORKLOAD           CLASS        CPU   IO   FOR  THROTTLES
  rustc              batch       -12% -31%   30s  cpu.idle · io.weight 1 · io.max 64M/s
  tracker-miner-f    background     ·  -8%   10s  cpu.weight 20 · uclamp.max 60%

PLUGINS  1 running, 1 not
  ● threshold p99 310µs   ◐ steam-games p99 900µs 3 errors (retrying in 2000ms)
  details: lindeskctl plugin list
```

* **Level**: GREEN does nothing beyond free, work-conserving weights; YELLOW asks
  background and batch work to yield; RED adds `io.max` admission control.
* **Yielding** deltas compare each throttled workload's CPU and I/O with its usage before
  the first throttle.
* `status --json` prints the daemon's JSON (see [D-Bus API](../reference/dbus.md#status-json)).

When nothing is running, `status` says so and how to start it (exit status 1):

```console
$ lindeskctl status
○ lindesk isn't running (or isn't reachable).
  lindesk-session  lindesk-session isn't running on the session bus.
  lindeskd         lindeskd isn't running on the system bus.

Start it:
  lindesk-session  systemctl --user enable --now lindesk-session
  lindeskd         sudo systemctl enable --now lindeskd

Then run `lindeskctl status` again. What this machine supports: lindeskctl doctor
Raw pressure right now: lindeskctl watch
```

## Live: `watch`

`lindeskctl watch` is a full-screen dashboard refreshed every 250 ms:

* the level, what lindesk is protecting, the mode (standalone, with broker, dry run) and uptime;
* system PSI gauges with 60-second sparklines (some and full), and the foreground's own stall;
* the protected workloads with their boosts, and the yielding ones with their throttles
  and CPU/IO deltas;
* a live event feed (level changes, actions with reasons, focus changes, plugin errors),
  newest first;
* plugin health: state, calls/s, errors, p99.

```text
 ● YELLOW   protecting firefox; 2 workloads yielding   lindesk-session · connected · up 2h13m · 32×CPU · 62 GiB · NVMe · AC         08:46:40
 foreground firefox  ⚡  interaction epoch   own stall cpu 0.4%  io 3.1%  memory 0.0%                                                        
╭ Pressure · system avg10 · last 60s ──────────────────────────────────────────────────────────────────────────────────────────────────────╮
│cpu     █▍░░░░░░░░░░░░░░░░░░   7.0%  some ▅▆▆▆▆▆▆▇▇▇▇▆▆▆▆▆▅▅▅▅▄▄▃▃▂▂▂▁▁▂▂▃▃▄▄▄▅▅▅▆▆  full                                            0.0% │
│io      ██████▌░░░░░░░░░░░░░  32.3%  some ▇▇▇▇██████████▇▇▇▆▆▆▅▅▄▄▄▃▃▂▂▃▃▄▄▅▅▆▆▆▇▇▇  full ▆▇▇▇█████████▇▇▇▇▆▆▅▅▄▄▃▃▂▂▁▁▂▃▃▄▄▅▅▆▆▆▇▇ 10.5% │
│memory  ░░░░░░░░░░░░░░░░░░░░   0.0%  some                                            full                                            0.0% │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
╭ Yielding · 2 ─────────────────────────────────────────────────────────────╮╭ Events · live ──────────────────────────────────────────────╮
│PROCESS / UNIT               CLASS       VS BEFORE THROTTLES               ││08:46:35 plugin steam-games: classifier.classify timed out   │
│rustc                        batch       -12% CPU  cpu.idle, io.weight 1,  ││08:46:10 apply  io.weight 1 on lindesk-run-cargo-77.scope  [t│
│lindesk-run-cargo-77.scope               -31% IO   io.max 64M/s            ││08:46:10 level GREEN → YELLOW  (threshold)                   │
│tracker-miner-f              background  · CPU     cpu.weight 20,          ││08:46:00 focus → org.mozilla.firefox                         │
│tracker-miner-fs-3.service               -8% IO    uclamp.max 60%          ││                                                             │
╰───────────────────────────────────────────────────────────────────────────╯│                                                             │
╭ Protected · 3 ────────────────────────────────────────────────────────────╮│                                                             │
│WORKLOAD               CLASS        BOOSTS                                 ││                                                             │
│▶ firefox              foreground   cpu.weight 500, uclamp.min 30%         ││                                                             │
│  session.slice        critical     cpu.weight 200, memory.low 512M        ││                                                             │
│  pipewire             critical                                            ││                                                             │
│                                                                           ││                                                             │
│                                                                           ││                                                             │
│                                                                           ││                                                             │
│                                                                           ││                                                             │
│                                                                           ││                                                             │
│                                                                           ││                                                             │
│                                                                           ││                                                             │
│                                                                           ││                                                             │
│                                                                           ││                                                             │
│                                                                           ││                                                             │
│                                                                           ││                                                             │
│                                                                           ││                                                             │
│                                                                           ││                                                             │
╰───────────────────────────────────────────────────────────────────────────╯╰─────────────────────────────────────────────────────────────╯
plugins ● threshold 0/s p99 310µs   ◐ steam-games 0/s p99 900µs 3err backoff                                                                
 ⚠ io.weight: inert → enable iocost (io.iocost = "auto")                                                                                    
 q quit  p pause  tab pane  ↑↓ select  enter explain  ? help                                                                                
```

Keys: `q` quit, `p` pause, `tab` switch pane, `↑`/`↓` select, `enter` explain the selected
unit in a side panel, `?` help. It degrades gracefully on small terminals, and when lindesk
isn't running it still shows the raw system pressure.

Without a terminal (or with `--plain`) it prints one compact line per second, plus new
events; `--json` prints JSON lines:

```console
$ lindeskctl watch --plain
14:02:11  ● YELLOW  fg firefox ⚡  cpu 8.1%  io 31.2%  memory 0.0%  yielding rustc -12% CPU -31% IO  [lindesk-session]
14:02:12  ● YELLOW  fg firefox  cpu 7.9%  io 30.8%  memory 0.0%  yielding rustc -12% CPU -31% IO  [lindesk-session]
14:02:12  ↳ apply  io.max 64M/s on lindesk-run-cargo-77.scope  [threshold: RED: foreground io stall 14% > 10%]
$ lindeskctl watch --json --count 60 > minute.jsonl
```

## Events: `events`

```console
$ lindeskctl events                       # the last 50
$ lindeskctl events -f                    # and keep streaming
$ lindeskctl events -f --kind level --kind action
$ lindeskctl events -f --json | jq -c 'select(.kind == "plugin_error")'
```

Events come from the daemon's `Events` signal (with `RecentEvents` for the backlog,
merged by sequence number, so nothing is shown twice). With a
daemon that predates them, `--follow` infers events from status changes, and the backlog
comes from the telemetry lake.

## Why? `explain`

```console
$ lindeskctl explain firefox
firefox → pid 4242
user/1000:app-gnome-org.mozilla.firefox-4242.scope
  process: firefox (pid 4242, uid 1000)
  class:   interactive ← classifier 'rules' (confidence 1.00): rule "browsers"
  foreground: yes (policies treat it as interactive)
  settings:
    cpu.weight=500               ← threshold: interaction epoch; was default
```

`explain` takes a pid, a unit (`app-foo.scope`, `user/1000:app.slice`,
`system:docker.service`) or a process name. Names are matched like `pidof` (comm,
executable, argv[0]); several processes in one unit resolve to the oldest, and matches in
different units are an error that lists them.

## Steer: `run`, `set`, `restore`

```console
$ lindeskctl run --batch make -j32             # lindesk-batch.slice, class batch
$ lindeskctl run --background rsync -a ~/photos /mnt/backup
$ lindeskctl run --class critical -- obs
$ lindeskctl set blender batch                 # an already running process
$ lindeskctl set --system 4242 batch           # someone else's process (polkit)
```

`run` starts the command in its own transient scope (`systemd-run --user --scope`) in the
slice for its class, tells `lindesk-session` the class, forwards signals and returns the
command's exit status. Without a systemd user manager, `--batch` falls back to nice 19 and
idle I/O priority, and `--background` to nice 10 and low best-effort I/O.

```console
$ lindeskctl restore --stop                    # stop lindesk-session; everything is restored
$ sudo lindeskctl restore --system --stop      # the same for lindeskd
```

Every change lindesk makes is runtime-only and restored when its daemon stops (even after a
crash). `restore` is the manual escape hatch; a running daemon would re-apply its
settings, hence `--stop`.

## On, off, pause

`lindeskctl status` (and plain `lindeskctl`) always says which of four states lindesk is in:
**on**, **paused**, **off** (installed, not running) or **not installed**.

```console
$ lindeskctl off             # back to the machine's default behaviour
$ lindeskctl on              # and back
$ lindeskctl pause --for 10m # A/B: revert everything, keep observing, resume by itself
$ lindeskctl resume          # end a pause early
```

* `off` stops and disables `lindeskd` (as root, or through polkit) and your
  `lindesk-session`; both restore everything they applied when they stop. As root it also
  puts conflicting daemons (system76-scheduler, ...) back to how they were before lindesk
  (`--keep-conflicts` leaves them), and records `"lindesk_enabled": false` in
  `/var/lib/lindesk/install-state.json`, so upgrades and package scripts don't turn it back
  on. Config, profiles, change history and telemetry are kept. `--all-users` (root) also
  stops every logged-in user's agent now.
* `on` enables and starts them again, offers to disable conflicting daemons (as the
  installer does; `--disable-conflicts` / `--keep-conflicts`), and waits until lindesk
  answers.
* `pause` doesn't stop anything: every running daemon reverts what it applied and keeps
  sensing and deciding without applying (status shows `⏸ paused (resumes in 9m)`). With
  `--for` it resumes by itself; an indefinite pause lasts, across restarts, until `resume`.
  `lindesk-bench run` uses it for its "off" phase (`--off-mode pause`, the default).
  Pausing lindeskd needs admin rights; your own agent doesn't.

To remove lindesk completely, `sudo lindesk-uninstall` (see [Installation](install.md)).

## Profiles, changes and hardware

```console
$ lindeskctl profile list               # active profile (●), what matches now, and why
$ lindeskctl profile show gaming        # its overlay and the threshold values it resolves to
$ lindeskctl profile use gaming         # pin it (a versioned change); `use auto` to go back
$ lindeskctl changes list               # every change, by you or an agent, with ttl left
$ lindeskctl changes apply --set plugins.threshold.config.epoch=400ms --ttl 10m -m "shorter epochs"
$ lindeskctl hardware                   # detected hardware and how lindesk adapts to it
$ lindeskctl swap                       # swap analysis with ready-to-run fixes
$ lindeskctl calibrate --quick          # measure the latency map
$ lindeskctl mcp                        # connect a local AI agent
```

`status` and `watch` show the active profile in the header and count down changes that
will auto-revert (`↺ change #7 reverts in 8m  keep: lindeskctl changes confirm 7`); the
event feed shows `profile.changed` and `change.*` events. See [Profiles](profiles.md),
[Changes](changes.md), [Hardware awareness](hardware.md) and [MCP](mcp.md).

## Conventions

* `lindeskctl foo args…` runs `lindeskctl-foo args…` from `PATH`, like git. A typo gets a
  suggestion: `error: unknown command 'stauts'. Did you mean 'status'?`
* `lindeskctl bench …` runs `lindesk-bench …`.
* `lindeskctl completions bash|zsh|fish|elvish|powershell` and `lindeskctl man DIR`.
* Output follows the terminal: colour only on a TTY (`NO_COLOR` and `--color` win), lines
  are cut to the terminal width (query results and JSON never are), and a non-UTF-8 locale
  gets ASCII instead of `●`, `█` and friends.
