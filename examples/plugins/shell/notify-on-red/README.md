# notify-on-red (POSIX sh sink)

When lindesk goes **red**, the app you are using is stalling and background work is being
throttled hard to rescue it. This plugin tells you so:

> **lindesk**
> Background work is yielding to keep firefox responsive

It sends at most one notification per 5 minutes. The time of the last one is kept in
`$XDG_RUNTIME_DIR`, so the limit survives plugin restarts.

It is here to show **how little a plugin needs**: a `while read` loop that answers four
methods. It is plain POSIX sh (dash, busybox ash and bash all work) and reads JSON with
`jq` when installed. Without jq it falls back to `sed`, which works because the host
writes flat, predictable JSON.

```sh
while IFS= read -r line; do
    header "$line"                      # sets $id and $method
    case $method in
    initialize)  reply "$id" '{"name":"notify-on-red","version":"0.1.0","api_version":1,"kinds":["sink"]}' ;;
    sink.record) record "$line" ;;      # a notification: no reply
    sink.flush)  reply "$id" null ;;
    shutdown)    break ;;
    *) [ "$id" != - ] && reply_error "$id" -32601 "method not found: $method" ;;
    esac
done
```

The rest is `record`: remember the app from `focus` events, and on a `level_changed`
event to `red`, run `notify-send`.

## Install

```sh
mkdir -p ~/.local/share/lindesk/plugins
cp -r examples/plugins/shell/notify-on-red ~/.local/share/lindesk/plugins/
lindeskctl plugin reload        # or: systemctl --user restart lindesk-session
```

You need `notify-send` (Debian/Ubuntu: `libnotify-bin`, Fedora/Arch: `libnotify`). lindesk
starts plugins with a minimal environment (`PATH`, `HOME`, `LANG`, `XDG_*`), so the
plugin finds the session bus at `$XDG_RUNTIME_DIR/bus` itself.

## Sample output

The smoke test runs a session through the plugin twice, once with jq and once with the sed
fallback. It uses `dry_run`, so nothing pops up:

```console
$ ./test.sh
ok (jq)
ok (sed)
$ lindeskctl plugin test .
plugin conformance: notify-on-red 0.1.0 (sink)
  ✓ plugin.toml is valid (0.2ms)
  ✓ initialize handshake (≤2000ms) (11.3ms)
  ✓ initialize reports the manifest's name and kinds (0.0ms)
  ✓ accepts sink.record, then answers sink.flush (≤1000ms) (8.1ms)
  ✓ unknown methods get METHOD_NOT_FOUND (-32601) (0.4ms)
  ✓ survives invalid params to sink.flush (0.5ms)
  ✓ stdout carries only JSON-RPC messages (0.0ms)
  ✓ exits by itself on shutdown (≤500ms, status 0) (5.0ms)
8 passed, 0 failed
```

Note that conformance sends a real green → red event, so outside `dry_run` you get one
real notification.

## Configuration

| Key | Default | |
|---|---|---|
| `min_interval_s` | `300` | at most one notification per this many seconds |
| `dry_run` | `false` | log `would run: notify-send ...` instead |
| `state_file` | `$XDG_RUNTIME_DIR/lindesk-notify-on-red.<uid>.last` | rate-limit state |

```toml
# ~/.config/lindesk/lindesk.toml
[plugins.notify-on-red]
config = { min_interval_s = 900 }
```

## Lessons for shell plugins

* Never run a process per message on the hot path. The conformance check gives a plugin
  500 ms to exit after `shutdown`, and on a loaded machine each `jq`, `sed` or `rm` spawn
  can cost tens of milliseconds. Here the id and method are parsed with parameter
  expansion only.
* A `cmd | while read` loop runs in a subshell, so variables set inside it are lost. Read
  from a here-document instead.
* The event kind on the wire is `level_changed`, not `level`.
