# Responsiveness tools and experiments

Lindesk 0.3 adds latency observations, cooperative job admission, explicit dependency
leases, GPU accounting, and an opt-in EEVDF experiment. These complement the balanced
attention policy: foreground, media, visible normal apps, and explicitly interactive work
remain protected across competing workloads.

## Record and diagnose latency

```sh
lindeskctl latency probe --duration 5s
lindeskctl latency show
lindeskctl latency show --json > responsiveness.json
lindeskctl latency show --system --json > broker-timings.json
```

The probe measures how late this process wakes after a 10 ms sleep. Reporting happens
after each measured sleep, so the sampling cadence includes reporting overhead. It does
**not** measure keyboard-to-display latency, compositor frame delivery, or audio deadlines.
The probe lasts at most 60 seconds. It needs the session agent.

Each daemon retains the last 240 engine steps in memory: timestamp, health level,
per-stage execution time, action count, and system pressure. The session additionally
retains the last 1024 recognized application observations, independently of the optional
telemetry lake. Export includes these observations, nearest-rank p50/p95/p99, maximum,
count and sum, and an eviction count. The two rings can cover different time spans;
use timestamps when correlating them. Restarting the daemon clears the rings. Missing
observations mean **unknown**, not zero latency. Context records exclude application names,
process IDs and metric labels. These measurements do not automatically change policy.

An application or compositor integration can report real measurements through the existing
session-bus `ReportMetric(name, value, labels)` API. For development:

```sh
lindeskctl latency report frame 18.2
lindeskctl latency report input 24.7
lindeskctl latency report runqueue 2.1
lindeskctl latency report audio-xruns 1
```

| CLI kind | Metric name | Meaning supplied by the integration |
|---|---|---|
| `frame` | `desktop.frame_ms` | Measured frame duration, milliseconds |
| `input` | `desktop.input_ms` | Measured input-to-presentation delay, milliseconds |
| `runqueue` | `desktop.runqueue_ms` | Measured scheduling wait, milliseconds |
| `audio-xruns` | `desktop.audio_xruns` | Newly observed underruns/errors, **delta count**, not cumulative total |
| `timer` | `desktop.timer_late_ms` | Timer wakeup lateness, milliseconds |

Use an in-process D-Bus client for continuous reporting; launching a CLI for every frame
adds avoidable overhead. Record observations promptly. The recorder uses receipt timestamps;
it does not reconstruct historical timestamps from labels. Metrics must be finite; the
CLI accepts nonnegative values. The general API limits names to 128 bytes and labels to
16 entries (128-byte keys, 256-byte values). Labels are retained by the optional telemetry
sink, but omitted from the in-memory latency ring.

Automatic compositor frame collection and PipeWire profiler/XRUN collection are future
integrations. PipeWire's [pw-top documentation](https://docs.pipewire.org/page_man_pw-top_1.html)
defines WAIT, BUSY and ERR; an XRUN alone does not establish CPU contention as the cause.

## Cooperatively admit background commands

Create `jobs.json` containing argument arrays:

```json
[
  ["make", "-C", "project-a", "-j2"],
  ["make", "-C", "project-b", "-j2"],
  ["tar", "-cf", "source.tar", "source-directory"]
]
```

```sh
lindeskctl batch --jobs 4 jobs.json
```

The launcher starts at one concurrent command, samples CPU/memory/I/O PSI deltas every
250 ms, and adds one slot after eight calm observations. It halves the admission limit
when CPU stall is at least 20%, memory stall at least 5%, or I/O stall at least 10%.
Recovery requires CPU below 5%, memory below 1%, and I/O below 2%. Missing/reset counters
return the limit to one. A lower limit waits for running commands to finish before
admitting more; it never suspends jobs holding locks. A minimum of one preserves progress.

Each command runs through `lindeskctl run --batch`, using its own systemd user scope when
available, or the existing nice/I/O-priority fallback. The limit counts **commands**,
not their internal threads or subprocesses. Choose suitable inner `-j` settings.
This is not a GNU Make jobserver and does not resize an already-running build.

Arguments are passed directly without shell expansion. Use an explicit `sh -c` command
only when you want shell semantics. Commands receive closed stdin and inherit stdout and
stderr; output can interleave. `--json` is rejected because command output is streamed.
Failed jobs are reported and other queued jobs continue; any failure gives exit status 1.
SIGINT/SIGTERM/SIGHUP stop admission and terminate running worker process groups. Detached
processes that deliberately leave those groups are outside the launcher's supervision.
The file is limited to 4 MiB and 10,000 commands; `--jobs` accepts 1..256.

## Explicit dependencies for editors, terminals and services

```sh
lindeskctl depend CLIENT_PID SERVER_PID --ttl 5s
lindeskctl depend CLIENT_PID SERVER_PID --cancel
```

Applications can call `ReportDependency(client_pid, server_pid, ttl_ms)` on the session
bus. Both processes must belong to the caller's user. Use 1..30000 ms; zero cancels a pair.
Renew while a request is outstanding, then cancel it. The session forwards hints to the
broker when connected, preserving update order and subtracting forwarding delay. Hints
are not replayed after a broker restart; integrations must renew.

The server's unit is protected from throttling only while the client has a direct
attention role: foreground, critical, media, explicitly interactive, or visible normal.
Protection also covers ancestor throttles, does not change the configured class, and does
not raise the server to realtime priority. The threshold policy removes its existing
throttles. Status identifies the server with the `dependency` attention role.

There is no transitive propagation: a protected server cannot protect another server
merely by citing it. Leases are bounded to 64 pairs per user and 256 per daemon, use
monotonic expiry, and validate process start times on every engine step. Exit, PID reuse,
expiry, or cancellation removes the hint on the next step; restoration follows normal
reconciliation. The scope is the owning unit, so processes sharing one scope share
protection. Separate scopes provide finer control.

This is an explicit cooperation API. It does not infer outstanding requests by inspecting
Unix-socket or D-Bus traffic. Existing loopback TCP detection remains available separately.

## Inspect GPU contention

```sh
lindeskctl gpu PID --interval 500ms
lindeskctl gpu PID --json
```

Read two snapshots of `/proc/PID/fdinfo`, deduplicate DRM clients, and report per-engine
busy percentages and resident memory by region. Engine capacity is included in utilization
normalization. Missing or decreasing counters are unknown, not zero. Resident memory can
be shared across clients; do not sum it as exclusive usage. Scans are bounded to 1024
descriptors and 64 KiB per descriptor; the interval accepts 100 ms..10 s.

This command requires readable fdinfo and driver-exported counters. It measures activity,
not GPU queue latency, and changes no GPU settings. The [kernel DRM interface](https://docs.kernel.org/gpu/drm-usage-stats.html)
is partially standardized; unavailable counters are expected on some drivers. Cooperative
GPU workload admission and driver-specific controls still need separate validation.

## Experiment with EEVDF slices

```sh
lindeskctl run --interactive --slice-us 500 -- your-test-application
```

On supporting fair schedulers, this requests a 500 microsecond CPU slice for the launched
command. The accepted CLI range is 100..100000 microseconds. It changes only the newly
spawned child's attributes before execution, retaining SCHED_OTHER and nice. The kernel
value is read back; unsupported or clamped requests fail launch. An active sched_ext
scheduler is rejected. Other processes, the parent shell, and global scheduler settings
are unchanged. Applications can subsequently change their own scheduling attributes.

This is an opt-in whole-command experiment, not automatic UI-thread selection. A smaller
slice can increase context-switch overhead. Compare tail latency, throughput and power
before using it routinely. The [EEVDF documentation](https://docs.kernel.org/scheduler/sched-eevdf.html)
describes requested slices; they are not CPU quotas or realtime deadlines.

## Memory-pressure safeguards and remaining research

Page-in now checks memory pressure before starting and between processes. It skips further
requests at memory PSI `some avg10 >= 5%` or `full avg10 >= 1%`, rechecks available-memory
headroom between processes, and cancels further work on a focus change or pause. A syscall
already issued cannot be recalled. Missing PSI retains the existing reserve/budget guard.
This extends the existing cooldown and global/per-app memory protection budgets.

Refault-based working-set estimation is still research: RSS and recent focus do not tell us
which pages will be reused. MGLRU is often already enabled. Do not blindly raise its
`min_ttl_ms`: the [kernel documentation](https://docs.kernel.org/admin-guide/mm/multigen_lru.html)
explains the resulting early-OOM tradeoff. This release does not change that setting.

sched_ext remains an optional external experiment, not a bundled replacement scheduler.
Before loading one, verify that it implements the CPU weights, quotas, idle and nice
semantics Lindesk relies on. The [sched_ext contract](https://docs.kernel.org/scheduler/sched-ext.html)
allows BPF schedulers to ignore those controls. Kernel fallback handles stalls/errors;
it does not establish better desktop latency or compatibility.

## Acceptance criteria

Use the existing counterbalanced benchmark alongside actual desktop workloads: a build
while typing/scrolling, video playback during copies, and app switching under memory
pressure. Compare repeated runs with equivalent workload and power settings. Record
p95/p99 input/frame latency, audio underruns, background completion time, energy, and
Lindesk overhead. A successful synthetic timer probe is not proof of smoother frames.
