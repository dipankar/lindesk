@lp.policy
def evaluate(snapshot):
    """Return the complete set of intents that should be in force right now.

    Anything you stop returning is reverted by the host, and every value passes through
    lindesk's safety caps, so a policy can't break the desktop.
    """
    io = next(
        (s for s in snapshot["pressure"] if s["resource"] == "io" and s["scope"]["type"] == "system"),
        None,
    )
    threshold = float(lp.config.get("io_threshold", 40.0))
    if io is None or io["some_avg10"] < threshold:
        return lp.output([], level="green")
    intents = [
        lp.intent(u["target"], io_weight=10, reason="io pressure %.0f%%" % io["some_avg10"])
        for u in snapshot["units"]
        if u["class"] in ("background", "batch") and not u.get("foreground")
    ]
    return lp.output(intents, level="yellow")
