# {{name}}

A [lindesk](https://github.com/dipankar/lindesk) **{{kind}}** plugin in bash.

| File | Purpose |
|---|---|
| `plugin.toml` | manifest: name, kinds, executable, `[config]` defaults |
| `plugin.sh` | the plugin: a read loop answering JSON-RPC lines |
| `test.sh` | smoke test that pipes requests through the plugin |

## Develop

```sh
./test.sh                  # smoke test
lindeskctl plugin test .   # protocol conformance checks
```

Shell plugins suit simple, flat logic. Once you need to parse snapshots, reach for `jq` or
switch to the Python SDK (`lindeskctl plugin new --lang python`).

## Install

```sh
mkdir -p ~/.local/share/lindesk/plugins
cp -r . ~/.local/share/lindesk/plugins/{{name}}
lindeskctl plugin reload
```
