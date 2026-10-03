    def snapshot(self, io_some):
        pressure = {"ts": 0, "scope": {"type": "system"}, "resource": "io", "some_avg10": io_some,
                    "some_delta_us": 0, "interval_ms": 1000}
        units = [
            {"target": unit("app-cargo-1.scope"), "class": "batch"},
            {"target": unit("app-firefox-2.scope"), "class": "normal", "foreground": True},
        ]
        return {"now": 0, "pressure": [pressure], "units": units, "events": [], "caps": CAPS}

    def test_quiet_system_is_left_alone(self):
        out = self.p.result("policy.evaluate", {"snapshot": self.snapshot(1.0)})
        self.assertEqual((out["level"], out["intents"]), ("green", []))

    def test_io_pressure_throttles_batch(self):
        out = self.p.result("policy.evaluate", {"snapshot": self.snapshot(80.0)})
        self.assertEqual(out["level"], "yellow")
        self.assertEqual([i["target"]["name"] for i in out["intents"]], ["app-cargo-1.scope"])
        self.assertEqual(out["intents"][0]["setting"], {"knob": "io_weight", "value": 10})
