# Hardware awareness

lindesk adapts to the machine it runs on. Nothing has to be configured: the daemons detect
the hardware at start, the threshold policy scales its `"auto"` tunables to it, and the
matching [profile](profiles.md) is picked automatically. Three commands show what lindesk
knows:

```console
$ lindeskctl hardware      # what was detected, and how lindesk adapts to it
$ lindeskctl calibrate     # measure the latency map (≈15 s; --quick ≈3 s)
$ lindeskctl swap          # swap analysis with ready-to-apply fixes
```

All three take `--json`. They ask the running daemon (`GetHardware`, `SwapReport`,
`CalibrateWith`) and fall back to probing locally when none is running, so they work
right after install.

## What is detected

| Area | Details | Used for |
|---|---|---|
| CPU | architecture (`x86_64`, `aarch64`, `riscv64`, …), vendor, model, cores/threads, SMT | profile matching, uclamp defaults |
| Topology | hybrid P/E cores (Intel) or big.LITTLE (ARM): the CPUs of each kind | `x86-hybrid` / `arm-eas` profiles, the latency map's CPU types |
| Scheduler | EAS (`sched_energy_aware`), ITMT core ranking, NUMA nodes | whether uclamp steers placement or only frequency |
| cpufreq | driver, governor, EPP and the values the CPU advertises | guardrails for `cpu_governor` / `cpu_epp` changes |
| Memory | RAM, swap, compressed swap (zram/zswap) | memory thresholds, swap analysis |
| Storage | each managed disk (behind `/` and `/home`): NVMe, SSD, HDD, eMMC, SD, virtual, network; I/O scheduler | `io.max` floors and steps, the `hdd` profile, scheduler guardrails |
| Power | AC or battery, laptop chassis | the `battery` profile |
| Virtualization | hypervisor or container name | caveats (PSI and cpufreq describe the guest only) |

```console
$ lindeskctl hardware
CPU        12th Gen Intel Core i9-12950HX — hybrid: 8P (16 threads) + 8E
           x86_64 · intel · 16 cores / 24 threads · SMT on · P 0-15 · E 16-23
cpufreq    intel_pstate · governor powersave · EPP balance_performance
           governors: performance powersave
           EPP choices: default performance balance_performance balance_power power
scheduler  EAS no · ITMT no · 1 NUMA node
memory     62.5 GiB RAM, 20 GiB swap (compressed)
storage    nvme0n1 259:0  NVMe  scheduler bfq  ~2 GiB/s nominal
power      AC
virtual    bare metal

How lindesk adapts here
  → hybrid P/E cores: foreground boosts (uclamp.min) land on P-cores, batch caps (uclamp.max) on E-cores (profile x86-hybrid)
  → compressed swap: reclaim costs CPU, so memory pressure is treated as CPU work too

Latency map  not calibrated yet: lindesk uses nominal values. Measure it (≈15 s): lindeskctl calibrate
```

## Architecture-specific behaviour

* **Hybrid x86 (Intel P/E cores).** `uclamp.min` on the foreground makes the scheduler
  prefer P-cores; `uclamp.max` on batch work keeps it on E-cores. The `x86-hybrid` profile
  sets those values; ITMT core ranking is reported.
* **ARM big.LITTLE with EAS.** Energy Aware Scheduling places tasks by their utilisation
  clamp, so uclamp is the main steering knob (`arm-eas` profile). Without EAS, uclamp only
  shapes frequency and `cpu.weight` does most of the work.
* **Rotational disks.** Seeks dominate, so I/O intervention starts earlier and the AIMD
  `io.max` controller moves in smaller steps (`hdd` profile). Swap on an HDD is flagged by
  `lindeskctl swap`.
* **Laptops on battery.** Boosts are shorter and smaller and background work yields harder
  (`battery` profile).
* **Virtual machines and containers.** Everything works, but PSI and cpufreq describe the
  guest, and I/O control may be limited by the host. `lindeskctl doctor` explains what is
  missing.

## The latency map

`lindeskctl calibrate` measures a baseline on an idle machine and stores it as the
*latency map*. Policies compare live behaviour to it and prefer measured bandwidth over
nominal values (an NVMe that sustains 900 MB/s is throttled differently from one that
sustains 5 GB/s).

| Measured | How |
|---|---|
| CPU wakeup latency (p50/p99) | a 50 µs timer sleep on a thread pinned to each sampled CPU |
| Relative CPU speed | single-thread integer throughput per CPU, relative to the fastest: confirms P vs E cores empirically |
| Memory | dependent-load latency (pointer chase over 256 MiB), `memcpy` bandwidth, first-touch page-fault cost |
| Disks | 4 KiB random-read p50/p99 and sequential read/write on a bounded scratch file (`O_DIRECT` where possible) |
| Swap | page-in estimate per swap area (zram: a decompression micro-benchmark; disk: the measured 4 KiB read) |

```console
$ lindeskctl calibrate --quick --force      # on a busy machine: NOISY, disks skipped
Latency map  measured 2026-10-04 18:34 (693ms ago) in 0.5 s
  ⚠ NOISY: the machine was busy while measuring, so these numbers are less reliable. Re-run when idle: lindeskctl calibrate
  CPU TYPE  CPUS   SPEED              WAKEUP p50   p99
  P-cores   0-15   █████████▌  0.95×       2.3µs  84µs
  E-cores   16-23  ██████▊░░░  0.67×       3.9µs  29µs
  E-cores run at 70% of P-cores speed (single thread, measured)
  memory    load 97.0 ns · copy 16.3 GiB/s · first-touch fault 1.7µs
  disks     not measured (nominal values are used)
  swap      /dev/dm-0 (partition): page-in ≈ 82µs
  swap      /dev/zram0 (zram): page-in ≈ 174µs
  note busy at start (cpu some avg10 32.0% > 10%, io some avg10 79.9% > 10%): results are noisy
  note sampled 4 of 24 CPUs; the others got the median of their type (P/E)
  note storage skipped: the disks are busy (io pressure), results would be unbounded
  …
✓ saved to ~/.local/state/lindesk/calibration.json (your user calibration; lindesk-session uses it)
```

On an idle machine the disks are measured too:

```text
  DISK            4K READ p50    p99  SEQ READ  SEQ WRITE
  nvme0n1 (NVMe)         82µs  240µs   3 GiB/s  1.8 GiB/s
```

* With `lindeskd` running, the broker measures and stores the **system** map in
  `/var/lib/lindesk/calibration.json` (polkit may ask for authentication). Without it,
  lindeskctl measures as your user and saves `~/.local/state/lindesk/calibration.json`.
* A busy machine (any PSI `some avg10` above 10%) is refused. `--force` measures anyway and
  marks the map **NOISY**; while I/O pressure is high the disks are skipped (a single write
  to a saturated disk can block longer than the whole run) and nominal values are used.
* Re-run it after changing disks, BIOS power settings or kernels. `lindeskctl doctor`
  reports when the map is missing, noisy or old.

## Swap analysis

`lindeskctl swap` reads `/proc/swaps`, zram and zswap, the reclaim sysctls
(`vm.swappiness`, `vm.page-cluster`, `vm.watermark_scale_factor`, `vm.vfs_cache_pressure`,
`min_free_kbytes`), MGLRU, swap-in/out rates and the units with the most swapped-out
memory, and turns them into findings. Every finding lindesk can fix comes with a
ready-to-run, [versioned and revertible](changes.md) command:

```console
$ lindeskctl swap
Swap  8 GiB swap, 1 GiB used (12%) · 16 GiB RAM
  sda2 partition on HDD 8 GiB (1 GiB used, prio -2)
  zswap     off · MGLRU on (0x0007)
  vm        swappiness 60, page-cluster 3, watermark_scale_factor 10, vfs_cache_pressure 100, min_free 66 MiB

Findings  1
⚠ Swap on a hard disk
  /dev/sda2 is on a rotational disk (page-in costs milliseconds) with vm.swappiness 60: …
  fix: lindeskctl changes apply --sysctl vm.swappiness=10 --zswap on --zswap-compressor zstd --zswap-max-pool 20 --ttl 10m -m 'Swap on a hard disk'
```

`--ttl 10m` means the fix reverts itself after ten minutes unless you
`lindeskctl changes confirm ID`, so you can try it under real load first. lindesk never
creates or removes swap itself; that advice (for example "no swap with 8 GiB RAM: enable
zram") has no command.
