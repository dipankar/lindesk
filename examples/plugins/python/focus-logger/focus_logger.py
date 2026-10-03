#!/usr/bin/env python3
"""focus-logger: a debugging aid for adapter authors (kinds: adapter + sink).

* As a **sink** it logs every focus and interaction event lindesk sees, with the unit the
  host resolved it to, to stderr, which ends up in the journal and in
  `lindeskctl plugin logs focus-logger`. An event without a unit means lindesk could not
  map the pid/app_id to a systemd scope, which is the most common adapter bug.
* As an **adapter** it reads commands from a FIFO and turns them into events, so you can
  drive the pipeline by hand without a desktop:

      echo "focus 1234"                > $XDG_RUNTIME_DIR/lindesk/focus-logger.fifo
      echo "focus org.mozilla.firefox" > $XDG_RUNTIME_DIR/lindesk/focus-logger.fifo
      echo "interaction 1234"          > $XDG_RUNTIME_DIR/lindesk/focus-logger.fifo
      echo "end 1234"                  > $XDG_RUNTIME_DIR/lindesk/focus-logger.fifo

Unit tests:   python3 test_focus_logger.py
Conformance:  lindeskctl plugin test .
"""
import os
import stat
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
if os.environ.get("LINDESK_SDK_PATH"):
    sys.path.insert(0, os.environ["LINDESK_SDK_PATH"])

import lindesk_plugin as lp  # noqa: E402

SOURCE = "focus-logger"


def say(line):
    sys.stderr.write(line + "\n")
    sys.stderr.flush()


def target_name(t):
    if not t:
        return None
    if t["type"] == "unit":
        m = t["manager"]
        return ("system:" if m["kind"] == "system" else "user/%d:" % m["uid"]) + t["name"]
    return t.get("path") or "pid:%s" % t.get("pid")


def describe(e, now):
    who = []
    if e.get("pid") is not None:
        who.append("pid=%d" % e["pid"])
    if e.get("app_id"):
        who.append("app_id=%s" % e["app_id"])
    what = e["kind"] + (" (end)" if e.get("end") else "")
    unit = target_name(e.get("target"))
    where = ("-> " + unit) if unit else "-> NO UNIT (pid/app_id did not map to a managed scope; check /proc/<pid>/cgroup)"
    return "%-11s %-40s %s  [source=%s, %dms ago]" % (
        what, " ".join(who) or "(no pid/app_id)", where, e.get("source") or "?", max(0, now - e["ts"])
    )


# --------------------------------------------------------------------------- sink


@lp.sink
def record(records):
    now = lp.now_ms()
    show = {"focus", "interaction"} | set(lp.config.get("also", []))
    for r in records:
        kind = r.get("kind")
        if kind in ("focus", "interaction"):
            if kind == "interaction" and not lp.config.get("interactions", True):
                continue
            say(describe(r, now))
        elif kind in show:
            fields = " ".join("%s=%s" % (k, v) for k, v in sorted(r.items()) if k not in ("table", "kind", "ts"))
            say("%-11s %s" % (kind, fields))


# --------------------------------------------------------------------------- adapter


def parse(line):
    """`focus 1234`, `focus org.app.Id`, `interaction 1234`, `end 1234` -> an event or None."""
    words = line.split()
    if len(words) != 2 or words[0] not in ("focus", "interaction", "end"):
        return None
    verb, arg = words
    if verb == "focus":
        return lp.focus(pid=int(arg), source=SOURCE) if arg.isdigit() else lp.focus(app_id=arg, source=SOURCE)
    if not arg.isdigit():
        return None
    return lp.interaction(pid=int(arg), end=(verb == "end"), source=SOURCE)


def fifo_path():
    path = lp.config.get("fifo")
    if path is None:
        runtime = os.environ.get("XDG_RUNTIME_DIR")
        path = os.path.join(runtime, "lindesk", "focus-logger.fifo") if runtime else ""
    return path


@lp.adapter
def watch(emit):
    path = fifo_path()
    if not path:
        say("adapter: no FIFO (set [config] fifo, or run with XDG_RUNTIME_DIR)")
        return
    os.makedirs(os.path.dirname(path), mode=0o700, exist_ok=True)
    if not os.path.exists(path):
        os.mkfifo(path, 0o600)
    elif not stat.S_ISFIFO(os.stat(path).st_mode):
        lp.log("%s exists and is not a FIFO" % path, "error")
        return
    say("adapter: write commands to %s (focus <pid|app_id>, interaction <pid>, end <pid>)" % path)
    while True:
        with open(path) as fifo:  # blocks until a writer opens it; reopened after each writer
            for line in fifo:
                line = line.strip()
                if not line:
                    continue
                event = parse(line)
                if event is None:
                    say("adapter: ignoring %r" % line)
                else:
                    emit(event)


if __name__ == "__main__":
    lp.run(name="focus-logger", version="0.1.0")
