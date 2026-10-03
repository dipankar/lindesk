evaluate() { # evaluate <id> <request line>
    # Snapshots are nested; for real logic parse them with jq, e.g.
    #   io=$(jq -r '.params.snapshot.pressure[] | select(.resource == "io") | .some_avg10' <<<"$2")
    # and answer with intents such as
    #   {"target":{...},"setting":{"knob":"io_weight","value":10},"reason":"io pressure"}
    reply "$1" '{"intents":[],"level":"green"}'
}
