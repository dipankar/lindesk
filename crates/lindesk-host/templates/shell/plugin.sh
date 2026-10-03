#!/usr/bin/env bash
# {{name}}: a lindesk {{kind}} plugin in plain bash.
#
# The host sends one JSON-RPC 2.0 message per line on stdin; answer requests (lines with
# an "id") with one line on stdout. Bash has no JSON parser, so this template pulls flat
# fields out with regexes; for nested data use jq, or the Python SDK. Anything written to
# stderr becomes a log line.
#
# Smoke test: ./test.sh        Conformance: lindeskctl plugin test .
set -u

reply() { # reply <id> <json-result>
    printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"
}

reply_error() { # reply_error <id> <code> <message>
    printf '{"jsonrpc":"2.0","id":%s,"error":{"code":%s,"message":"%s"}}\n' "$1" "$2" "$3"
}

log() { # log <trace|debug|info|warn|error> <message>
    printf '{"jsonrpc":"2.0","method":"log","params":{"level":"%s","message":"%s"}}\n' "$1" "$2"
}

field() { # field <name> <json>: the first "name":"string" value
    if [[ $2 =~ \"$1\":\"([^\"]*)\" ]]; then printf '%s' "${BASH_REMATCH[1]}"; fi
}

{{body}}

while IFS= read -r line; do
    id=""
    if [[ $line =~ \"id\":([0-9]+) ]]; then id=${BASH_REMATCH[1]}; fi
    method=$(field method "$line")
    case "$method" in
    initialize)
        reply "$id" '{"name":"{{name}}","version":"0.1.0","api_version":1,"kinds":["{{kind}}"]{{init_extra}}}'
        ;;
    shutdown)
        break
        ;;
{{cases}}
    *)
        if [ -n "$id" ]; then reply_error "$id" -32601 "method not found: $method"; fi
        ;;
    esac
done
exit 0
