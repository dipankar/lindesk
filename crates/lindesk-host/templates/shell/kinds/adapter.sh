start_adapter() { # start_adapter <id>
    reply "$1" null
    # A real adapter watches the desktop (e.g. `xprop -spy -root _NET_ACTIVE_WINDOW` in a
    # background job) and emits one event per focus change:
    #   printf '{"jsonrpc":"2.0","method":"event","params":{"kind":"focus","ts":%s,"pid":%s,"source":"{{name}}"}}\n' \
    #       "$(date +%s%3N)" "$pid"
}
