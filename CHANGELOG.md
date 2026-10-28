# Changelog

All notable changes to lindesk. Versions follow [SemVer](https://semver.org); the plugin
API version (`lindesk_plugin_api::API_VERSION`) changes only on breaking protocol changes.

## [Unreleased]

## [0.3.0] - 2026-10-28

First public GitHub release, including the locally tested 0.1.x and 0.2.0 features.

Packages: core and optional telemetry as Debian `.deb` and Fedora `.rpm` for x86_64
and ARM64; Arch `.pkg.tar.zst` for x86_64; binary `.tar.zst` archives for both
architectures. `SHA256SUMS` covers every attached package and archive. Debian packages
are built on Ubuntu 24.04; use the package for your distribution and architecture.

### Added
- Bounded in-memory latency observations and engine/PSI context, D-Bus `LatencyReport`,
  CLI summaries/JSON export, application metric reporting, and a timer-wakeup probe.
- Pressure-adaptive admission of explicitly queued background commands with bounded
  concurrency, slow recovery, direct argv execution and signal cleanup.
- Same-user one-hop dependency leases with monotonic expiry, start-time validation,
  limits, ordered broker forwarding and protection through ancestor cgroups.
- Read-only per-process DRM GPU diagnostics and opt-in verified EEVDF slices for launched commands.
- Feature guide covering CLI/API usage, safety bounds, benchmark criteria and remaining
  frame/audio, refault, jobserver, GPU-control and sched_ext research.

### Fixed
- Reject partial benchmark pauses and verify both daemons' state at phase boundaries.
- Start the telemetry maintenance timer in existing desktop sessions after installation.
- Update locked TLS, Tokio and compression dependencies for current advisories; document
  narrowly scoped exceptions for unused transitive APIs in `docs/RELEASING.md`.
- Cancel further page-in work after focus changes/pause; check reclaim pressure and
  available-memory headroom before issuing additional requests.
- Bound application metric names/labels and reject non-finite D-Bus values.

### Measurement limits
- The short local off/on storage comparison was inconclusive. Synthetic wakeup and read
  timings do not establish input-to-display improvements; no universal speedup is claimed.
- Refocus your application after a synthetic foreground benchmark if needed.
- Packaged `lindesk-session` includes the GPL-3.0 COSMIC adapter; see `THIRD-PARTY.md`.

## [0.2.0] - 2026-10-09

### Added
- Balanced desktop attention: protect active playback/capture, visible normal apps, and
  explicitly interactive work across focus changes with bounded CPU/I/O shares and memory.
- Reject indirect throttles through their ancestor cgroups and identify each protected
  app's attention role in status. Visible builds still yield; unknown presence is not guessed.
- Tunable media, interactive and visible shares, with regression coverage for pressure,
  presence changes, competing policies and memory bounds.
- Attribute local PulseAudio-compatible streams to their application rather than the
  pipewire-pulse bridge; do not mistake remote PulseAudio PIDs for local processes.

## [0.1.1] - 2026-10-09

### Fixed
- Allocate bounded memory protection through parent cgroups; diagnose ancestor ceilings
  instead of implying that a configured leaf value guarantees protection.
- Persist recovery records before changing settings, retain uncertain writes for recovery,
  and refuse mutations when the original value or journal is unavailable.
- Stop treating ordinary PSI file readability as pressure, eliminating needless idle wakeups.
- Detect local dependencies through the focused application's network namespace while
  retaining the broker's private network isolation.
- Describe utilization clamps as conditional performance hints, and explain terminal focus
  granularity with an explicit interactive-command workflow.
- Run benchmark phases in counterbalanced repetitions, report medians/ranges, and support
  preserving desktop focus with clear measurement limits.

## [0.1.0]

The first release.

### Added
- **The governor.** `lindeskd` (root broker) and `lindesk-session` (per-user agent):
  pressure-driven GREEN/YELLOW/RED control with hysteresis, a short boost when you click or
  type and a longer one when you switch apps, cgroup v2 CPU, I/O and memory control through
  systemd, an adaptive `io.max` limit that tightens only while the foreground still stalls.
  Everything is runtime-only and restored on exit, on a crash and on uninstall.
- **Apps you just left stay warm.** A recently used app keeps a raised share and its memory
  protected while you are in another window, so switching back to your browser doesn't wait
  for it to be paged in.
- **A swapped-out app is read back in when you switch to it**, in one go and in the
  background, instead of one stalled page fault at a time (`[plugins.pagein]`).
- **The threads you are waiting on go first.** In the app in use, the UI threads of
  Chromium- and Firefox-based apps (compositor, I/O, main) are put ahead of their worker
  pools, recognised by thread name; undone when focus moves on (`[plugins.thread-qos]`).
- **What your app is waiting on is not held back.** A unit the app in use has an open
  local connection to (a dev server rebuilding in a terminal tab, a database) is not
  throttled while the connection lasts (`[plugins.serving]`).
- **Less churn under bursty load.** A level that keeps bouncing backs off before stepping
  down again, and the input boost holds through short typing pauses; on the development
  machine the level had been changing four times a minute.
- **App Nap.** An app whose windows are all off screen and that is silent has its timers
  coalesced and is steered to the efficient cores. Nothing is frozen or capped; it wakes
  when it is seen or heard (GNOME and Hyprland; needs PipeWire).
- **An app is all of its processes.** Browsers put the window's process in one unit and
  their renderers in another; focusing the window protects every unit the app runs in.
- **Builds yield.** A terminal tab or app scope running compilers and linkers is background
  work while it builds. The focused app is never throttled.
- **Heavy neighbours yield.** When the app in use is stalling, anything else doing real
  I/O is limited too, whatever its class.
- **I/O weights that are enforced.** lindesk gives the kernel's iocost controller latency
  targets, so a saturated disk is shared by weight instead of first come, first served.
- **Foreground tracking** for GNOME (Shell extension), KDE (KWin script), COSMIC, Hyprland
  (built in, so it drops into Omarchy) and X11; input activity on Wayland compositors
  through `ext-idle-notify-v1`.
- **Plugins** for every stage (sensors, adapters, classifiers, policies, actuators, sinks)
  in any language over stdio JSON-RPC; Rust and Python SDKs; conformance tests;
  `lindeskctl plugin new|test|install`; `lindeskctl simulate` to replay history through a policy.
- **Hardware awareness:** CPU architecture and hybrid topology, storage kinds, power source,
  latency calibration, swap analysis, requirement checks, and defaults scaled to them.
  Profiles (`balanced`, `latency`, `battery`, `throughput`, `gaming`, `arm-eas`,
  `x86-hybrid`, `hdd`) chosen automatically.
- **`lindeskctl`:** `doctor` (including limits and scheduler rules lindesk didn't make),
  `status`, the live `watch` dashboard, `events`, `explain`, `run`, `on`/`off`,
  `pause`/`resume`, `profile`, `changes`, `calibrate`, `hardware`, `swap`.
- **Versioned, guardrailed, revertible changes** with an auto-revert timer, also used to
  switch off conflicting priority daemons and put them back on uninstall.
- **`lindesk-mcp`:** an MCP server so a local agent can assess a machine and apply
  guardrailed changes that revert themselves unless confirmed.
- **`lindesk-bench`:** a foreground latency probe and an off/on comparison harness.
- **Optional `lindesk-telemetry`:** history in a Vortex lake queryable with DuckDB, with
  compaction, rollups, retention and deletion. The core package links neither.
- **Packaging:** `.deb`, `.rpm`, Arch, `install.sh` and `lindesk-uninstall`; x86_64 and aarch64.
- lindesk guarantees its own resources (protected, unswappable, memory-locked working set;
  high CPU and I/O share) so it stays responsive under the load it is managing. Its logging
  never waits for the journal: when journald is stuck behind a saturated disk, records are
  dropped and counted instead of stalling the governor.
- **Cheap to run.** Only the system and the focused app's cgroups are sampled on every
  step, through file descriptors kept open; everything else is sampled every two seconds.
  The loop drops to one step a second while the machine is calm and wakes at once on input
  or rising pressure. mimalloc is the allocator of every binary.

### Known limitations
- On GNOME Wayland the Shell extension loads at the next login after the first install.
- The KDE adapter is tested against stubs, not yet on a real Plasma session; the Hyprland
  adapter against a stand-in for Hyprland's sockets, not yet on an Omarchy machine.
- No published benchmark numbers yet: run `lindesk-bench run` on your machine.
- The `lindesk-session` binary in the packages is GPL-3.0 because of the COSMIC adapter's
  dependency (see THIRD-PARTY.md); the source is MIT OR Apache-2.0.
