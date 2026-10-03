@lp.sink
def record(records):
    """Receive a batch of rows. Events carry a "kind" (focus, level, action_applied, ...)."""
    for r in records:
        if r.get("kind") == "level":
            lp.log("level changed %s -> %s (by %s)" % (r["from"], r["to"], r["policy"]))


# The tables to receive: pressure, units, events, metrics (omit for all).
record.tables = ["events"]
