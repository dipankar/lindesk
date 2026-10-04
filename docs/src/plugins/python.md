# Your first plugin (Python)

In about ten minutes we'll build **steam-games**: a classifier that marks Proton/Wine games
as `interactive` and Steam's web helper as `background`. The finished plugin is in
[`examples/plugins/python/steam-games`](https://github.com/dipankar/lindesk/tree/main/examples/plugins/python/steam-games).

You need Python 3.8 or newer. The SDK is a single file that uses only the standard
library.

## 1. Scaffold

```console
$ lindeskctl plugin new --kind classifier --lang python steam-games
$ cd steam-games && ls
README.md  lindesk_plugin.py  plugin.toml  steam_games.py  test_steam_games.py
```

| File | What it is |
|---|---|
| `plugin.toml` | the manifest: name, kinds, executable, priority, `[config]` |
| `steam_games.py` | the plugin. It already works: it classifies compilers as `batch` |
| `lindesk_plugin.py` | the SDK, vendored, so the plugin has no dependencies |
| `test_steam_games.py` | unit tests that run the plugin as a subprocess and speak JSON-RPC to it |

Both kinds of test pass straight away:

```console
$ python3 test_steam_games.py
......
Ran 6 tests in 0.21s
OK
$ lindeskctl plugin test .
plugin conformance: steam-games 0.1.0 (classifier)
  ✓ plugin.toml is valid (0.9ms)
  ✓ initialize handshake (≤2000ms) (26.9ms)
  ...
9 passed, 0 failed
```

## 2. Write the rules

Open `steam_games.py`. Below the SDK import, replace the generated classifier:

```python
@lp.classifier
def classify(proc):
    comm = proc["comm"]
    exe = proc.get("exe") or ""     # optional fields can be missing: use .get()

    if comm == "steamwebhelper":
        return lp.classification("background", reason="Steam's embedded browser")
    if comm.lower().endswith(".exe"):
        return lp.classification("interactive", reason="Windows program under Wine/Proton (%s)" % comm)
    if "wine" in os.path.basename(exe):
        # comm is cut to 15 chars ("HorizonZeroDawn.exe" -> "HorizonZeroDawn"),
        # but the executable is still Wine's preloader.
        return lp.classification("interactive", confidence=0.8, reason="Wine/Proton process")
    return None   # no opinion: the next classifier decides
```

That is the whole contract. `proc` has `pid`, `uid`, `comm`, `cgroup`, and optionally
`exe`, `unit`, `slice` and `app_id`. Return one of:

* `None`: no opinion. Most processes should get this.
* a class name: `"interactive"`.
* `lp.classification(cls, confidence=1.0, reason="...")`. The reason shows up in
  `lindeskctl explain`, so make it useful.

Remember that lindesk classifies **units** (scopes and services) by their main process.
Steam runs games inside its own scope unless you give them one. The example's README shows
the launch option that does that, and its classifier also matches `steam_app_*` units.

Try it by hand. A plugin is just stdin and stdout:

```console
$ printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"api_version":1,"host":"cli","privilege":"user","caps":{},"config":{}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"classifier.classify","params":{"proc":{"pid":7,"uid":1000,"comm":"eldenring.exe","cgroup":"/x"}}}' \
  | ./steam_games.py
{"jsonrpc":"2.0","id":1,"result":{"name":"steam-games","version":"0.1.0","api_version":1,"kinds":["classifier"]}}
{"jsonrpc":"2.0","id":2,"result":{"class":"interactive","confidence":1.0,"reason":"Windows program under Wine/Proton (eldenring.exe)"}}
```

## 3. Add a prefilter

Your classifier is asked about every new unit on the machine, and it has 50 ms to answer.
Let the host do the cheap filtering instead. In `plugin.toml`:

```toml
[classifier]
priority = 50          # ahead of the built-in rules classifier
min_confidence = 0.5
prefilter = { exe = ["*wine*", "*/gamescope*", "*/steamwebhelper", "*/reaper"] }
```

Now lindesk only sends processes whose executable matches one of these globs. Your editor
and browser never reach Python at all. Lists are ANDed (a process must match one glob in
*every* non-empty list), so keep alternatives in one list.

## 4. Make it configurable

Anything under `[config]` in plugin.toml reaches the plugin as `lp.config`. Admins can
override it in `lindesk.toml` without touching your files:

```toml
# plugin.toml
[config]
webhelper_class = "background"
```

```python
@lp.on_init
def check_config(context):
    # Raising here refuses to start, so bad config shows up in `lindeskctl plugin list`
    # instead of failing every call.
    if lp.config.get("webhelper_class", "background") not in lp.CLASSES:
        raise lp.PluginError("webhelper_class must be one of " + ", ".join(lp.CLASSES))
```

…and use `lp.config.get("webhelper_class", "background")` in `classify`.

## 5. Test

Add cases to `test_steam_games.py`. The generated `Plugin` helper runs your plugin exactly
as lindesk does:

```python
    def test_windows_games_are_interactive(self):
        proc = {"pid": 42, "uid": 1000, "comm": "eldenring.exe", "cgroup": "/x"}
        self.assertEqual(self.p.result("classifier.classify", {"proc": proc})["class"], "interactive")

    def test_config_changes_the_web_helper_class(self):
        p = Plugin(config={"webhelper_class": "normal"})
        try:
            proc = {"pid": 43, "uid": 1000, "comm": "steamwebhelper", "cgroup": "/x"}
            self.assertEqual(p.result("classifier.classify", {"proc": proc})["class"], "normal")
        finally:
            p.close()
```

Then run the unit tests and the protocol conformance suite:

```console
$ python3 test_steam_games.py && lindeskctl plugin test .
```

Conformance spawns the plugin like the host does and checks that:

* the manifest is valid;
* the handshake works;
* answers parse and arrive within the timeouts;
* the slice pseudo-process (`pid: 0`, empty `comm`) doesn't crash it;
* unknown methods get `-32601`;
* garbage params don't kill it;
* nothing but JSON-RPC appears on stdout;
* it exits cleanly on `shutdown`.

## 6. Install

```console
$ lindeskctl plugin install .
testing steam-games (protocol conformance)…
✓ installed steam-games into ~/.local/share/lindesk/plugins/steam-games
sent SIGHUP to lindesk-session; it re-reads its config and plugins
$ lindeskctl plugin list
NAME         KINDS       ORIGIN   STATE      CALLS  ERRORS    P99  DETAIL
rules        classifier  builtin  ● running    388       0  0.1ms
threshold    policy      builtin  ● running  14211       0  0.3ms
steam-games  classifier  user     ● running     12       0  0.9ms  ~/.local/share/lindesk/plugins/steam-games
```

`plugin install` runs the conformance suite first, copies the directory (following
symlinks, skipping `target/`, `.git` and caches) and asks `lindesk-session` to reload.
By hand, that's `cp -rL . ~/.local/share/lindesk/plugins/steam-games` followed by
`lindeskctl plugin reload`. `lindeskctl plugin disable steam-games` turns it off without
uninstalling it; `lindeskctl plugin uninstall steam-games` removes it.

Start a game and see what lindesk thinks:

```console
$ lindeskctl explain eldenring.exe
steam_app_1245620.scope (user 1000)   class interactive   via classifier:steam-games
  reason: Steam game scope steam_app_1245620.scope
  ...
$ lindeskctl plugin logs steam-games
```

`plugin logs` shows your stderr (`print()` goes there; the SDK keeps stdout for the
protocol), your `lp.log(...)` calls, and the host's notes about your plugin: restarts,
timeouts and protocol errors. The same lines reach the journal:
`journalctl --user -u lindesk-session`.

## Things to know

* **Exceptions are failures.** An uncaught exception or a `KeyError` in your handler goes
  back as an error reply, and lindesk counts it like a crash: backoff, and after five in
  five minutes, the plugin is disabled. Use `.get()` for optional fields, and raise
  `lp.PluginError("…")` for a deliberate, non-fatal "can't answer".
* **Be fast.** Classifiers get 50 ms. Policies get 100 ms. Do slow work (network, big
  files) in `on_init` or a background thread.
* **Never print to stdout yourself.** `print()` is already redirected to stderr. Raw writes
  to `sys.__stdout__` corrupt the protocol, and the conformance check catches that.
* **Time comes from the snapshot** (`snapshot["now"]`) in policies, never from
  `time.time()`, so `simulate` can replay them.

## SDK cheat sheet

| Decorator | Handler | Returns |
|---|---|---|
| `@lp.classifier` | `fn(proc)` | `None`, a class name, or `lp.classification(cls, confidence, reason)` |
| `@lp.policy` | `fn(snapshot)` | a (nested) list of `lp.intent(...)`, or `lp.output(intents, level=...)` |
| `@lp.sensor` | `fn(input)` | a list of `lp.sample(...)`, or `{"samples": [...], "usage": [...]}` |
| `@lp.sink` | `fn(records)` | nothing. Each record has a `table` key (`pressure`, `units`, `events`, `metrics`) |
| `@lp.adapter` | `fn(emit)` | runs in a thread; call `emit(lp.focus(pid=...))` |
| `@lp.on_init` | `fn(context)` | raise to refuse to start |
| `@lp.on_shutdown` | `fn()` | called once before exit |

Helpers:

* `lp.intent(target, reason=..., cpu_weight=50)` returns one intent per knob;
* `lp.unit(name, uid=...)`, `lp.cgroup(path)` and `lp.process(pid)` build targets;
* `lp.log(msg, level)`, `lp.metric(name, value, **labels)`, `lp.emit(event)` and
  `lp.samples([...])` are thread-safe.

The full reference is in [`sdk/python/README.md`](https://github.com/dipankar/lindesk/blob/main/sdk/python/README.md).
Next: more patterns in the [cookbook](cookbook.md), or [Rust plugins](rust.md).
