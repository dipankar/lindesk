# Changelog

All notable changes to lindesk. Versions follow [SemVer](https://semver.org); the plugin
API version (`lindesk_plugin_api::API_VERSION`) changes only on breaking protocol changes.

## [Unreleased]

First development series; nothing has been tagged yet. `0.1.0` will be cut from this.

### Added
- Plugin pipeline (sensors, adapters, classifiers, policies, actuators, sinks) with an
  any-language stdio JSON-RPC protocol, Rust and Python SDKs, conformance tests and
  `lindeskctl plugin new|test|install`.
- `lindeskd` (root broker) and `lindesk-session` (per-user agent): PSI-driven
  GREEN/YELLOW/RED control with interaction epochs, cgroup v2 CPU/I/O/memory knobs through
  systemd, adaptive `io.max`, everything runtime-only and restored on exit or crash.
- Foreground adapters for GNOME (Shell extension), KDE (KWin script), COSMIC and X11.
- Hardware awareness: CPU arch/vendor/hybrid topology, storage kinds, power, latency
  calibration, swap analysis, requirement checks; hardware-scaled defaults.
- CPU profiles (`balanced`, `latency`, `battery`, `throughput`, `gaming`, `arm-eas`,
  `x86-hybrid`, `hdd`) with automatic selection.
- Versioned, guardrailed, revertible changes (`lindeskctl changes`), with a dead-man
  auto-revert, and handling of conflicting priority daemons through the same mechanism.
- `lindeskctl`: `doctor`, `status`, live `watch`, `events`, `explain`, `run`, `on`/`off`,
  `pause`/`resume`, `profile`, `calibrate`, `hardware`, `swap`, `simulate`.
- `lindesk-mcp`: an MCP server for local agents (assessment tools, guardrailed changes).
- `lindesk-bench`: foreground latency probe and A/B harness.
- Optional `lindesk-telemetry` package: a Vortex telemetry lake queryable with DuckDB,
  with compaction, rollups, retention and deletion. The core package links neither.
- Packaging: `.deb`, `.rpm`, Arch, `install.sh`, `lindesk-uninstall`.

### Fixed
- Session agent crash on busy machines: sorting a unit's processes read `/proc` start
  times during the sort; an engine panic now also exits non-zero so systemd restarts it.
