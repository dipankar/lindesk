# Benchmarking

`lindesk-bench` measures what lindesk is for: the app you are using stays responsive while
heavy background work runs, and that background work still gets done. It runs the same load
twice, once with lindesk stopped and once with it running, and prints a table comparing the
two.

No stress-ng or fio is needed. The probe and the load generators are built in.
`lindeskctl bench …` runs `lindesk-bench` with the same arguments (git-style), so
`lindeskctl bench run --quick` and `lindesk-bench run --quick` do the same thing.

## Running it

```bash
# A first try: 10 s, 512 MiB of writes, baseline only (lindesk stopped)
lindesk-bench run --quick --phases off

# The full A/B run ("torture"): about 2.5 minutes
bench/torture.sh --manage-services
```

`run` executes each phase in `--phases` (default `off,on`) in turn:

1. **Checks lindesk's state.** The off phase needs lindesk stopped and the on phase needs
   it running. If the state is wrong, the harness tells you what to run:

   ```bash
   systemctl --user stop lindesk-session
   sudo systemctl stop lindeskd
   ```

   lindesk counts as running if any of these is true: `org.lindesk.QoS1` is owned on the
   session bus, `org.lindesk.Broker1` is owned on the system bus, `lindesk-session.service`
   (user) or `lindeskd.service` is active, or a `lindesk-session` or `lindeskd` process
   exists.

   With `--manage-services`, the harness runs `systemctl --user stop|start
   lindesk-session` itself and puts it back the way it was at the end. It never touches
   `lindeskd` and never uses sudo. With a system-wide install, stop `lindeskd` yourself for
   the off phase.
2. **Starts the probe** as the "foreground app" in a transient scope in `app.slice`
   (`systemd-run --user --scope`). The probe first creates its 256 MiB read file.
3. **Starts the load** in a transient scope in `lindesk-batch.slice` (`--slice background`
   for `background.slice`). Once every generator runs, the load ramps up for a few seconds.
4. **On phase only:** marks the probe as the foreground app with
   `org.lindesk.QoS1.SetForegroundProcess(pid)` (or `Broker1.ReportForeground` when only
   lindeskd runs). It repeats the call every second so that the real focus (your terminal)
   doesn't take over. `--interact` also sends `BeginInteraction(pid)` every second, as if
   you were typing in the probe.
5. **Measures** for the phase duration. While it runs, the harness samples system PSI,
   MemAvailable and lindesk's level (`Status()`) once a second.
6. **Stops the load** and collects both JSON reports.

Results land in the current directory (or `--out`):

* `bench-results-<ts>.json` holds everything: config, host, per-phase probe and load
  reports, and the per-second timeline.
* `bench-results-<ts>.md` holds the table that is also printed.

### Profiles

| | `--quick` | default | `--profile full` (torture.sh) |
|---|---|---|---|
| Measured per phase | 10 s | 45 s | 60 s |
| Ramp-up / cooldown | 2 s / 3 s | 5 s / 5 s | 5 s / 5 s |
| CPU workers | nproc | nproc | nproc |
| Writes | buffered, cap 512 MiB | buffered + O_DIRECT, cap 4 GiB | buffered + O_DIRECT, cap 4 GiB |
| Sequential reads | yes | yes | yes |
| Memory pressure | 10% of MemAvailable | 25% | 40% |

Flags override the profile: `--duration`, `--cpu`, `--write-cap`, `--mem 8G|30%`,
`--no-write`, `--no-direct`, `--no-read`, `--no-mem`, and `--build-cmd "cargo build"` to
add a real build.

### The pieces on their own

```bash
lindesk-bench probe --duration 30s --stream    # JSON line per second, then a summary
lindesk-bench load --cpu 8 --write --mem 20% --duration 60s
```

You can run `probe` in a terminal while you do something heavy by hand, and compare the
numbers to an idle run.

## What is measured

The probe models an interactive app with three light threads:

| Measurement | How | What it stands for |
|---|---|---|
| **Wakeup latency** | sleeps 1 ms (`clock_nanosleep`, `CLOCK_MONOTONIC`) in a loop and records the overshoot | CPU scheduling delay: typing lag, dropped animation frames |
| **4K read latency** | one random 4 KiB `O_DIRECT` read every 10 ms from a 256 MiB file (buffered reads plus `POSIX_FADV_DONTNEED` where O_DIRECT is unsupported) | opening a file, a thumbnail, paging code back in |
| **Page fault latency** | every 10 ms, maps 64 fresh pages and times the first touch of each | allocation stalls from direct reclaim under memory pressure |
| **Foreground stall** | PSI of the probe's own cgroup (`cpu`, `io`, `memory.pressure`) over the window | the share of time the foreground app was waiting for a resource |

Latencies go into log-bucketed histograms with about 3% precision, and the bench reports
p50, p90, p95, p99, p99.9 and max.

The load counts its progress: bytes written and read, CPU operations and memory pages
touched. It counts only from the start of the measurement window, so that both phases are
compared over the same span.

## Reading the table

```text
| Metric                     | lindesk off | lindesk on | Δ            |
|----------------------------|------------:|-----------:|-------------:|
| Wakeup latency p99         |     …       |     …      | -80% better  |
| 4K read latency p99        |     …       |     …      | -90% better  |
| Foreground I/O stall       |     …       |     …      | -85% better  |
| Background written         |     …       |     …      | -15% worse   |
| Background CPU work (ops)  |     …       |     …      | -3%          |
```

* **Δ** is the change from the first column to the second. "better" and "worse" take the
  direction of the metric into account: lower is better for latency and stall, and higher
  is better for background progress. Changes under 5% are noise and get no verdict.
* **The demo works** when wakeup p99/max, read p99 and foreground I/O stall drop sharply,
  while background progress drops only a little. lindesk is meant to make background work
  *yield*, not to stop it.
* **p99 and max matter more than p50.** Stutter is a tail problem. A desktop with a fine
  median and a 300 ms max still feels broken.
* **The system stall rows** show how hard the machine as a whole was pushed. They should be
  similar in both phases. If they differ a lot, the two phases didn't see comparable load.
* **lindesk level** shows the number of seconds at green, yellow and red during the on
  phase. If it stays green the whole time, the load was too light for lindesk to step in.
  Use a heavier profile.

Run each configuration more than once before you draw conclusions. Background daemons,
thermal throttling and your own activity add noise. The off phase runs first, so it may
also pay for a cold cache.

## Safety caps

The bench is designed to run on your real workstation:

* **Memory:** the load takes at most `--max-mem-frac` of MemAvailable at start (default
  0.5). Values above 0.75 are clamped to 0.75, whatever you ask for. If MemAvailable drops
  below 2% of RAM (at least 512 MiB) while it runs, the load gives memory back.
* **Disk:** the load never keeps more than the write cap on disk. It writes files up to
  the cap, deletes them and starts over. The cap is also lowered to half of the free space
  left after a 5 GiB reserve. Scratch files live in `$XDG_CACHE_HOME/lindesk-bench` (or
  `--dir`) and are deleted at the end.
* **Time:** each phase is bounded (`--duration` at most 10 minutes). The load also stops
  itself after its own deadline, even if the harness disappears.
* **Ctrl-C** stops every generator immediately, deletes the scratch files and restores any
  service the harness stopped. Probe and load are started with a parent-death signal, so
  they stop and clean up even if the harness is killed. I/O already in the kernel (an
  `fsync` in progress) can't be cancelled. The load deletes its files after at most 3 s
  anyway, but on a saturated disk the process can take a few more seconds to exit while
  the kernel finishes that I/O.

Afterwards there should be nothing left. You can check with:

```bash
ls ${XDG_CACHE_HOME:-~/.cache}/lindesk-bench        # gone or empty
systemctl --user list-units 'lindesk-bench*'        # no units
```

## Results in the telemetry lake

The results JSON is the primary output; each phase carries its flat summary in `metrics`
(for example `wakeup_overshoot_us` with `labels.stat = "p99"`). When the optional
lindesk-telemetry package is installed, `run` also stores the results in the `bench_runs`
and `bench_samples` tables of your telemetry lake by running `lindesk-data import-bench
<results.json>` (`--lake DIR` picks another lake, `--no-lake` skips it): one sample per
metric, plus the per-second system PSI timeline with `labels['series'] = 'timeline'`.
lindesk-bench links no lake code itself. Without telemetry it says so and keeps the JSON,
which `lindesk-data import-bench` can store later.

```console
$ lindeskctl query "select run_id, label, ts from bench_runs order by ts desc"
$ lindeskctl query "select phase, value from bench_samples
                    where metric = 'wakeup_overshoot_us' and labels['stat'] = 'p99'"
```

Retention never removes bench tables. Delete them explicitly with
`lindeskctl data delete --table bench_samples --where "run_id = '…'"`.

A run is also a recording. You can replay its window through a candidate policy with
`lindeskctl simulate --policy ./my-policy --from <run start> --to <run end>`.

## Sharing results

Attach the `bench-results-<ts>.json` file. It contains the host facts that matter for
interpreting the numbers: kernel, CPU, RAM, desktop, the scratch filesystem and its I/O
scheduler, and lindesk's support level from the probe (FULL, IO-PARTIAL, CPU-ONLY,
MINIMAL). It also contains your hostname, the scratch directory path and the build command
if you used one. Edit them out first if they are private. It contains no other file names
and no process list beyond lindesk's own processes.

To compare runs made at different times or on different machines:

```bash
lindesk-bench compare before.json after.json          # before's off vs after's on
lindesk-bench compare a.json:on b.json:on             # pick phases explicitly
lindesk-bench compare run.json                        # one file: its phases side by side
```

The `.md` file pastes straight into an issue or pull request.
