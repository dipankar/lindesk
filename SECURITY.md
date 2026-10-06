# Security policy

lindesk includes a daemon that runs as root (`lindeskd`) and changes cgroup, scheduler and
sysctl settings, so we take reports seriously.

## Reporting a vulnerability

Please **don't open a public issue**. Use GitHub's private reporting
("Security" → "Report a vulnerability" on this repository), or email the maintainer listed
in `Cargo.toml`. Include what you found, how to reproduce it, and the output of
`lindeskctl --version`.

You'll get an acknowledgement within a few days. We'll agree on a disclosure date with you
and credit you in the release notes unless you'd rather not be named.

## What counts

* A local user gaining privileges, or changing another user's processes or settings, through
  `lindeskd`'s D-Bus API, the change system, plugins or the package scripts.
* A way around the guardrails: a change outside the allow-lists, loosening the safety caps,
  a realtime class, a change that isn't recorded or can't be reverted.
* A user plugin affecting anything beyond that user's own units, or a system plugin loaded
  from a directory that isn't root-owned.
* The MCP server performing a change without going through the daemons, or its HTTP
  endpoint being reachable without the token or from a browser origin.
* Telemetry exposing one user's activity to another.

## Supported versions

Until 1.0, only the latest release receives fixes.
