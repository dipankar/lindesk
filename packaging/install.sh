#!/usr/bin/env bash
# lindesk installer: the single source of truth for the installed file layout.
#
# The .deb, .rpm and Arch packages all call this script with --destdir, so the layout is
# defined here and nowhere else. Run it directly to install from a source tree:
#
#   ./packaging/install.sh                     # build, install to /usr and /etc, enable
#   cargo build --release && sudo ./packaging/install.sh --no-build
#   ./packaging/install.sh --destdir=/tmp/stage --no-build      # stage only (packagers)
#   sudo ./packaging/install.sh --uninstall
#
# Environment:
#   LINDESK_BIN_DIR   directory holding prebuilt lindeskd, lindesk-session, lindeskctl,
#                     lindesk-data and lindesk-bench (implies --no-build)
#   CARGO_TARGET_DIR  honoured when locating/building binaries (default: <repo>/target)
#   CARGO             cargo executable (default: cargo from PATH)
#
# shellcheck disable=SC2317  # functions are invoked indirectly through run()
set -euo pipefail

PROG=${0##*/}
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
PKG="$ROOT/packaging"

BINARIES=(lindeskd lindesk-session lindeskctl lindesk-data lindesk-bench)
SYSTEM_UNITS=(lindeskd.service lindesk-compact.service lindesk-compact.timer)
USER_UNITS=(lindesk-session.service lindesk-compact.service lindesk-compact.timer)
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

if ((live)) && ((!dry_run)) && [[ $(id -u) -ne 0 ]]; then
	die "installing onto the live system needs root (try: sudo $0 ${ORIG_ARGS[*]}), or use --destdir/--dry-run"
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
	local cargo=${CARGO:-cargo} pkgs=() b
	for b in "${BINARIES[@]}"; do pkgs+=(-p "$b"); done
	if ! have "$cargo"; then
		# Common case: `sudo ./install.sh`, where root's PATH lacks ~/.cargo/bin.
		if [[ -n ${SUDO_USER:-} ]]; then
			local home
			home=$(getent passwd "$SUDO_USER" | cut -d: -f6)
			[[ -x $home/.cargo/bin/cargo ]] && cargo=$home/.cargo/bin/cargo
		fi
		have "$cargo" || [[ -x $cargo ]] ||
			die "cargo not found. Build first (cargo build --release) and re-run with --no-build"
	fi
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

	((do_build)) && build
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

	if ((systemd)); then
		try systemctl daemon-reload
		if ((do_enable)); then
			info "enabling lindeskd and the maintenance timer"
			try systemctl enable --now "${ENABLE_SYSTEM[@]}"
			try systemctl --global enable "${ENABLE_USER[@]}"
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

next_steps() {
	local systemd=$1
	((dry_run)) && return 0
	cat <<EOF

lindesk is installed.

EOF
	if ((systemd)); then
		if ((do_enable)); then
			echo "  lindeskd is running; lindesk-session starts with your next graphical login."
			echo "  To start it in the current session now:  systemctl --user start lindesk-session.service"
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

  Desktop adapters (per user):
    GNOME:  gnome-extensions enable $GNOME_UUID     (log out and back in on Wayland first)
    KDE:    kwriteconfig6 --file kwinrc --group Plugins --key lindeskEnabled true
            qdbus6 org.kde.KWin /KWin reconfigure     (or System Settings > KWin Scripts)
    COSMIC and X11 sessions are handled by lindesk-session itself.

  Check what your system supports:  lindeskctl doctor
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
