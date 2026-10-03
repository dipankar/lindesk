"""Tests for lindesk_plugin.py: drive real plugin processes over pipes, like the host does.

Run with ``python3 test_lindesk_plugin.py`` (plain unittest, no pytest needed).
"""

import json
import os
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))

PLUGIN = textwrap.dedent(
    """
    import sys
    sys.path.insert(0, %(sdk)r)
    import lindesk_plugin as lp

    @lp.on_init
    def init(ctx):
        lp.log("hello from init, threshold=%%s" %% lp.config.get("threshold"))

    @lp.classifier
    def classify(proc):
        if proc["comm"] == "rustc":
            return lp.classification("batch", 0.9, "compiler")
        if proc["comm"] == "boom":
            raise lp.PluginError("cannot classify boom")
        if proc["comm"] == "crash":
            raise RuntimeError("bug")
        if proc["comm"] == "shell":
            return "interactive"
        return None

    @lp.policy
    def evaluate(snapshot):
        print("print() must not corrupt stdout")
        lp.metric("units_seen", len(snapshot["units"]), source="test")
        return lp.output(
            [lp.intent(u["target"], cpu_weight=20, io_weight=10, reason="batch") for u in snapshot["units"] if u["class"] == "batch"],
            level="yellow",
        )

    records = []

    @lp.sink
    def sink(rs):
        records.extend(rs)
        lp.log("got %%d records" %% len(records))
    sink.tables = ["events"]

    @lp.adapter
    def adapter(emit):
        emit(lp.focus(pid=42, source="test"))

    if __name__ == "__main__":
        lp.run(name="sdk-test", version="1.2.3")
    """
)


def init_params(**config):
    return {
        "api_version": 1,
        "host": "test",
        "privilege": "user",
        "caps": {
            "cgroup_v2": True, "systemd": True, "psi": True, "uclamp": False, "io_latency": False, "io_cost": False,
            "io_weight_effective": False, "io_max": True, "sched_ext": False, "bpf": False,
        },
        "config": config,
    }


class Plugin:
    """A plugin subprocess with a line-oriented JSON-RPC client."""

    def __init__(self, source):
        self.dir = tempfile.TemporaryDirectory()
        path = os.path.join(self.dir.name, "plugin.py")
        with open(path, "w") as f:
            f.write(source)
        self.proc = subprocess.Popen(
            [sys.executable, path], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
        )
        self.next_id = 1
        self.notifications = []

    def send(self, obj):
        self.proc.stdin.write(json.dumps(obj) + "\n")
        self.proc.stdin.flush()

    def request(self, method, params=None):
        rid = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}})
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise AssertionError("plugin exited; stderr:\n" + self.proc.stderr.read())
            msg = json.loads(line)  # every stdout line must be valid JSON
            if msg.get("id") == rid:
                return msg
            self.notifications.append(msg)

    def notify(self, method, params=None):
        self.send({"jsonrpc": "2.0", "method": method, "params": params or {}})

    def read_notification(self):
        line = self.proc.stdout.readline()
        return json.loads(line)

    def close(self):
        if self.proc.poll() is None:
            try:
                self.notify("shutdown")
            except (BrokenPipeError, ValueError):
                pass
        try:
            self.proc.stdin.close()
        except BrokenPipeError:
            pass
        try:
            self.proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.proc.stdout.close()
        self.proc.stderr.close()
        self.dir.cleanup()


def unit(name):
    return {"type": "unit", "manager": {"kind": "user", "uid": 1000}, "name": name}


def proc(comm):
    return {"pid": 1, "uid": 1000, "comm": comm, "cgroup": "/x"}


class SdkTest(unittest.TestCase):
    def setUp(self):
        self.p = Plugin(PLUGIN % {"sdk": HERE})

    def tearDown(self):
        self.p.close()

    def init(self):
        r = self.p.request("initialize", init_params(threshold=7))
        self.assertIn("result", r, r)
        return r["result"]

    def test_handshake(self):
        res = self.init()
        self.assertEqual(res["name"], "sdk-test")
        self.assertEqual(res["version"], "1.2.3")
        self.assertEqual(res["api_version"], 1)
        self.assertEqual(res["kinds"], ["adapter", "classifier", "policy", "sink"])
        self.assertEqual(res["tables"], ["events"])
        logs = [n for n in self.p.notifications if n["method"] == "log"]
        self.assertTrue(any("threshold=7" in n["params"]["message"] for n in logs))

    def test_wrong_api_version_is_refused(self):
        params = init_params()
        params["api_version"] = 99
        r = self.p.request("initialize", params)
        self.assertEqual(r["error"]["code"], -32000)

    def test_classify(self):
        self.init()
        r = self.p.request("classifier.classify", {"proc": proc("rustc")})
        self.assertEqual(r["result"], {"class": "batch", "confidence": 0.9, "reason": "compiler"})
        r = self.p.request("classifier.classify", {"proc": proc("shell")})
        self.assertEqual(r["result"]["class"], "interactive")
        r = self.p.request("classifier.classify", {"proc": proc("firefox")})
        self.assertIsNone(r["result"])

    def test_errors(self):
        self.init()
        r = self.p.request("classifier.classify", {"proc": proc("boom")})
        self.assertEqual(r["error"]["code"], -32000)
        self.assertIn("cannot classify boom", r["error"]["message"])
        # Handler bugs fail the call (-32000) without counting as protocol failures.
        r = self.p.request("classifier.classify", {"proc": proc("crash")})
        self.assertEqual(r["error"]["code"], -32000)
        r = self.p.request("classifier.classify", {"nope": 1})
        self.assertEqual(r["error"]["code"], -32000)
        r = self.p.request("no.such.method")
        self.assertEqual(r["error"]["code"], -32601)
        r = self.p.request("sensor.poll", {"now": 0})  # kind not registered
        self.assertEqual(r["error"]["code"], -32601)
        # Still alive after all that.
        r = self.p.request("classifier.classify", {"proc": proc("rustc")})
        self.assertIn("result", r)

    def test_policy_and_metric(self):
        self.init()
        snap = {
            "now": 1,
            "pressure": [],
            "units": [
                {"target": unit("a.scope"), "class": "batch"},
                {"target": unit("b.scope"), "class": "interactive"},
            ],
            "events": [],
            "caps": init_params()["caps"],
        }
        r = self.p.request("policy.evaluate", {"snapshot": snap})
        out = r["result"]
        self.assertEqual(out["level"], "yellow")
        self.assertEqual(len(out["intents"]), 2)
        self.assertEqual(out["intents"][0]["setting"], {"knob": "cpu_weight", "value": 20})
        self.assertEqual(out["intents"][1]["setting"], {"knob": "io_weight", "value": 10})
        metric = [n for n in self.p.notifications if n["method"] == "metric"][0]
        self.assertEqual(metric["params"]["name"], "units_seen")
        self.assertEqual(metric["params"]["value"], 2.0)
        self.assertEqual(metric["params"]["labels"], {"source": "test"})

    def test_sink_notification_and_flush(self):
        self.init()
        self.p.notify("sink.record", {"records": [{"table": "events", "kind": "focus", "ts": 1}]})
        r = self.p.request("sink.flush")
        self.assertIsNone(r["result"])
        logs = [n["params"]["message"] for n in self.p.notifications if n["method"] == "log"]
        self.assertIn("got 1 records", logs)

    def test_adapter_emits_events(self):
        self.init()
        r = self.p.request("adapter.start")
        self.assertIsNone(r["result"])
        deadline = time.time() + 5
        while not any(n["method"] == "event" for n in self.p.notifications):
            self.assertLess(time.time(), deadline)
            self.p.notifications.append(self.p.read_notification())
        ev = [n for n in self.p.notifications if n["method"] == "event"][0]["params"]
        self.assertEqual((ev["kind"], ev["pid"], ev["source"]), ("focus", 42, "test"))

    def test_shutdown_exits_cleanly(self):
        self.init()
        self.p.notify("shutdown")
        self.assertEqual(self.p.proc.wait(timeout=2), 0)

    def test_eof_exits_cleanly(self):
        self.init()
        self.p.proc.stdin.close()
        self.assertEqual(self.p.proc.wait(timeout=2), 0)


class HelperTest(unittest.TestCase):
    def setUp(self):
        sys.path.insert(0, HERE)
        import lindesk_plugin  # noqa: E402

        self.lp = lindesk_plugin

    def test_settings(self):
        lp = self.lp
        self.assertEqual(lp.setting(uclamp_min=30), {"knob": "uclamp_min", "percent": 30.0})
        self.assertEqual(lp.setting(memory_high=None), {"knob": "memory_high", "bytes": None})
        self.assertEqual(lp.setting(io_prio=("idle", 7)), {"knob": "io_prio", "class": "idle", "level": 7})
        self.assertEqual(
            lp.setting(io_max=("8:0", {"wbps": 1024, "rbps": None})),
            {"knob": "io_max", "device": "8:0", "limit": {"wbps": 1024}},
        )
        with self.assertRaises(ValueError):
            lp.setting(bogus=1)

    def test_targets(self):
        lp = self.lp
        self.assertEqual(lp.unit("a.service"), {"type": "unit", "manager": {"kind": "system"}, "name": "a.service"})
        self.assertEqual(lp.user_unit(1000, "b.scope")["manager"], {"kind": "user", "uid": 1000})
        self.assertEqual(lp.process(5), {"type": "process", "pid": 5})

    def test_classification_validates(self):
        with self.assertRaises(ValueError):
            self.lp.classification("urgent")


if __name__ == "__main__":
    unittest.main()
