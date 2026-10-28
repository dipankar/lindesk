# GitHub releases

The workspace version in `Cargo.toml`, `Cargo.lock` and tag must agree. `v0.3.0` is
the first public release; earlier changelog versions describe local development builds.

1. Update the version's changelog section and verify CI on the release commit: tests,
   Clippy, MSRV, dependency policy, and package installation checks.
2. Create an annotated `vX.Y.Z` tag on that commit and push it. For subsequent versions,
   `scripts/release.sh X.Y.Z` can prepare the version, changelog, commit and tag.
3. The Release workflow builds seven binaries natively on Ubuntu 24.04 x86_64 and ARM64.
   It packages core and telemetry as Debian and Fedora packages on both architectures,
   Arch packages on x86_64, and binary tarballs on both architectures.
4. The workflow checks the complete 14-file asset set and generates `SHA256SUMS`, then
   creates a **draft** release. A manually dispatched workflow always creates a dry-run
   draft, regardless of the legacy `dry_run` input. It never publishes a release.
5. Download the draft assets, run `sha256sum -c SHA256SUMS`, verify version/architecture
   metadata and confirm CI passed for the tagged commit. Publish with
   `gh release edit vX.Y.Z --draft=false --latest` (omit `--latest` for prereleases).

Binary tarballs contain programs and licence notices; use distribution packages or the
source installer for service units, desktop integration and configuration. The Debian
packages require Ubuntu 24.04-compatible libraries. ARM64 Arch packages are not provided.

## Dependency advisory review for v0.3.0

Reviewed on 2026-10-27 against the locked crates' source and RustSec advisory text.
Compatible dependencies were updated: rustls 0.23.46, rustls-webpki 0.103.16,
webpki-roots 1.0.11, Tokio 1.54.1, zstd 0.14.3, zstd-safe 8.2.0 and zstd-sys 2.1.2.
The following exceptions in `deny.toml` are about unreachable APIs, not patched versions.
Re-review them whenever dependency versions, features or call sites change; remove them
when the upstream dependency permits a patched version.

| Advisories | Locked dependency | Evidence for this build |
|---|---|---|
| RUSTSEC-2026-0367, 0368, 0369 | zip 6.0.0 via libduckdb-sys 1.10506.0 | `libduckdb-sys/build.rs` places ZIP extraction in `build_linked`, guarded by `cfg(not(feature = "bundled"))`. The workspace explicitly enables `duckdb/bundled`; that module is excluded. ZIP is a build dependency, not a runtime archive importer. |
| RUSTSEC-2026-0391 | zstd 0.13.3 via pinned vortex-zstd 0.87.0 | `src/array.rs` and `src/zstd_buffers.rs` use bulk compressors/decompressors and dictionary helpers. No affected streaming `with_context` constructor is called. |
| RUSTSEC-2026-0398 | zstd-safe 7.3.0 via that same Vortex dependency | The affected `Seekable::decompress` API is not used; the `seekable` feature is not enabled. |
| RUSTSEC-2026-0403 | zstd-safe 7.3.0 via that same Vortex dependency | The advisory explicitly excludes the bulk decompression API used by Vortex; the affected streaming write decoder is not called. |

The Parquet path uses updated zstd 0.14.3 / zstd-safe 8.2.0. Core daemon binaries do not
link the telemetry stack. The standard cargo-deny checks remain enabled for other advisories.
