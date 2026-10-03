#!/usr/bin/env bash
# Smoke test for {{name}}: ./test.sh
set -euo pipefail
cd "$(dirname "$0")"

init='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"api_version":1,"host":"test","privilege":"user","caps":{},"config":{}}}'
out=$(printf '%s\n' "$init" {{test_request}} \
    '{"jsonrpc":"2.0","id":99,"method":"no.such.method"}' \
    '{"jsonrpc":"2.0","method":"shutdown"}' | ./plugin.sh)

expect() {
    if ! grep -qF -- "$1" <<<"$out"; then
        printf 'FAIL: expected %s in:\n%s\n' "$1" "$out"
        exit 1
    fi
}
expect '"id":1,"result":{"name":"{{name}}"'
expect '{{test_expect}}'
expect '"id":99,"error":{"code":-32601'
echo "ok"
