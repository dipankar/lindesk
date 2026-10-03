#!/usr/bin/env python3
"""Tests for focus-logger: python3 test_focus_logger.py"""
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest

PLUGIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "focus_logger.py")


class Plugin:
    def __init__(self, config):
        self.proc = subprocess.Popen(
            [sys.executable, PLUGIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
        )
        self.next_id = 0
        self.notifications = []
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
            self.notifications.append(msg)

    def next_notification(self):
        return json.loads(self.proc.stdout.readline())

    def close(self):
        """Shut down; returns (exit code, stderr)."""
        self.send({"jsonrpc": "2.0", "method": "shutdown"})
        self.proc.stdin.close()
        err = self.proc.stderr.read()
        code = self.proc.wait(timeout=5)
        self.proc.stdout.close()
        self.proc.stderr.close()
        return code, err


class TestFocusLogger(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.fifo = os.path.join(self.tmp.name, "lindesk", "focus.fifo")
        self.p = Plugin({"fifo": self.fifo})

    def tearDown(self):
        if self.p.proc.poll() is None:
            self.p.close()
        self.tmp.cleanup()

    def test_kinds(self):
        self.assertEqual(self.p.init["result"]["kinds"], ["adapter", "sink"])

    def test_logs_resolved_and_unresolved_events(self):
        target = {"type": "unit", "manager": {"kind": "user", "uid": 1000}, "name": "app-gnome-firefox-42.scope"}
        now = int(time.time() * 1000)
        records = [
            {"table": "events", "kind": "focus", "ts": now, "pid": 42, "app_id": "firefox", "target": target,
             "source": "x11"},
            {"table": "events", "kind": "interaction", "ts": now, "pid": 99, "end": False, "source": "gnome-shell"},
            {"table": "events", "kind": "level_changed", "ts": now, "from": "green", "to": "red", "policy": "t"},
        ]
        self.send_records(records)
        code, err = self.p.close()
        self.assertEqual(code, 0)
        self.assertIn("pid=42 app_id=firefox", err)
        self.assertIn("-> user/1000:app-gnome-firefox-42.scope", err)
        self.assertIn("NO UNIT", err)  # pid 99 had no target
        self.assertNotIn("level_changed", err)  # not in [config] also

    def test_fifo_commands_become_events(self):
        self.assertIn("result", self.p.request("adapter.start"))
        deadline = time.time() + 5
        while not os.path.exists(self.fifo) and time.time() < deadline:
            time.sleep(0.02)
        with open(self.fifo, "w") as f:
            f.write("focus 1234\nfocus org.mozilla.firefox\nbogus\nend 1234\n")
        events = [self.p.next_notification()["params"] for _ in range(3)]
        self.assertEqual((events[0]["kind"], events[0]["pid"], events[0]["source"]), ("focus", 1234, "focus-logger"))
        self.assertEqual(events[1]["app_id"], "org.mozilla.firefox")
        self.assertEqual((events[2]["kind"], events[2]["end"]), ("interaction", True))
        code, err = self.p.close()
        self.assertEqual(code, 0)
        self.assertIn("ignoring 'bogus'", err)

    def send_records(self, records):
        self.p.send({"jsonrpc": "2.0", "method": "sink.record", "params": {"records": records}})
        self.assertIn("result", self.p.request("sink.flush"))


if __name__ == "__main__":
    unittest.main()
