//! Other daemons that re-prioritise processes, and which of them conflict with lindesk.
//!
//! The probe (`lindesk_sys::probe`) detects them, `lindeskctl doctor` reports them, and the
//! versioned change system (`lindesk-changes`, op `service_state`) may enable or disable only
//! the units listed here, so `lindeskctl setup conflicts --disable` is always recorded and
//! revertible. docs/src/guide/conflicts.md explains each one.

/// Known priority daemons: (name, systemd units, process names).
pub const PRIORITY_DAEMONS: &[(&str, &[&str], &[&str])] = &[
    ("system76-scheduler", &["com.system76.Scheduler.service", "system76-scheduler.service"], &["system76-scheduler"]),
    ("ananicy-cpp", &["ananicy-cpp.service"], &["ananicy-cpp"]),
    ("ananicy", &["ananicy.service"], &["ananicy"]),
    ("uresourced", &["uresourced.service"], &["uresourced"]),
    ("gamemode", &["gamemoded.service"], &["gamemoded"]),
];

/// Daemons that coexist with lindesk: temporary, per-game changes that don't fight it.
pub const COMPATIBLE: &[&str] = &["gamemode"];

/// True for a daemon that fights lindesk over the same knobs.
pub fn is_conflict(name: &str) -> bool {
    PRIORITY_DAEMONS.iter().any(|(n, ..)| *n == name) && !COMPATIBLE.contains(&name)
}

/// Every systemd unit of a conflicting daemon (the `service_state` allow-list).
pub fn conflicting_units() -> Vec<&'static str> {
    PRIORITY_DAEMONS.iter().filter(|(n, ..)| is_conflict(n)).flat_map(|(_, units, _)| units.iter().copied()).collect()
}

/// The daemon a unit belongs to.
pub fn daemon_of_unit(unit: &str) -> Option<&'static str> {
    PRIORITY_DAEMONS.iter().find(|(_, units, _)| units.contains(&unit)).map(|(n, ..)| *n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamemode_is_not_a_conflict() {
        assert!(is_conflict("system76-scheduler") && is_conflict("ananicy-cpp") && is_conflict("uresourced"));
        assert!(!is_conflict("gamemode") && !is_conflict("nope"));
        let units = conflicting_units();
        assert!(units.contains(&"com.system76.Scheduler.service") && units.contains(&"ananicy.service"));
        assert!(!units.contains(&"gamemoded.service"));
        assert_eq!(daemon_of_unit("uresourced.service"), Some("uresourced"));
    }
}
