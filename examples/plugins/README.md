# Example plugins

| Plugin | Language | Kind | What it shows |
|---|---|---|---|
| [steam-games](python/steam-games/) | Python | classifier | prefilters, config, classifying by comm/exe/unit |
| [on-battery](rust/on-battery/) | Rust | policy (modifier) | the Rust SDK, `testing::Harness`, fixtures, composing with `threshold` by priority |
| [prometheus-exporter](python/prometheus-exporter/) | Python | sink | background threads, all record tables, `on_init`/`on_shutdown` |
| [notify-on-red](shell/notify-on-red/) | POSIX sh | sink | how little a plugin needs; jq with a sed fallback |
| [focus-logger](python/focus-logger/) | Python | adapter + sink | one process serving two kinds; debugging adapters |

Every example passes `lindeskctl plugin test` (the conformance suite), and CI checks that
with one Rust test:

```sh
cargo test -p on-battery --test conformance     # needs python3 and sh
```

Each Python example has its own `python3 test_*.py`, and the shell example has `./test.sh`.

The Python examples link to the SDK (`lindesk_plugin.py -> sdk/python/lindesk_plugin.py`)
instead of vendoring a copy, so install them with `cp -rL`. Plugins made with
`lindeskctl plugin new` vendor the file and need nothing special.

Every directory under `rust/` is a member of the cargo workspace (`examples/plugins/rust/*`).
A new Rust example needs a `Cargo.toml` there.
