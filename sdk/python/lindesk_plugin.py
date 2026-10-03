"""lindesk plugin SDK for Python.

A single file with no dependencies beyond the standard library (Python >= 3.8).
Copy it next to your plugin, or point ``LINDESK_SDK_PATH`` at the directory holding it.

    import lindesk_plugin as lp

    @lp.classifier
    def classify(proc):
        if proc["comm"].startswith("steam"):
            return lp.classification("interactive", reason="games come first")
        return None          # no opinion: the next classifier decides

    if __name__ == "__main__":
        lp.run(name="steam-games", version="0.1.0")

The host talks to the plugin with newline-delimited JSON-RPC 2.0 over stdin/stdout.
``run()`` handles the handshake (``initialize``), routing, errors and ``shutdown``.
Anything you ``print()`` is redirected to stderr, which the host records as logs, so
stdout stays clean for the protocol.
"""

import json
import os
import sys
import threading
import time
import traceback

API_VERSION = 1

__all__ = [
    "API_VERSION",
    "PluginError",
    "Unsupported",
    "classifier",
    "policy",
    "sensor",
    "sink",
    "adapter",
    "on_init",
    "on_shutdown",
    "run",
    "log",
    "metric",
    "emit",
    "samples",
    "config",
    "context",
    "now_ms",
    "unit",
    "user_unit",
    "cgroup",
    "process",
    "setting",
    "intent",
    "output",
    "classification",
    "sample",
    "focus",
    "interaction",
]

# JSON-RPC error codes (mirrors lindesk_plugin_api::protocol::codes).
PARSE_ERROR = -32700
INVALID_REQUEST = -32600
METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
INTERNAL = -32603
FAILED = -32000
UNSUPPORTED = -32001

KINDS = ("sensor", "adapter", "classifier", "policy", "actuator", "sink")
CLASSES = ("critical", "interactive", "normal", "background", "batch")

#: The plugin's ``[config]`` table (manifest merged with admin overrides), set by ``initialize``.
config = {}
#: The full ``initialize`` params: host, privilege, uid, caps, config.
context = {}

_handlers = {}
_init_hooks = []
_shutdown_hooks = []
_write_lock = threading.Lock()
_out = sys.stdout


class PluginError(Exception):
    """Raise to fail a request; the host sees an RPC error with this message."""

    code = FAILED


class Unsupported(PluginError):
    """Raise when a request is valid but this plugin can't serve it."""

    code = UNSUPPORTED


# --------------------------------------------------------------------------- decorators


def _register(kind):
    def deco(fn):
        if kind in _handlers:
            raise ValueError("a %s handler is already registered" % kind)
        _handlers[kind] = fn
        return fn

    return deco


def classifier(fn):
    """``fn(proc: dict) -> classification | "class-name" | None``.

    ``proc`` has ``pid, uid, comm, exe, cgroup, unit, slice, app_id``. Return ``None`` to
    pass, a class name, or :func:`classification` for a confidence and reason.
    """
    return _register("classifier")(fn)


def policy(fn):
    """``fn(snapshot: dict) -> [intent, ...] | output(...) | None``.

    Return the *complete* set of intents that should be in force now; anything you stop
    returning is reverted by the host. Lists may be nested (``intent()`` returns a list).
    """
    return _register("policy")(fn)


def sensor(fn):
    """``fn(input: dict) -> {"samples": [...], "usage": [...], "metrics": [...]} | [sample, ...]``."""
    return _register("sensor")(fn)


def sink(fn):
    """``fn(records: list)``; each record has a ``table`` key (pressure, units, events, metrics).

    Set ``fn.tables = ["events"]`` (or use ``@sink`` then ``tables=``) to subscribe to a subset.
    """
    return _register("sink")(fn)


def adapter(fn):
    """``fn(emit)`` runs in a background thread once the host sends ``adapter.start``.

    Call ``emit(lp.focus(pid=...))`` or ``emit(lp.interaction(pid=...))`` for each event.
    """
    return _register("adapter")(fn)


def on_init(fn):
    """``fn(context: dict)`` runs once after the handshake; raise to refuse to start."""
    _init_hooks.append(fn)
    return fn


def on_shutdown(fn):
    """``fn()`` runs once before the plugin exits."""
    _shutdown_hooks.append(fn)
    return fn


# --------------------------------------------------------------------------- wire helpers


def now_ms():
    """Milliseconds since the Unix epoch, the timestamp unit used everywhere in lindesk."""
    return int(time.time() * 1000)


def _write(obj):
    line = json.dumps(obj, separators=(",", ":"))
    with _write_lock:
        _out.write(line + "\n")
        _out.flush()


def _notify(method, params):
    _write({"jsonrpc": "2.0", "method": method, "params": params})


def log(message, level="info"):
    """Send a log line to the host (``trace|debug|info|warn|error``). Thread-safe."""
    _notify("log", {"level": level, "message": str(message)})


def metric(name, value, **labels):
    """Record a numeric metric into the ``metrics`` table. Thread-safe."""
    _notify(
        "metric",
        {"ts": now_ms(), "name": name, "value": float(value), "labels": {k: str(v) for k, v in labels.items()}},
    )


def emit(event):
    """Push an event (see :func:`focus`, :func:`interaction`) to the host. Thread-safe."""
    _notify("event", event)


def samples(items):
    """Push pressure samples (see :func:`sample`) outside of a poll. Thread-safe."""
    _notify("samples", {"samples": list(items)})


# --------------------------------------------------------------------------- data helpers


def unit(name, uid=None):
    """A systemd unit target; pass ``uid`` for a unit in that user's manager."""
    manager = {"kind": "system"} if uid is None else {"kind": "user", "uid": int(uid)}
    return {"type": "unit", "manager": manager, "name": name}


def user_unit(uid, name):
    """Shorthand for ``unit(name, uid=uid)``."""
    return unit(name, uid)


def cgroup(path):
    """A raw cgroup path target, relative to the cgroup mount."""
    return {"type": "cgroup", "path": path}


def process(pid):
    """A single-process target (per-process backends: nice, ioprio)."""
    return {"type": "process", "pid": int(pid)}


def _settings_from_kwargs(kwargs):
    out = []
    for knob, value in kwargs.items():
        if knob in ("cpu_weight", "io_weight"):
            out.append({"knob": knob, "value": int(value)})
        elif knob == "cpu_idle":
            out.append({"knob": knob, "value": bool(value)})
        elif knob in ("uclamp_min", "uclamp_max"):
            out.append({"knob": knob, "percent": float(value)})
        elif knob == "memory_low":
            out.append({"knob": knob, "bytes": int(value)})
        elif knob == "memory_high":
            out.append({"knob": knob, "bytes": None if value is None else int(value)})
        elif knob == "nice":
            out.append({"knob": knob, "value": int(value)})
        elif knob == "io_prio":
            cls, level = value if isinstance(value, (tuple, list)) else (value, 7)
            out.append({"knob": knob, "class": cls, "level": int(level)})
        elif knob == "io_max":
            device, limit = value
            out.append({"knob": knob, "device": device, "limit": {k: v for k, v in dict(limit).items() if v is not None}})
        else:
            raise ValueError("unknown knob '%s'" % knob)
    return out


def setting(**kwargs):
    """One setting, e.g. ``setting(cpu_weight=50)`` or ``setting(io_max=("8:0", {"wbps": 1 << 20}))``.

    Knobs: cpu_weight, cpu_idle, uclamp_min, uclamp_max, io_weight, io_max, memory_low,
    memory_high, nice, io_prio (``("idle", 7)`` or ``("best_effort", 4)``).
    """
    s = _settings_from_kwargs(kwargs)
    if len(s) != 1:
        raise ValueError("setting() takes exactly one knob")
    return s[0]


def intent(target, reason="", priority=0, **knobs):
    """Intents for one target, one per knob: ``intent(t, cpu_weight=50, reason="...")``.

    Returns a list (policies may return nested lists). ``priority=0`` means "use the
    plugin's priority from plugin.toml".
    """
    if not knobs:
        raise ValueError("intent() needs at least one knob, e.g. cpu_weight=50")
    return [
        {"target": target, "setting": s, "priority": int(priority), "reason": reason} for s in _settings_from_kwargs(knobs)
    ]


def output(intents=(), level=None, metrics=()):
    """A full policy answer: intents plus an optional level (``green|yellow|red``)."""
    out = {"intents": _flatten(intents)}
    if level is not None:
        out["level"] = level
    if metrics:
        out["metrics"] = list(metrics)
    return out


def classification(cls, confidence=1.0, reason=""):
    """A classifier verdict. ``cls`` is one of critical|interactive|normal|background|batch."""
    if cls not in CLASSES:
        raise ValueError("unknown class '%s' (expected %s)" % (cls, "|".join(CLASSES)))
    return {"class": cls, "confidence": float(confidence), "reason": reason}


def sample(resource, some_avg10, some_delta_us=0, interval_ms=1000, scope=None, full_avg10=None, full_delta_us=None):
    """A PSI-style pressure sample. ``scope`` is None (system) or a cgroup path."""
    s = {
        "ts": now_ms(),
        "scope": {"type": "system"} if scope is None else {"type": "cgroup", "path": scope},
        "resource": resource,
        "some_avg10": float(some_avg10),
        "some_delta_us": int(some_delta_us),
        "interval_ms": int(interval_ms),
    }
    if full_avg10 is not None:
        s["full_avg10"] = float(full_avg10)
    if full_delta_us is not None:
        s["full_delta_us"] = int(full_delta_us)
    return s


def focus(pid=None, app_id=None, source=""):
    """A Focus event for adapters: the user focused a window owned by ``pid``/``app_id``."""
    e = {"kind": "focus", "ts": now_ms(), "source": source}
    if pid is not None:
        e["pid"] = int(pid)
    if app_id is not None:
        e["app_id"] = app_id
    return e


def interaction(pid=None, end=False, source=""):
    """An Interaction event: input is happening in ``pid`` (``end=True`` ends the epoch)."""
    e = {"kind": "interaction", "ts": now_ms(), "end": bool(end), "source": source}
    if pid is not None:
        e["pid"] = int(pid)
    return e


def _flatten(items):
    out = []
    for i in items or ():
        if isinstance(i, (list, tuple)):
            out.extend(_flatten(i))
        elif i is not None:
            out.append(i)
    return out


# --------------------------------------------------------------------------- dispatch


def _do_classify(params):
    r = _handlers["classifier"](params["proc"])
    if r is None:
        return None
    if isinstance(r, str):
        return classification(r)
    return r


def _do_evaluate(params):
    r = _handlers["policy"](params["snapshot"])
    if r is None:
        return {"intents": []}
    if isinstance(r, dict):
        r = dict(r)
        r["intents"] = _flatten(r.get("intents", []))
        return r
    return {"intents": _flatten(r)}


def _do_poll(params):
    r = _handlers["sensor"](params)
    if r is None:
        return {"samples": []}
    if isinstance(r, (list, tuple)):
        return {"samples": list(r)}
    return r


def _do_record(params):
    _handlers["sink"](params.get("records", []))
    return None


def _do_flush(params):
    flush = getattr(_handlers["sink"], "flush", None)
    if callable(flush):
        flush()
    return None


def _adapter_thread(fn):
    try:
        fn(emit)
    except Exception:  # noqa: BLE001 - report and keep the plugin alive
        log("adapter crashed:\n" + traceback.format_exc(), "error")


def _do_adapter_start(params):
    threading.Thread(target=_adapter_thread, args=(_handlers["adapter"],), daemon=True).start()
    return None


_METHODS = {
    "classifier.classify": ("classifier", _do_classify),
    "policy.evaluate": ("policy", _do_evaluate),
    "sensor.poll": ("sensor", _do_poll),
    "sink.record": ("sink", _do_record),
    "sink.flush": ("sink", _do_flush),
    "adapter.start": ("adapter", _do_adapter_start),
}


class _State:
    name = None
    version = "0.1.0"
    description = ""


def _initialize(params):
    global config, context
    if params.get("api_version") != API_VERSION:
        raise PluginError("unsupported api_version %r (this SDK speaks %d)" % (params.get("api_version"), API_VERSION))
    context = params
    config = params.get("config") or {}
    for hook in _init_hooks:
        hook(context)
    result = {
        "name": _State.name,
        "version": _State.version,
        "api_version": API_VERSION,
        "kinds": [k for k in KINDS if k in _handlers],
    }
    tables = getattr(_handlers.get("sink"), "tables", None)
    if tables:
        result["tables"] = list(tables)
    return result


def _respond(rid, result=None, error=None):
    if error is None:
        _write({"jsonrpc": "2.0", "id": rid, "result": result})
    else:
        code, message = error
        _write({"jsonrpc": "2.0", "id": rid, "error": {"code": code, "message": message}})


def _handle(msg):
    """Handle one decoded message. Returns False when the plugin should exit."""
    method = msg.get("method")
    rid = msg.get("id")
    params = msg.get("params")
    if params is None:
        params = {}
    if method is None:
        return True  # a response; the host never sends requests that need our answers
    if method == "shutdown":
        return False
    try:
        if method == "initialize":
            result = _initialize(params)
        elif method in _METHODS and _METHODS[method][0] in _handlers:
            result = _METHODS[method][1](params)
        else:
            if rid is not None:
                _respond(rid, error=(METHOD_NOT_FOUND, "method not found: %s" % method))
            return True
    except PluginError as e:
        if rid is not None:
            _respond(rid, error=(e.code, str(e)))
        else:
            log("%s failed: %s" % (method, e), "error")
        return True
    except (KeyError, TypeError, ValueError) as e:
        if rid is not None:
            _respond(rid, error=(INVALID_PARAMS, "%s: %s: %s" % (method, type(e).__name__, e)))
        else:
            log("%s failed: %s" % (method, e), "error")
        return True
    except Exception as e:  # noqa: BLE001 - never let a handler bug kill the protocol loop
        log("%s raised:\n%s" % (method, traceback.format_exc()), "error")
        if rid is not None:
            _respond(rid, error=(INTERNAL, "%s: %s" % (type(e).__name__, e)))
        return True
    if rid is not None:
        _respond(rid, result)
    return True


def run(name=None, version="0.1.0", description=""):
    """Serve the registered handlers on stdin/stdout until ``shutdown`` or EOF."""
    global _out
    _State.name = name or os.path.splitext(os.path.basename(sys.argv[0] or "plugin"))[0]
    _State.version = version
    _State.description = description
    _out = sys.stdout
    # print() must never corrupt the protocol stream.
    sys.stdout = sys.stderr
    try:
        while True:
            line = sys.stdin.readline()
            if not line:
                break
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except ValueError as e:
                log("unparseable line from host: %s" % e, "error")
                continue
            if not isinstance(msg, dict):
                log("ignoring non-object message", "warn")
                continue
            if not _handle(msg):
                break
    except KeyboardInterrupt:
        pass
    finally:
        for hook in _shutdown_hooks:
            try:
                hook()
            except Exception:  # noqa: BLE001
                traceback.print_exc()
