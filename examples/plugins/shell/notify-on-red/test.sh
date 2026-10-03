#!/bin/sh
# Smoke test for notify-on-red: ./test.sh
# Runs the plugin with jq and (always) with the sed fallback.
set -eu
cd "$(dirname "$0")"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

run() { # run <label>: feed a session through ./plugin.sh, leave stdout in $out
    rm -f "$tmp/last"
    init='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"api_version":1,"host":"test","privilege":"user","caps":{},"config":{"dry_run":true,"min_interval_s":300,"state_file":"'"$tmp/last"'"}}}'
    # Key order differs: the host sends records as JSON values with sorted keys (focus),
    # but nothing guarantees that (red, green).
    focus='{"app_id":"org.mozilla.firefox","kind":"focus","pid":4242,"source":"x11","table":"events","target":{"manager":{"kind":"user","uid":1000},"name":"app-firefox-4242.scope","type":"unit"},"ts":1}'
    red='{"table":"events","kind":"level_changed","ts":2,"from":"yellow","to":"red","policy":"threshold"}'
    green='{"table":"events","kind":"level_changed","ts":3,"from":"red","to":"yellow","policy":"threshold"}'
    pressure='{"table":"pressure","ts":1,"scope":{"type":"system"},"resource":"io","some_avg10":1.0,"some_delta_us":3,"interval_ms":1000}'
    out=$(printf '%s\n' "$init" \
        '{"jsonrpc":"2.0","method":"sink.record","params":{"records":['"$pressure,$focus,$red"']}}' \
        '{"jsonrpc":"2.0","id":2,"method":"sink.flush"}' \
        '{"jsonrpc":"2.0","method":"sink.record","params":{"records":['"$green,$red"']}}' \
        '{"jsonrpc":"2.0","id":3,"method":"sink.flush"}' \
        'not json at all' \
        '{"jsonrpc":"2.0","id":99,"method":"no.such.method"}' \
        '{"jsonrpc":"2.0","method":"shutdown"}' | ./plugin.sh)
    expect "$1" '"id":1,"result":{"name":"notify-on-red"'
    expect "$1" 'would run: notify-send lindesk \"Background work is yielding to keep firefox responsive\"'
    expect "$1" 'rate-limited'
    expect "$1" '"id":3,"result":null'
    expect "$1" '"id":99,"error":{"code":-32601'
    n=$(printf '%s\n' "$out" | grep -c 'would run' || true)
    [ "$n" = 1 ] || fail "$1" "expected exactly one notification, got $n"
    [ -s "$tmp/last" ] || fail "$1" "state file not written"
    echo "ok ($1)"
}

expect() { # expect <label> <text>
    printf '%s\n' "$out" | grep -qF -- "$2" || fail "$1" "expected $2"
}

fail() {
    printf 'FAIL (%s): %s in:\n%s\n' "$1" "$2" "$out"
    exit 1
}

if command -v jq >/dev/null 2>&1; then
    run jq
fi
NOTIFY_ON_RED_NO_JQ=1 run sed
