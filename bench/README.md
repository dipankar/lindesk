# bench/

The lindesk benchmark: does the app you are using stay responsive while a compile, a big
copy and memory pressure run in the background, and does the background work still get
done?

| File | What |
|---|---|
| `torture.sh` | Runs `lindesk-bench run --profile full`: lindesk off, then on, and prints the comparison table. |

Everything else lives in the `lindesk-bench` binary (`crates/lindesk-bench`):

```text
lindesk-bench probe     foreground latency probe (wakeup, 4K read, page fault, PSI)
lindesk-bench load      bounded background load (CPU, writes, reads, memory, a real build)
lindesk-bench run       A/B harness: probe in app.slice vs load in lindesk-batch.slice
lindesk-bench compare   markdown table from saved results
```

No stress-ng or fio needed: the load generators are built in.

## Quick start

```bash
# Baseline only, 10 s, small (a good first try; lindesk must be stopped)
lindesk-bench run --quick --phases off

# The demo: lindesk off, then on
systemctl --user stop lindesk-session && sudo systemctl stop lindeskd
bench/torture.sh --phases off
systemctl --user start lindesk-session && sudo systemctl start lindeskd
bench/torture.sh --phases on
lindesk-bench compare bench-results-<off>.json bench-results-<on>.json

# Or let the harness switch the user agent itself (needs lindeskd stopped, or user-only
# mode)
bench/torture.sh --manage-services
```

## Prerequisites

* cgroup v2 with PSI and a systemd user session (`lindeskctl doctor`).
* About 10 GiB free in `$XDG_CACHE_HOME` (or `--dir`) for the full profile.
* Off phase: lindesk stopped. On phase: lindesk running. The harness checks both, and only
  ever runs `systemctl --user stop/start lindesk-session` itself, and only with
  `--manage-services`.
* An idle desktop: your own activity during the run shows up in the numbers.

## Safety caps

| | quick | default | full |
|---|---|---|---|
| Measured per phase | 10 s | 45 s | 60 s |
| Disk (write cap) | 512 MiB | 4 GiB | 4 GiB |
| Memory (of MemAvailable) | 10% | 25% | 40% |
| CPU workers | nproc | nproc | nproc |

* Memory never exceeds `--max-mem-frac` of MemAvailable (default 0.5, hard limit 0.75).
  The load gives memory back if MemAvailable drops below 2% of RAM (at least 512 MiB).
* The write cap is lowered to half of the free space left after a 5 GiB reserve.
* Ctrl-C stops every generator immediately and deletes the scratch directory (after at
  most 3 s, even if a thread is stuck in an fsync; the process itself exits once the kernel
  finishes that I/O). If the harness dies, its children get SIGTERM (parent-death signal)
  and clean up themselves.

See `docs/src/guide/bench.md` for how to read the table and share results.
