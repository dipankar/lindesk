# doctor: what your system supports

```console
$ lindeskctl doctor
DOCTOR_OUTPUT
```

`doctor` has four parts:

1. **Requirements**: a one-line summary of the hard requirements of `lindeskd`,
   `lindesk-session` and `lindeskctl` (details: `lindeskctl doctor --preflight`), and the
   hardware in one line.
2. **System**: what the kernel, cgroups, PSI, I/O stack, filesystems and desktop support,
   the hardware (CPU topology including hybrid P/E cores, memory and swap, storage,
   power, virtualization), and conflicting daemons (system76-scheduler, ananicy-cpp,
   uresourced, gamemode). It ends with the **responsiveness level**:

   | Level | Means |
   |---|---|
   | FULL | cgroup v2, PSI, CPU and I/O control with an effective proportional I/O controller (io.latency, iocost or BFQ) and cgroup writeback |
   | IO-PARTIAL | I/O control limited to `io.max` (for example iocost available but not yet enabled, which `lindeskd` does with `io.iocost = "auto"`), or limited writeback (ZFS, NFS) |
   | CPU-ONLY | cgroup v1/hybrid, no PSI, or no I/O controller |
   | MINIMAL | per-process nice/ioprio/uclamp only |

3. **lindesk services**: whether `lindeskd` (system bus) and `lindesk-session` (session
   bus) are running, whether the GNOME extension or KWin script is installed and enabled
   (COSMIC and X11 adapters are built in), whether your config is valid, and how much
   telemetry is stored.
4. **The verdict**: either `✓ lindesk is protecting your desktop (FULL)` or the single
   most important next step.

Every warning carries a remedy after `→`. The exit status is 0 when nothing needs
action and 1 when something does, so `lindeskctl doctor -q && echo ok` works in scripts.
`lindeskctl doctor --json` has the same content as structured data (`level`, `reasons`,
`findings`, `probe`, `services`, `telemetry`, `requirements`, `needs_action`).

## Preflight

Installers and package scripts run:

```console
$ lindeskctl doctor --preflight            # the full checklist
$ lindeskctl doctor --preflight --quiet    # only warnings and failures (nothing when all pass)
```

```console
PREFLIGHT_OUTPUT
```

Each requirement is PASS, WARN (lindesk works, with a fallback) or FAIL (that component
can't run). `lindeskd` is checked as root, since that's how it runs; `lindesk-session` as
you (or as `$SUDO_UID` under sudo). The exit status is 1 only when a requirement fails.
The daemons run the same checks at startup and refuse to start (exit 78) on a failure.
