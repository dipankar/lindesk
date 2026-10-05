#!/usr/bin/env bash
# Build lindesk packages into dist/: for each format two packages, `lindesk` (the core:
# lindeskd, lindesk-session, lindeskctl, lindesk-mcp, lindesk-bench; no DuckDB or Vortex)
# and `lindesk-telemetry` (lindesk-data, the telemetry plugin, the compaction timers;
# depends on lindesk).
#
#   packaging/build-packages.sh deb      # .deb with dpkg-deb (Debian/Ubuntu host, or a container)
#   packaging/build-packages.sh rpm      # .rpm with rpmbuild (Fedora container)
#   packaging/build-packages.sh arch     # .pkg.tar.zst with makepkg (Arch container)
#   packaging/build-packages.sh all
#
# Every format stages files with packaging/install.sh --destdir (--without-telemetry for the
# core, --telemetry-only for lindesk-telemetry), so the layout is identical.
# rpm and arch run natively when rpmbuild/makepkg exist, or inside fedora:latest /
# archlinux:latest via docker otherwise. CI runs them in those containers directly:
#   docker run --rm -v "$PWD":/src -w /src fedora:latest ./packaging/build-packages.sh rpm
# Inside a container (only there) the script installs its own build dependencies.
#
# Environment:
#   LINDESK_BIN_DIR    prebuilt binaries; skips cargo (the rpm/arch builds then package them
#                      instead of compiling from the source tarball)
#   CARGO_TARGET_DIR   where `cargo build --release` puts binaries for the deb (default target/)
#   LINDESK_DIST       output directory (default: <repo>/dist)
#   LINDESK_PACKAGES   deb only: all (default), core (just lindesk) or telemetry
#                      (just lindesk-telemetry); only those binaries are needed
#   LINDESK_CHOWN      uid:gid to give the produced files (set automatically when this script
#                      dispatches to docker, so dist/ is not left root-owned)
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
PKG=$ROOT/packaging
DIST=${LINDESK_DIST:-$ROOT/dist}
CORE_BINARIES=(lindeskd lindesk-session lindeskctl lindesk-bench lindesk-mcp)
TELEMETRY_BINARIES=(lindesk-data lindesk-telemetry-sink)
PACKAGES=${LINDESK_PACKAGES:-all}
case $PACKAGES in
all) BINARIES=("${CORE_BINARIES[@]}" "${TELEMETRY_BINARIES[@]}") ;;
core) BINARIES=("${CORE_BINARIES[@]}") ;;
telemetry) BINARIES=("${TELEMETRY_BINARIES[@]}") ;;
*) echo "build-packages: LINDESK_PACKAGES must be all, core or telemetry" >&2; exit 2 ;;
esac
DEB_IMAGE=${LINDESK_DEB_IMAGE:-debian:stable-slim}
RPM_IMAGE=${LINDESK_RPM_IMAGE:-fedora:latest}
ARCH_IMAGE=${LINDESK_ARCH_IMAGE:-archlinux:latest}

CLEANUP=()
cleanup() { ((${#CLEANUP[@]})) && rm -rf "${CLEANUP[@]}"; return 0; }
trap cleanup EXIT
# scratch VAR: set VAR to a temporary directory that is removed when the script exits.
scratch() { local d; d=$(mktemp -d); CLEANUP+=("$d"); printf -v "$1" '%s' "$d"; }

die() { printf 'build-packages: error: %s\n' "$*" >&2; exit 1; }
info() { printf '==> %s\n' "$*"; }
have() { command -v "$1" >/dev/null 2>&1; }

usage() {
	sed -n '2,/^set -euo/{/^set -euo/d;s/^# \{0,1\}//;p}' "$0"
}

in_container() {
	[[ -n ${LINDESK_IN_CONTAINER:-} || -f /.dockerenv || -f /run/.containerenv ]]
}

# Installing build dependencies is only ever done as root inside a throwaway container.
can_install_deps() { in_container && [[ $(id -u) -eq 0 ]]; }

version() {
	awk '
		/^\[workspace\.package\]/ { inpkg = 1; next }
		/^\[/ { inpkg = 0 }
		inpkg && /^version[[:space:]]*=/ { gsub(/.*=[[:space:]]*"|".*/, ""); print; exit }
	' "$ROOT/Cargo.toml"
}

VERSION=$(version)
[[ -n $VERSION ]] || die "cannot read [workspace.package] version from Cargo.toml"
# Pre-release suffixes sort before the release with '~' (deb, rpm); pacman forbids '-'.
DEB_VERSION="${VERSION//-/\~}-1"
RPM_VERSION="${VERSION//-/\~}"
ARCH_VERSION="${VERSION//-/_}"

# Binaries for the deb (and for rpm/arch when given): LINDESK_BIN_DIR or a cargo build.
resolve_bins() {
	if [[ -n ${LINDESK_BIN_DIR:-} ]]; then
		BIN_DIR=$(cd -- "$LINDESK_BIN_DIR" && pwd)
	else
		have cargo || die "cargo not found; install Rust or set LINDESK_BIN_DIR"
		local p=() b
		for b in "${BINARIES[@]}"; do p+=(-p "$b"); done
		info "cargo build --release --locked ${p[*]}"
		cargo build --release --locked --manifest-path "$ROOT/Cargo.toml" "${p[@]}"
		BIN_DIR=${CARGO_TARGET_DIR:-$ROOT/target}/release
	fi
	local b
	for b in "${BINARIES[@]}"; do [[ -x $BIN_DIR/$b ]] || die "missing $BIN_DIR/$b"; done
}

# source_tarball OUT: the working tree as lindesk-<version>/, without build outputs.
source_tarball() {
	local out=$1 topdir=$2
	tar -C "$ROOT" \
		--exclude=./.git --exclude=./target --exclude='./target-*' --exclude=./dist \
		--exclude=__pycache__ --exclude=./docs/book --exclude='*.parquet.tmp' \
		--transform="s,^\\.,$topdir," --owner=0 --group=0 --numeric-owner \
		-czf "$out" .
}

finish() {
	local f
	for f in "$@"; do
		[[ -n ${LINDESK_CHOWN:-} ]] && chown "$LINDESK_CHOWN" "$f"
		info "built $f"
	done
	if [[ -n ${LINDESK_CHOWN:-} ]]; then chown "$LINDESK_CHOWN" "$DIST"; fi
}

# dispatch IMAGE TARGET: rerun this script for TARGET inside a container.
dispatch() {
	local image=$1 target=$2 args=()
	have docker || die "$target needs ${3:-its build tool} or docker"
	in_container && die "$target: build tools missing inside the container and cannot be installed"
	args=(--rm -v "$ROOT:/src" -w /src -e LINDESK_IN_CONTAINER=1 -e "LINDESK_CHOWN=$(id -u):$(id -g)")
	if [[ -n ${LINDESK_BIN_DIR:-} ]]; then
		args+=(-v "$(cd -- "$LINDESK_BIN_DIR" && pwd):/lindesk-bin:ro" -e LINDESK_BIN_DIR=/lindesk-bin)
	fi
	if [[ -n ${LINDESK_DIST:-} ]]; then
		mkdir -p "$DIST"
		args+=(-v "$(cd -- "$DIST" && pwd):/dist" -e LINDESK_DIST=/dist)
	fi
	info "running '$target' in $image"
	docker run "${args[@]}" "$image" ./packaging/build-packages.sh "$target"
}

is_elf() { [[ $(head -c4 "$1" | od -An -c | tr -d ' ') == '177ELF' ]]; }

# ── deb ──────────────────────────────────────────────────────────────────────────────────
build_deb() {
	if ! have dpkg-deb; then
		if can_install_deps && have apt-get; then
			{ apt-get update -qq && apt-get install -y -qq --no-install-recommends dpkg-dev; } \
				>/tmp/deps.log 2>&1 || { cat /tmp/deps.log; exit 1; }
		else
			dispatch "$DEB_IMAGE" deb dpkg-deb
			return
		fi
	fi
	resolve_bins
	local arch work
	arch=$(dpkg --print-architecture 2>/dev/null || dpkg-architecture -qDEB_HOST_ARCH)
	scratch work
	mkdir -p "$DIST"
	DEBS=()
	[[ $PACKAGES == telemetry ]] || deb_package lindesk "$work" "$arch" --without-telemetry
	[[ $PACKAGES == core ]] || deb_package lindesk-telemetry "$work" "$arch" --telemetry-only
	finish "${DEBS[@]}"
}

# deb_package NAME WORK ARCH INSTALL_OPT: stage NAME with install.sh INSTALL_OPT and build
# it into $DIST; appends the .deb to DEBS.
deb_package() {
	local name=$1 work=$2 arch=$3 opt=$4 stage f size shlibs="" elves=() scripts control
	stage=$work/$name
	if [[ $name == lindesk ]]; then
		scripts=$PKG/debian
		control=$PKG/debian/control.in
	else
		scripts=$PKG/debian/telemetry
		control=$PKG/debian/control-telemetry.in
	fi
	LINDESK_BIN_DIR=$BIN_DIR "$PKG/install.sh" --destdir="$stage" --prefix=/usr --sysconfdir=/etc \
		--no-build --no-enable --no-manifest "$opt" >"$work/$name-install.log" ||
		{ cat "$work/$name-install.log"; exit 1; }
	install -D -m 0644 "$PKG/debian/copyright" "$stage/usr/share/doc/$name/copyright"

	install -d -m 0755 "$stage/DEBIAN"
	for f in postinst prerm postrm; do install -m 0755 "$scripts/$f" "$stage/DEBIAN/$f"; done
	if [[ -d $stage/etc ]]; then
		(cd "$stage" && find etc -type f | sort | sed 's|^|/|') >"$stage/DEBIAN/conffiles"
	fi
	(cd "$stage" && find usr -type f -print0 | sort -z | xargs -0 md5sum) >"$stage/DEBIAN/md5sums"
	size=$(du -sk --exclude=DEBIAN "$stage" | cut -f1)

	# Shared-library dependencies of the real (ELF) binaries; stubs have none.
	while IFS= read -r -d '' f; do is_elf "$f" && elves+=("$f"); done < <(find "$stage/usr" -type f -perm -0100 -print0)
	if ((${#elves[@]})) && have dpkg-shlibdeps; then
		mkdir -p "$work/shlibs-$name/debian"
		printf 'Source: lindesk\n\nPackage: %s\nArchitecture: any\n' "$name" >"$work/shlibs-$name/debian/control"
		shlibs=$(cd "$work/shlibs-$name" && dpkg-shlibdeps -O "${elves[@]}" 2>/dev/null |
			sed -n 's/^shlibs:Depends=//p') || shlibs=""
	fi

	sed -e "s/@VERSION@/$DEB_VERSION/g" -e "s/@ARCH@/$arch/" -e "s/@INSTALLED_SIZE@/$size/" \
		-e "s/@SHLIBS_DEPENDS@/${shlibs:+, $shlibs}/" \
		"$control" >"$stage/DEBIAN/control"

	local deb=$DIST/${name}_${DEB_VERSION}_${arch}.deb
	dpkg-deb --build --root-owner-group -Zxz "$stage" "$deb" >/dev/null
	DEBS+=("$deb")
}

# ── rpm ──────────────────────────────────────────────────────────────────────────────────
build_rpm() {
	if ! have rpmbuild; then
		if can_install_deps && have dnf; then
			local deps=(rpm-build systemd-rpm-macros tar gzip findutils diffutils)
			[[ -z ${LINDESK_BIN_DIR:-} ]] && deps+=(cargo rust gcc gcc-c++ make)
			info "installing ${deps[*]}"
			dnf install -y -q --setopt=install_weak_deps=False "${deps[@]}" >/tmp/deps.log 2>&1 ||
				{ cat /tmp/deps.log; exit 1; }
		else
			dispatch "$RPM_IMAGE" rpm rpmbuild
			return
		fi
	fi
	local top defs=()
	scratch top
	mkdir -p "$top"/{SOURCES,SPECS,BUILD,RPMS,SRPMS}
	source_tarball "$top/SOURCES/lindesk-$RPM_VERSION.tar.gz" "lindesk-$RPM_VERSION"
	defs=(--define "_topdir $top" --define "lindesk_version $RPM_VERSION")
	if [[ -n ${LINDESK_BIN_DIR:-} ]]; then
		defs+=(--define "lindesk_bin_dir $(cd -- "$LINDESK_BIN_DIR" && pwd)")
	fi
	info "rpmbuild lindesk $RPM_VERSION"
	rpmbuild -ba "${defs[@]}" "$PKG/rpm/lindesk.spec" >"$top/rpmbuild.log" 2>&1 || {
		cat "$top/rpmbuild.log"
		exit 1
	}
	mkdir -p "$DIST"
	local out=() f
	for f in "$top"/RPMS/*/*.rpm "$top"/SRPMS/*.rpm; do
		cp -f "$f" "$DIST/"
		out+=("$DIST/${f##*/}")
	done
	finish "${out[@]}"
}

# ── arch ─────────────────────────────────────────────────────────────────────────────────
build_arch() {
	# The archlinux image ships makepkg (part of pacman) but not fakeroot/base-devel.
	if ! have makepkg || ! have fakeroot || { [[ -z ${LINDESK_BIN_DIR:-} ]] && ! have cargo; }; then
		if can_install_deps && have pacman; then
			# The split packages' runtime depends (makepkg checks them) and, for a real
			# build, the PKGBUILD's makedepends.
			local deps=(base-devel namcap dbus polkit systemd gcc-libs glibc)
			# shellcheck disable=SC1091,SC2154
			[[ -z ${LINDESK_BIN_DIR:-} ]] &&
				mapfile -t -O "${#deps[@]}" deps < <(source "$PKG/aur/PKGBUILD"; printf '%s\n' "${makedepends[@]}")
			info "installing ${deps[*]}"
			pacman -Syu --noconfirm --needed --noprogressbar "${deps[@]}" >/tmp/deps.log 2>&1 ||
				{ cat /tmp/deps.log; exit 1; }
		else
			dispatch "$ARCH_IMAGE" arch makepkg
			return
		fi
	fi
	local work as=()
	scratch work
	cp "$PKG/aur/PKGBUILD" "$PKG/aur/lindesk.install" "$PKG/aur/lindesk-telemetry.install" "$work/"
	# makepkg refuses to run as root.
	if [[ $(id -u) -eq 0 ]]; then
		id builder >/dev/null 2>&1 || useradd -m builder
		chown -R builder: "$work"
		as=(runuser -u builder --)
	fi
	(cd "$work" && "${as[@]}" makepkg --printsrcinfo) >"$work/SRCINFO"

	# Build the local tree instead of the release tarball named in the PKGBUILD.
	source_tarball "$work/lindesk-$ARCH_VERSION.tar.gz" "lindesk-$ARCH_VERSION"
	# shellcheck disable=SC2016  # $pkgbase/$pkgver are for makepkg
	sed -i -e "s/^pkgver=.*/pkgver=$ARCH_VERSION/" \
		-e 's|^source=.*|source=("$pkgbase-$pkgver.tar.gz")|' "$work/PKGBUILD"
	if [[ -n ${LINDESK_BIN_DIR:-} ]]; then
		# Prebuilt binaries: skip cargo; install.sh picks them up from LINDESK_BIN_DIR.
		printf '\nmakedepends=()\nprepare() { :; }\nbuild() { :; }\ncheck() { :; }\n' >>"$work/PKGBUILD"
	fi

	[[ $(id -u) -eq 0 ]] && chown -R builder: "$work"
	info "makepkg lindesk $ARCH_VERSION"
	(cd "$work" && "${as[@]}" env ${LINDESK_BIN_DIR:+LINDESK_BIN_DIR="$LINDESK_BIN_DIR"} \
		PKGDEST="$work/out" makepkg -f --nocheck --noconfirm) >"$work/makepkg.log" 2>&1 || {
		cat "$work/makepkg.log"
		exit 1
	}
	local pkgs=() pkg out=()
	mapfile -t pkgs < <(find "$work/out" -name 'lindesk-*.pkg.tar.*' ! -name '*-debug-*' | sort)
	((${#pkgs[@]} == 2)) || die "makepkg produced ${#pkgs[@]} packages, expected lindesk and lindesk-telemetry"
	if have namcap; then
		info "namcap"
		namcap "$PKG/aur/PKGBUILD" || true
		for pkg in "${pkgs[@]}"; do namcap "$pkg" || true; done
	fi
	mkdir -p "$DIST"
	for pkg in "${pkgs[@]}"; do
		cp -f "$pkg" "$DIST/"
		out+=("$DIST/${pkg##*/}")
	done
	cp -f "$work/SRCINFO" "$DIST/.SRCINFO"
	finish "${out[@]}" "$DIST/.SRCINFO"
}

case ${1:-} in
deb) build_deb ;;
rpm) build_rpm ;;
arch) build_arch ;;
all) build_deb; build_rpm; build_arch ;;
-h | --help) usage ;;
*) usage >&2; exit 2 ;;
esac
