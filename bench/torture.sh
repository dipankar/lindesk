#!/usr/bin/env bash
# lindesk torture benchmark: compile-like CPU load + a big copy + memory pressure, with
# lindesk off and then on, and a markdown table comparing the two.
#
#   bench/torture.sh                                  # off, then on (the "killer demo")
#   bench/torture.sh --manage-services                # let it stop/start lindesk-session
#   bench/torture.sh --phases off                     # baseline only
#   bench/torture.sh --build-cmd "cargo build --release"   # add a real build
#
# Every extra argument goes to `lindesk-bench run` (see `lindesk-bench run --help`).
#
# Profile "full": CPU workers = nproc, 4 GiB of buffered + O_DIRECT writes (deleted and
# rewritten in a loop), sequential reads, 40% of MemAvailable touched in a loop, 60 s
# measured per phase (about 75 s per phase with ramp-up and cooldown).
#
# Prerequisites
#   * Linux with cgroup v2 and PSI (`lindeskctl doctor` tells you), systemd user session.
#   * lindesk-bench on PATH, or a source tree (it is built with cargo if needed).
#   * ~10 GiB free in $XDG_CACHE_HOME (or pass --dir): 4 GiB write cap + 256 MiB probe
#     file, and the bench always leaves 5 GiB free.
#   * Off phase: lindesk stopped
#         systemctl --user stop lindesk-session
#         sudo systemctl stop lindeskd            # if installed system-wide
#     On phase: lindesk running (systemctl --user start lindesk-session; and lindeskd).
#     --manage-services stops/starts lindesk-session for you (never lindeskd, never sudo).
#   * Close heavy apps first and don't use the machine during the run; your own activity
#     shows up in the numbers.
#
# Safety: memory pressure never exceeds 75% of MemAvailable (default cap 50%) and is given
# back if MemAvailable runs low; writes never exceed the cap; Ctrl-C stops all load at
# once and deletes the scratch files.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)

find_bench() {
    if [[ -n ${LINDESK_BENCH:-} ]]; then
        echo "$LINDESK_BENCH"
        return
    fi
    local target=${CARGO_TARGET_DIR:-$ROOT/target}
    if [[ -f $ROOT/Cargo.toml ]] && command -v "${CARGO:-cargo}" >/dev/null; then
        echo "building lindesk-bench (release)..." >&2
        "${CARGO:-cargo}" build --release --quiet -p lindesk-bench --manifest-path "$ROOT/Cargo.toml" >&2
        echo "$target/release/lindesk-bench"
        return
    fi
    if command -v lindesk-bench >/dev/null; then
        command -v lindesk-bench
        return
    fi
    echo "torture.sh: lindesk-bench not found; install lindesk or run from a source tree" >&2
    exit 1
}

for tool in systemd-run systemctl; do
    command -v "$tool" >/dev/null || {
        echo "torture.sh: $tool not found (pass --no-scope to run without transient scopes)" >&2
    }
done

BENCH=$(find_bench)
exec "$BENCH" run --profile full "$@"
