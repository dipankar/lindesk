# doctor: what your system supports

```console
$ lindeskctl doctor
⚠ requirements met with 2 warnings (lindeskctl doctor --preflight)
24×CPU · hybrid 16P+8E · 63 GiB · NVMe · AC

System
Kernel                   7.0.11-76070011-generic
CPU                      12th Gen Intel Core i9-12950HX — hybrid: 8P (16 threads) + 8E → foreground boosts favour P-cores via uclamp.min (EAS/ITMT); batch is capped with uclamp.max under pressure, which keeps it on E-cores
ITMT                     off on a hybrid Intel CPU → the scheduler does not rank P-cores first; foreground boosts rely on uclamp alone
Memory                   62.5 GiB RAM, 20 GiB swap (compressed)
Storage                  nvme0n1 NVMe (bfq)
Power                    AC
Virtualized              no
cgroup v2                yes
Controllers              cpuset cpu io memory hugetlb pids rdma misc dmem
User delegation (1000)   cpu io memory pids
systemd                  255 (running)
PSI                      global + per-cgroup
uclamp                   yes
io.latency               not built (CONFIG_BLK_CGROUP_IOLATENCY) → using iocost + io.max
iocost                   available, disabled → lindeskd enables it when io.iocost = "auto"
I/O scheduler            nvme0n1 bfq
io.weight                effective
io.max                   yes
Filesystem /             ext4, cgroup writeback supported
Filesystem /home         ext4, same filesystem as /
sched_ext                available, disabled
BPF/BTF                  yes
Desktop                  GNOME (Wayland) [ubuntu:GNOME] → install the lindesk GNOME Shell extension for focus tracking
system76-scheduler       running → system76-scheduler active: disable its foreground boost or stop it (systemctl disable --now com.system76.Scheduler.service)
Backend                  systemd
Responsiveness level: FULL

lindesk services
lindeskd                 not installed → install lindesk: sudo apt install ./lindesk_*.deb (or dnf/AUR), or ./packaging/install.sh
lindesk-session          not installed → install lindesk: sudo apt install ./lindesk_*.deb (or dnf/AUR), or ./packaging/install.sh
GNOME extension          not installed → foreground tracking on GNOME needs it: install the lindesk package or adapters/gnome-shell-extension
Config                   ~/.config/lindesk/lindesk.toml not present; defaults apply
Telemetry                22.0 KiB in ~/.local/state/lindesk/telemetry

→ next step: install lindesk: sudo apt install ./lindesk_*.deb (or dnf/AUR), or ./packaging/install.sh  (lindesk-session is not installed, so nothing is protected yet)
```

`doctor` has five parts:

1. **Requirements**: a one-line summary of the hard requirements of `lindeskd`,
   `lindesk-session` and `lindeskctl` (details: `lindeskctl doctor --preflight`), and the
   hardware in one line.
2. **System**: what the kernel, cgroups, PSI, I/O stack, filesystems and desktop support,
   the hardware (CPU topology including hybrid P/E cores, memory and swap, storage,
   power, virtualization), and conflicting daemons (system76-scheduler, ananicy(-cpp),
   uresourced; see [Other priority daemons](conflicts.md); gamemode is shown as compatible). It ends with the **responsiveness level**:

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
4. **Control plane**: the active [profile](profiles.md) and why it was chosen (or which one
   lindesk would pick when no daemon runs), [changes](changes.md) still waiting for
   confirmation with their countdown (`change #7 reverts in 8m`), whether the
   [latency map](hardware.md#the-latency-map) is calibrated (and how old or noisy it is),
   and whether the [MCP server](mcp.md) is installed and its optional user socket enabled.
   These lines are informational: they never make `doctor` exit 1.
5. **The verdict**: either `✓ lindesk is protecting your desktop (FULL)` or the single
   most important next step.

Every warning carries a remedy after `→`. The exit status is 0 when nothing needs
action and 1 when something does, so `lindeskctl doctor -q && echo ok` works in scripts.
`lindeskctl doctor --json` has the same content as structured data (`level`, `reasons`,
`findings`, `probe`, `services`, `control_plane`, `telemetry`, `requirements`,
`needs_action`).

## Preflight

Installers and package scripts run:

```console
$ lindeskctl doctor --preflight            # the full checklist
$ lindeskctl doctor --preflight --quiet    # only warnings and failures (nothing when all pass)
```

```console
$ lindeskctl doctor --preflight --quiet
  ⚠ WARN  polkit                      org.lindesk.policy not installed: only root can change classes  (lindeskd)
                                      → install /usr/share/polkit-1/actions/org.lindesk.policy (packaging/polkit)
  ⚠ WARN  Conflicting daemons         system76-scheduler running: it also adjusts priorities/cgroups and will fight lindesk  (lindeskd, lindesk-session, lindeskctl)
                                      → systemctl disable --now com.system76.Scheduler.service
⚠ requirements met with 2 warnings
```

Each requirement is PASS, WARN (lindesk works, with a fallback) or FAIL (that component
can't run). `lindeskd` is checked as root, since that's how it runs; `lindesk-session` as
you (or as `$SUDO_UID` under sudo). The exit status is 1 only when a requirement fails.
The daemons run the same checks at startup and refuse to start (exit 78) on a failure.
