#!/usr/bin/env bash
# Cut a release: bump the workspace version, update CHANGELOG.md, commit and tag.
#
#   scripts/release.sh 0.2.0          # or 0.2.0-rc.1
#   git push origin main v0.2.0       # the tag triggers .github/workflows/release.yml
#
# Nothing is pushed by this script. The workspace version in Cargo.toml is the single
# source of truth: package versions (deb/rpm/arch) and `--version` output derive from it.
set -euo pipefail

die() { echo "release: $*" >&2; exit 1; }

new=${1:-}
[[ $new =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || die "usage: scripts/release.sh X.Y.Z[-rc.N]"

cd "$(git rev-parse --show-toplevel)"
[[ $(git rev-parse --abbrev-ref HEAD) == main ]] || die "not on main"
[[ -z $(git status --porcelain) ]] || die "working tree is not clean"
git rev-parse -q --verify "refs/tags/v$new" >/dev/null && die "tag v$new already exists"

old=$(awk -F'"' '/^\[workspace.package\]/ {p=1} p && /^version/ {print $2; exit}' Cargo.toml)
[[ -n $old ]] || die "cannot read the current version from Cargo.toml"

if [[ $old == "$new" ]]; then
	# The version in Cargo.toml was never released (no tag yet): release it as it is.
	echo "release: $new (already the version in Cargo.toml; not tagged yet)"
else
	echo "release: $old -> $new"
	# [workspace.package] version and the internal crates' `version = "…"` requirements.
	OLD=$old NEW=$new perl -0pi -e '
	  my ($o, $n) = ($ENV{OLD}, $ENV{NEW});
	  s/(\[workspace\.package\]\s*\nversion = ")\Q$o\E"/$1$n"/;
	  s/(lindesk-[a-z-]+ = \{ path = "[^"]+", version = ")\Q$o\E"/$1$n"/g;
	' Cargo.toml
	grep -q "^version = \"$new\"" Cargo.toml || die "version bump failed"
	cargo update --workspace --quiet
fi

# Changelog: the curated "Unreleased" section becomes this version's section, and a fresh
# empty "Unreleased" goes on top. (Draft entries from commits with: git-cliff --unreleased)
date=$(date -u +%Y-%m-%d)
grep -q '^## \[Unreleased\]' CHANGELOG.md || die "CHANGELOG.md has no '## [Unreleased]' section"
body=$(awk '/^## \[Unreleased\]/ {on=1; next} /^## / {on=0} on' CHANGELOG.md | sed '/^[[:space:]]*$/d')
[[ -n $body ]] || die "the Unreleased section of CHANGELOG.md is empty: write the release notes first"
NEW=$new DATE=$date perl -0pi -e 's/^## \[Unreleased\]\n/## [Unreleased]\n\n## [$ENV{NEW}] - $ENV{DATE}\n/m' CHANGELOG.md

cargo check --quiet -p lindesk-version
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -q -m "release: v$new"
git tag -a "v$new" -m "lindesk $new"
echo "release: committed and tagged v$new. Publish with:"
echo "  git push origin main v$new"
