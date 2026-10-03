_last = {}


def read_psi(resource):
    """(avg10, total_us) from /proc/pressure/<resource>, or None without PSI."""
    try:
        with open("/proc/pressure/%s" % resource) as f:
            fields = dict(kv.split("=") for kv in f.readline().split()[1:])
        return float(fields["avg10"]), int(fields["total"])
    except (OSError, KeyError, ValueError):
        return None


@lp.sensor
def poll(inp):
    """Called every [sensor] interval_ms with {"now": ms, "targets": [...]}.

    Return a list of lp.sample(...) or {"samples": [...], "usage": [...], "metrics": [...]}.
    """
    samples = []
    for resource in ("cpu", "io", "memory"):
        psi = read_psi(resource)
        if psi is None:
            continue
        avg10, total = psi
        prev = _last.get(resource)
        _last[resource] = (total, inp["now"])
        delta = total - prev[0] if prev else 0
        interval = inp["now"] - prev[1] if prev else 0
        samples.append(lp.sample(resource, avg10, some_delta_us=max(delta, 0), interval_ms=max(interval, 0)))
    return samples
