#!/usr/bin/env python3
"""Tests for prometheus-exporter: python3 test_prometheus_exporter.py

Runs the plugin as a subprocess on an ephemeral port, feeds it records over JSON-RPC like
lindesk does, then scrapes /metrics.
"""
import json
import os
import re
import subprocess
import sys
import unittest
import urllib.request

PLUGIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "prometheus_exporter.py")


def unit(name):
    return {"type": "unit", "manager": {"kind": "user", "uid": 1000}, "name": name}


class Plugin:
    def __init__(self, config):
        self.proc = subprocess.Popen([sys.executable, PLUGIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self.next_id = 0
        self.logs = []
        self.init = self.request(
            "initialize",
            {"api_version": 1, "host": "test", "privilege": "user", "uid": 1000, "caps": {}, "config": config},
        )

    def send(self, msg):
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def request(self, method, params=None):
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params or {}})
        while True:
            msg = json.loads(self.proc.stdout.readline())
            if msg.get("id") == self.next_id:
                return msg
            if msg.get("method") == "log":
                self.logs.append(msg["params"]["message"])

    def record(self, *records):
        self.send({"jsonrpc": "2.0", "method": "sink.record", "params": {"records": list(records)}})
        self.request("sink.flush")  # a round trip, so the records have been processed

    def close(self):
        self.send({"jsonrpc": "2.0", "method": "shutdown"})
        self.proc.stdin.close()
        code = self.proc.wait(timeout=5)
        self.proc.stdout.close()
        return code


class TestExporter(unittest.TestCase):
    def setUp(self):
        self.p = Plugin({"listen": "127.0.0.1:0"})
        url = next((m.split()[-1] for m in self.p.logs if m.startswith("serving ")), None)
        self.assertIsNotNone(url, self.p.logs)
        self.url = url

    def tearDown(self):
        self.assertEqual(self.p.close(), 0)

    def scrape(self):
        with urllib.request.urlopen(self.url, timeout=5) as r:
            self.assertIn("text/plain", r.headers["Content-Type"])
            return r.read().decode()

    def value(self, text, series):
        m = re.search("^%s (\\S+)$" % re.escape(series), text, re.M)
        self.assertIsNotNone(m, "%s not in:\n%s" % (series, text))
        return float(m.group(1))

    def test_subscribes_through_the_manifest(self):
        self.assertNotIn("tables", self.p.init["result"])  # plugin.toml's [sink] tables apply

    def test_exports_pressure_level_settings_and_events(self):
        pressure = {
            "table": "pressure", "ts": 1, "scope": {"type": "system"}, "resource": "io",
            "some_avg10": 42.5, "full_avg10": 20.0, "some_delta_us": 425000, "interval_ms": 1000,
        }
        cgroup = dict(pressure, scope={"type": "cgroup", "path": "/user.slice/x.scope"})
        build = {
            "table": "units", "ts": 1, "target": unit("app-cargo-1.scope"), "class": "batch", "foreground": False,
            "usage": {"cpu_usage_us": 0, "io_rbytes": 0, "io_wbytes": 0, "io_rios": 0, "io_wios": 0,
                      "mem_current": 0, "interval_ms": 1000},
            "applied": [{"knob": "cpu_idle", "value": True}, {"knob": "io_weight", "value": 10}],
        }
        red = {"table": "events", "kind": "level_changed", "ts": 2, "from": "yellow", "to": "red", "policy": "threshold"}
        focus = {"table": "events", "kind": "focus", "ts": 3, "pid": 7, "source": "x11"}
        self.p.record(pressure, cgroup, build, red, focus, focus)

        text = self.scrape()
        self.assertEqual(self.value(text, 'lindesk_pressure_some_avg10{resource="io",scope="system"}'), 42.5)
        self.assertEqual(self.value(text, 'lindesk_pressure_full_avg10{resource="io",scope="system"}'), 20.0)
        self.assertNotIn("x.scope", text, "per-cgroup pressure is off by default")
        self.assertEqual(self.value(text, "lindesk_level"), 2)
        self.assertEqual(self.value(text, 'lindesk_applied_settings{class="batch"}'), 2)
        self.assertEqual(self.value(text, 'lindesk_units{class="interactive"}'), 0)
        self.assertEqual(self.value(text, 'lindesk_events_total{kind="focus"}'), 2)
        self.assertEqual(self.value(text, 'lindesk_events_total{kind="level_changed"}'), 1)
        self.assertIn("# TYPE lindesk_events_total counter", text)

    def test_other_paths_are_404(self):
        with self.assertRaises(urllib.error.HTTPError) as e:
            urllib.request.urlopen(self.url.replace("/metrics", "/nope"), timeout=5)
        self.assertEqual(e.exception.code, 404)


class TestBusyPort(unittest.TestCase):
    def test_a_busy_port_is_logged_not_fatal(self):
        first = Plugin({"listen": "127.0.0.1:0"})
        try:
            port = next(m for m in first.logs if m.startswith("serving ")).split(":")[-1].split("/")[0]
            second = Plugin({"listen": "127.0.0.1:" + port})
            self.assertIn("result", second.init)
            self.assertTrue(any("cannot listen" in m for m in second.logs), second.logs)
            self.assertEqual(second.close(), 0)
        finally:
            first.close()


if __name__ == "__main__":
    unittest.main()
