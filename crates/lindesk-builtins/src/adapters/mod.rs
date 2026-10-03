//! Built-in foreground adapters.
//!
//! Adapters tell lindesk which application the human is using (`Event::Focus`) and when they
//! are actively interacting with it (`Event::Interaction`), so the engine can protect it.
//!
//! | Session | Adapter | Mechanism |
//! |---|---|---|
//! | GNOME (Wayland) | shell extension (`adapters/gnome-shell-extension`) | pushes to `org.lindesk.QoS1` over D-Bus |
//! | KDE Plasma (Wayland) | KWin script (`adapters/kwin-script`) | pushes to `org.lindesk.QoS1` over D-Bus |
//! | COSMIC | [`cosmic::CosmicAdapter`] (`"cosmic"`) | `ext_foreign_toplevel_list_v1` + `zcosmic_toplevel_info_v1` |
//! | any X11 session | [`x11::X11Adapter`] (`"x11"`) | `_NET_ACTIVE_WINDOW` / `_NET_WM_PID` + XInput2 raw events |
//! | other wlroots compositors (sway, Hyprland, river, ...) | none in v1 | integrate through the D-Bus API (see `adapters/README.md`) |
//!
//! [`detect`] picks the built-in adapters for a session; the shell-side ones are installed
//! separately and need nothing from the host.

use std::time::{Duration, Instant};

#[cfg(feature = "cosmic")]
pub mod cosmic;
#[cfg(feature = "x11")]
pub mod x11;

#[cfg(any(feature = "x11", feature = "cosmic"))]
mod worker;

#[cfg(all(test, feature = "x11"))]
mod x11_tests;

use lindesk_plugin_api::Adapter;

/// Plugin name of [`x11::X11Adapter`].
pub const X11: &str = "x11";
/// Plugin name of [`cosmic::CosmicAdapter`].
pub const COSMIC: &str = "cosmic";

/// Minimum spacing between two `Event::Interaction`s from one adapter.
pub const INTERACTION_THROTTLE: Duration = Duration::from_millis(250);

/// Names of the adapters compiled into this build.
pub fn available() -> Vec<&'static str> {
    let mut v = Vec::new();
    if cfg!(feature = "x11") {
        v.push(X11);
    }
    if cfg!(feature = "cosmic") {
        v.push(COSMIC);
    }
    v
}

/// Instantiates a built-in adapter by plugin name. `None` for unknown names and for adapters
/// whose cargo feature is disabled.
pub fn create(name: &str) -> Option<Box<dyn Adapter>> {
    match name {
        #[cfg(feature = "x11")]
        X11 => Some(Box::new(x11::X11Adapter::default())),
        #[cfg(feature = "cosmic")]
        COSMIC => Some(Box::new(cosmic::CosmicAdapter::default())),
        _ => None,
    }
}

/// The parts of the process environment that decide which adapter to run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionEnv {
    /// `XDG_CURRENT_DESKTOP`, a colon-separated list such as `ubuntu:GNOME`.
    pub current_desktop: Option<String>,
    /// `XDG_SESSION_TYPE`: `wayland`, `x11`, `tty`, ...
    pub session_type: Option<String>,
    /// `WAYLAND_DISPLAY`
    pub wayland_display: Option<String>,
    /// `DISPLAY`
    pub display: Option<String>,
}

impl SessionEnv {
    /// Reads the variables through `get` (e.g. `|k| std::env::var(k).ok()`). Empty values count
    /// as unset.
    pub fn from_fn(get: impl Fn(&str) -> Option<String>) -> Self {
        let get = |k: &str| get(k).filter(|v| !v.trim().is_empty());
        SessionEnv {
            current_desktop: get("XDG_CURRENT_DESKTOP"),
            session_type: get("XDG_SESSION_TYPE"),
            wayland_display: get("WAYLAND_DISPLAY"),
            display: get("DISPLAY"),
        }
    }

    /// Reads the variables from this process's environment.
    pub fn from_process() -> Self {
        Self::from_fn(|k| std::env::var(k).ok())
    }

    fn has_desktop(&self, name: &str) -> bool {
        self.current_desktop.as_deref().is_some_and(|d| d.split(':').any(|part| part.trim().eq_ignore_ascii_case(name)))
    }

    fn session_type_is(&self, ty: &str) -> bool {
        self.session_type.as_deref().is_some_and(|t| t.trim().eq_ignore_ascii_case(ty))
    }

    /// Wayland session: `XDG_SESSION_TYPE=wayland`, or no usable session type but a
    /// `WAYLAND_DISPLAY`.
    pub fn is_wayland(&self) -> bool {
        self.session_type_is("wayland") || (!self.session_type_is("x11") && self.wayland_display.is_some())
    }

    /// X11 session: `XDG_SESSION_TYPE=x11`, or not Wayland and a `DISPLAY` is set. A `DISPLAY`
    /// inside a Wayland session is Xwayland, which only sees X clients, so it does not count.
    pub fn is_x11(&self) -> bool {
        !self.is_wayland() && (self.session_type_is("x11") || self.display.is_some())
    }
}

/// Picks the built-in adapters to enable for a session.
///
/// * COSMIC → `["cosmic"]`.
/// * GNOME and KDE Plasma on Wayland → `[]`: their shell-side scripts push focus and input
///   over D-Bus, and nothing outside the compositor can observe focus there.
/// * Any X11 session (GNOME/Plasma on Xorg, Xfce, MATE, i3, ...) → `["x11"]`. The GNOME
///   extension and KWin script also work on X11; running both just reports the same focus twice.
/// * Other Wayland compositors (wlroots: sway, Hyprland, river, labwc, ...) → `[]` in v1. They
///   can integrate with a few lines of IPC glue calling `org.lindesk.QoS1` (see
///   `adapters/README.md`); a `wlr-foreign-toplevel-management` adapter is planned.
/// * No graphical session (tty, ssh) → `[]`.
///
/// Names are returned even when the matching cargo feature is off; [`create`] returns `None`
/// for those.
pub fn detect(env: &SessionEnv) -> Vec<&'static str> {
    if env.is_wayland() {
        if env.has_desktop("COSMIC") {
            return vec![COSMIC];
        }
        // GNOME, KDE (shell scripts) and everything else: nothing built in.
        return Vec::new();
    }
    if env.is_x11() {
        return vec![X11];
    }
    Vec::new()
}

/// Suppresses consecutive duplicates: [`Debouncer::accept`] returns true only when the value
/// differs from the last accepted one. Used for focus, where compositors and window managers
/// often re-announce the same window.
#[derive(Clone, Debug)]
pub struct Debouncer<T> {
    last: Option<T>,
}

impl<T> Default for Debouncer<T> {
    fn default() -> Self {
        Debouncer { last: None }
    }
}

impl<T: PartialEq> Debouncer<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `value` and returns true if it differs from the previous one.
    pub fn accept(&mut self, value: T) -> bool {
        if self.last.as_ref() == Some(&value) {
            return false;
        }
        self.last = Some(value);
        true
    }

    /// Forgets the last value, so the next one is always accepted (e.g. after a reconnect or
    /// when focus moved to nothing).
    pub fn reset(&mut self) {
        self.last = None;
    }

    pub fn last(&self) -> Option<&T> {
        self.last.as_ref()
    }
}

/// Rate limiter: [`Throttle::ready`] returns true at most once per `interval`. Time is passed in,
/// so the logic is pure and testable.
#[derive(Clone, Debug)]
pub struct Throttle {
    interval: Duration,
    last: Option<Instant>,
}

impl Throttle {
    pub fn new(interval: Duration) -> Self {
        Throttle { interval, last: None }
    }

    /// True (and the window restarts at `now`) if at least `interval` has passed since the last
    /// time this returned true.
    pub fn ready(&mut self, now: Instant) -> bool {
        match self.last {
            Some(last) if now.saturating_duration_since(last) < self.interval => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }

    pub fn reset(&mut self) {
        self.last = None;
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> SessionEnv {
        SessionEnv::from_fn(|k| vars.iter().find(|(name, _)| *name == k).map(|(_, v)| v.to_string()))
    }

    #[test]
    fn detect_gnome_wayland_is_shell_side() {
        let e = env(&[
            ("XDG_CURRENT_DESKTOP", "ubuntu:GNOME"),
            ("XDG_SESSION_TYPE", "wayland"),
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("DISPLAY", ":0"),
        ]);
        assert!(e.is_wayland());
        assert!(!e.is_x11(), "Xwayland's DISPLAY must not count as an X11 session");
        assert!(detect(&e).is_empty());
    }

    #[test]
    fn detect_kde_wayland_is_shell_side() {
        let e =
            env(&[("XDG_CURRENT_DESKTOP", "KDE"), ("XDG_SESSION_TYPE", "wayland"), ("WAYLAND_DISPLAY", "wayland-0")]);
        assert!(detect(&e).is_empty());
    }

    #[test]
    fn detect_cosmic() {
        let e = env(&[
            ("XDG_CURRENT_DESKTOP", "COSMIC"),
            ("XDG_SESSION_TYPE", "wayland"),
            ("WAYLAND_DISPLAY", "wayland-1"),
        ]);
        assert_eq!(detect(&e), vec![COSMIC]);
        // Session type missing, but a Wayland socket is there.
        let e = env(&[("XDG_CURRENT_DESKTOP", "pop:COSMIC"), ("WAYLAND_DISPLAY", "wayland-1")]);
        assert_eq!(detect(&e), vec![COSMIC]);
    }

    #[test]
    fn detect_x11_sessions() {
        for desktop in ["GNOME", "KDE", "XFCE", "i3"] {
            let e = env(&[("XDG_CURRENT_DESKTOP", desktop), ("XDG_SESSION_TYPE", "x11"), ("DISPLAY", ":0")]);
            assert_eq!(detect(&e), vec![X11], "{desktop}");
        }
        // Bare startx: only DISPLAY.
        assert_eq!(detect(&env(&[("DISPLAY", ":1")])), vec![X11]);
        // XDG_SESSION_TYPE=x11 wins over a stray WAYLAND_DISPLAY.
        let e = env(&[("XDG_SESSION_TYPE", "x11"), ("DISPLAY", ":0"), ("WAYLAND_DISPLAY", "wayland-0")]);
        assert_eq!(detect(&e), vec![X11]);
    }

    #[test]
    fn detect_wlroots_and_headless() {
        for desktop in ["sway", "Hyprland", "river", "labwc:wlroots"] {
            let e = env(&[
                ("XDG_CURRENT_DESKTOP", desktop),
                ("XDG_SESSION_TYPE", "wayland"),
                ("WAYLAND_DISPLAY", "wayland-1"),
            ]);
            assert!(detect(&e).is_empty(), "{desktop}");
        }
        assert!(detect(&env(&[("XDG_SESSION_TYPE", "tty")])).is_empty());
        assert!(detect(&env(&[])).is_empty());
        // Empty values count as unset.
        assert!(detect(&env(&[("DISPLAY", ""), ("WAYLAND_DISPLAY", " ")])).is_empty());
    }

    #[test]
    fn create_matches_features() {
        for name in available() {
            let a = create(name).expect("compiled-in adapter");
            assert_eq!(a.info().name, name);
        }
        assert!(create("gnome").is_none());
    }

    #[test]
    fn debouncer_suppresses_consecutive_duplicates() {
        let mut d = Debouncer::new();
        assert!(d.accept((Some(1), "a")));
        assert!(!d.accept((Some(1), "a")));
        assert!(d.accept((Some(2), "b")));
        assert!(d.accept((Some(1), "a")), "A after B is a change");
        assert_eq!(d.last(), Some(&(Some(1), "a")));
        d.reset();
        assert!(d.last().is_none());
        assert!(d.accept((Some(1), "a")), "accepted again after reset");
    }

    #[test]
    fn throttle_allows_one_per_interval() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut t = Throttle::new(INTERACTION_THROTTLE);
        assert!(t.ready(t0));
        assert!(!t.ready(t0 + ms(1)));
        assert!(!t.ready(t0 + ms(249)));
        assert!(t.ready(t0 + ms(250)));
        assert!(!t.ready(t0 + ms(400)));
        assert!(t.ready(t0 + ms(500)));
        // A clock that goes backwards never panics and doesn't fire.
        assert!(!t.ready(t0));
        t.reset();
        assert!(t.ready(t0));
        assert_eq!(t.interval(), INTERACTION_THROTTLE);
    }

    #[test]
    fn throttle_bounds_rate_under_a_burst() {
        let t0 = Instant::now();
        let mut t = Throttle::new(INTERACTION_THROTTLE);
        // 1 event per ms for 1 s → 4 accepted (0, 250, 500, 750).
        let n = (0..1000).filter(|i| t.ready(t0 + Duration::from_millis(*i))).count();
        assert_eq!(n, 4);
    }
}
