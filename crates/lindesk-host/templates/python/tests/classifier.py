    def test_compilers_are_batch(self):
        proc = {"pid": 42, "uid": 1000, "comm": "rustc", "cgroup": "/user.slice/app-cargo-1.scope"}
        self.assertEqual(self.p.result("classifier.classify", {"proc": proc})["class"], "batch")

    def test_no_opinion_on_other_processes(self):
        proc = {"pid": 43, "uid": 1000, "comm": "firefox", "cgroup": "/user.slice/app-firefox-2.scope"}
        self.assertIsNone(self.p.result("classifier.classify", {"proc": proc}))

    def test_config_adds_commands(self):
        p = Plugin(config={"batch": ["blender"]})
        try:
            proc = {"pid": 44, "uid": 1000, "comm": "blender", "cgroup": "/x"}
            self.assertEqual(p.result("classifier.classify", {"proc": proc})["class"], "batch")
        finally:
            p.close()
