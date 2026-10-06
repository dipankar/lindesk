# Third-party licences

lindesk's own source code is licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option.

The binaries link third-party Rust crates. Their licences are checked on every change by
`cargo deny check` against the allow-list in [`deny.toml`](deny.toml): MIT, Apache-2.0
(including the LLVM exception), BSD-2-Clause, BSD-3-Clause, ISC, Zlib, 0BSD, BSL-1.0,
CDLA-Permissive-2.0, Unicode-3.0 and Unlicense. To list every dependency with its licence:

```bash
cargo install cargo-deny && cargo deny list
```

## The one copyleft dependency

| Crate | Licence | Used by | Effect |
|---|---|---|---|
| [`cosmic-protocols`](https://github.com/pop-os/cosmic-protocols) | GPL-3.0-only | the built-in COSMIC foreground adapter (`cosmic` feature of `lindesk-builtins`, on by default) | a `lindesk-session` binary built with it is a combined work distributed under **GPL-3.0** |

MIT and Apache-2.0 are both compatible with GPL-3.0, so this is permitted; it means the
`lindesk-session` binary in the official packages comes with the GPL's terms (the complete
corresponding source is this repository at the tagged version). The other binaries
(`lindeskd`, `lindeskctl`, `lindesk-mcp`, `lindesk-bench`, `lindesk-data`,
`lindesk-telemetry-sink`) don't link it.

To build a `lindesk-session` that stays under MIT/Apache-2.0, leave the adapter out:

```bash
cargo build --release -p lindesk-session --no-default-features --features x11
```

COSMIC users of such a build can still report focus through the D-Bus API
([adapters/README.md](adapters/README.md)) from a separate adapter process.

## Bundled in the optional telemetry package

| Component | Licence | Where |
|---|---|---|
| [DuckDB](https://duckdb.org) (bundled build) | MIT | `lindesk-data` |
| [Vortex](https://vortex.dev) | Apache-2.0 | `lindesk-data`, `lindesk-telemetry-sink` |
| [Apache Arrow](https://arrow.apache.org) (Rust) | Apache-2.0 | `lindesk-data`, `lindesk-telemetry-sink` |
