#!/usr/bin/env bash
# Verify install.sh against packaging/expected-files.txt using stub binaries.
#
#   packaging/check-layout.sh                 # run all checks (no root, nothing touches /)
#   packaging/check-layout.sh --stubs DIR     # only create stub binaries in DIR
#   packaging/check-layout.sh --update        # regenerate expected-files.txt
#
# Checks: staged tree == expected list; a second run is idempotent; a modified config is
# kept (the new default goes to .new); --uninstall leaves the destdir empty; a modified
# config survives --uninstall; --dry-run writes nothing; non-default prefixes are rewritten.
# shellcheck disable=SC2015  # ok() never fails, so `A && ok || bad` is safe here
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
INSTALL="$SCRIPT_DIR/install.sh"
EXPECTED="$SCRIPT_DIR/expected-files.txt"

# make_stubs DIR: dummy binaries. lindeskctl answers `completions <shell>` and `man <dir>`
# like the real one is expected to, so the generated-file paths are exercised too.
make_stubs() {
	local dir=$1 b
	mkdir -p "$dir"
	for b in lindeskd lindesk-session lindesk-data lindesk-bench; do
		printf '#!/bin/sh\n# stub %s\nexit 0\n' "$b" >"$dir/$b"
	done
	cat >"$dir/lindeskctl" <<'EOF'
#!/bin/sh
# stub lindeskctl
case "$1" in
completions) echo "# lindeskctl $2 completion (stub)" ;;
man) mkdir -p "$2" && printf '.TH LINDESKCTL 1\n' >"$2/lindeskctl.1" &&
	printf '.TH LINDESKD 8\n' >"$2/lindeskd.8" ;;
esac
exit 0
EOF
	chmod 0755 "$dir"/*
}

listing() {
	(cd "$1" && find . -mindepth 1 \( -type f -o -type l -o \( -type d -empty \) \) -printf '%m %y /%P\n') |
		awk '{ p = $3; if ($2 == "d") p = p "/"; print $1, p }' | LC_ALL=C sort -k2
}

case ${1:-} in
--stubs) make_stubs "${2:?usage: $0 --stubs DIR}"; exit 0 ;;
--update | "") ;;
*) echo "usage: $0 [--update | --stubs DIR]" >&2; exit 2 ;;
esac

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
make_stubs "$work/bin"
export LINDESK_BIN_DIR="$work/bin"
dest="$work/dest"
mkdir -p "$dest"
fail=0
ok() { printf 'ok   %s\n' "$*"; }
bad() { printf 'FAIL %s\n' "$*"; fail=1; }

quiet() { "$@" >"$work/log" 2>&1 || { cat "$work/log"; return 1; }; }

quiet "$INSTALL" --destdir="$dest" --no-build
listing "$dest" | grep -v ' /usr/share/lindesk/install-manifest.txt$' >"$work/actual"

if [[ ${1:-} == --update ]]; then
	{
		echo "# Files and empty directories staged by: install.sh --destdir=DIR --no-build"
		echo "# with the stub binaries from check-layout.sh (mode, path; dirs end in /)."
		echo "# The manifest (/usr/share/lindesk/install-manifest.txt) is omitted; it is"
		echo "# skipped by --no-manifest, which packages use. Regenerate: check-layout.sh --update"
		cat "$work/actual"
	} >"$EXPECTED"
	echo "wrote $EXPECTED"
	exit 0
fi

if diff -u <(grep -v '^#' "$EXPECTED") "$work/actual"; then ok "layout matches expected-files.txt"; else bad "layout differs from expected-files.txt"; fi
[[ -s $dest/usr/share/lindesk/install-manifest.txt ]] && ok "manifest written" || bad "manifest missing"

# Idempotent: same tree, no .new for an untouched config.
quiet "$INSTALL" --destdir="$dest" --no-build
listing "$dest" | grep -v ' /usr/share/lindesk/install-manifest.txt$' >"$work/actual2"
diff -q "$work/actual" "$work/actual2" >/dev/null && ok "second run is idempotent" || bad "second run changed the tree"

# A modified config is preserved; the default goes to .new.
echo "# admin edit" >>"$dest/etc/lindesk/lindesk.toml"
quiet "$INSTALL" --destdir="$dest" --no-build
if grep -q 'admin edit' "$dest/etc/lindesk/lindesk.toml" && [[ -f $dest/etc/lindesk/lindesk.toml.new ]]; then
	ok "modified config kept, default installed as lindesk.toml.new"
else
	bad "modified config not preserved"
fi

# Uninstall keeps the admin's config and removes everything else.
quiet "$INSTALL" --destdir="$dest" --uninstall
left=$(cd "$dest" && find . -mindepth 1 -type f)
if [[ $left == "./etc/lindesk/lindesk.toml" ]]; then ok "uninstall keeps only the modified config"; else bad "uninstall left: $left"; fi
rm -rf "${dest:?}"/*

# Clean install + uninstall leaves the destdir empty.
quiet "$INSTALL" --destdir="$dest" --no-build
quiet "$INSTALL" --destdir="$dest" --uninstall
if [[ -z $(ls -A "$dest") ]]; then ok "uninstall leaves the destdir empty"; else bad "uninstall left: $(cd "$dest" && find . -mindepth 1)"; fi

# Dry run touches nothing.
quiet "$INSTALL" --destdir="$dest" --no-build --dry-run
if [[ -z $(ls -A "$dest") ]]; then ok "--dry-run writes nothing"; else bad "--dry-run wrote files"; fi

# Non-default prefix: paths in units are rewritten.
quiet "$INSTALL" --destdir="$dest" --no-build --prefix=/opt/lindesk --sysconfdir=/etc/opt/lindesk
unit=$dest/opt/lindesk/lib/systemd/system/lindeskd.service
if grep -q '^ExecStart=/opt/lindesk/bin/lindeskd$' "$unit" && ! grep -q /usr/bin "$unit" &&
	[[ -f $dest/etc/opt/lindesk/lindesk/lindesk.toml ]] &&
	grep -q '^Exec=/opt/lindesk/bin/lindesk-session$' "$dest/etc/opt/lindesk/xdg/autostart/lindesk-session.desktop"; then
	ok "--prefix/--sysconfdir rewrite unit and autostart paths"
else
	bad "prefix rewrite"
fi
quiet "$INSTALL" --destdir="$dest" --prefix=/opt/lindesk --sysconfdir=/etc/opt/lindesk --uninstall
[[ -z $(ls -A "$dest") ]] && ok "prefixed uninstall leaves the destdir empty" || bad "prefixed uninstall left files"

# --init=openrc adds the init script.
quiet "$INSTALL" --destdir="$dest" --no-build --init=openrc --no-manifest
[[ -x $dest/etc/init.d/lindeskd ]] && ok "--init=openrc installs /etc/init.d/lindeskd" || bad "--init=openrc"
rm -rf "${dest:?}"/*

exit "$fail"
