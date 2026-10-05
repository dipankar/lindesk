#!/usr/bin/env bash
# Verify install.sh against packaging/expected-files*.txt using stub binaries.
#
#   packaging/check-layout.sh                 # run all checks (no root, nothing touches /)
#   packaging/check-layout.sh --stubs DIR     # only create stub binaries in DIR
#   packaging/check-layout.sh --update        # regenerate the expected-files lists
#
# Two lists, one per package: expected-files.txt is the core (install.sh
# --without-telemetry, the `lindesk` package) and expected-files-telemetry.txt the telemetry
# component (install.sh --telemetry-only, the `lindesk-telemetry` package). The default
# install is the core; --with-telemetry is both. Checks: each variant's staged tree == its list; switching between them in
# place adds/removes exactly the telemetry files; a second run is idempotent; a modified config is
# kept (the new default goes to .new); --uninstall leaves the destdir empty; a modified
# config survives --uninstall; --dry-run writes nothing; non-default prefixes are rewritten;
# the preflight passes, warns and fails on fake host trees, and the package scripts never
# fail the transaction when `lindeskctl doctor --preflight` reports a failure.
# shellcheck disable=SC2015  # ok() never fails, so `A && ok || bad` is safe here
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
INSTALL="$SCRIPT_DIR/install.sh"
EXPECTED="$SCRIPT_DIR/expected-files.txt"
EXPECTED_TELEMETRY="$SCRIPT_DIR/expected-files-telemetry.txt"

# make_stubs DIR: dummy binaries. lindeskctl answers `completions <shell>` and `man <dir>`
# like the real one is expected to, so the generated-file paths are exercised too.
make_stubs() {
	local dir=$1 b
	mkdir -p "$dir"
	for b in lindeskd lindesk-session lindesk-bench lindesk-mcp lindesk-telemetry-sink; do
		printf '#!/bin/sh\n# stub %s\nexit 0\n' "$b" >"$dir/$b"
	done
	cat >"$dir/lindesk-data" <<'EOF'
#!/bin/sh
# stub lindesk-data
case "$1" in
man) mkdir -p "$2" && printf '.TH LINDESK-DATA 1\n' >"$2/lindesk-data.1" ;;
esac
exit 0
EOF
	cat >"$dir/lindeskctl" <<'EOF'
#!/bin/sh
# stub lindeskctl
case "$1" in
completions) echo "# lindeskctl $2 completion (stub)" ;;
man) mkdir -p "$2" && printf '.TH LINDESKCTL 1\n' >"$2/lindeskctl.1" &&
	printf '.TH LINDESKD 8\n' >"$2/lindeskd.8" &&
	printf '.TH LINDESK-MCP 1\n' >"$2/lindesk-mcp.1" ;;
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

strip_manifest() { grep -v ' /usr/share/lindesk/install-manifest.txt$' || true; }

# The two package layouts.
quiet "$INSTALL" --destdir="$dest" --no-build --without-telemetry
listing "$dest" | strip_manifest >"$work/core"
rm -rf "${dest:?}"/*
quiet "$INSTALL" --destdir="$dest" --no-build --no-manifest --telemetry-only
listing "$dest" | strip_manifest >"$work/telemetry"
rm -rf "${dest:?}"/*

if [[ ${1:-} == --update ]]; then
	{
		echo "# The lindesk (core) package: files and empty directories staged by"
		echo "#   install.sh --destdir=DIR --no-build --without-telemetry"
		echo "# with the stub binaries from check-layout.sh (mode, path; dirs end in /)."
		echo "# The manifest (/usr/share/lindesk/install-manifest.txt) is omitted; it is"
		echo "# skipped by --no-manifest, which packages use. Regenerate: check-layout.sh --update"
		cat "$work/core"
	} >"$EXPECTED"
	{
		echo "# The lindesk-telemetry package: files staged by"
		echo "#   install.sh --destdir=DIR --no-build --no-manifest --telemetry-only"
		echo "# (mode, path). The default install.sh run installs both lists."
		echo "# Regenerate: check-layout.sh --update"
		cat "$work/telemetry"
	} >"$EXPECTED_TELEMETRY"
	echo "wrote $EXPECTED and $EXPECTED_TELEMETRY"
	exit 0
fi

if diff -u <(grep -v '^#' "$EXPECTED") "$work/core"; then ok "core layout matches expected-files.txt"; else bad "core layout differs from expected-files.txt"; fi
if diff -u <(grep -v '^#' "$EXPECTED_TELEMETRY") "$work/telemetry"; then
	ok "telemetry layout matches expected-files-telemetry.txt"
else
	bad "telemetry layout differs from expected-files-telemetry.txt"
fi
if grep -Eq 'lindesk-data|telemetry|compact' "$work/core"; then bad "the core package contains telemetry files"; else ok "the core package has no telemetry files"; fi

# full: both lists, minus the directories that are empty only in the core layout.
full_expected() {
	grep -hv '^#' "$EXPECTED" "$EXPECTED_TELEMETRY" | LC_ALL=C sort -k2 |
		awk '{ line[NR] = $0; path[NR] = $2 }
			END {
				for (i = 1; i <= NR; i++) {
					keep = 1
					if (path[i] ~ /\/$/) for (j = 1; j <= NR; j++) if (j != i && index(path[j], path[i]) == 1) keep = 0
					if (keep) print line[i]
				}
			}'
}
full_expected >"$work/full-expected"

quiet "$INSTALL" --destdir="$dest" --no-build
listing "$dest" | strip_manifest >"$work/actual"
if diff -u "$work/core" "$work/actual"; then ok "default install = core only"; else bad "default install differs from the core"; fi
rm -rf "${dest:?}"/*
quiet "$INSTALL" --destdir="$dest" --no-build --with-telemetry
listing "$dest" | strip_manifest >"$work/actual"
if diff -u "$work/full-expected" "$work/actual"; then ok "--with-telemetry = core + telemetry"; else bad "--with-telemetry differs from core + telemetry"; fi
[[ -s $dest/usr/share/lindesk/install-manifest.txt ]] && ok "manifest written" || bad "manifest missing"

# Switching in place: --without-telemetry removes exactly the telemetry files (and their
# now-empty directories); --with-telemetry puts them back.
quiet "$INSTALL" --destdir="$dest" --no-build --without-telemetry
listing "$dest" | strip_manifest >"$work/switched"
if diff -u "$work/core" "$work/switched"; then ok "--without-telemetry over a full install leaves the core layout"; else bad "--without-telemetry left a different tree"; fi
quiet "$INSTALL" --destdir="$dest" --no-build --with-telemetry
listing "$dest" | strip_manifest >"$work/switched"
if diff -u "$work/actual" "$work/switched"; then ok "--with-telemetry restores the full layout"; else bad "--with-telemetry gave a different tree"; fi
quiet "$INSTALL" --destdir="$dest" --no-build
listing "$dest" | strip_manifest >"$work/switched"
if diff -u "$work/actual" "$work/switched"; then ok "a plain re-run keeps installed telemetry"; else bad "a plain re-run dropped telemetry"; fi

# Idempotent: same tree, no .new for an untouched config.
quiet "$INSTALL" --destdir="$dest" --no-build
listing "$dest" | strip_manifest >"$work/actual2"
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

# --purge removes even a modified config (and the state a destdir may hold).
quiet "$INSTALL" --destdir="$dest" --no-build
echo "# admin edit" >>"$dest/etc/lindesk/lindesk.toml"
mkdir -p "$dest/var/lib/lindesk/changes" && echo '{}' >"$dest/var/lib/lindesk/install-state.json"
quiet "$SCRIPT_DIR/uninstall.sh" --destdir="$dest" --purge --yes
if [[ -z $(ls -A "$dest") ]]; then ok "uninstall --purge leaves the destdir empty"; else bad "--purge left: $(cd "$dest" && find . -mindepth 1)"; fi
quiet "$INSTALL" --destdir="$dest" --no-build
"$SCRIPT_DIR/uninstall.sh" --destdir="$dest" --dry-run >"$work/log" 2>&1
[[ -x $dest/usr/bin/lindeskd ]] && grep -q 'dry-run' "$work/log" && ok "uninstall --dry-run changes nothing" ||
	bad "uninstall --dry-run"
[[ -x $dest/usr/bin/lindesk-uninstall && -x $dest/usr/share/lindesk/uninstall.sh ]] &&
	grep -q -- "--prefix=/usr" "$dest/usr/bin/lindesk-uninstall" && ok "lindesk-uninstall is installed" ||
	bad "lindesk-uninstall missing"
quiet "$SCRIPT_DIR/uninstall.sh" --destdir="$dest"
rm -rf "${dest:?}"/*

# A package postinst keeps lindesk off after `lindeskctl off` (install-state.json), and
# never disables conflicting daemons itself. Every system tool is a logging stub.
mkdir -p "$work/pistub" "$work/pilib"
for t in systemctl deb-systemd-helper deb-systemd-invoke systemd-sysusers systemd-tmpfiles loginctl addgroup getent; do
	printf '#!/bin/sh\necho "%s $*" >>"%s/calls"\nexit 0\n' "$t" "$work" >"$work/pistub/$t"
	chmod +x "$work/pistub/$t"
done
run_postinst() {
	: >"$work/calls"
	PATH="$work/pistub:/usr/bin:/bin" LINDESK_LIB_DIR="$work/pilib" sh "$SCRIPT_DIR/debian/postinst" configure 0.0.9 \
		>"$work/pi.log" 2>&1
}
echo '{"schema": 1, "lindesk_enabled": false, "units": []}' >"$work/pilib/install-state.json"
run_postinst
if grep -q "turned off with 'lindeskctl off'" "$work/pi.log" && ! grep -Eq 'invoke (try-restart|restart|start) lindeskd|systemctl (try-restart|restart|start) lindeskd' "$work/calls"; then
	ok "postinst keeps lindesk off after lindeskctl off"
else
	bad "postinst re-enabled a lindesk that was turned off: $(cat "$work/pi.log" "$work/calls")"
fi
echo '{"schema": 1, "lindesk_enabled": true, "units": []}' >"$work/pilib/install-state.json"
run_postinst
if [[ ! -d /run/systemd/system ]] || grep -Eq '(deb-systemd-invoke|systemctl) restart lindeskd' "$work/calls"; then
	ok "postinst restarts lindeskd when it is on"
else
	bad "postinst didn't restart lindeskd: $(cat "$work/pi.log" "$work/calls")"
fi
if ! grep -Eq 'systemctl (disable|stop|mask)' "$work/calls"; then ok "postinst disables nothing"; else bad "postinst disabled something"; fi

# Core-only install and uninstall.
quiet "$INSTALL" --destdir="$dest" --no-build --without-telemetry
quiet "$INSTALL" --destdir="$dest" --uninstall
if [[ -z $(ls -A "$dest") ]]; then ok "core-only uninstall leaves the destdir empty"; else bad "core-only uninstall left files"; fi
if "$INSTALL" --telemetry-only --no-build >/dev/null 2>&1; then bad "--telemetry-only on the live system"; else ok "--telemetry-only needs --destdir"; fi

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

# ── preflight ────────────────────────────────────────────────────────────────────────────
# fake_host DIR RELEASE: a minimal modern host tree (cgroup v2, PSI, systemd, polkit, bus).
fake_host() {
	local h=$1
	mkdir -p "$h/proc/sys/kernel" "$h/proc/pressure" "$h/sys/fs/cgroup" "$h/run/systemd/system" \
		"$h/run/dbus" "$h/usr/lib/polkit-1" "$h/proc/1" "$h/boot"
	echo "$2" >"$h/proc/sys/kernel/osrelease"
	echo "cpu io memory pids" >"$h/sys/fs/cgroup/cgroup.controllers"
	echo "some avg10=0.00 avg60=0.00 avg300=0.00 total=0" >"$h/proc/pressure/cpu"
	echo "BOOT_IMAGE=/vmlinuz root=/dev/sda1 quiet" >"$h/proc/cmdline"
	echo systemd >"$h/proc/1/comm"
	: >"$h/run/dbus/system_bus_socket"
	: >"$h/usr/lib/polkit-1/polkitd"
}
# pf ROOT [ARGS...]: run --preflight against a fake root; sets pf_rc and pf_out.
pf() {
	local root=$1
	shift
	pf_rc=0
	pf_out=$(LINDESK_PREFLIGHT_ROOT=$root "$INSTALL" --preflight --no-build "$@" 2>&1) || pf_rc=$?
}

fake_host "$work/h1" 6.8.0-45-generic
pf "$work/h1"
[[ $pf_rc -eq 0 && $pf_out == *"preflight: all requirements met"* ]] && ok "preflight passes on a modern host" ||
	bad "preflight on a modern host: $pf_out"

fake_host "$work/h2" 4.19.0-27-amd64
rm -rf "$work/h2/proc/pressure" "$work/h2/sys/fs/cgroup/cgroup.controllers" "$work/h2/run/systemd" \
	"$work/h2/usr/lib/polkit-1" "$work/h2/run/dbus"
mkdir -p "$work/h2/proc/700"
echo ananicy-cpp >"$work/h2/proc/700/comm"
pf "$work/h2"
if [[ $pf_rc -eq 0 && $pf_out == *"older than 4.20"* && $pf_out == *"no cgroup v2"* &&
	$pf_out == *"systemd is not running"* && $pf_out == *"polkit not found"* &&
	$pf_out == *"no D-Bus system bus"* && $pf_out == *"conflicting daemon(s) running: ananicy-cpp"* &&
	$pf_out == *"passed with 7 warning(s)"* ]]; then
	ok "preflight warns on an old, degraded host"
else
	bad "preflight warnings: $pf_out"
fi

fake_host "$work/h3" 6.1.0
rm -rf "$work/h3/proc/pressure"
echo "BOOT_IMAGE=/vmlinuz psi=0" >"$work/h3/proc/cmdline"
pf "$work/h3"
[[ $pf_rc -eq 0 && $pf_out == *"PSI disabled by psi=0"* ]] && ok "preflight detects psi=0" || bad "psi=0: $pf_out"
echo "BOOT_IMAGE=/vmlinuz" >"$work/h3/proc/cmdline"
echo "CONFIG_PSI_DEFAULT_DISABLED=y" >"$work/h3/boot/config-6.1.0"
pf "$work/h3"
[[ $pf_out == *"add psi=1"* ]] && ok "preflight detects CONFIG_PSI_DEFAULT_DISABLED" || bad "psi default: $pf_out"

fake_host "$work/h4" 3.2.0-4-amd64
pf "$work/h4"
[[ $pf_rc -eq 1 && $pf_out == *"older than 3.10"* && $pf_out == *"preflight: 1 failed"* ]] &&
	ok "preflight fails on kernel < 3.10" || bad "old kernel: rc=$pf_rc $pf_out"
pf "$work/nonexistent"
[[ $pf_rc -eq 1 && $pf_out == *"/proc is not mounted"* ]] && ok "preflight fails without /proc" || bad "no /proc: $pf_out"
pf_rc=0
pf_out=$(LINDESK_PREFLIGHT_ROOT="$work/h1" LINDESK_PREFLIGHT_OS=FreeBSD "$INSTALL" --preflight --no-build 2>&1) ||
	pf_rc=$?
[[ $pf_rc -eq 1 && $pf_out == *"only runs on Linux"* ]] && ok "preflight fails off Linux" || bad "non-Linux: $pf_out"

# Toolchain: a fake rustc older than the MSRV fails, a new one passes.
mkdir -p "$work/rust"
printf '#!/bin/sh\necho "rustc 1.70.0 (90c541806 2023-05-31)"\n' >"$work/rust/rustc"
printf '#!/bin/sh\nexit 0\n' >"$work/rust/cargo"
chmod +x "$work/rust/rustc" "$work/rust/cargo"
pf_rc=0
pf_out=$(env -u LINDESK_BIN_DIR LINDESK_PREFLIGHT_ROOT="$work/h1" CARGO="$work/rust/cargo" "$INSTALL" --preflight 2>&1) || pf_rc=$?
[[ $pf_rc -eq 1 && $pf_out == *"rustc 1.70.0 is older than the workspace MSRV"* ]] &&
	ok "preflight fails on an old rustc" || bad "old rustc: $pf_out"
printf '#!/bin/sh\necho "rustc 9.99.0 (0 2099-01-01)"\n' >"$work/rust/rustc"
pf_rc=0
pf_out=$(env -u LINDESK_BIN_DIR LINDESK_PREFLIGHT_ROOT="$work/h1" CARGO="$work/rust/cargo" "$INSTALL" --preflight 2>&1) || pf_rc=$?
[[ $pf_rc -eq 0 && $pf_out == *"Rust toolchain: rustc 9.99.0"* ]] && ok "preflight accepts a new rustc" ||
	bad "new rustc: $pf_out"
pf_rc=0
pf_out=$(env -u LINDESK_BIN_DIR LINDESK_PREFLIGHT_ROOT="$work/h1" CARGO="$work/nonexistent/cargo" PATH=/usr/bin:/bin \
	"$INSTALL" --preflight 2>&1) || pf_rc=$?
[[ $pf_rc -eq 1 && $pf_out == *"cargo not found"*"rustup"* ]] && ok "preflight fails without cargo" ||
	bad "no cargo: $pf_out"

# Staging (packagers) skips host checks; --skip-preflight is accepted.
quiet "$INSTALL" --destdir="$dest" --no-build --no-manifest --skip-preflight
[[ -x $dest/usr/bin/lindeskd ]] && ok "--skip-preflight installs" || bad "--skip-preflight"
rm -rf "${dest:?}"/*
LINDESK_PREFLIGHT_OS=FreeBSD "$INSTALL" --destdir="$dest" --no-build --no-manifest >"$work/log" 2>&1 &&
	grep -q "host checks skipped" "$work/log" && ok "staging skips host checks" || bad "staging ran host checks"
rm -rf "${dest:?}"/*

# Package scripts: a failing `lindeskctl doctor --preflight` never fails the transaction
# and keeps lindeskd from starting. Run them with a stub PATH (lindeskctl at /usr/bin is
# not ours to fake, so only syntax and the no-lindeskctl path are exercised here).
bash -n "$SCRIPT_DIR/uninstall.sh" && ok "uninstall.sh parses" || bad "uninstall.sh syntax"
for s in "$SCRIPT_DIR/debian/postinst" "$SCRIPT_DIR/aur/lindesk.install" "$SCRIPT_DIR/debian/telemetry/postinst" \
	"$SCRIPT_DIR/debian/telemetry/prerm" "$SCRIPT_DIR/debian/telemetry/postrm" "$SCRIPT_DIR/aur/lindesk-telemetry.install"; do
	sh -n "$s" && ok "${s#"$SCRIPT_DIR"/} parses (sh -n)" || bad "${s#"$SCRIPT_DIR"/} syntax"
done
if grep -q 'doctor --preflight --quiet' "$SCRIPT_DIR/debian/postinst" &&
	grep -q 'doctor --preflight --quiet' "$SCRIPT_DIR/rpm/lindesk.spec" &&
	grep -q 'doctor --preflight --quiet' "$SCRIPT_DIR/aur/lindesk.install"; then
	ok "deb/rpm/arch scripts run lindeskctl doctor --preflight --quiet"
else
	bad "a package script lacks the requirement check"
fi
(
	# shellcheck source=/dev/null
	. "$SCRIPT_DIR/aur/lindesk.install"
	mkdir -p "$work/stub"
	printf '#!/bin/sh\nexit 0\n' >"$work/stub/systemctl"
	chmod +x "$work/stub/systemctl"
	PATH="$work/stub:$PATH"
	post_install >/dev/null && post_upgrade >/dev/null
) && ok "arch post_install/post_upgrade succeed" || bad "arch hooks failed"

exit "$fail"
