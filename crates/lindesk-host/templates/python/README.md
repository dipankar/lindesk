# {{name}}

A [lindesk](https://github.com/dipankar/lindesk) **{{kind}}** plugin in Python
(standard library only, Python 3.8+).

| File | Purpose |
|---|---|
| `plugin.toml` | manifest: name, kinds, executable, `[config]` defaults |
| `{{module}}.py` | the plugin |
| `lindesk_plugin.py` | the SDK, vendored (set `LINDESK_SDK_PATH` to use another copy) |
| `test_{{module}}.py` | unit tests that drive the plugin over its stdio protocol |

## Develop

```sh
python3 test_{{module}}.py     # unit tests
lindeskctl plugin test .       # protocol conformance checks
```

`print()` output goes to stderr and shows up in `lindeskctl plugin logs {{name}}`; use
`lp.log(...)` and `lp.metric(...)` for structured logs and metrics.

## Install

```sh
mkdir -p ~/.local/share/lindesk/plugins
cp -r . ~/.local/share/lindesk/plugins/{{name}}
lindeskctl plugin reload
lindeskctl plugin info {{name}}
```
