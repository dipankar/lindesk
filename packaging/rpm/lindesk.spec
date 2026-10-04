# lindesk RPM spec. The file layout comes from packaging/install.sh (%install).
#
# Build (packaging/build-packages.sh rpm does this inside fedora:latest):
#   rpmbuild -ba --define "_topdir $PWD/rpmbuild" --define "lindesk_version 0.1.0" \
#            [--define "lindesk_bin_dir /path/to/prebuilt/bins"] lindesk.spec
# Source0 is a tarball of the source tree with top-level directory lindesk-%%{version}/.

%global debug_package %{nil}

Name:           lindesk
Version:        %{?lindesk_version}%{!?lindesk_version:0.1.0}
Release:        1%{?dist}
Summary:        macOS-style QoS for the Linux desktop
License:        MIT OR Apache-2.0
URL:            https://github.com/dipankar/lindesk
Source0:        %{name}-%{version}.tar.gz

ExclusiveArch:  x86_64 aarch64 ppc64le s390x riscv64
BuildRequires:  systemd-rpm-macros
%if %{undefined lindesk_bin_dir}
BuildRequires:  cargo >= 1.88
BuildRequires:  rust >= 1.88
BuildRequires:  gcc
BuildRequires:  gcc-c++
BuildRequires:  make
%endif
%{?systemd_requires}
Requires:       dbus
Requires:       polkit
Recommends:     gnome-shell-extension-prefs
Suggests:       duckdb

%description
lindesk is a userspace resource governor that keeps the application you are
using responsive while compiles, copies, indexers and package upgrades run in
the background. It connects desktop focus and input activity to PSI, cgroup v2
weights, uclamp and io.max, intervening only when the foreground suffers, and
reverting every change on exit.

This package contains the root broker (lindeskd), the per-user session agent
(lindesk-session), the lindeskctl CLI, telemetry maintenance (lindesk-data),
the benchmark tool, the MCP server for local AI agents (lindesk-mcp), the
GNOME Shell extension and the KWin script.

%prep
%autosetup -n %{name}-%{version}

%build
%if %{undefined lindesk_bin_dir}
cargo build --release --locked \
    -p lindeskd -p lindesk-session -p lindeskctl -p lindesk-data -p lindesk-bench -p lindesk-mcp
%endif

%install
%if %{defined lindesk_bin_dir}
export LINDESK_BIN_DIR=%{lindesk_bin_dir}
%endif
./packaging/install.sh --destdir=%{buildroot} --prefix=%{_prefix} --sysconfdir=%{_sysconfdir} \
    --no-build --no-enable --no-manifest
# Completions and man pages exist only if `lindeskctl completions|man` are supported.
(cd %{buildroot} && find .%{_datadir}/bash-completion .%{_datadir}/zsh .%{_datadir}/fish \
    .%{_mandir} -type f 2>/dev/null | sed 's|^\.||') > %{name}-generated.files || :

%post
%{_bindir}/systemd-sysusers %{_sysusersdir}/lindesk.conf >/dev/null 2>&1 || :
%tmpfiles_create %{_tmpfilesdir}/lindesk.conf
%systemd_post lindeskd.service lindesk-compact.timer
%systemd_user_post lindesk-session.service lindesk-compact.timer lindesk-mcp.socket
# Requirement check: never fails the transaction. On a hard failure (exit 1) lindeskd is
# installed and enabled by the preset, but not started.
lindesk_rc=0
lindesk_out=
if [ -x %{_bindir}/lindeskctl ]; then
    if command -v timeout >/dev/null 2>&1; then
        lindesk_out=$(timeout 30 %{_bindir}/lindeskctl doctor --preflight --quiet 2>&1 </dev/null) || lindesk_rc=$?
    else
        lindesk_out=$(%{_bindir}/lindeskctl doctor --preflight --quiet 2>&1 </dev/null) || lindesk_rc=$?
    fi
fi
if [ "$lindesk_rc" -eq 1 ]; then
    {
        echo "lindesk: WARNING: this system does not meet lindesk's requirements:"
        [ -z "$lindesk_out" ] || printf '%%s\n' "$lindesk_out" | sed 's/^/lindesk:   /'
        echo "lindesk: lindeskd was installed but NOT started. Fix the failures, check with"
        echo "lindesk:   lindeskctl doctor --preflight"
        echo "lindesk: then start it:  systemctl start lindeskd.service"
    } >&2
elif [ "$1" -eq 1 ] && [ -d /run/systemd/system ]; then
    # The preset enables lindeskd; start it on first install, and the session agent in
    # every graphical session that is already open.
    systemctl start lindeskd.service lindesk-compact.timer >/dev/null 2>&1 || :
    # tmo CMD...: run with a 15 s timeout when timeout(1) exists.
    tmo() { if command -v timeout >/dev/null 2>&1; then timeout 15 "$@"; else "$@"; fi; }
    # lindesk_start_sessions: start lindesk-session for every user logged in to a
    # graphical session, through their user manager, so nobody has to log out.
    # Best effort, never fatal.
    lindesk_start_sessions() {
        command -v loginctl >/dev/null 2>&1 || return 0
        [ -d /run/systemd/system ] || return 0
        for _sid in $(loginctl list-sessions --no-legend 2>/dev/null | awk '{ print $1 }'); do
            _type=$(loginctl show-session "$_sid" -p Type --value 2>/dev/null) || continue
            case $_type in wayland | x11) ;; *) continue ;; esac
            loginctl show-session "$_sid" -p Name --value 2>/dev/null
        done | sort -u | while read -r _u; do
            [ -n "$_u" ] || continue
            tmo systemctl --user -M "$_u@" daemon-reload >/dev/null 2>&1 || true
            if tmo systemctl --user -M "$_u@" start lindesk-session.service >/dev/null 2>&1; then
                echo "lindesk: started lindesk-session for $_u"
            fi
        done
        return 0
    }

    lindesk_start_sessions || :
    echo "lindesk: what now: 'lindeskctl' shows what lindesk is doing, 'lindeskctl watch' follows it live,"
    echo "lindesk:   'lindeskctl doctor' shows what this machine supports."
    echo "lindesk: GNOME: lindesk-session enables its Shell extension on first start; if it stays off:"
    echo "lindesk:   gnome-extensions enable lindesk@lindesk.org   (on Wayland, log out and back in first)"
    echo "lindesk: KDE: enable 'lindesk' in System Settings > Window Management > KWin Scripts"
    echo "lindesk: AI agents (MCP): claude mcp add lindesk -- lindesk-mcp"
fi
:

%preun
%systemd_preun lindeskd.service lindesk-compact.timer
%systemd_user_preun lindesk-session.service lindesk-compact.timer lindesk-mcp.socket lindesk-mcp.service
if [ $1 -eq 0 ]; then
    # Undo every runtime change (the unit's ExecStopPost does this too under systemd).
    %{_bindir}/lindeskd --restore >/dev/null 2>&1 || :
fi

%postun
%systemd_postun_with_restart lindeskd.service
%systemd_postun lindesk-compact.timer
%systemd_user_postun lindesk-session.service lindesk-compact.timer lindesk-mcp.socket lindesk-mcp.service

%files -f %{name}-generated.files
%license %{_datadir}/licenses/%{name}/LICENSE-MIT
%license %{_datadir}/licenses/%{name}/LICENSE-APACHE
%dir %{_datadir}/licenses/%{name}
%doc %{_docdir}/%{name}
%{_bindir}/lindeskd
%{_bindir}/lindesk-session
%{_bindir}/lindeskctl
%{_bindir}/lindesk-data
%{_bindir}/lindesk-bench
%{_bindir}/lindesk-mcp
%{_unitdir}/lindeskd.service
%{_unitdir}/lindesk-compact.service
%{_unitdir}/lindesk-compact.timer
%{_userunitdir}/lindesk-session.service
%{_userunitdir}/lindesk-compact.service
%{_userunitdir}/lindesk-compact.timer
%{_userunitdir}/lindesk-mcp.service
%{_userunitdir}/lindesk-mcp.socket
%{_presetdir}/80-lindesk.preset
%{_userpresetdir}/80-lindesk.preset
%{_sysusersdir}/lindesk.conf
%{_tmpfilesdir}/lindesk.conf
%{_datadir}/dbus-1/system.d/org.lindesk.Broker1.conf
%{_datadir}/dbus-1/system-services/org.lindesk.Broker1.service
%{_datadir}/dbus-1/services/org.lindesk.QoS1.service
%{_datadir}/polkit-1/actions/org.lindesk.policy
%dir %{_sysconfdir}/lindesk
%config(noreplace) %{_sysconfdir}/lindesk/lindesk.toml
%dir %{_sysconfdir}/lindesk/rules.d
%dir %{_sysconfdir}/lindesk/plugins
%dir %{_sysconfdir}/lindesk/profiles.d
%config(noreplace) %{_sysconfdir}/xdg/autostart/lindesk-session.desktop
%{_datadir}/lindesk
%dir %{_prefix}/lib/lindesk
%dir %{_prefix}/lib/lindesk/plugins
%{_datadir}/gnome-shell/extensions/lindesk@lindesk.org
%dir %{_datadir}/kwin
%dir %{_datadir}/kwin/scripts
%{_datadir}/kwin/scripts/lindesk

%changelog
* Sat Oct 03 2026 Dipankar Sarkar <me@dipankar.name> - 0.1.0-1
- Initial package
