# Changelog

All notable changes to lindesk. Versions follow [SemVer](https://semver.org); the plugin
API version (`lindesk_plugin_api::API_VERSION`) changes only on breaking protocol changes.

## [Unreleased]

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
- **An app is all of its processes.** Browsers put the window's process in one unit and
  their renderers in another; focusing the window protects every unit the app runs in.
- **Builds yield.** A terminal tab or app scope running compilers and linkers is background
  work while it builds. The focused app is never throttled.
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
