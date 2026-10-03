# {{name}}

A [lindesk](https://github.com/dipankar/lindesk) **{{kind}}** plugin in Rust, built on the
[`lindesk-plugin`](https://docs.rs/lindesk-plugin) SDK.

| File | Purpose |
|---|---|
| `plugin.toml` | manifest: name, kinds, executable, `[config]` defaults |
| `src/main.rs` | the plugin, plus `cargo test` unit tests over the JSON protocol |

## Develop

```sh
cargo test                       # unit tests (testing::Harness, no process needed)
cargo build --release            # plugin.toml runs target/release/{{name}}
lindeskctl plugin test .         # protocol conformance checks on the binary
```

Building against a local lindesk checkout? Switch the `lindesk-plugin` dependency in
`Cargo.toml` to the commented `path = ...` line.

## Install

```sh
cargo build --release
mkdir -p ~/.local/share/lindesk/plugins/{{name}}
cp plugin.toml ~/.local/share/lindesk/plugins/{{name}}/
mkdir -p ~/.local/share/lindesk/plugins/{{name}}/target/release
cp target/release/{{name}} ~/.local/share/lindesk/plugins/{{name}}/target/release/
lindeskctl plugin reload
```

System plugins (actuators, `privilege = "system"`) go in `/etc/lindesk/plugins/{{name}}/`
and must be owned by root and not group/world-writable.
