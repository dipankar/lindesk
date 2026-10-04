# Configuration

| Scope | File | Read by |
|---|---|---|
| system | `/etc/lindesk/lindesk.toml` | `lindeskd` |
| user | `$XDG_CONFIG_HOME/lindesk/lindesk.toml` (`~/.config/lindesk/lindesk.toml`) | `lindesk-session` |

Every key is optional; an empty (or missing) file means the defaults. Unknown keys are
errors, which catches typos. The packaged
[`lindesk.toml`](https://github.com/dipankar/lindesk/blob/main/packaging/config/lindesk.toml)
documents every key with its default.

## Commands

```console
$ lindeskctl config path                 # which file is in use (--system for /etc)
$ lindeskctl config check                # parse + validate; exit 1 on errors
$ lindeskctl config check ./lindesk.toml
$ lindeskctl config show                 # the effective config: commented defaults + your values
$ lindeskctl config show --json
$ lindeskctl config show --resolved      # threshold's tunables resolved for this hardware, with why
$ lindeskctl config init                 # write the commented default file (if absent)
```

No config file is needed: every command works with the defaults. `config init` writes the
shipped, fully commented `lindesk.toml` (`sudo lindeskctl config init --system` for
`/etc`), and `config show` prints that same commented file with your values applied.

```console
$ lindeskctl config show --resolved
threshold, resolved for this machine  24×CPU · hybrid 16P+8E · 63 GiB · NVMe · AC

KEY                                  VALUE                    SOURCE  WHY
epoch                                500ms                    auto    on AC
enter.cpu                            0.30                     auto    24 logical CPUs
enter.io                             0.20                     auto    no rotational disk
enter.memory                         0.15                     auto    compressed swap (zram/zswap): reclaim is cheap
critical_memory_low                  512MiB                   auto    min(512MiB, 4% of 62.5 GiB), at least 128MiB
aimd_floor[nvme0n1]                  40MiB/s                  auto    NVMe nominal ~2000MiB/s × 2%
aimd_step[nvme0n1]                   80MiB/s                  auto    NVMe nominal ~2000MiB/s × 4%
…
```

`config check` goes beyond parsing: it compiles `[[override]]` globs, validates the
`threshold` policy's settings, and warns about `[plugins.<name>]` sections that match no
built-in or installed plugin and `plugin_dirs` that don't exist.

```console
$ lindeskctl config check
warning: [plugins.steam-game]: no built-in or installed plugin has this name (typo, or not installed yet?)
✓ ~/.config/lindesk/lindesk.toml is valid
```

## Sections

| Section | What it controls |
|---|---|
| `[general]` | `tick`, extra `plugin_dirs`, `dry_run` (observe only) |
| `[io]` | `iocost = "auto" \| "on" \| "off"`, managed `devices` |
| `[safety]` | the caps no plugin can exceed (see [Safety invariants](../reference/safety.md)) |
| `[telemetry]` | `enabled`, `interval`, `flush`, `dir`, `[telemetry.retention]` |
| `[plugins.<name>]` | `enabled`, `priority`, `config = { … }` for any plugin, built-in or external |
| `[[override]]` | admin rules that always win: pin a `class` and/or `settings` on units (`unit` glob) or processes (`comm` glob) |

## Turning plugins on and off

```console
$ lindeskctl plugin disable rules          # [plugins.rules] enabled = false, then reload
$ lindeskctl plugin enable rules
$ sudo lindeskctl plugin disable --system on-battery
```

These edit the file in place with `toml_edit`, keeping your comments and layout, validate
the result, and then ask the daemon to reload: `lindeskd` through its `Reload` D-Bus
method, `lindesk-session` with `SIGHUP` (it re-reads its config and plugin directories on
SIGHUP or restart).

## Trying settings safely

* `[general] dry_run = true` computes, records and explains everything without applying
  it. `lindeskctl status` and `watch` show **DRY RUN**.
* Replay your own history with different thresholds before deploying them:

  ```console
  $ cp ~/.config/lindesk/lindesk.toml /tmp/tuned.toml && $EDITOR /tmp/tuned.toml
  $ lindeskctl simulate --policy builtin:threshold --config /tmp/tuned.toml --from -6h
  ```
