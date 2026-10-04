#!/usr/bin/env bash
# lindesk installer: the single source of truth for the installed file layout.
#
# The .deb, .rpm and Arch packages all call this script with --destdir, so the layout is
# defined here and nowhere else. Run it directly to install from a source tree:
#
#   ./packaging/install.sh                     # check, build, install to /usr and /etc, enable
#                                               # and start everything (asks for sudo)
#   cargo build --release && sudo ./packaging/install.sh --no-build
#   ./packaging/install.sh --destdir=/tmp/stage --no-build      # stage only (packagers)
#   sudo ./packaging/install.sh --uninstall
#   ./packaging/install.sh --preflight         # only check this host's requirements
#
# Before building or installing, a preflight step checks the host (Linux, kernel version,
# cgroup v2, PSI, systemd, polkit, D-Bus, conflicting daemons) and, when building, the Rust
# toolchain against the workspace MSRV. Hard failures stop the install; --skip-preflight
# bypasses it. After a live install, `lindeskctl doctor --preflight` runs the daemons' own
# requirement checks; if they fail, the services are installed but not enabled or started.
# On success lindeskd is enabled and started, and lindesk-session is started for every user
# already logged in to a graphical session (no logout needed).
#
# Run as a regular user, the script checks and builds as that user, then re-runs itself
# with sudo for the install step (so target/ never gets root-owned files).
#
# Environment:
#   LINDESK_BIN_DIR   directory holding prebuilt lindeskd, lindesk-session, lindeskctl,
#                     lindesk-data, lindesk-bench and lindesk-mcp (implies --no-build)
#   CARGO_TARGET_DIR  honoured when locating/building binaries (default: <repo>/target)
#   CARGO             cargo executable (default: cargo from PATH)
#   RUSTC             rustc executable for the version check (default: next to cargo)
#   LINDESK_PREFLIGHT_ROOT  check a host tree mounted here instead of / (tests)
#   LINDESK_PREFLIGHT_OS    override `uname -s` (tests)
#
# shellcheck disable=SC2317  # functions are invoked indirectly through run()
set -euo pipefail

PROG=${0##*/}
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
PKG="$ROOT/packaging"

BINARIES=(lindeskd lindesk-session lindeskctl lindesk-data lindesk-bench lindesk-mcp)
SYSTEM_UNITS=(lindeskd.service lindesk-compact.service lindesk-compact.timer)
# lindesk-mcp.{service,socket} are installed but not enabled: stdio clients need no unit, and
# the HTTP endpoint is opt-in (systemctl --user enable --now lindesk-mcp.socket).
USER_UNITS=(lindesk-session.service lindesk-compact.service lindesk-compact.timer lindesk-mcp.service lindesk-mcp.socket)
ENABLE_SYSTEM=(lindeskd.service lindesk-compact.timer)
ENABLE_USER=(lindesk-session.service lindesk-compact.timer)
GNOME_UUID="lindesk@lindesk.org"

prefix=/usr
sysconfdir=/etc
destdir=
bindir=
do_build=1
do_enable=1
do_manifest=1
uninstall=0
dry_run=0
init=auto
preflight_only=0
skip_preflight=0
preflight_failed=0
preflight_done=0
reexec_sudo=0
started_users=()

usage() {
	cat <<EOF
Usage: $PROG [options]

Install lindesk from this source tree.

Options:
  --prefix=DIR       installation prefix (default: /usr)
  --sysconfdir=DIR   configuration directory (default: /etc)
  --bindir=DIR       binary directory (default: PREFIX/bin)
  --destdir=DIR      stage into DIR instead of the live system (for packagers);
                     implies no system integration (no systemctl, no sysusers)
  --init=SYSTEM      auto|systemd|openrc|runit|none (default: auto). systemd units are
                     always installed; openrc/runit add a service for lindeskd
  --no-build         do not run cargo; use binaries from \$LINDESK_BIN_DIR or
                     \${CARGO_TARGET_DIR:-<repo>/target}/release
  --no-enable        do not enable or start services
  --no-manifest      do not write PREFIX/share/lindesk/install-manifest.txt
                     (packages: the package manager tracks the files)
  --uninstall        remove everything a previous run installed (uses the manifest)
  --dry-run          print what would be done without changing anything
  --preflight        only check this host's requirements (no root needed), then exit;
                     with --no-build the Rust toolchain is not checked
  --skip-preflight   skip the requirement checks before and after installing
  -h, --help         show this help
EOF
}

die() { printf '%s: error: %s\n' "$PROG" "$*" >&2; exit 1; }
warn() { printf '%s: warning: %s\n' "$PROG" "$*" >&2; }
info() { printf '==> %s\n' "$*"; }

# run CMD...: execute, or only print it under --dry-run.
run() {
	if ((dry_run)); then
		printf '[dry-run]'; printf ' %q' "$@"; printf '\n'
	else
		"$@"
	fi
}

# try CMD...: like run, but a failure only warns. For best-effort system integration.
try() {
	if ((dry_run)); then
		run "$@"
	elif ! "$@"; then
		warn "command failed (continuing): $*"
	fi
}

have() { command -v "$1" >/dev/null 2>&1; }

with_timeout() {
	if have timeout; then timeout 30 "$@"; else "$@"; fi
}

# ── argument parsing ─────────────────────────────────────────────────────────────────────
ORIG_ARGS=("$@")
while (($#)); do
	arg=$1; shift
	case $arg in
	--*=*) opt=${arg%%=*}; val=${arg#*=} ;;
	--prefix | --sysconfdir | --destdir | --bindir | --init)
		(($#)) || die "$arg needs a value"
		opt=$arg; val=$1; shift ;;
	*) opt=$arg; val= ;;
	esac
	case $opt in
	--prefix) prefix=$val ;;
	--sysconfdir) sysconfdir=$val ;;
	--destdir) destdir=$val ;;
	--bindir) bindir=$val ;;
	--init) init=$val ;;
	--no-build) do_build=0 ;;
	--no-enable) do_enable=0 ;;
	--no-manifest) do_manifest=0 ;;
	--uninstall) uninstall=1 ;;
	--dry-run) dry_run=1 ;;
	--preflight) preflight_only=1 ;;
	--skip-preflight) skip_preflight=1 ;;
	--preflight-done) preflight_done=1 ;; # internal: set by the sudo re-run
	-h | --help) usage; exit 0 ;;
	*) usage >&2; die "unknown option: $arg" ;;
	esac
done

case $init in auto | systemd | openrc | runit | none) ;; *) die "--init: unknown init system '$init'" ;; esac
for v in prefix sysconfdir; do
	[[ ${!v} == /* ]] || die "--$v must be an absolute path"
done
prefix=${prefix%/}; prefix=${prefix:-/}
sysconfdir=${sysconfdir%/}
bindir=${bindir:-$prefix/bin}
bindir=${bindir%/}
[[ $bindir == /* ]] || die "--bindir must be an absolute path"
if [[ -n $destdir ]]; then
	[[ $destdir == /* ]] || destdir="$PWD/$destdir"
	destdir=${destdir%/}
fi
# A destdir of "/" is the live system.
[[ $destdir == "/" ]] && destdir=

if [[ -n ${LINDESK_BIN_DIR:-} ]]; then
	bin_src=$LINDESK_BIN_DIR
	do_build=0
else
	bin_src=${CARGO_TARGET_DIR:-$ROOT/target}/release
fi

live=0
[[ -z $destdir ]] && live=1

# ── preflight ────────────────────────────────────────────────────────────────────────────
# Written in POSIX sh (no bashisms) so other installers can reuse it verbatim.
# Host checks look at ${LINDESK_PREFLIGHT_ROOT:-}/proc, /sys, /run and /usr.
pf_fails=0
pf_warns=0
pf_ok() { printf '  ok    %s\n' "$*"; }
pf_warn() { printf '  WARN  %s\n' "$*"; pf_warns=$((pf_warns + 1)); }
pf_fail() { printf '  FAIL  %s\n' "$*"; pf_fails=$((pf_fails + 1)); }

# pf_ver_lt A B: dotted version A is older than B (compares up to three components).
pf_ver_lt() {
	awk -v a="$1" -v b="$2" 'BEGIN {
		split(a, x, "."); split(b, y, ".")
		for (i = 1; i <= 3; i++) { if (x[i] + 0 < y[i] + 0) exit 0; if (x[i] + 0 > y[i] + 0) exit 1 }
		exit 1
	}'
}

# find_cargo: set cargo_bin to a usable cargo, or to "" when there is none.
find_cargo() {
	cargo_bin=${CARGO:-cargo}
	if have "$cargo_bin" || [ -x "$cargo_bin" ]; then return 0; fi
	# Common case: `sudo ./install.sh`, where root's PATH lacks ~/.cargo/bin.
	if [ -n "${SUDO_USER:-}" ]; then
		pf_home=$(getent passwd "$SUDO_USER" | cut -d: -f6)
		if [ -x "$pf_home/.cargo/bin/cargo" ]; then
			cargo_bin=$pf_home/.cargo/bin/cargo
			return 0
		fi
	fi
	cargo_bin=
}

# as_build_user CMD...: run as the invoking user under sudo (rustup toolchains are per user).
as_build_user() {
	if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && have sudo; then
		sudo -u "$SUDO_USER" -H -- "$@"
	else
		"$@"
	fi
}

pf_toolchain() {
	pf_rustup="install Rust with rustup: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
	pf_msrv=$(sed -n 's/^rust-version[[:space:]]*=[[:space:]]*"\([0-9.]*\)".*/\1/p' "$ROOT/Cargo.toml" | head -n 1)
	find_cargo
	if [ -z "$cargo_bin" ]; then
		pf_fail "cargo not found ($pf_rustup), or build first and re-run with --no-build"
		return 0
	fi
	pf_rustc=${RUSTC:-}
	if [ -z "$pf_rustc" ]; then
		pf_rustc=rustc
		case $cargo_bin in */*) [ -x "${cargo_bin%/*}/rustc" ] && pf_rustc=${cargo_bin%/*}/rustc ;; esac
	fi
	pf_rv=$(as_build_user "$pf_rustc" --version 2>/dev/null | awk '{ print $2 }' | sed 's/-.*//')
	if [ -z "$pf_rv" ]; then
		pf_fail "rustc not found or not working ($pf_rustup)"
	elif [ -n "$pf_msrv" ] && pf_ver_lt "$pf_rv" "$pf_msrv"; then
		pf_fail "rustc $pf_rv is older than the workspace MSRV $pf_msrv (run: rustup update stable)"
	else
		pf_ok "Rust toolchain: rustc $pf_rv (needs >= ${pf_msrv:-?})"
	fi
}

pf_host() {
	pf_root=${LINDESK_PREFLIGHT_ROOT:-}
	pf_os=${LINDESK_PREFLIGHT_OS:-$(uname -s)}
	if [ "$pf_os" != Linux ]; then
		pf_fail "operating system is $pf_os: lindesk only runs on Linux"
		return 0
	fi
	if [ ! -r "$pf_root/proc/sys/kernel/osrelease" ]; then
		pf_fail "/proc is not mounted (cannot read /proc/sys/kernel/osrelease)"
		return 0
	fi
	pf_rel=$(cat "$pf_root/proc/sys/kernel/osrelease")
	pf_kv=$(printf '%s\n' "$pf_rel" | sed -n 's/^\([0-9][0-9]*\)\.\([0-9][0-9]*\).*/\1.\2/p')
	if [ -z "$pf_kv" ]; then
		pf_warn "kernel $pf_rel: version not recognised"
	elif pf_ver_lt "$pf_kv" 3.10; then
		pf_fail "kernel $pf_rel is older than 3.10: lindesk cannot run (upgrade to >= 5.4)"
	elif pf_ver_lt "$pf_kv" 4.20; then
		pf_warn "kernel $pf_rel is older than 4.20: no PSI, lindesk only applies static class weights"
	elif pf_ver_lt "$pf_kv" 5.4; then
		pf_warn "kernel $pf_rel is older than 5.4: no cgroup uclamp, boosts use weights only"
	else
		pf_ok "kernel $pf_rel"
	fi

	if [ -f "$pf_root/sys/fs/cgroup/cgroup.controllers" ]; then
		pf_ok "cgroup v2 ($(cat "$pf_root/sys/fs/cgroup/cgroup.controllers"))"
	else
		pf_warn "no cgroup v2 at /sys/fs/cgroup: CPU weights only, no I/O control" \
			"(boot with systemd.unified_cgroup_hierarchy=1)"
	fi

	pf_cmdline=" $(cat "$pf_root/proc/cmdline" 2>/dev/null || true) "
	if [ -r "$pf_root/proc/pressure/cpu" ]; then
		pf_ok "PSI (/proc/pressure)"
	else
		case $pf_cmdline in
		*" psi=0 "*) pf_warn "PSI disabled by psi=0 on the kernel command line: replace it with psi=1" ;;
		*)
			if grep -qs '^CONFIG_PSI_DEFAULT_DISABLED=y' "$pf_root/boot/config-$pf_rel"; then
				pf_warn "PSI compiled but disabled by default: add psi=1 to the kernel command line"
			else
				pf_warn "no PSI (/proc/pressure missing): no stall feedback; add psi=1 to the kernel command line"
			fi
			;;
		esac
	fi

	if [ -d "$pf_root/run/systemd/system" ]; then
		pf_ok "systemd is running"
	else
		pf_warn "systemd is not running: lindeskd uses the raw cgroup backend (see --init)"
	fi

	pf_polkit=0
	for pf_p in /usr/lib/polkit-1/polkitd /usr/libexec/polkitd /usr/libexec/polkit-1/polkitd \
		/usr/lib/systemd/system/polkit.service /lib/systemd/system/polkit.service; do
		if [ -e "$pf_root$pf_p" ]; then pf_polkit=1; fi
	done
	if [ "$pf_polkit" -eq 1 ]; then
		pf_ok "polkit"
	else
		pf_warn "polkit not found: only root will be able to change classes (install polkit)"
	fi

	if [ -e "$pf_root/run/dbus/system_bus_socket" ]; then
		pf_ok "D-Bus system bus"
	else
		pf_warn "no D-Bus system bus (/run/dbus/system_bus_socket): lindeskd needs it (or --no-dbus)"
	fi

	pf_conflicts=
	for pf_c in "$pf_root"/proc/[0-9]*/comm; do
		[ -r "$pf_c" ] || continue
		read -r pf_comm <"$pf_c" 2>/dev/null || continue
		case $pf_comm in
		system76-schedu) pf_unit=com.system76.Scheduler ;;
		ananicy-cpp | ananicy | uresourced) pf_unit=$pf_comm ;;
		*) continue ;;
		esac
		case " $pf_conflicts " in *" $pf_unit "*) ;; *) pf_conflicts="$pf_conflicts $pf_unit" ;; esac
	done
	if [ -n "$pf_conflicts" ]; then
		pf_warn "conflicting daemon(s) running:$pf_conflicts; they also adjust priorities" \
			"(stop them: systemctl disable --now$pf_conflicts)"
	else
		pf_ok "no conflicting daemons running"
	fi
}

# preflight [build]: run the checks and print a summary; returns 1 on any hard failure.
preflight() {
	pf_fails=0
	pf_warns=0
	info "preflight: checking requirements"
	if [ -n "$destdir" ] && [ "$preflight_only" -eq 0 ]; then
		pf_ok "staging into $destdir: host checks skipped (the target is checked at install time)"
	else
		pf_host
	fi
	if [ "${1:-}" = build ]; then pf_toolchain; fi
	if [ "$pf_fails" -gt 0 ]; then
		printf 'preflight: %s failed, %s warning(s)\n' "$pf_fails" "$pf_warns"
		return 1
	fi
	if [ "$pf_warns" -gt 0 ]; then
		printf 'preflight: passed with %s warning(s); lindesk will run with reduced control\n' "$pf_warns"
	else
		printf 'preflight: all requirements met\n'
	fi
	return 0
}

if ((preflight_only)); then
	rc=0
	if ((do_build)); then preflight build || rc=$?; else preflight || rc=$?; fi
	exit "$rc"
fi

if ((live)) && ((!dry_run)) && [[ $(id -u) -ne 0 ]]; then
	if ((uninstall)) || ! have sudo; then
		die "installing onto the live system needs root (try: sudo $0 ${ORIG_ARGS[*]}), or use --destdir/--dry-run"
	fi
	# Check and build as this user, then re-run the install step as root (see do_install).
	reexec_sudo=1
fi

# ── layout ───────────────────────────────────────────────────────────────────────────────
unitdir=$prefix/lib/systemd/system
userunitdir=$prefix/lib/systemd/user
presetdir=$prefix/lib/systemd/system-preset
userpresetdir=$prefix/lib/systemd/user-preset
sysusersdir=$prefix/lib/sysusers.d
tmpfilesdir=$prefix/lib/tmpfiles.d
datadir=$prefix/share
dbusdir=$datadir/dbus-1
polkitdir=$datadir/polkit-1/actions
pkgdatadir=$datadir/lindesk
pkglibdir=$prefix/lib/lindesk
docdir=$datadir/doc/lindesk
licensedir=$datadir/licenses/lindesk
mandir=$datadir/man
confdir=$sysconfdir/lindesk
manifest=$pkgdatadir/install-manifest.txt

if ((live)) && ((!uninstall)) && [[ $prefix != /usr ]]; then
	warn "prefix $prefix: D-Bus and polkit only read /usr/share (and /etc) by default, so the" \
		"broker policy may not load. systemd does search $prefix/lib/systemd."
fi

# Resolve the init system for service files beyond the systemd units.
if [[ $init == auto ]]; then
	init=systemd
	if ((live)) && [[ ! -d /run/systemd/system ]]; then
		if have openrc-run || have rc-update; then
			init=openrc
		elif have runsvdir || [[ -d /etc/sv ]]; then
			init=runit
		else
			init=none
		fi
	fi
fi

# ── tracked filesystem helpers ───────────────────────────────────────────────────────────
declare -A new_dirs=() new_files=() new_confs=()
declare -a dir_order=() file_order=()

# mkdirp PATH: create PATH (relative to destdir) and record every directory created.
mkdirp() {
	local path=$1 cur='' part
	local -a parts
	IFS=/ read -r -a parts <<<"${path#/}"
	for part in "${parts[@]}"; do
		[[ -n $part ]] || continue
		cur+="/$part"
		if [[ ! -d $destdir$cur ]] && [[ -z ${new_dirs[$cur]:-} ]]; then
			run install -d -m 0755 "$destdir$cur"
			new_dirs[$cur]=1
			dir_order+=("$cur")
		fi
	done
}

record_file() {
	if [[ -z ${new_files[$1]:-} ]]; then
		new_files[$1]=1
		file_order+=("$1")
	fi
}

# put MODE SRC DST: install SRC as DST (DST is the final, destdir-less path).
put() {
	local mode=$1 src=$2 dst=$3
	mkdirp "$(dirname -- "$dst")"
	run install -m "$mode" "$src" "$destdir$dst"
	record_file "$dst"
}

# put_text MODE SRC DST: like put, but rewrites hard-coded paths for non-default prefixes.
put_text() {
	local mode=$1 src=$2 dst=$3
	if [[ $bindir == /usr/bin && $sysconfdir == /etc ]]; then
		put "$mode" "$src" "$dst"
		return
	fi
	local tmp
	tmp=$(mktemp)
	sed -e "s|/usr/bin/|$bindir/|g" -e "s|/etc/lindesk|$confdir|g" -e "s|/etc/xdg|$sysconfdir/xdg|g" \
		"$src" >"$tmp"
	put "$mode" "$tmp" "$dst"
	rm -f "$tmp"
}

# put_tree SRC DST: copy a directory tree, files 0644.
put_tree() {
	local src=$1 dst=$2 f rel
	while IFS= read -r -d '' f; do
		rel=${f#"$src"/}
		put 0644 "$f" "$dst/$rel"
	done < <(find "$src" -type f -print0 | sort -z)
}

# put_dir DIR: an (intentionally empty) directory owned by lindesk.
put_dir() { mkdirp "$1"; }

sha256() { sha256sum "$1" | cut -d' ' -f1; }

# put_config SRC DST: never overwrite an admin's file. If DST exists and differs, the new
# default goes to DST.new.
put_config() {
	local src=$1 dst=$2
	mkdirp "$(dirname -- "$dst")"
	if [[ -e $destdir$dst ]]; then
		if cmp -s "$src" "$destdir$dst"; then
			new_confs[$dst]=$(sha256 "$src")
			return
		fi
		# A file we installed earlier that is unchanged stays ours; otherwise it is the admin's.
		local old
		old=$(manifest_conf_sum "$dst")
		if [[ -n $old && $old == "$(sha256 "$destdir$dst")" ]]; then
			run install -m 0644 "$src" "$destdir$dst"
			new_confs[$dst]=$(sha256 "$src")
			return
		fi
		info "keeping modified $dst; new default installed as $dst.new"
		put 0644 "$src" "$dst.new"
		[[ -n $old ]] && new_confs[$dst]=$old
		return
	fi
	run install -m 0644 "$src" "$destdir$dst"
	new_confs[$dst]=$(sha256 "$src")
}

old_manifest_lines=()
load_manifest() {
	local m=$destdir$manifest
	[[ -f $m ]] || return 0
	mapfile -t old_manifest_lines < <(grep -v '^#' "$m" || true)
}
manifest_conf_sum() {
	local line
	for line in "${old_manifest_lines[@]}"; do
		if [[ $line == "c "*" $1" ]]; then
			line=${line#c }
			printf '%s\n' "${line%% *}"
			return
		fi
	done
}

# ── build ────────────────────────────────────────────────────────────────────────────────
build() {
	local cargo pkgs=() b
	for b in "${BINARIES[@]}"; do pkgs+=(-p "$b"); done
	find_cargo
	[[ -n $cargo_bin ]] || die "cargo not found. Build first (cargo build --release) and re-run with --no-build"
	cargo=$cargo_bin
	info "building release binaries"
	if [[ $(id -u) -eq 0 && -n ${SUDO_USER:-} ]]; then
		# Don't leave root-owned files in the user's target dir.
		run sudo -u "$SUDO_USER" -H -- "$cargo" build --release --locked \
			--manifest-path "$ROOT/Cargo.toml" "${pkgs[@]}"
	else
		run "$cargo" build --release --locked --manifest-path "$ROOT/Cargo.toml" "${pkgs[@]}"
	fi
}

# ── generated files: completions and man pages ───────────────────────────────────────────
install_generated() {
	local ctl=$bin_src/lindeskctl tmp shell out f page sect
	if ((dry_run)); then
		info "would generate shell completions and man pages with $ctl (if supported)"
		return
	fi
	if [[ ! -x $ctl ]] || ! with_timeout "$ctl" --help >/dev/null 2>&1 </dev/null; then
		info "lindeskctl cannot run on this host; skipping completions and man pages"
		return
	fi
	tmp=$(mktemp -d)
	for shell in bash zsh fish; do
		case $shell in
		bash) out=$datadir/bash-completion/completions/lindeskctl ;;
		zsh) out=$datadir/zsh/site-functions/_lindeskctl ;;
		fish) out=$datadir/fish/vendor_completions.d/lindeskctl.fish ;;
		esac
		if with_timeout "$ctl" completions "$shell" >"$tmp/c" 2>/dev/null </dev/null && [[ -s $tmp/c ]]; then
			put 0644 "$tmp/c" "$out"
		else
			info "lindeskctl does not support 'completions $shell'; skipping"
		fi
	done
	mkdir -p "$tmp/man"
	if with_timeout "$ctl" man "$tmp/man" >/dev/null 2>&1 </dev/null; then
		local found=0
		while IFS= read -r -d '' f; do
			page=${f##*/}
			[[ $page == *.gz ]] || { gzip -9n -f "$f"; f+=.gz; page+=.gz; }
			sect=${page%.gz}; sect=${sect##*.}
			[[ $sect =~ ^[1-9][a-z]*$ ]] || continue
			put 0644 "$f" "$mandir/man${sect:0:1}/$page"
			found=1
		done < <(find "$tmp/man" -type f \( -name '*.[1-9]*' \) -print0 | sort -z)
		((found)) || info "lindeskctl man produced no pages; skipping"
	else
		info "lindeskctl does not support 'man'; skipping man pages"
	fi
	rm -rf "$tmp"
}

# ── install ──────────────────────────────────────────────────────────────────────────────
do_install() {
	local b f

	if ((!skip_preflight && !preflight_done)); then
		local pf_rc=0
		if ((do_build)); then preflight build || pf_rc=$?; else preflight || pf_rc=$?; fi
		((pf_rc == 0)) ||
			die "preflight failed: fix the FAIL items above, or re-run with --skip-preflight to install anyway"
	fi
	((do_build)) && build
	if ((reexec_sudo)); then
		info "installing as root (sudo)"
		exec sudo env LINDESK_BIN_DIR="$bin_src" "$SCRIPT_DIR/$PROG" "${ORIG_ARGS[@]}" --no-build --preflight-done
	fi
	for b in "${BINARIES[@]}"; do
		[[ -f $bin_src/$b ]] || die "missing binary $bin_src/$b (build first, or set LINDESK_BIN_DIR)"
	done
	load_manifest

	info "installing lindesk into ${destdir:-/} (prefix $prefix, sysconfdir $sysconfdir)"

	for b in "${BINARIES[@]}"; do put 0755 "$bin_src/$b" "$bindir/$b"; done

	# systemd
	for f in "${SYSTEM_UNITS[@]}"; do put_text 0644 "$PKG/systemd/system/$f" "$unitdir/$f"; done
	for f in "${USER_UNITS[@]}"; do put_text 0644 "$PKG/systemd/user/$f" "$userunitdir/$f"; done
	put 0644 "$PKG/systemd/lindesk.system.preset" "$presetdir/80-lindesk.preset"
	put 0644 "$PKG/systemd/lindesk.user.preset" "$userpresetdir/80-lindesk.preset"
	put 0644 "$PKG/systemd/lindesk.sysusers.conf" "$sysusersdir/lindesk.conf"
	put_text 0644 "$PKG/systemd/lindesk.tmpfiles.conf" "$tmpfilesdir/lindesk.conf"

	# D-Bus and polkit
	put 0644 "$PKG/dbus/system.d/org.lindesk.Broker1.conf" "$dbusdir/system.d/org.lindesk.Broker1.conf"
	put_text 0644 "$PKG/dbus/system-services/org.lindesk.Broker1.service" \
		"$dbusdir/system-services/org.lindesk.Broker1.service"
	put_text 0644 "$PKG/dbus/services/org.lindesk.QoS1.service" "$dbusdir/services/org.lindesk.QoS1.service"
	put 0644 "$PKG/polkit/org.lindesk.policy" "$polkitdir/org.lindesk.policy"

	# configuration, rules, plugins
	put_config "$PKG/config/lindesk.toml" "$confdir/lindesk.toml"
	put_dir "$confdir/rules.d"
	put_dir "$confdir/plugins"
	for f in "$PKG"/config/rules.d/*.toml; do
		put 0644 "$f" "$pkgdatadir/rules.d/${f##*/}"
	done
	# profiles: the shipped ones (also built into the binaries); admins override in /etc
	put_dir "$confdir/profiles.d"
	for f in "$PKG"/config/profiles.d/*.toml; do
		put 0644 "$f" "$pkgdatadir/profiles.d/${f##*/}"
	done
	put_dir "$pkglibdir/plugins"

	# desktop integration
	put_tree "$ROOT/adapters/gnome-shell-extension/$GNOME_UUID" "$datadir/gnome-shell/extensions/$GNOME_UUID"
	local schemas=$datadir/gnome-shell/extensions/$GNOME_UUID/schemas
	if [[ -d $destdir$schemas ]] && compgen -G "$destdir$schemas/*.gschema.xml" >/dev/null && have glib-compile-schemas; then
		run glib-compile-schemas "$destdir$schemas"
		record_file "$schemas/gschemas.compiled"
	fi
	put_tree "$ROOT/adapters/kwin-script/lindesk" "$datadir/kwin/scripts/lindesk"
	put_text 0644 "$PKG/xdg/lindesk-session.desktop" "$sysconfdir/xdg/autostart/lindesk-session.desktop"

	# non-systemd service definitions
	case $init in
	openrc) put_text 0755 "$PKG/openrc/lindeskd" "$sysconfdir/init.d/lindeskd" ;;
	runit)
		put_text 0755 "$PKG/runit/lindeskd/run" "$sysconfdir/sv/lindeskd/run"
		put_text 0755 "$PKG/runit/lindeskd/finish" "$sysconfdir/sv/lindeskd/finish"
		;;
	esac

	install_generated

	# docs and licenses
	put 0644 "$ROOT/README.md" "$docdir/README.md"
	put 0644 "$ROOT/docs/ARCHITECTURE.md" "$docdir/ARCHITECTURE.md"
	put 0644 "$ROOT/adapters/README.md" "$docdir/adapters.md"
	put 0644 "$ROOT/LICENSE-MIT" "$licensedir/LICENSE-MIT"
	put 0644 "$ROOT/LICENSE-APACHE" "$licensedir/LICENSE-APACHE"

	if ((do_manifest)); then
		write_manifest
	fi

	if ((live)); then
		integrate_live
	else
		info "staged into $destdir; skipping system integration"
	fi
}

write_manifest() {
	local line path tmp d
	mkdirp "$(dirname -- "$manifest")"
	# Keep directories created by earlier runs (we still own them) and drop stale files.
	for line in "${old_manifest_lines[@]}"; do
		path=${line#? }
		case $line in
		"d "*)
			if [[ -d $destdir$path && -z ${new_dirs[$path]:-} ]]; then
				new_dirs[$path]=1
				dir_order+=("$path")
			fi
			;;
		"f "*)
			if [[ -z ${new_files[$path]:-} && $path != "$manifest" && -e $destdir$path ]]; then
				info "removing stale $path"
				run rm -f "$destdir$path"
			fi
			;;
		esac
	done
	record_file "$manifest"
	if ((dry_run)); then
		info "would write manifest $manifest (${#file_order[@]} files, ${#dir_order[@]} dirs)"
		return
	fi
	tmp=$(mktemp)
	{
		printf '# lindesk install manifest (install.sh). d=directory f=file c=<sha256> config\n'
		printf '# prefix=%s sysconfdir=%s bindir=%s init=%s\n' "$prefix" "$sysconfdir" "$bindir" "$init"
		for d in "${dir_order[@]}"; do printf 'd %s\n' "$d"; done
		for path in "${file_order[@]}"; do printf 'f %s\n' "$path"; done
		for path in "${!new_confs[@]}"; do printf 'c %s %s\n' "${new_confs[$path]}" "$path"; done
	} >"$tmp"
	install -m 0644 "$tmp" "$destdir$manifest"
	rm -f "$tmp"
}

# Create the group and directories without systemd (sysusers.d / tmpfiles.d equivalents).
manual_sysusers_tmpfiles() {
	if ! getent group lindesk >/dev/null 2>&1; then
		if have groupadd; then
			try groupadd -r lindesk
		elif have addgroup; then
			try addgroup -S lindesk
		else
			warn "cannot create the 'lindesk' group: no groupadd/addgroup"
		fi
	fi
	local type path mode user group _
	while read -r type path mode user group _; do
		[[ $type == d ]] || continue
		getent group "$group" >/dev/null 2>&1 || group=root
		try install -d -m "$mode" -o "$user" -g "$group" "$path"
	done < <(grep -v '^[[:space:]]*#' "$tmpfilesdir/lindesk.conf")
}

integrate_live() {
	local systemd=0
	[[ -d /run/systemd/system ]] && systemd=1

	info "setting up the lindesk group and state directories"
	if have systemd-sysusers && have systemd-tmpfiles; then
		try systemd-sysusers "$sysusersdir/lindesk.conf"
		try systemd-tmpfiles --create "$tmpfilesdir/lindesk.conf"
	else
		manual_sysusers_tmpfiles
	fi

	# Make the bus pick up the new system.d policy before lindeskd claims its name.
	if [[ ! -S /run/dbus/system_bus_socket ]]; then
		: # no system bus running; it reads the policy when it starts
	elif have busctl && ((systemd)); then
		try busctl call org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus ReloadConfig
	elif have dbus-send; then
		try dbus-send --system --type=method_call --dest=org.freedesktop.DBus \
			/org/freedesktop/DBus org.freedesktop.DBus.ReloadConfig
	fi

	post_install_check

	if ((preflight_failed)); then
		if ((systemd)); then try systemctl daemon-reload; fi
	elif ((systemd)); then
		try systemctl daemon-reload
		if ((do_enable)); then
			info "enabling lindeskd and the maintenance timer"
			try systemctl enable --now "${ENABLE_SYSTEM[@]}"
			try systemctl --global enable "${ENABLE_USER[@]}"
			start_sessions
		fi
	else
		case $init in
		openrc)
			if ((do_enable)); then
				try rc-update add lindeskd default
				try rc-service lindeskd start
			fi
			;;
		runit)
			if ((do_enable)); then
				local svdir=/var/service
				[[ -d /etc/runit/runsvdir/default ]] && svdir=/etc/runit/runsvdir/default
				[[ -e $svdir/lindeskd ]] || try ln -s "$sysconfdir/sv/lindeskd" "$svdir/lindeskd"
			fi
			;;
		esac
	fi
	next_steps "$systemd"
}

# post_install_check: the daemons' own requirement checks (lindeskctl doctor --preflight,
# exit 1 on a hard failure). On failure the services are not enabled or started.
post_install_check() {
	local ctl=$bindir/lindeskctl out rc=0
	((skip_preflight)) && return 0
	if ((dry_run)); then
		run "$ctl" doctor --preflight
		return 0
	fi
	if [[ ! -x $ctl ]] || ! with_timeout "$ctl" --help >/dev/null 2>&1 </dev/null; then
		info "lindeskctl cannot run on this host; skipping the requirement check"
		return 0
	fi
	info "checking the daemons' requirements (lindeskctl doctor --preflight)"
	out=$(with_timeout "$ctl" doctor --preflight 2>&1 </dev/null) || rc=$?
	case $rc in
	0) if [[ -n $out ]]; then printf '%s\n' "$out"; fi ;;
	1)
		preflight_failed=1
		printf '%s\n' "$out" >&2
		warn "requirement checks failed: lindesk is installed, but its services were NOT enabled or started"
		;;
	*) info "lindeskctl doctor --preflight is not supported by this build (exit $rc); skipping" ;;
	esac
}

# start_sessions: start lindesk-session for every user logged in to a graphical (Wayland or
# X11) session, through their user manager, so nobody has to log out. Best effort.
start_sessions() {
	have loginctl || return 0
	local sid _rest type user
	local -a users=()
	while read -r sid _rest; do
		[[ -n $sid ]] || continue
		type=$(loginctl show-session "$sid" -p Type --value 2>/dev/null) || continue
		[[ $type == wayland || $type == x11 ]] || continue
		user=$(loginctl show-session "$sid" -p Name --value 2>/dev/null) || continue
		[[ -n $user && " ${users[*]} " != *" $user "* ]] && users+=("$user")
	done < <(loginctl list-sessions --no-legend 2>/dev/null || true)
	for user in "${users[@]}"; do
		if ((dry_run)); then
			run systemctl --user -M "$user@" start lindesk-session.service
			continue
		fi
		with_timeout systemctl --user -M "$user@" daemon-reload >/dev/null 2>&1 || true
		if with_timeout systemctl --user -M "$user@" start lindesk-session.service >/dev/null 2>&1; then
			info "started lindesk-session for $user"
			started_users+=("$user")
		else
			warn "could not start lindesk-session for $user; it starts at their next login"
		fi
	done
}

next_steps() {
	local systemd=$1
	((dry_run)) && return 0
	if ((preflight_failed)); then
		cat <<EOF

lindesk is installed, but this system does not meet its requirements (see above), so
lindeskd and lindesk-session were not enabled or started.

  1. Fix the FAIL items reported by:   lindeskctl doctor --preflight
  2. Then enable the services:
       sudo systemctl enable --now ${ENABLE_SYSTEM[*]}
       sudo systemctl --global enable ${ENABLE_USER[*]}
     (or re-run this installer, which checks again and enables them on success)
EOF
		return 0
	fi
	cat <<EOF

lindesk is installed.

EOF
	if ((systemd)); then
		if ((do_enable)); then
			echo "  lindeskd is running."
			if ((${#started_users[@]})); then
				echo "  lindesk-session is running for: ${started_users[*]} (and starts at every graphical login)."
			else
				echo "  lindesk-session starts at your next graphical login, or now with:"
				echo "    systemctl --user start lindesk-session.service"
			fi
		else
			echo "  Enable the services:"
			echo "    sudo systemctl enable --now ${ENABLE_SYSTEM[*]}"
			echo "    sudo systemctl --global enable ${ENABLE_USER[*]}"
		fi
	else
		echo "  systemd is not running: lindesk-session starts from $sysconfdir/xdg/autostart at login."
		case $init in
		openrc) echo "  lindeskd: OpenRC service installed (rc-update add lindeskd default; rc-service lindeskd start)." ;;
		runit) echo "  lindeskd: runit service installed in $sysconfdir/sv/lindeskd (ln -s it into /var/service)." ;;
		*)
			echo "  Start the broker as root from your init system, e.g.:"
			echo "    OpenRC:  sudo $0 --init=openrc   (installs $sysconfdir/init.d/lindeskd)"
			echo "    runit:   sudo $0 --init=runit    (installs $sysconfdir/sv/lindeskd)"
			echo "    other:   run '$bindir/lindeskd' as root at boot and '$bindir/lindeskd --restore' at shutdown."
			;;
		esac
	fi
	cat <<EOF

What now:
  lindeskctl              what lindesk is doing right now
  lindeskctl watch        the same, live
  lindeskctl doctor       what this machine supports, and what to fix
  lindeskctl calibrate    measure this machine's latency baseline (≈15 s, when idle)

  AI agents (MCP): claude mcp add lindesk -- lindesk-mcp
    (optional HTTP endpoint: systemctl --user enable --now lindesk-mcp.socket)

  GNOME: lindesk-session enables its Shell extension on first start. If it stays off:
           gnome-extensions enable $GNOME_UUID   (on Wayland, log out and back in first)
  KDE:   enable "lindesk" in System Settings > Window Management > KWin Scripts.
  COSMIC and X11 need nothing: lindesk-session handles them itself.
EOF
}

# ── uninstall ────────────────────────────────────────────────────────────────────────────
do_uninstall() {
	local m=$destdir$manifest line path sum
	[[ -f $m ]] || die "no install manifest at $m. Was lindesk installed by a package? Use your package manager."
	load_manifest

	if ((live)); then
		info "stopping lindesk and restoring all runtime changes"
		if [[ -d /run/systemd/system ]]; then
			try systemctl disable --now "${ENABLE_SYSTEM[@]}"
			try systemctl --global disable "${ENABLE_USER[@]}"
		fi
		if have rc-service && [[ -e $sysconfdir/init.d/lindeskd ]]; then
			try rc-service lindeskd stop
			try rc-update del lindeskd default
		fi
		for path in /var/service/lindeskd /etc/runit/runsvdir/default/lindeskd; do
			[[ -L $path ]] && try rm -f "$path"
		done
		if [[ -x $bindir/lindeskd ]]; then
			if ((dry_run)); then
				run "$bindir/lindeskd" --restore
			else
				with_timeout "$bindir/lindeskd" --restore </dev/null || warn "lindeskd --restore failed"
			fi
		fi
	fi

	info "removing files listed in $manifest"
	local -a dirs=()
	for line in "${old_manifest_lines[@]}"; do
		case $line in
		"f "*)
			path=${line#f }
			[[ $path == "$manifest" ]] && continue
			if [[ -e $destdir$path || -L $destdir$path ]]; then run rm -f "$destdir$path"; fi
			;;
		"c "*)
			line=${line#c }; sum=${line%% *}; path=${line#* }
			[[ -f $destdir$path ]] || continue
			if [[ $(sha256 "$destdir$path") == "$sum" ]]; then
				run rm -f "$destdir$path"
			else
				info "keeping modified $path"
			fi
			;;
		"d "*) dirs+=("${line#d }") ;;
		esac
	done
	run rm -f "$m"
	# Deepest first; only directories we created, and only if empty.
	while IFS= read -r path; do
		[[ -n $path && -d $destdir$path ]] || continue
		if ((dry_run)); then
			run rmdir "$destdir$path"
		else
			rmdir -- "$destdir$path" 2>/dev/null || info "keeping non-empty $path"
		fi
	done < <(printf '%s\n' "${dirs[@]}" | awk '{ print gsub("/", "/"), $0 }' | sort -rn | cut -d' ' -f2-)

	if ((live)); then
		if [[ -d /run/systemd/system ]]; then try systemctl daemon-reload; fi
		cat <<EOF

lindesk is uninstalled. Kept: /var/lib/lindesk (telemetry), the 'lindesk' group and any
per-user data in ~/.local/state/lindesk. Remove them by hand if you no longer need them.
Running session agents stop at your next logout.
EOF
	fi
}

if ((uninstall)); then
	do_uninstall
else
	do_install
fi
