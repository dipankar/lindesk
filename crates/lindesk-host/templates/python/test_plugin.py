#!/usr/bin/env python3
"""Tests for {{name}}: python3 test_{{module}}.py (plain unittest, no pytest needed).

They run the plugin as a subprocess and speak JSON-RPC to it, exactly like lindesk does.
"""
import json
import os
import subprocess
import sys
import unittest

PLUGIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "{{module}}.py")

CAPS = {
    "cgroup_v2": True, "systemd": True, "psi": True, "uclamp": False, "io_latency": False,
    "io_cost": False, "io_weight_effective": False, "io_max": True, "sched_ext": False, "bpf": False,
}


def unit(name, uid=1000):
    return {"type": "unit", "manager": {"kind": "user", "uid": uid}, "name": name}


class Plugin:
    """The plugin as a subprocess, plus a tiny JSON-RPC client."""

    def __init__(self, config=None):
        self.proc = subprocess.Popen(
            [sys.executable, PLUGIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True
        )
        self.next_id = 0
        self.notifications = []
        self.init = self.request(
            "initialize",
            {"api_version": 1, "host": "test", "privilege": "user", "uid": 1000, "caps": CAPS, "config": config or {}},
        )

    def request(self, method, params=None):
        self.next_id += 1
        self._send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params or {}})
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise AssertionError("plugin exited while handling %s" % method)
            msg = json.loads(line)
            if msg.get("id") == self.next_id:
                return msg
            self.notifications.append(msg)

    def result(self, method, params=None):
        msg = self.request(method, params)
        if "error" in msg:
            raise AssertionError("%s failed: %s" % (method, msg["error"]))
        return msg["result"]

    def notify(self, method, params=None):
        self._send({"jsonrpc": "2.0", "method": method, "params": params or {}})

    def _send(self, msg):
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def close(self):
        if self.proc.poll() is None:
            self.notify("shutdown")
            self.proc.stdin.close()
            self.proc.wait(timeout=5)
        self.proc.stdout.close()
        return self.proc.returncode


class Test{{Struct}}(unittest.TestCase):
    def setUp(self):
        self.p = Plugin()

    def tearDown(self):
        if not self.p.proc.stdout.closed:
            self.p.close()

    def test_handshake(self):
        self.assertIn("result", self.p.init, self.p.init)
        self.assertEqual(self.p.init["result"]["kinds"], ["{{kind}}"])

    def test_unknown_method_is_an_error(self):
        self.assertEqual(self.p.request("no.such.method")["error"]["code"], -32601)

    def test_clean_shutdown(self):
        self.assertEqual(self.p.close(), 0)

{{tests}}

if __name__ == "__main__":
    unittest.main()
