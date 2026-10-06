# Contributing to lindesk

## Building

The toolchain is pinned in `rust-toolchain.toml`; `rustup` installs it on first use, and CI
uses the same one. The minimum supported Rust version is `rust-version` in `Cargo.toml`
(checked by CI's `msrv` job for the core crates).

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The core crates build quickly. `lindesk-data`, `lindesk-query`, `lindesk-store` and
`lindesk-telemetry-sink` build the bundled DuckDB and Vortex and take much longer; to work
on the core only, pass `-p <crate>` or `--exclude` those four.

The core binaries must never depend on DuckDB, Vortex or Arrow
(docs/ARCHITECTURE.md, "Cargo feature hygiene"); CI's `core-is-lean` job enforces it.

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org): `feat(session): …`,
`fix(lake): …`, `docs: …`, `ci: …`, `chore(deps): …`. Mark breaking changes with `!` or a
`BREAKING CHANGE:` footer.

User-visible changes also get a line under `## [Unreleased]` in `CHANGELOG.md`, written
for the person using lindesk, not for the reviewer. `git-cliff --unreleased` (config in
`cliff.toml`) drafts that list from the commits if you want a starting point.

## CI (`.github/workflows/ci.yml`)

| Job | What | Blocking |
|---|---|---|
| `lint` | `cargo fmt --check`, clippy `-D warnings` on the pinned toolchain | yes |
| `test` | `cargo test --workspace`, Python SDK tests, example plugin conformance | yes |
| `core-is-lean` | core binaries link no DuckDB/Vortex | yes |
| `msrv` | `cargo check` of the core crates on the MSRV | yes |
| `deny` | licences and RustSec advisories (`deny.toml`) | yes |
| `packages` | `.deb`, `.rpm`, Arch packages; install/remove smoke tests in containers | yes |
| `stable-latest` | clippy on the newest stable Rust | no (early warning) |

## Versions and releases

The workspace `version` in `Cargo.toml` is the single source of truth. Package versions
derive from it, and every binary prints it with the commit and build date:

```console
$ lindeskctl --version
lindeskctl 0.1.0 (2c021b4, 2026-10-05)
```

To release:

```bash
scripts/release.sh 0.2.0        # bumps the version, turns "Unreleased" into 0.2.0, commits, tags v0.2.0
git push origin main v0.2.0     # the tag starts .github/workflows/release.yml
```

The release workflow builds x86_64 and aarch64 binaries, `.deb`/`.rpm`/Arch packages and
tarballs, writes `SHA256SUMS`, and publishes a GitHub Release with that version's changelog
section. Tags containing `-rc`, `-beta` or `-alpha` become pre-releases. To rehearse
without publishing: `gh workflow run release.yml -f dry_run=true` (creates a draft).

## Telemetry dependencies

DuckDB, the DuckDB `vortex` extension, the `vortex` crate and Arrow are version-coupled.
Dependabot leaves them alone; bump them together by hand and run the lake tests
(`cargo test -p lindesk-store -p lindesk-query -p lindesk-data`) plus a manual
`lindeskctl query` against a real lake on both backends (`--backend extension|native`).

## Licences

lindesk's own code is `MIT OR Apache-2.0`. `cargo deny check` (CI's `deny` job) enforces
the dependency licence allow-list in `deny.toml`.

One dependency is copyleft: **`cosmic-protocols` is GPL-3.0-only**. It is used by the
built-in COSMIC foreground adapter (feature `cosmic` of `lindesk-builtins`, on by default),
which is linked into `lindesk-session`. A `lindesk-session` binary built with that feature
is therefore distributed under GPL-3.0 as a whole; the other binaries are unaffected. To
build a permissively-licensed session agent, disable the feature
(`--no-default-features --features x11` on `lindesk-builtins`); COSMIC users can then use
the D-Bus push API from a small external adapter instead.
