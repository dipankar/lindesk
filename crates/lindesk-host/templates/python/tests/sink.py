    def test_logs_level_changes(self):
        event = {"table": "events", "kind": "level", "ts": 0, "from": "green", "to": "red", "policy": "threshold"}
        self.p.notify("sink.record", {"records": [event]})
        self.p.result("sink.flush")
        logs = [n["params"]["message"] for n in self.p.notifications if n["method"] == "log"]
        self.assertTrue(any("green -> red" in m for m in logs), logs)

    def test_subscribes_to_events(self):
        self.assertEqual(self.p.init["result"]["tables"], ["events"])
