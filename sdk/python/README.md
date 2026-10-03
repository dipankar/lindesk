# lindesk Python plugin SDK

`lindesk_plugin.py` is a single file, standard library only, Python 3.8 or newer. Copy it
next to your plugin (that's what `lindeskctl plugin new --lang python` does), or set
`LINDESK_SDK_PATH` to this directory.

```python
#!/usr/bin/env python3
import lindesk_plugin as lp

@lp.classifier
def classify(proc):
    if proc["comm"] in ("rustc", "cc1plus", "ld"):
        return lp.classification("batch", reason="compiler")
    return None                      # no opinion: the next classifier decides

@lp.policy
def evaluate(snapshot):
    io = next((s for s in snapshot["pressure"] if s["resource"] == "io"), None)
    if not io or io["some_avg10"] < 20:
        return lp.output([], level="green")
    batch = [u for u in snapshot["units"] if u["class"] == "batch"]
    return lp.output([lp.intent(u["target"], io_weight=10, reason="io pressure") for u in batch], level="yellow")

if __name__ == "__main__":
    lp.run(name="my-plugin", version="0.1.0")
```

With a `plugin.toml` next to it:

```toml
[plugin]
name = "my-plugin"
version = "0.1.0"
api = 1
kinds = ["classifier", "policy"]
exec = "my_plugin.py"
```

Then `lindeskctl plugin test .` checks it against the protocol.

## Reference

| Decorator | Handler | Returns |
|---|---|---|
| `@lp.classifier` | `fn(proc)` | `None`, a class name, or `lp.classification(cls, confidence, reason)` |
| `@lp.policy` | `fn(snapshot)` | a (nested) list of intents, or `lp.output(intents, level=...)` |
| `@lp.sensor` | `fn(input)` | a list of `lp.sample(...)`, or `{"samples": [...], "usage": [...]}` |
| `@lp.sink` | `fn(records)` | nothing; set `fn.tables = ["events"]` to subscribe to a subset |
| `@lp.adapter` | `fn(emit)` | runs in a thread; call `emit(lp.focus(pid=...))` |
| `@lp.on_init` | `fn(context)` | raise to refuse to start; `lp.config` holds the `[config]` table |
| `@lp.on_shutdown` | `fn()` | called once before exit |

Helpers: `lp.log(msg, level)`, `lp.metric(name, value, **labels)`, `lp.emit(event)`,
`lp.samples([...])` (all thread-safe), `lp.unit(name, uid=None)`, `lp.cgroup(path)`,
`lp.process(pid)`, `lp.setting(cpu_weight=50)`, `lp.intent(target, reason=..., **knobs)`.
Raise `lp.PluginError("...")` or `lp.Unsupported("...")` to fail a request.

`print()` goes to stderr, which the host records as logs, so it never corrupts the protocol.

Run the SDK's own tests with `python3 test_lindesk_plugin.py`.
