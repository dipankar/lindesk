#!/usr/bin/env bash
# lindesk uninstaller: stop lindesk, put back everything it changed, then remove it.
#
#   sudo lindesk-uninstall                 # restore, remove an install.sh install, keep data
#   sudo lindesk-uninstall --purge         # also /etc/lindesk, /var/lib/lindesk and (asked) user data
#   sudo lindesk-uninstall --package       # package installs: also run apt/dnf/pacman remove
#   lindesk-uninstall --dry-run            # print the plan, change nothing
#   ./packaging/install.sh --uninstall     # the same (install.sh delegates here)
#
# Installed as /usr/share/lindesk/uninstall.sh and /usr/bin/lindesk-uninstall. The package
# removal scripts run it with --restore-only (stop + restore, no file removal).
#
# Order:
#  1. stop: every logged-in user's lindesk-session/lindesk-mcp, then lindeskd and the timers;
#  2. restore: lindeskd --restore and lindesk-session --restore (cgroup settings, iocost),
#     `lindeskctl changes revert-all` for the system store and every user's (sysctls, I/O
#     schedulers, zswap, EPP/governor, services lindesk disabled, config edits), persisted
#     sysctl.d/udev drop-ins, conflicting daemons back to the install-time snapshot
#     (/var/lib/lindesk/install-state.json; asked on a terminal, else --restore-conflicts /
#     --keep-current), and the GNOME extension / KWin script lindesk-session enabled;
#  3. remove the files (install.sh installs, from the install manifest; packages: says which
#     command, or runs it with --package);
#  4. --purge: config, state and, after confirmation, per-user data. The install-time
#     snapshot and the change store are only deleted when the restore succeeded.
#
# Idempotent and safe on a half-installed system: every step skips what is missing and
# failures are reported in the summary instead of aborting.
#
# shellcheck disable=SC2317  # functions are invoked indirectly through run()/try()
set -uo pipefail

PROG=${0##*/}
prefix=/usr
sysconfdir=/etc
destdir=
bindir=
dry_run=0
yes=0
purge=0
package=0
restore_only=0
conflicts_choice=
fixture_root=
only_user=
restore_failed=0
declare -a restored=() removed=() kept=() problems=()

usage() {
	cat <<EOF
Usage: $PROG [options]

Stop lindesk, restore everything it changed, and remove it.

  --purge               also remove /etc/lindesk, /var/lib/lindesk, /run/lindesk and, after
                        confirmation, per-user data (~/.config/lindesk, ~/.local/state/lindesk,
                        ~/.local/share/lindesk/plugins)
  --user NAME           limit per-user restore and purge to NAME
  --package             lindesk came from a package: also run the package manager's remove
  --restore-only        stop and restore, remove nothing (the package scripts use this)
  --restore-conflicts   put conflicting daemons back to their install-time state without asking
  --keep-current        leave conflicting daemons as they are now (default without a terminal)
  --yes, -y             don't ask (per-user data is purged only with --yes or a "y")
  --dry-run             print the plan, change nothing
  --prefix=DIR --sysconfdir=DIR --bindir=DIR --destdir=DIR
                        where install.sh put it (defaults /usr, /etc; --destdir: a staged
                        tree, only its files are removed)
  -h, --help            this help
EOF
}

die() { printf '%s: error: %s\n' "$PROG" "$*" >&2; exit 1; }
info() { printf '==> %s\n' "$*"; }
warn() { printf '%s: warning: %s\n' "$PROG" "$*" >&2; }
have() { command -v "$1" >/dev/null 2>&1; }
with_timeout() { if have timeout; then timeout "${TMO:-60}" "$@"; else "$@"; fi; }

# run CMD...: execute, or print under --dry-run.
run() {
	if ((dry_run)); then
		printf '[dry-run]'
		printf ' %q' "$@"
		printf '\n'
	else
		"$@"
	fi
}

# try WHAT CMD...: run; on failure note WHAT as a problem and continue.
try() {
	local what=$1
	shift
	if ((dry_run)); then
		run "$@"
		return 0
	fi
	if ! with_timeout "$@"; then
		problems+=("$what")
		return 1
	fi
	return 0
}

while (($#)); do
	arg=$1
	shift
	case $arg in
	--*=*) opt=${arg%%=*}; val=${arg#*=} ;;
	--prefix | --sysconfdir | --bindir | --destdir | --user | --root)
		(($#)) || die "$arg needs a value"
		opt=$arg; val=$1; shift ;;
	*) opt=$arg; val= ;;
	esac
	case $opt in
	--prefix) prefix=$val ;;
	--sysconfdir) sysconfdir=$val ;;
	--bindir) bindir=$val ;;
	--destdir) destdir=$val ;;
	--user) only_user=$val ;;
	--root) fixture_root=$val ;; # fixture /proc /sys /etc for `changes revert-all` (tests)
	--purge) purge=1 ;;
	--package) package=1 ;;
	--restore-only) restore_only=1 ;;
	--restore-conflicts) conflicts_choice=restore ;;
	--keep-current) conflicts_choice=keep ;;
	--yes | -y) yes=1 ;;
	--dry-run) dry_run=1 ;;
	-h | --help) usage; exit 0 ;;
	*) usage >&2; die "unknown option: $arg" ;;
	esac
done

prefix=${prefix%/}; prefix=${prefix:-/}
sysconfdir=${sysconfdir%/}
bindir=${bindir:-$prefix/bin}
if [[ -n $destdir ]]; then
	[[ $destdir == /* ]] || destdir="$PWD/$destdir"
	destdir=${destdir%/}
	[[ $destdir == / ]] && destdir=
fi
live=0
[[ -z $destdir ]] && live=1
manifest=$prefix/share/lindesk/install-manifest.txt
confdir=$sysconfdir/lindesk
libdir=/var/lib/lindesk
etc=${fixture_root:+$fixture_root/etc}
etc=${etc:-/etc}
ctl=$bindir/lindeskctl

if ((live && !dry_run)) && [[ $(id -u) -ne 0 ]]; then
	die "uninstalling needs root: sudo $0 $*"
fi
interactive=0
[[ -t 0 && -t 1 ]] && ((!yes)) && interactive=1

# ask QUESTION: yes with --yes, the answer on a terminal, else no.
ask() {
	((yes)) && return 0
	((interactive)) || return 1
	local a
	read -r -p "$1 [y/N] " a || return 1
	[[ $a == [yY]* ]]
}

# ── who and what ─────────────────────────────────────────────────────────────────────────
# Users with a running user manager (logged in), as "name uid home".
logged_in_users() {
	local d uid name home
	for d in /run/user/[0-9]*; do
		[[ -d $d ]] || continue
		uid=${d##*/}
		((uid >= 1000)) || continue
		name=$(getent passwd "$uid" | cut -d: -f1) || continue
		home=$(getent passwd "$uid" | cut -d: -f6)
		[[ -z $only_user || $name == "$only_user" ]] && printf '%s %s %s\n' "$name" "$uid" "$home"
	done
}

# Every regular user (uid >= 1000 with a home directory), as "name uid home".
all_users() {
	getent passwd | awk -F: '$3 >= 1000 && $3 < 60000 && $6 != "" { print $1, $3, $6 }' | while read -r n u h; do
		[[ -d $h ]] || continue
		[[ -z $only_user || $n == "$only_user" ]] && printf '%s %s %s\n' "$n" "$u" "$h"
	done
}

# as_user NAME UID HOME CMD...: run in the user's session environment.
as_user() {
	local name=$1 uid=$2 home=$3
	shift 3
	local env=(HOME="$home" XDG_RUNTIME_DIR="/run/user/$uid" DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$uid/bus")
	if have runuser; then
		run runuser -u "$name" -- env "${env[@]}" "$@"
	else
		run su -s /bin/sh "$name" -c "$(printf '%q ' env "${env[@]}" "$@")"
	fi
}

method=none
if [[ -f $destdir$manifest ]]; then
	method=script
elif ((live)); then
	if have dpkg && dpkg -S "$bindir/lindeskd" >/dev/null 2>&1; then
		method=deb
	elif have rpm && rpm -qf "$bindir/lindeskd" >/dev/null 2>&1; then
		method=rpm
	elif have pacman && pacman -Qo "$bindir/lindeskd" >/dev/null 2>&1; then
		method=arch
	fi
fi

# ── 1. stop ──────────────────────────────────────────────────────────────────────────────
stop_all() {
	info "stopping lindesk"
	local name uid home
	if have systemctl; then
		while read -r name uid home; do
			[[ -n $name ]] || continue
			try "stop lindesk-session for $name" systemctl --user -M "$name@" stop \
				lindesk-session.service lindesk-mcp.socket lindesk-mcp.service lindesk-compact.timer 2>/dev/null
		done < <(logged_in_users)
		if [[ -d /run/systemd/system ]]; then
			try "stop lindeskd" systemctl stop lindesk-compact.timer lindeskd.service 2>/dev/null
		fi
	fi
	if have rc-service && [[ -e $sysconfdir/init.d/lindeskd ]]; then
		try "stop lindeskd (OpenRC)" rc-service lindeskd stop
	fi
}

# ── 2. restore ───────────────────────────────────────────────────────────────────────────
restore_all() {
	local name uid home
	info "restoring what lindesk changed"
	# Runtime settings (cgroups, iocost). Idempotent; also run by the units' ExecStopPost.
	if [[ -x $bindir/lindeskd ]]; then
		if try "lindeskd --restore" "$bindir/lindeskd" --restore </dev/null; then
			restored+=("system cgroup settings and iocost")
		else
			restore_failed=1
		fi
	fi
	if [[ -x $bindir/lindesk-session ]]; then
		while read -r name uid home; do
			[[ -n $name ]] || continue
			if try "lindesk-session --restore for $name" as_user "$name" "$uid" "$home" "$bindir/lindesk-session" --restore </dev/null; then
				restored+=("$name's unit settings")
			fi
		done < <(logged_in_users)
	fi

	# Versioned changes: sysctls, schedulers, zswap, cpufreq, services, config.
	if [[ -x $ctl ]]; then
		local root_args=()
		[[ -n $fixture_root ]] && root_args=(--root "$fixture_root")
		if [[ -d $libdir/changes ]]; then
			if try "revert system changes" "$ctl" changes revert-all --scope system --yes "${root_args[@]}"; then
				restored+=("system changes (lindeskctl changes revert-all)")
			else
				restore_failed=1
			fi
		fi
		while read -r name uid home; do
			[[ -n $name && -d $home/.local/state/lindesk/changes ]] || continue
			if try "revert $name's changes" as_user "$name" "$uid" "$home" "$ctl" changes revert-all --scope user --yes; then
				restored+=("$name's changes")
			fi
		done < <(all_users)
	elif [[ -d $libdir/changes ]]; then
		warn "lindeskctl is missing: the changes recorded in $libdir/changes can't be reverted automatically"
		restore_failed=1
	fi

	# Persisted drop-ins (normally already removed by the revert).
	local f reload_sysctl=0 reload_udev=0
	for f in "$etc/sysctl.d/90-lindesk.conf" "$etc/udev/rules.d/90-lindesk-iosched.rules"; do
		[[ -e $f ]] || continue
		run rm -f "$f" && removed+=("$f")
		[[ $f == *sysctl.d* ]] && reload_sysctl=1 || reload_udev=1
	done
	if [[ -z $fixture_root ]]; then
		((reload_sysctl)) && have sysctl && try "sysctl --system" sysctl --system >/dev/null
		((reload_udev)) && have udevadm && try "udev reload" udevadm control --reload
	fi

	# Conflicting daemons: the changesets are reverted above; now anything changed by hand.
	if [[ -x $ctl && -f $libdir/install-state.json ]]; then
		local choice=()
		case $conflicts_choice in
		restore) choice=(--restore-conflicts) ;;
		keep) choice=(--keep-current) ;;
		*) ((interactive)) || choice=(--keep-current) ;;
		esac
		if ! try "reconcile conflicting daemons" "$ctl" setup conflicts --reconcile "${choice[@]}"; then
			restore_failed=1
		fi
	fi

	# Desktop integration that lindesk-session switched on (first-run.json).
	while read -r name uid home; do
		[[ -n $name ]] || continue
		f=$home/.local/state/lindesk/first-run.json
		[[ -f $f ]] || continue
		if grep -q 'enabled the GNOME extension' "$f" && have gnome-extensions; then
			try "disable the GNOME extension for $name" as_user "$name" "$uid" "$home" \
				gnome-extensions disable lindesk@lindesk.org && restored+=("$name's GNOME extension disabled")
		fi
		if grep -q 'enabled the KWin script' "$f"; then
			local kw
			kw=$(command -v kwriteconfig6 || command -v kwriteconfig5 || true)
			if [[ -n $kw ]]; then
				try "disable the KWin script for $name" as_user "$name" "$uid" "$home" \
					"$kw" --file kwinrc --group Plugins --key lindeskEnabled false && restored+=("$name's KWin script disabled")
			fi
		fi
	done < <(all_users)

	if ((!dry_run)) && [[ -d $libdir ]]; then
		if ((restore_failed)); then
			: >"$libdir/restore-failed" 2>/dev/null || true
		else
			rm -f "$libdir/restore-failed"
		fi
	fi
}

# ── 3. remove files ──────────────────────────────────────────────────────────────────────
remove_manifest_files() {
	local m=$destdir$manifest line path sum
	local -a lines=() dirs=()
	mapfile -t lines < <(grep -v '^#' "$m" || true)
	if ((live)) && [[ -d /run/systemd/system ]]; then
		try "disable lindesk units" systemctl disable lindeskd.service lindesk-compact.timer 2>/dev/null
		try "disable user units" systemctl --global disable lindesk-session.service lindesk-compact.timer \
			lindesk-mcp.socket lindesk-mcp.service 2>/dev/null
	fi
	if ((live)); then
		have rc-update && [[ -e $sysconfdir/init.d/lindeskd ]] && try "rc-update del" rc-update del lindeskd default
		for path in /var/service/lindeskd /etc/runit/runsvdir/default/lindeskd; do
			[[ -L $path ]] && run rm -f "$path"
		done
	fi
	info "removing the files listed in $manifest"
	for line in "${lines[@]}"; do
		case $line in
		"f "*)
			path=${line#f }
			[[ $path == "$manifest" ]] && continue
			if [[ -e $destdir$path || -L $destdir$path ]]; then run rm -f "$destdir$path" && removed+=("$path"); fi
			;;
		"c "*)
			line=${line#c }; sum=${line%% *}; path=${line#* }
			[[ -f $destdir$path ]] || continue
			if ((purge)) || [[ $(sha256sum "$destdir$path" | cut -d' ' -f1) == "$sum" ]]; then
				run rm -f "$destdir$path" && removed+=("$path")
			else
				info "keeping modified $path"
				kept+=("$path (modified)")
			fi
			;;
		"d "*) dirs+=("${line#d }") ;;
		esac
	done
	run rm -f "$m"
	# Deepest first; only directories install.sh created, and only if empty.
	while IFS= read -r path; do
		[[ -n $path && -d $destdir$path ]] || continue
		if ((dry_run)); then
			run rmdir "$destdir$path"
		else
			rmdir -- "$destdir$path" 2>/dev/null || true
		fi
	done < <(printf '%s\n' "${dirs[@]}" | awk '{ print gsub("/", "/"), $0 }' | sort -rn | cut -d' ' -f2-)
	if ((live)) && [[ -d /run/systemd/system ]]; then try "daemon-reload" systemctl daemon-reload; fi
}

remove_package() {
	local cmd=()
	case $method in
	deb) cmd=(apt-get remove -y lindesk-telemetry lindesk) ;;
	rpm) cmd=(dnf remove -y lindesk-telemetry lindesk) ;;
	arch) cmd=(pacman -R --noconfirm lindesk-telemetry lindesk) ;;
	esac
	if ((!package)); then
		info "lindesk was installed by a package; remove it with: sudo ${cmd[*]}"
		info "(or re-run this with --package to do it now)"
		kept+=("the $method packages (not removed without --package)")
		return
	fi
	# Each package manager refuses an unknown name: drop lindesk-telemetry when absent.
	case $method in
	deb) dpkg -s lindesk-telemetry >/dev/null 2>&1 || cmd=(apt-get remove -y lindesk) ;;
	rpm) rpm -q lindesk-telemetry >/dev/null 2>&1 || cmd=(dnf remove -y lindesk) ;;
	arch) pacman -Q lindesk-telemetry >/dev/null 2>&1 || cmd=(pacman -R --noconfirm lindesk) ;;
	esac
	# The package's own removal scripts restore again; harmless (idempotent).
	if try "${cmd[*]}" "${cmd[@]}"; then removed+=("the $method packages"); fi
}

# ── 4. purge ─────────────────────────────────────────────────────────────────────────────
purge_all() {
	local d name uid home
	info "purging configuration and state"
	for d in "$destdir$confdir" "$destdir/run/lindesk"; do
		[[ -e $d ]] && run rm -rf "$d" && removed+=("${d#"$destdir"}")
	done
	if [[ -e $destdir$libdir ]]; then
		if ((restore_failed)); then
			warn "the restore did not fully succeed: keeping $libdir (install-state.json and the change store)"
			kept+=("$libdir: restore incomplete; re-run after fixing the problems above")
		else
			run rm -rf "$destdir$libdir" && removed+=("$libdir")
		fi
	fi
	if ((!live)); then
		# A staged tree: drop the parents the state lived in when they are now empty.
		rmdir "$destdir/var/lib" "$destdir/var" "$destdir/run" 2>/dev/null || true
		return 0
	fi
	local -a homes=()
	while read -r name uid home; do
		for d in "$home/.config/lindesk" "$home/.local/state/lindesk" "$home/.local/share/lindesk/plugins"; do
			[[ -e $d ]] && homes+=("$d")
		done
	done < <(all_users)
	((${#homes[@]})) || return 0
	printf '  per-user data:\n'
	printf '    %s\n' "${homes[@]}"
	if ask "Remove this per-user data too?"; then
		for d in "${homes[@]}"; do run rm -rf "$d" && removed+=("$d"); done
	else
		kept+=("per-user data (${#homes[@]} directories)")
	fi
}

# ── main ─────────────────────────────────────────────────────────────────────────────────
if ((live)); then
	info "lindesk install: ${method/none/not found (cleaning up leftovers)}"
	stop_all
	restore_all
fi
if ((!restore_only)); then
	case $method in
	script) remove_manifest_files ;;
	deb | rpm | arch) remove_package ;;
	none) [[ -n $destdir ]] && info "no install manifest under $destdir" ;;
	esac
	((purge)) && purge_all
	if ((!purge)) && ((live)); then
		kept+=("$confdir, $libdir and per-user data (use --purge to remove them)")
	fi
fi

echo
if ((dry_run)); then echo "lindesk uninstall summary (dry run):"; else echo "lindesk uninstall summary:"; fi
((${#restored[@]})) && printf '  restored: %s\n' "${restored[@]}"
((${#removed[@]})) && printf '  removed:  %s\n' "${removed[@]}"
((${#kept[@]})) && printf '  kept:     %s\n' "${kept[@]}"
if ((${#problems[@]})); then
	printf '  problems: %s\n' "${problems[@]}"
	exit 1
fi
exit 0
