#!/usr/bin/env python3
"""steam-games: a lindesk classifier for Steam, Proton and Wine.

* Windows games under Wine/Proton, Steam game scopes and gamescope -> interactive
* steamwebhelper (Steam's embedded browser: store, library UI, downloads) -> background

The [classifier] prefilter in plugin.toml means lindesk only asks about processes whose
executable looks like Wine, gamescope or Steam, so this never runs for your editor.

Unit tests:   python3 test_steam_games.py
Conformance:  lindeskctl plugin test .
"""
import fnmatch
import os
import sys

# Use the SDK in LINDESK_SDK_PATH if set, else the copy next to this file.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
if os.environ.get("LINDESK_SDK_PATH"):
    sys.path.insert(0, os.environ["LINDESK_SDK_PATH"])

import lindesk_plugin as lp  # noqa: E402

# Scopes created per game, e.g. by the Steam launch option in README.md.
GAME_UNITS = ("steam_app_*", "steam-app-*")


def matches(value, globs):
    return bool(value) and any(fnmatch.fnmatchcase(value, g) for g in globs)


@lp.on_init
def check_config(context):
    # Fail at startup (visible in `lindeskctl plugin list`), not on every classify call.
    if lp.config.get("webhelper_class", "background") not in lp.CLASSES:
        raise lp.PluginError("webhelper_class must be one of %s" % ", ".join(lp.CLASSES))


@lp.classifier
def classify(proc):
    comm = proc["comm"]
    exe = proc.get("exe") or ""
    unit = proc.get("unit") or ""

    if matches(unit, GAME_UNITS) or matches(unit, lp.config.get("game_units", [])):
        return lp.classification("interactive", reason="Steam game scope %s" % unit)
    if comm == "steamwebhelper":
        # The Steam client UI. It stays usable, it just stops competing with the game.
        return lp.classification(lp.config.get("webhelper_class", "background"), reason="Steam's embedded browser")
    if comm.startswith("gamescope"):
        return lp.classification("interactive", reason="gamescope game compositor")
    if comm.lower().endswith(".exe"):
        return lp.classification("interactive", reason="Windows program under Wine/Proton (%s)" % comm)
    if "wine" in os.path.basename(exe):
        # comm is cut to 15 characters, so "HorizonZeroDawn.exe" shows up as
        # "HorizonZeroDawn"; the executable is still Wine's preloader. Also wineserver.
        return lp.classification("interactive", confidence=0.8, reason="Wine/Proton process (%s)" % comm)
    if comm == "reaper" and "steam" in exe.lower():
        # Steam starts every game under its "reaper" process.
        return lp.classification("interactive", reason="Steam game launcher")
    return None  # no opinion: the next classifier (e.g. the built-in rules) decides


if __name__ == "__main__":
    lp.run(name="steam-games", version="0.1.0")
