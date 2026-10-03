import time  # noqa: E402


@lp.adapter
def watch(emit):
    """Runs in a background thread once the host sends adapter.start.

    A real adapter subscribes to the desktop (D-Bus, Wayland, X11) and calls
    emit(lp.focus(pid=...)) when focus changes and emit(lp.interaction(pid=...)) on input.
    """
    interval = float(lp.config.get("interval_s", 5))
    while True:
        time.sleep(interval)
        # emit(lp.focus(pid=..., source="{{name}}"))
