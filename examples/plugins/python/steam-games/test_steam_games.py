#!/usr/bin/env python3
"""Tests for steam-games: python3 test_steam_games.py (plain unittest, no pytest needed).

They run the plugin as a subprocess and speak JSON-RPC to it, exactly like lindesk does.
"""
import fnmatch
import json
import os
import subprocess
import sys
import unittest

try:
    import tomllib  # Python 3.11+
except ImportError:  # pragma: no cover
    tomllib = None

HERE = os.path.dirname(os.path.abspath(__file__))
PLUGIN = os.path.join(HERE, "steam_games.py")


class Plugin:
    """The plugin as a subprocess, plus a tiny JSON-RPC client."""

    def __init__(self, config=None):
        self.proc = subprocess.Popen([sys.executable, PLUGIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self.next_id = 0
        self.init = self.request(
            "initialize",
            {"api_version": 1, "host": "test", "privilege": "user", "uid": 1000, "caps": {}, "config": config or {}},
        )

    def request(self, method, params=None):
        self.next_id += 1
        msg = {"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params or {}}
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise AssertionError("plugin exited while handling %s" % method)
            reply = json.loads(line)
            if reply.get("id") == self.next_id:
                return reply

    def classify(self, comm, exe=None, unit=None):
        proc = {"pid": 4242, "uid": 1000, "comm": comm, "cgroup": "/user.slice/x"}
        if exe:
            proc["exe"] = exe
        if unit:
            proc["unit"] = unit
        reply = self.request("classifier.classify", {"proc": proc})
        if "error" in reply:
            raise AssertionError(reply["error"])
        return reply["result"]

    def close(self):
        self.proc.stdin.write('{"jsonrpc":"2.0","method":"shutdown"}\n')
        self.proc.stdin.close()
        code = self.proc.wait(timeout=5)
        self.proc.stdout.close()
        return code


PROTON = "/home/me/.steam/steam/steamapps/common/Proton 9.0 (Beta)/files/bin/wine64-preloader"
STEAM = "/home/me/.steam/debian-installation/ubuntu12_64/steamwebhelper"


class TestSteamGames(unittest.TestCase):
    def setUp(self):
        self.p = Plugin()

    def tearDown(self):
        self.assertEqual(self.p.close(), 0)

    def cls(self, *args, **kwargs):
        r = self.p.classify(*args, **kwargs)
        return r and r["class"]

    def test_windows_games_are_interactive(self):
        self.assertEqual(self.cls("eldenring.exe", exe=PROTON), "interactive")

    def test_truncated_comm_is_caught_by_the_wine_executable(self):
        self.assertEqual(self.cls("HorizonZeroDawn", exe=PROTON), "interactive")
        self.assertEqual(self.cls("wineserver", exe="/usr/bin/wineserver"), "interactive")

    def test_steam_game_scopes_and_gamescope(self):
        reaper = "/home/me/.steam/debian-installation/ubuntu12_32/reaper"
        self.assertEqual(self.cls("reaper", exe=reaper, unit="steam_app_1245620.scope"), "interactive")
        self.assertEqual(self.cls("reaper", exe=reaper), "interactive")
        self.assertEqual(self.cls("gamescope-wl", exe="/usr/bin/gamescope"), "interactive")

    def test_web_helper_is_background(self):
        self.assertEqual(self.cls("steamwebhelper", exe=STEAM), "background")

    def test_no_opinion_otherwise(self):
        self.assertIsNone(self.p.classify("firefox", exe="/usr/lib/firefox/firefox"))
        self.assertIsNone(self.p.classify("", unit="background.slice"))

    def test_config_changes_the_web_helper_class(self):
        p = Plugin(config={"webhelper_class": "normal"})
        try:
            self.assertEqual(p.classify("steamwebhelper", exe=STEAM)["class"], "normal")
        finally:
            p.close()

    def test_bad_config_refuses_to_start(self):
        p = Plugin(config={"webhelper_class": "urgent"})
        try:
            self.assertIn("webhelper_class", p.init["error"]["message"])
        finally:
            p.close()


@unittest.skipIf(tomllib is None, "needs Python 3.11+ for tomllib")
class TestPrefilter(unittest.TestCase):
    """The manifest prefilter must let through everything the plugin has rules for."""

    def test_prefilter_matches_the_rules(self):
        with open(os.path.join(HERE, "plugin.toml"), "rb") as f:
            globs = tomllib.load(f)["classifier"]["prefilter"]["exe"]
        # globset's `*` crosses `/`, like fnmatch's.
        for exe in (PROTON, STEAM, "/usr/bin/gamescope", "/usr/bin/wineserver", "/x/ubuntu12_32/reaper"):
            self.assertTrue(any(fnmatch.fnmatchcase(exe, g) for g in globs), exe)
        self.assertFalse(any(fnmatch.fnmatchcase("/usr/lib/firefox/firefox", g) for g in globs))


if __name__ == "__main__":
    unittest.main()
