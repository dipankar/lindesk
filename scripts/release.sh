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
[[ $old != "$new" ]] || die "already at $new"
echo "release: $old -> $new"

# [workspace.package] version and the internal crates' `version = "…"` requirements.
OLD=$old NEW=$new perl -0pi -e '
  my ($o, $n) = ($ENV{OLD}, $ENV{NEW});
  s/(\[workspace\.package\]\s*\nversion = ")\Q$o\E"/$1$n"/;
  s/(lindesk-[a-z-]+ = \{ path = "[^"]+", version = ")\Q$o\E"/$1$n"/g;
' Cargo.toml
grep -q "^version = \"$new\"" Cargo.toml || die "version bump failed"
cargo update --workspace --quiet

# Changelog: git-cliff when available, otherwise a plain list of commit subjects.
date=$(date -u +%Y-%m-%d)
if command -v git-cliff >/dev/null; then
	git-cliff --config cliff.toml --tag "v$new" --output CHANGELOG.md
else
	echo "release: git-cliff not found (cargo install git-cliff); writing a plain section" >&2
	last=$(git describe --tags --abbrev=0 2>/dev/null || true)
	range=${last:+$last..}HEAD
	{
		awk '/^## /{exit} {print}' CHANGELOG.md
		echo "## [$new] - $date"
		echo
		git log --no-merges --format='- %s' "$range"
		echo
		awk '/^## /{p=1} p && !/^## \[?Unreleased/ {print}' CHANGELOG.md
	} >CHANGELOG.md.new
	mv CHANGELOG.md.new CHANGELOG.md
fi

cargo check --quiet -p lindesk-version
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -q -m "release: v$new"
git tag -a "v$new" -m "lindesk $new"
echo "release: committed and tagged v$new. Publish with:"
echo "  git push origin main v$new"
