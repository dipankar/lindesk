# steam-games (Python classifier)

Puts games first: Proton/Wine games, Steam game scopes and gamescope are `interactive`.
Steam's embedded browser, `steamwebhelper` (store, library and downloads UI), is
`background`.

| Process | Class | Why |
|---|---|---|
| unit `steam_app_*` (see below) | interactive | a Steam game's own scope |
| comm `*.exe` | interactive | a Windows program under Wine/Proton |
| exe `*wine*` (wine64-preloader, wineserver, ...) | interactive (confidence 0.8) | catches games whose comm was cut to 15 characters |
| comm `reaper` from Steam | interactive | Steam starts every game under `reaper` |
| comm `gamescope*` | interactive | the game compositor |
| comm `steamwebhelper` | background (configurable) | Steam's UI must not compete with the game |
| anything else | no opinion | the next classifier (the built-in rules) decides |

The whole plugin is about 30 lines of `steam_games.py`. The [Python
tutorial](../../../../docs/src/plugins/python.md) builds it step by step.

## The prefilter

```toml
[classifier]
prefilter = { exe = ["*wine*", "*/gamescope*", "*/steamwebhelper", "*/reaper"] }
```

lindesk checks the prefilter itself and only calls the plugin for matching processes. For
your editor and browser there is no RPC at all. Two details:

* Every non-empty list must match (`comm` AND `exe` AND `unit`), so the patterns all live
  in one list. A `unit` list next to the `exe` list would *narrow* the filter, not widen it.
* `exe` beats `comm` here: comm is truncated to 15 characters (`HorizonZeroDawn.exe`
  becomes `HorizonZeroDawn`), but every Wine/Proton process runs Wine's preloader.

## Games need their own scope

lindesk classifies **units** (systemd scopes and services). It hands the classifier the
unit's main process. Steam starts games inside its own scope, so out of the box lindesk
sees one unit, `app-…-steam-….scope`, whose main process is `steam`. To give each game a
scope of its own, which lindesk can then protect separately from the Steam client, set the
game's launch options (Properties → General) to:

```sh
systemd-run --user --scope --unit="steam_app_$SteamAppId" -- %command%
```

Steam runs launch options through a shell, so `$SteamAppId` expands to the game's id.
Lutris and Heroic users can point `[config] game_units` at the unit names their launchers
create. Those processes still have to pass the prefilter.

## Test

```console
$ python3 test_steam_games.py
........
----------------------------------------------------------------------
Ran 8 tests in 0.289s

OK
$ lindeskctl plugin test .
plugin conformance: steam-games 0.1.0 (classifier)
  ✓ plugin.toml is valid (0.9ms)
  ✓ initialize handshake (≤2000ms) (26.9ms)
  ✓ initialize reports the manifest's name and kinds (0.0ms)
  ✓ classifier.classify answers a regular process (≤50ms) (0.4ms)
  ✓ classifier.classify answers a slice pseudo-process (pid 0) (≤50ms) (0.1ms)
  ✓ unknown methods get METHOD_NOT_FOUND (-32601) (0.1ms)
  ✓ survives invalid params to classifier.classify (0.1ms)
  ✓ stdout carries only JSON-RPC messages (0.0ms)
  ✓ exits by itself on shutdown (≤500ms, status 0) (5.1ms)
9 passed, 0 failed
```

Or talk to it by hand. It is just stdin and stdout:

```console
$ printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"api_version":1,"host":"cli","privilege":"user","caps":{},"config":{}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"classifier.classify","params":{"proc":{"pid":4242,"uid":1000,"comm":"eldenring.exe","cgroup":"/user.slice/user-1000.slice/user@1000.service/app.slice/steam_app_1245620.scope","unit":"steam_app_1245620.scope"}}}' \
  | ./steam_games.py
{"jsonrpc":"2.0","id":1,"result":{"name":"steam-games","version":"0.1.0","api_version":1,"kinds":["classifier"]}}
{"jsonrpc":"2.0","id":2,"result":{"class":"interactive","confidence":1.0,"reason":"Steam game scope steam_app_1245620.scope"}}
```

## Install

`lindesk_plugin.py` is a symlink to the SDK in `sdk/python/`, so copy with `-L`:

```sh
mkdir -p ~/.local/share/lindesk/plugins
cp -rL examples/plugins/python/steam-games ~/.local/share/lindesk/plugins/
lindeskctl plugin reload        # or: systemctl --user restart lindesk-session
lindeskctl plugin list
lindeskctl explain "$(pidof -s eldenring.exe)"
```

## Configuration

```toml
# ~/.config/lindesk/lindesk.toml
[plugins.steam-games]
config = { webhelper_class = "normal", game_units = ["lutris-*.scope"] }
```
