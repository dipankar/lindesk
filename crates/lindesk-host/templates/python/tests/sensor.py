    def test_poll_returns_samples(self):
        self.p.result("sensor.poll", {"now": 1000, "targets": []})
        out = self.p.result("sensor.poll", {"now": 2000, "targets": []})
        self.assertIsInstance(out["samples"], list)
        for s in out["samples"]:
            self.assertIn(s["resource"], ("cpu", "io", "memory"))
            self.assertEqual(s["interval_ms"], 1000)
