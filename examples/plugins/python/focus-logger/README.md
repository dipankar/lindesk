# focus-logger (Python adapter + sink, a debugging aid)

For people writing or debugging a desktop adapter. It answers two questions:

1. **Does lindesk see my focus and input events, and which unit does it map them to?**
   As a sink it logs every `focus` and `interaction` event to stderr, together with the
   unit the host resolved it to. stderr ends up in the journal and in
   `lindeskctl plugin logs focus-logger`.
2. **Can I drive the pipeline without a desktop?** As an adapter it reads commands from a
   FIFO and emits them as real events.

```text
focus       pid=48211 app_id=org.gnome.TextEditor     -> user/1000:app-gnome-org.gnome.TextEditor-48211.scope  [source=gnome-shell, 2ms ago]
interaction pid=48211                                 -> user/1000:app-gnome-org.gnome.TextEditor-48211.scope  [source=gnome-shell, 1ms ago]
focus       pid=3141                                  -> NO UNIT (pid/app_id did not map to a managed scope; check /proc/<pid>/cgroup)  [source=my-adapter, 0ms ago]
```

`NO UNIT` is the bug this tool exists to catch. lindesk maps a pid to its systemd unit
through `/proc/<pid>/cgroup`. A pid in a container, in another user's session, or one that
already exited cannot be mapped, so its focus changes nothing.

## Injecting events

```sh
fifo=$XDG_RUNTIME_DIR/lindesk/focus-logger.fifo
echo "focus $(pidof -s gnome-text-editor)" > "$fifo"   # SetForegroundProcess
echo "focus org.mozilla.firefox"            > "$fifo"   # SetForegroundApp
echo "interaction 48211"                    > "$fifo"   # BeginInteraction
echo "end 48211"                            > "$fifo"   # EndInteraction
```

Each command becomes an `Event::Focus` or `Event::Interaction` with `source =
"focus-logger"`, exactly as an adapter would send it. Then watch the result with
`lindeskctl watch` or `lindeskctl explain <pid>`.

## Install (while you need it)

```sh
mkdir -p ~/.local/share/lindesk/plugins
cp -rL examples/plugins/python/focus-logger ~/.local/share/lindesk/plugins/
lindeskctl plugin reload             # or: systemctl --user restart lindesk-session
journalctl --user -u lindesk-session -f | grep focus-logger
# done? lindeskctl plugin disable focus-logger
```

## Configuration

| Key | Default | |
|---|---|---|
| `interactions` | `true` | also log interaction events (up to ~4/s while typing) |
| `also` | `[]` | more event kinds to log, e.g. `["class_set", "level_changed"]` |
| `fifo` | `$XDG_RUNTIME_DIR/lindesk/focus-logger.fifo` | `""` disables the adapter side |

## Test

```sh
python3 test_focus_logger.py
lindeskctl plugin test .
```
