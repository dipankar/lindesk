# Profiles

A profile is a named **overlay on `lindesk.toml`**: a few tunables that suit a situation
(on battery, playing a game, a spinning disk, an ARM board with EAS) plus optional rules that
say when the profile applies. lindesk works out of the box: the shipped profiles are built
into the binary, `[general] profile = "auto"` is the default, and auto-selection picks a
sensible one on any machine.

```console
$ lindeskctl profile list
  NAME         PRIO  MATCHES  DESCRIPTION
* battery        50  yes      On battery: short, small boosts and harder background yielding
  x86-hybrid     30  yes      x86 hybrid CPUs: P-core boosts for the foreground, E-core caps
  gaming         60  no       Max foreground protection while a game is focused
  hdd            40  no       Rotational storage: earlier I/O intervention
  arm-eas        30  no       aarch64 with EAS: steer via uclamp
  balanced        -  -        The shipped defaults
  latency         -  -        Earlier intervention, longer epochs, stronger boosts
  throughput      -  -        Build servers: background yields less
active: battery (battery power)
$ lindeskctl profile use gaming     # pin one (a versioned change, see "Changes")
$ lindeskctl profile use auto       # back to automatic selection
```

## How selection works

Only **one profile is active at a time**.

* `profile = "auto"` (default): every tick lindesk evaluates each profile's `[match]` against
  the machine (power source, CPU architecture/vendor/features, hybrid cores, storage kinds,
  CPU count, RAM) and the focused app. The matching profile with the **highest priority**
  wins; ties go to the alphabetically first name. When nothing matches, `balanced` is used.
* A switch happens at most once every **10 seconds** (the dwell), so unplugging and
  replugging the charger, or alt-tabbing out of a game for a moment, doesn't thrash the
  configuration. The decision is re-made every tick, so a blip shorter than the dwell is
  simply ignored.
* `profile = "<name>"` pins a profile: `[match]` rules are ignored. Pinning and unpinning
  take effect immediately. Pinning a profile that doesn't exist (or whose `inherits` chain
  is broken) falls back to `balanced` and says so.

Every switch recomputes the effective config and reloads the engine, and is recorded as a
`profile.changed` event (`{from, to, reason}`) in the telemetry lake:

```console
$ lindeskctl query "select ts, data->>'from' as from, data->>'to' as to, data->>'reason' as why
                    from events where name = 'profile.changed' order by ts desc limit 5"
```

## Shipped profiles

| Profile | Selected when | Priority | What it changes (threshold policy) |
|---|---|---|---|
| `balanced` | nothing else matches | (fallback) | nothing: the defaults in `lindesk.toml` |
| `latency` | manual only | - | epoch 1s; foreground YELLOW/RED at 3%/7% stall, RED after 500ms; fg weights 800/300, uclamp.min 40% |
| `battery` | `power = "battery"` | 50 | epoch 250ms; uclamp.min 10%; background weight 25 (10 + uclamp.max 30% at YELLOW); batch uclamp.max 20% |
| `throughput` | manual only | - | foreground YELLOW/RED at 10%/20%, RED after 3s; background/batch weights 80/50; gentler caps and AIMD |
| `gaming` | focused app is `steam_app_*`, `gamescope*`, `*.exe`, Lutris | 60 | inherits `latency`; epoch 2s; fg weight 1000, uclamp.min 50% (the safety maxima); background yields hard; gamescope pinned interactive |
| `arm-eas` | `arch = aarch64` and EAS active | 30 | epoch uclamp.min 45% (big cores), batch uclamp.max 25% and background 40% (little cores) |
| `x86-hybrid` | `arch = x86_64` and hybrid P/E cores | 30 | epoch uclamp.min 40% (P-cores), batch uclamp.max 30% and background 50% (E-cores) |
| `hdd` | a managed disk is rotational | 40 | enter.io 0.12; AIMD floor/step 4MiB/s, ×0.4 every 2s; batch io.weight 5 |

Priorities: **gaming > battery > hdd > arm-eas = x86-hybrid > balanced**. So a game in focus
always gets `gaming`; an unplugged laptop gets `battery` even if it has hybrid cores; a
desktop with an HDD gets `hdd`. The files in `packaging/config/profiles.d/` explain every
value they set and why.

### Combining profiles

Profiles don't stack automatically: exactly one is active. To combine two, write a profile
that `inherits` one and copies the other's settings, with a `[match]` covering both and a
higher priority. For example an ARM laptop on battery that should keep EAS steering:

```toml
# ~/.config/lindesk/profiles.d/arm-battery.toml
[profile]
name = "arm-battery"
description = "Battery savings plus EAS steering"
inherits = "battery"

[match]
power = "battery"
arch = ["aarch64"]
features = ["eas"]
priority = 55          # above battery (50)

[config.plugins.threshold.config.values]
yellow_batch_uclamp_max = 20.0
```

## Writing a profile

Profiles live in (later directories win, by profile **name**, not file name):

1. built into the binary (the shipped profiles)
2. `/usr/share/lindesk/profiles.d/*.toml` (packages)
3. `/etc/lindesk/profiles.d/*.toml` (administrator)
4. `$XDG_CONFIG_HOME/lindesk/profiles.d/*.toml` (you; read by lindesk-session only)

```toml
[profile]
name = "my-daw"                    # lowercase letters, digits, '-' and '_'; not "auto"
description = "Bitwig gets everything"
inherits = "latency"               # optional; overlays apply base → latency → my-daw

[match]                            # optional; omit it for a manual-only profile
foreground_app_id = ["com.bitwig.BitwigStudio"]   # globs, * and ?, case-insensitive
power = "ac"                       # ac | battery
arch = ["x86_64", "aarch64"]       # any of
vendor = ["amd", "intel"]          # intel amd arm apple qualcomm ampere other
hybrid = true                      # P/E or big.LITTLE cores
features = ["itmt"]                # all of: eas itmt smt numa compressed_swap portable virtualized
storage = ["nvme", "ssd"]          # any managed disk of these kinds: nvme ssd hdd emmc sd virtual network
min_cpus = 8
min_mem = "16GiB"
max_mem = "64GiB"
priority = 70                      # highest matching wins

[config]                           # exactly the lindesk.toml schema
[config.plugins.threshold.config]
epoch = "1500ms"

[config.plugins.threshold.config.values]
fg_epoch_uclamp_min = 50.0

[[config.override]]                # appended after lindesk.toml's [[override]]s
comm = "bitwig-studio"
class = "interactive"
```

Rules:

* Every given `[match]` criterion must hold. A `[match]` with no criteria is an error (remove
  the section instead to make the profile manual-only).
* `[config]` merges over `lindesk.toml`: tables merge key by key, values and arrays replace,
  except `[[config.override]]` entries, which are **appended** to the base's overrides.
  The result must still be a valid config (unknown keys are errors).
* `[config.safety]` is not allowed: safety caps belong in `lindesk.toml` only.
  `[config.general] profile` is not allowed either.
* An `inherits` chain must end; cycles and missing parents are reported and such a profile
  is never auto-selected.
* To stop a shipped profile from being auto-selected, redefine it under the same name
  without `[match]` (e.g. `[profile] name = "gaming"` with `inherits = "latency"`).

`lindeskctl profile list` shows parse errors, where each profile came from, and why it
matches (or not) right now.

## Agents and profiles

`SetProfile(name)` on D-Bus (and the MCP tool `set_profile`) is a shortcut for a
`profile_use` change: it is versioned, revertible, and when it comes from an agent it
auto-reverts unless confirmed. See [Changes](changes.md).
