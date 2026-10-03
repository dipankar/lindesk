#!/usr/bin/env python3
"""prometheus-exporter: a lindesk sink that serves /metrics for Prometheus.

lindesk pushes records (pressure samples, per-unit state, events) to this sink; an HTTP
server in a background thread renders the latest values in the Prometheus text format.
Standard library only.

Unit tests:   python3 test_prometheus_exporter.py
Conformance:  lindeskctl plugin test .
"""
import http.server
import os
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
if os.environ.get("LINDESK_SDK_PATH"):
    sys.path.insert(0, os.environ["LINDESK_SDK_PATH"])

import lindesk_plugin as lp  # noqa: E402

LEVELS = {"green": 0, "yellow": 1, "red": 2}


class Metrics:
    """Latest values, shared between the protocol loop and the HTTP thread."""

    def __init__(self):
        self.lock = threading.Lock()
        self.pressure = {}  # (scope, resource) -> (some_avg10, full_avg10 or None)
        self.level = "green"  # until a level_changed event says otherwise
        self.units = {}  # target -> (class, number of applied settings, last seen)
        self.events = {}  # kind -> count
        self.records = 0

    def update(self, records, now):
        with self.lock:
            for r in records:
                self.records += 1
                table = r.get("table")
                if table == "pressure":
                    scope = r["scope"].get("path", "system")
                    if scope == "system" or lp.config.get("cgroup_pressure", False):
                        self.pressure[(scope, r["resource"])] = (r["some_avg10"], r.get("full_avg10"))
                elif table == "units":
                    self.units[target_name(r["target"])] = (r["class"], len(r.get("applied", [])), now)
                elif table == "events":
                    kind = r.get("kind", "unknown")
                    self.events[kind] = self.events.get(kind, 0) + 1
                    # The wire name is "level_changed"; older docs said "level".
                    if kind in ("level_changed", "level"):
                        self.level = r["to"]

    def render(self, now):
        stale_after = float(lp.config.get("stale_after_s", 60))
        out = []

        def family(name, kind, help_text, samples):
            out.append("# HELP %s %s" % (name, help_text))
            out.append("# TYPE %s %s" % (name, kind))
            for labels, value in samples:
                out.append("%s%s %s" % (name, fmt_labels(labels), fmt_value(value)))

        with self.lock:
            self.units = {t: u for t, u in self.units.items() if now - u[2] <= stale_after}
            per_class = {c: [0, 0] for c in lp.CLASSES}
            for cls, applied, _ in self.units.values():
                per_class.setdefault(cls, [0, 0])
                per_class[cls][0] += 1
                per_class[cls][1] += applied
            pressure = sorted(self.pressure.items())
            family(
                "lindesk_pressure_some_avg10",
                "gauge",
                "PSI some avg10: percent of time at least one task stalled on the resource.",
                [({"scope": s, "resource": r}, v[0]) for (s, r), v in pressure],
            )
            family(
                "lindesk_pressure_full_avg10",
                "gauge",
                "PSI full avg10: percent of time all non-idle tasks stalled on the resource.",
                [({"scope": s, "resource": r}, v[1]) for (s, r), v in pressure if v[1] is not None],
            )
            family(
                "lindesk_level",
                "gauge",
                "Health level from the policies: 0 green (hands off), 1 yellow, 2 red.",
                [({}, LEVELS.get(self.level, -1))],
            )
            family(
                "lindesk_units",
                "gauge",
                "Workloads lindesk tracks, by QoS class.",
                [({"class": c}, n[0]) for c, n in sorted(per_class.items())],
            )
            family(
                "lindesk_applied_settings",
                "gauge",
                "Settings lindesk currently has applied, by the QoS class of the workload.",
                [({"class": c}, n[1]) for c, n in sorted(per_class.items())],
            )
            family(
                "lindesk_events_total",
                "counter",
                "Events seen since the exporter started, by kind.",
                [({"kind": k}, n) for k, n in sorted(self.events.items())],
            )
            family("lindesk_exporter_records_total", "counter", "Records received from lindesk.", [({}, self.records)])
        return "\n".join(out) + "\n"


def target_name(t):
    """Same spelling as `lindeskctl status`: user/1000:app.scope, system:x.service, ..."""
    if t["type"] == "unit":
        m = t["manager"]
        return ("system:%s" if m["kind"] == "system" else "user/%d:%%s" % m["uid"]) % t["name"]
    if t["type"] == "cgroup":
        return "cgroup:" + t["path"]
    return "pid:%d" % t["pid"]


def fmt_labels(labels):
    if not labels:
        return ""
    esc = lambda v: str(v).replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n")  # noqa: E731
    return "{%s}" % ",".join('%s="%s"' % (k, esc(v)) for k, v in sorted(labels.items()))


def fmt_value(v):
    return repr(float(v)) if isinstance(v, float) else str(v)


METRICS = Metrics()
SERVER = None


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802 - http.server's naming
        if self.path.split("?")[0] == "/metrics":
            body, ctype, code = METRICS.render(time.monotonic()), "text/plain; version=0.0.4; charset=utf-8", 200
        elif self.path == "/":
            body, ctype, code = '<a href="/metrics">/metrics</a>\n', "text/html", 200
        else:
            body, ctype, code = "not found\n", "text/plain", 404
        data = body.encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, fmt, *args):  # keep scrapes out of the journal
        pass


@lp.on_init
def start_server(context):
    global SERVER
    host, _, port = lp.config.get("listen", "127.0.0.1:9469").rpartition(":")
    try:
        SERVER = http.server.ThreadingHTTPServer((host or "127.0.0.1", int(port)), Handler)
    except (OSError, ValueError) as e:
        # Keep running: a busy port shouldn't crash-loop the plugin. The error shows up in
        # `lindeskctl plugin logs prometheus-exporter`.
        lp.log("cannot listen on %s: %s" % (lp.config.get("listen"), e), "error")
        return
    SERVER.daemon_threads = True
    threading.Thread(target=SERVER.serve_forever, kwargs={"poll_interval": 0.1}, daemon=True).start()
    lp.log("serving http://%s:%d/metrics" % SERVER.server_address[:2])


@lp.on_shutdown
def stop_server():
    if SERVER is not None:
        SERVER.shutdown()
        SERVER.server_close()


@lp.sink
def record(records):
    METRICS.update(records, time.monotonic())


if __name__ == "__main__":
    lp.run(name="prometheus-exporter", version="0.1.0")
