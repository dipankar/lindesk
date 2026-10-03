//! The iocost controller (`io.cost.qos` at the cgroup root).
//!
//! With the `none` scheduler (the default for NVMe), neither `io.weight` nor ioprio has any
//! effect until iocost is enabled on the device. lindeskd enables it with
//! [`IoCost::enable`] when `io.iocost = "auto"` and puts the previous configuration back
//! with [`IoCost::restore`] on exit.
//!
//! A line looks like
//! `259:0 enable=1 ctrl=auto rpct=0.00 rlat=250000 wpct=0.00 wlat=250000 min=1.00 max=10000.00`.
//! The file is empty until a device has been configured.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::util::{check_device, read_opt, write_attr};

/// Name of the QoS file at the cgroup root.
pub const QOS_FILE: &str = "io.cost.qos";

/// One device line of `io.cost.qos`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct QosLine {
    /// `MAJ:MIN`.
    pub device: String,
    pub enabled: bool,
    /// The remaining `key=value` pairs (`ctrl`, `rpct`, `rlat`, ...).
    pub params: BTreeMap<String, String>,
    /// The line as read.
    pub raw: String,
}

impl QosLine {
    /// True when the parameters are kernel-chosen (`ctrl=auto`, the default).
    pub fn is_auto(&self) -> bool {
        self.params.get("ctrl").is_none_or(|c| c == "auto")
    }
}

/// Parse `io.cost.qos`. Malformed lines are skipped.
pub fn parse_qos(s: &str) -> Vec<QosLine> {
    s.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let device = it.next()?;
            check_device(device).ok()?;
            let params: BTreeMap<String, String> =
                it.filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
            Some(QosLine {
                device: device.to_string(),
                enabled: params.get("enable").is_some_and(|v| v == "1"),
                params: params.into_iter().filter(|(k, _)| k != "enable").collect(),
                raw: line.trim().to_string(),
            })
        })
        .collect()
}

/// The line to write to put a device back to `previous` (a line returned by
/// [`IoCost::enable`]); `None` means the device had no configuration, so iocost is
/// disabled again.
///
/// With `ctrl=auto`, only `enable` and `ctrl=auto` are written: echoing the numeric
/// parameters would switch the device to user-controlled QoS.
pub fn restore_command(device: &str, previous: Option<&str>) -> String {
    let Some(prev) = previous.and_then(|p| parse_qos(p).into_iter().find(|l| l.device == device)) else {
        return format!("{device} enable=0");
    };
    let enable = u8::from(prev.enabled);
    if prev.is_auto() {
        return format!("{device} enable={enable} ctrl=auto");
    }
    let params: Vec<String> = prev.params.iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!("{device} enable={enable} {}", params.join(" "))
}

/// iocost configuration of the hierarchy mounted at `cgroup_root`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoCost {
    root: PathBuf,
}

impl IoCost {
    /// iocost for the hierarchy at `cgroup_root` (usually `/sys/fs/cgroup`).
    pub fn new(cgroup_root: impl Into<PathBuf>) -> Self {
        IoCost { root: cgroup_root.into() }
    }

    /// Path of `io.cost.qos`.
    pub fn qos_path(&self) -> PathBuf {
        self.root.join(QOS_FILE)
    }

    /// True if the kernel has iocost (`CONFIG_BLK_CGROUP_IOCOST`): `io.cost.qos` exists.
    pub fn available(&self) -> bool {
        self.qos_path().exists()
    }

    /// All configured devices. Errors with `NotFound` when iocost is unavailable.
    pub fn qos(&self) -> io::Result<Vec<QosLine>> {
        match read_opt(&self.qos_path())? {
            Some(s) => Ok(parse_qos(&s)),
            None => Err(io::Error::new(io::ErrorKind::NotFound, "io.cost.qos not present (no iocost)")),
        }
    }

    /// The configuration line of one device, if it has one.
    pub fn line(&self, device: &str) -> io::Result<Option<QosLine>> {
        check_device(device)?;
        Ok(self.qos()?.into_iter().find(|l| l.device == device))
    }

    /// True if iocost is enabled on the device.
    pub fn is_enabled(&self, device: &str) -> io::Result<bool> {
        Ok(self.line(device)?.is_some_and(|l| l.enabled))
    }

    /// Enable iocost on a whole-disk device (`MAJ:MIN enable=1`) and return its previous
    /// line, to hand to [`IoCost::restore`] later. Does not write if already enabled.
    pub fn enable(&self, device: &str) -> io::Result<Option<String>> {
        let prev = self.line(device)?;
        if !prev.as_ref().is_some_and(|l| l.enabled) {
            write_attr(&self.qos_path(), &format!("{device} enable=1"))?;
        }
        Ok(prev.map(|l| l.raw))
    }

    /// Put a device back to the configuration returned by [`IoCost::enable`].
    pub fn restore(&self, device: &str, previous: Option<&str>) -> io::Result<()> {
        check_device(device)?;
        write_attr(&self.qos_path(), &restore_command(device, previous))
    }

    /// The cgroup root this operates on.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = "259:0 enable=1 ctrl=auto rpct=0.00 rlat=250000 wpct=0.00 wlat=250000 min=1.00 max=10000.00";

    #[test]
    fn parsing() {
        let v = parse_qos(&format!("{LINE}\n8:0 enable=0 ctrl=user rpct=95.00 rlat=5000\njunk\n"));
        assert_eq!(v.len(), 2);
        assert!(v[0].enabled && v[0].is_auto());
        assert_eq!(v[0].params["rlat"], "250000");
        assert_eq!(v[0].raw, LINE);
        assert!(!v[1].enabled && !v[1].is_auto());
        assert!(parse_qos("").is_empty());
    }

    #[test]
    fn restore_commands() {
        assert_eq!(restore_command("259:0", None), "259:0 enable=0");
        assert_eq!(restore_command("259:0", Some(LINE)), "259:0 enable=1 ctrl=auto");
        assert_eq!(restore_command("259:0", Some("259:0 enable=0 ctrl=auto rpct=0.00")), "259:0 enable=0 ctrl=auto");
        assert_eq!(
            restore_command("8:0", Some("8:0 enable=1 ctrl=user rpct=95.00 rlat=5000 wpct=90.00 wlat=10000")),
            "8:0 enable=1 ctrl=user rlat=5000 rpct=95.00 wlat=10000 wpct=90.00"
        );
        // A line for another device doesn't count.
        assert_eq!(restore_command("8:0", Some(LINE)), "8:0 enable=0");
    }

    #[test]
    fn enable_and_restore_on_fixture() {
        let d = tempfile::tempdir().unwrap();
        let ioc = IoCost::new(d.path());
        assert!(!ioc.available());
        assert!(ioc.enable("259:0").is_err());
        std::fs::write(ioc.qos_path(), "").unwrap();
        assert!(ioc.available());
        assert!(!ioc.is_enabled("259:0").unwrap());
        assert_eq!(ioc.enable("259:0").unwrap(), None);
        assert_eq!(std::fs::read_to_string(ioc.qos_path()).unwrap(), "259:0 enable=1");
        assert!(ioc.is_enabled("259:0").unwrap());
        ioc.restore("259:0", None).unwrap();
        assert!(!ioc.is_enabled("259:0").unwrap());

        std::fs::write(ioc.qos_path(), format!("{LINE}\n")).unwrap();
        assert_eq!(ioc.enable("259:0").unwrap().as_deref(), Some(LINE));
        assert_eq!(std::fs::read_to_string(ioc.qos_path()).unwrap(), format!("{LINE}\n"), "no rewrite");
        assert!(ioc.enable("sda").is_err());
        assert!(ioc.restore("sda", None).is_err());
    }
}
