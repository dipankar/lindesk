# Commands this plugin puts in the batch class; add more with [config] batch = [...].
BATCH = {"rustc", "cc1", "cc1plus", "ld", "make", "ninja"}


@lp.classifier
def classify(proc):
    """Classify one process: pid, uid, comm, exe, cgroup, unit, slice, app_id.

    Return a class name, lp.classification(...), or None for "no opinion" (the next
    classifier decides). Set a [classifier] prefilter in plugin.toml so the host only asks
    about processes you care about.
    """
    if proc["comm"] in BATCH or proc["comm"] in lp.config.get("batch", []):
        return lp.classification("batch", confidence=0.9, reason="%s is a build tool" % proc["comm"])
    return None
