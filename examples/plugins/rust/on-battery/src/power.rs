//! Power source detection from `<sysfs>/class/power_supply`.
//!
//! * AC adapters (`type` = `Mains`, `USB`, `Wireless`) report `online` = `1` when plugged in.
//! * System batteries (`type` = `Battery`) report `status` = `Discharging` when in use.
//!   Peripheral batteries (mice, headsets) have `scope` = `Device` and are ignored.
//!
//! The machine is "on battery" when no AC adapter is online and a system battery is
//! discharging. A desktop without batteries is never on battery.

use std::fs;
use std::path::Path;

/// Where the power comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Power {
    /// An adapter is online, or a battery is charging/full.
    Ac,
    /// Running on a system battery.
    Battery {
        /// e.g. `BAT0`.
        name: String,
        /// Percent, when the battery reports it.
        capacity: Option<u8>,
    },
    /// No AC adapter and no system battery (desktops, VMs, containers).
    NoBattery,
}

impl Power {
    pub fn on_battery(&self) -> bool {
        matches!(self, Power::Battery { .. })
    }

    /// `BAT0 64%`, `AC`, `no battery`.
    pub fn describe(&self) -> String {
        match self {
            Power::Ac => "AC".into(),
            Power::NoBattery => "no battery".into(),
            Power::Battery { name, capacity: Some(c) } => format!("{name} {c}%"),
            Power::Battery { name, capacity: None } => name.clone(),
        }
    }
}

fn attr(dir: &Path, name: &str) -> Option<String> {
    fs::read_to_string(dir.join(name)).ok().map(|s| s.trim().to_string())
}

/// Read the power state below `sysfs_root` (normally `/sys`). Never fails: unreadable
/// entries are skipped.
pub fn read(sysfs_root: &Path) -> Power {
    let Ok(entries) = fs::read_dir(sysfs_root.join("class/power_supply")) else {
        return Power::NoBattery;
    };
    let mut dirs: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    dirs.sort();

    let mut ac_online = false;
    let mut batteries = 0;
    let mut discharging = None;
    for dir in dirs {
        let kind = attr(&dir, "type").unwrap_or_default();
        match kind.as_str() {
            "Mains" | "USB" | "Wireless" => ac_online |= attr(&dir, "online").as_deref() == Some("1"),
            "Battery" => {
                // Peripheral batteries (scope = Device) say nothing about the machine.
                if attr(&dir, "scope").as_deref() == Some("Device") || attr(&dir, "present").as_deref() == Some("0") {
                    continue;
                }
                batteries += 1;
                if discharging.is_none() && attr(&dir, "status").as_deref() == Some("Discharging") {
                    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let capacity = attr(&dir, "capacity").and_then(|c| c.parse().ok());
                    discharging = Some(Power::Battery { name, capacity });
                }
            }
            _ => {}
        }
    }
    match discharging {
        Some(b) if !ac_online => b,
        _ if batteries == 0 && !ac_online => Power::NoBattery,
        _ => Power::Ac,
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    use std::path::Path;

    /// Write a `class/power_supply/<name>` entry with the given attributes.
    pub fn supply(root: &Path, name: &str, attrs: &[(&str, &str)]) {
        let dir = root.join("class/power_supply").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for (k, v) in attrs {
            std::fs::write(dir.join(k), format!("{v}\n")).unwrap();
        }
    }

    /// A laptop with an AC adapter and BAT0.
    pub fn laptop(root: &Path, plugged_in: bool) {
        supply(root, "AC", &[("type", "Mains"), ("online", if plugged_in { "1" } else { "0" })]);
        let status = if plugged_in { "Charging" } else { "Discharging" };
        supply(root, "BAT0", &[("type", "Battery"), ("status", status), ("capacity", "64"), ("scope", "System")]);
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;

    #[test]
    fn laptop_unplugged_is_on_battery() {
        let tmp = tempfile::tempdir().unwrap();
        laptop(tmp.path(), false);
        assert_eq!(read(tmp.path()), Power::Battery { name: "BAT0".into(), capacity: Some(64) });
        laptop(tmp.path(), true);
        assert_eq!(read(tmp.path()), Power::Ac);
    }

    #[test]
    fn desktops_and_peripheral_batteries_are_not_on_battery() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read(tmp.path()), Power::NoBattery, "no power_supply directory at all");
        // A wireless mouse running on its own battery.
        supply(tmp.path(), "hidpp_battery_0", &[("type", "Battery"), ("status", "Discharging"), ("scope", "Device")]);
        assert_eq!(read(tmp.path()), Power::NoBattery);
    }

    #[test]
    fn usb_c_power_counts_as_ac() {
        let tmp = tempfile::tempdir().unwrap();
        laptop(tmp.path(), false);
        supply(tmp.path(), "ucsi-source-psy-USBC000:001", &[("type", "USB"), ("online", "1")]);
        assert_eq!(read(tmp.path()), Power::Ac);
    }

    #[test]
    fn checked_in_fixture_is_on_battery() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/laptop-on-battery");
        assert_eq!(read(&root).describe(), "BAT0 64%");
    }
}
