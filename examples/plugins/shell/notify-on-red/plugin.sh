#!/bin/sh
# notify-on-red: a lindesk sink in plain POSIX sh.
#
# When lindesk's level goes red (the app you are using is stalling, so background work is
# being throttled hard), show a desktop notification naming that app. At most one
# notification per `min_interval_s` (default 5 minutes), remembered in a state file in
# $XDG_RUNTIME_DIR so restarts don't reset the limit.
#
# The protocol: one JSON-RPC 2.0 message per line on stdin. Requests (lines with an "id")
# get exactly one line back on stdout; notifications (sink.record, shutdown) get nothing.
# stderr is captured as logs. JSON is read with jq when installed, else with sed.
#
# Smoke test: ./test.sh        Conformance: lindeskctl plugin test .
set -u

min_interval=300
dry_run=false
state_file="${XDG_RUNTIME_DIR:-/tmp}/lindesk-notify-on-red.$(id -u).last"
app=""

if command -v jq >/dev/null 2>&1 && [ -z "${NOTIFY_ON_RED_NO_JQ:-}" ]; then
    have_jq=true
else
    have_jq=false
fi

# ---------------------------------------------------------------------------- protocol

reply() { # reply <id> <json>
    printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"
}

reply_error() { # reply_error <id> <code> <message>
    printf '{"jsonrpc":"2.0","id":%s,"error":{"code":%s,"message":"%s"}}\n' "$1" "$2" "$3"
}

log() { # log <trace|debug|info|warn|error> <message>
    msg=$(printf '%s' "$2" | sed 's/[\\"]/\\&/g')
    printf '{"jsonrpc":"2.0","method":"log","params":{"level":"%s","message":"%s"}}\n' "$1" "$msg"
}

# header <line>: set $id and $method ("-" when missing) with parameter expansion only:
# no jq or sed process per message, so shutdown is instant even on a loaded machine.
header() {
    h=${1%%\"params\":*} # the host writes jsonrpc, id, method, then params
    case $h in
    *'"method":"'*) method=${h#*\"method\":\"}; method=${method%%\"*} ;;
    *) method=- ;;
    esac
    case $h in
    *'"id":'[0-9]*) id=${h#*\"id\":}; id=${id%%[!0-9]*} ;;
    *) id=- ;;
    esac
}

# config <line> <key>: a scalar from params.config.
config() {
    if $have_jq; then
        printf '%s\n' "$1" | jq -r --arg k "$2" '.params.config[$k] // empty | tostring'
    else
        printf '%s\n' "$1" | sed -n "s/.*\"$2\":\"\{0,1\}\([^\",}]*\)\"\{0,1\}.*/\1/p"
    fi
}

# events <line>: one line per event record: kind|to|app_id|pid. (Not tabs: read would
# merge empty tab-separated fields.)
events() {
    if $have_jq; then
        printf '%s\n' "$1" | jq -r '.params.records[]? | select(.table == "events")
            | [.kind, (.to // ""), (.app_id // ""), (.pid // "" | tostring)] | join("|")'
    else
        # Key order inside a record is not guaranteed, so: flatten nested objects
        # ("target":{...} -> "target":0, repeated), put each record on its own line, keep
        # the events, then pull out each field independently.
        printf '%s\n' "$1" | sed -e ':a' -e 's/:{[^{}]*}/:0/' -e 'ta' | sed 's/{[^{}]*}/\
&\
/g' | sed -n '/^{[^{}]*"table":"events"[^{}]*}$/p' | while IFS= read -r rec; do
            kind=$(printf '%s\n' "$rec" | sed -n 's/.*"kind":"\([a-z_]*\)".*/\1/p')
            to=$(printf '%s\n' "$rec" | sed -n 's/.*"to":"\([a-z]*\)".*/\1/p')
            app_id=$(printf '%s\n' "$rec" | sed -n 's/.*"app_id":"\([^"]*\)".*/\1/p')
            pid=$(printf '%s\n' "$rec" | sed -n 's/.*"pid":\([0-9][0-9]*\).*/\1/p')
            printf '%s|%s|%s|%s\n' "$kind" "$to" "$app_id" "$pid"
        done
    fi
}

# ---------------------------------------------------------------------------- the plugin

# Remember a short name for the focused app: org.mozilla.firefox -> firefox, else
# /proc/<pid>/comm.
app_name() { # app_name <app_id> <pid>
    app=""
    if [ -n "$1" ]; then
        app=${1##*.}
    elif [ -n "$2" ] && [ -r "/proc/$2/comm" ]; then
        read -r app <"/proc/$2/comm"
    fi
}

notify() {
    now=$(date +%s)
    last=0
    [ -r "$state_file" ] && read -r last <"$state_file"
    case $last in '' | *[!0-9]*) last=0 ;; esac
    if [ $((now - last)) -lt "$min_interval" ]; then
        log debug "level red; notification rate-limited ($((now - last))s since the last one)"
        return
    fi
    printf '%s\n' "$now" >"$state_file"
    body="Background work is yielding to keep ${app:-the app you are using} responsive"
    if $dry_run; then
        log info "would run: notify-send lindesk \"$body\""
        return
    fi
    if ! command -v notify-send >/dev/null 2>&1; then
        log warn "notify-send not found (install libnotify-bin / libnotify)"
        return
    fi
    # lindesk passes plugins a minimal environment; find the session bus ourselves.
    if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ] && [ -S "${XDG_RUNTIME_DIR:-/nonexistent}/bus" ]; then
        export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
    fi
    notify-send --app-name=lindesk --icon=dialog-information "lindesk" "$body" >/dev/null 2>&1 &
    log info "notified: $body"
}

record() { # record <sink.record line>
    # A here-document, not a pipe: a piped loop runs in a subshell and would forget $app.
    while IFS='|' read -r kind to app_id pid; do
        case $kind in
        focus) app_name "$app_id" "$pid" ;;
        # "level_changed" on the wire; early templates said "level".
        level_changed | level) [ "$to" = red ] && notify ;;
        esac
    done <<EOF
$(events "$1")
EOF
}

while IFS= read -r line; do
    header "$line"
    case $method in
    initialize)
        v=$(config "$line" min_interval_s)
        case $v in '' | *[!0-9]*) ;; *) min_interval=$v ;; esac
        v=$(config "$line" dry_run) && [ "$v" = true ] && dry_run=true
        v=$(config "$line" state_file) && [ -n "$v" ] && state_file=$v
        reply "$id" '{"name":"notify-on-red","version":"0.1.0","api_version":1,"kinds":["sink"]}'
        $have_jq || log debug "jq not found; parsing JSON with sed"
        ;;
    sink.record) record "$line" ;;
    sink.flush) reply "$id" null ;;
    shutdown) break ;;
    *) [ "$id" != - ] && reply_error "$id" -32601 "method not found: $method" ;;
    esac
done
exit 0
