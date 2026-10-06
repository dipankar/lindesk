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
//!
//! # Enabled is not the same as in control
//!
//! Left at the kernel's parameters (`rpct=0.00`, `wpct=0.00`) iocost has no latency target.
//! It then never finds the device saturated, lets its virtual rate run to the maximum
//! (`cost.vrate=10000.00` in the root `io.stat`), and hands every cgroup all the budget it
//! asks for: `io.weight` decides nothing while requests pile up in the device queue for
//! seconds. A [`Tuning`] gives it targets: when the 95th-percentile completion latency
//! exceeds them, the kernel slows issuance down until the queue is short again, and what
//! the device can do within the target is shared by weight.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::util::{check_device, read_opt, write_attr};

/// Name of the QoS file at the cgroup root.
pub const QOS_FILE: &str = "io.cost.qos";
/// Name of the cost-model file at the cgroup root.
pub const MODEL_FILE: &str = "io.cost.model";

/// The kernel's default cost model for a solid-state disk.
const SSD_MODEL: &str =
    "ctrl=user model=linear rbps=488636629 rseqiops=8932 rrandiops=8518 wbps=427891549 wseqiops=28755 wrandiops=21940";

/// Latency targets (and, for solid-state disks, a cost model) that put iocost in control of
/// a device. See the [module docs](self).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tuning {
    /// `io.cost.qos` parameters after `enable=1`.
    pub qos: String,
    /// `io.cost.model` parameters, or `None` to keep the kernel's choice.
    pub model: Option<String>,
}

impl Tuning {
    /// Targets for a kind of disk.
    ///
    /// The virtual rate may move across the kernel's whole range (1 % to 10000 % of the
    /// model): a consumer SSD that is hot, nearly full or out of write cache delivers a
    /// hundredth of its rated speed, and the same disk an hour later all of it. The targets
    /// find the level; the model only has to be a fixed yardstick, so solid-state disks get
    /// the kernel's own default SSD coefficients rather than whatever it guessed from the
    /// first seconds of I/O (its guess for a fast NVMe puts the 1 % floor above what a
    /// struggling one can do).
    pub fn for_disk(name: &str, rotational: Option<bool>) -> Tuning {
        let qos = |rlat_us: u32, wlat_us: u32, min: u32| {
            format!("ctrl=user rpct=95.00 rlat={rlat_us} wpct=95.00 wlat={wlat_us} min={min}.00 max=10000.00")
        };
        if rotational == Some(true) {
            // A seek is ~10 ms; the kernel's model for rotational disks is close enough.
            Tuning { qos: qos(150_000, 300_000, 10), model: None }
        } else if name.starts_with("nvme") {
            Tuning { qos: qos(10_000, 40_000, 1), model: Some(SSD_MODEL.into()) }
        } else {
            Tuning { qos: qos(20_000, 60_000, 1), model: Some(SSD_MODEL.into()) }
        }
    }
}

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

/// The line to write to put a device's cost model back to `previous`. A kernel-chosen model
/// (`ctrl=auto`) is restored as just that: echoing its coefficients would pin them.
pub fn restore_model_command(device: &str, previous: &str) -> String {
    let params: Vec<&str> = previous.split_whitespace().skip(1).collect();
    if params.is_empty() || params.contains(&"ctrl=auto") {
        return format!("{device} ctrl=auto");
    }
    format!("{device} {}", params.join(" "))
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

    /// Like [`IoCost::enable`], with latency targets. Returns the previous QoS line and the
    /// previous model line (`None` in the second place when the model is left alone).
    pub fn enable_tuned(&self, device: &str, tuning: &Tuning) -> io::Result<(Option<String>, Option<String>)> {
        let prev = self.line(device)?;
        if prev.as_ref().is_some_and(|l| l.enabled) {
            return Ok((prev.map(|l| l.raw), None));
        }
        let mut prev_model = None;
        if let Some(model) = &tuning.model {
            // The kernel lists a device here once iocost has seen it; before that it has no
            // line, and "auto" is what to go back to.
            prev_model = Some(self.model_line(device)?.unwrap_or_else(|| format!("{device} ctrl=auto")));
            write_attr(&self.model_path(), &format!("{device} {model}"))?;
        }
        write_attr(&self.qos_path(), &format!("{device} enable=1 {}", tuning.qos))?;
        Ok((prev.map(|l| l.raw), prev_model))
    }

    /// Path of `io.cost.model`.
    pub fn model_path(&self) -> PathBuf {
        self.root.join(MODEL_FILE)
    }

    /// The device's line in `io.cost.model`, if it has one.
    pub fn model_line(&self, device: &str) -> io::Result<Option<String>> {
        check_device(device)?;
        Ok(read_opt(&self.model_path())?.and_then(|s| {
            s.lines().find(|l| l.split_whitespace().next() == Some(device)).map(|l| l.trim().to_string())
        }))
    }

    /// Put a device's cost model back to a line returned by [`IoCost::enable_tuned`].
    pub fn restore_model(&self, device: &str, previous: &str) -> io::Result<()> {
        check_device(device)?;
        write_attr(&self.model_path(), &restore_model_command(device, previous))
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

    #[test]
    fn tuning_puts_iocost_in_control() {
        let dir = tempfile::tempdir().unwrap();
        let ioc = IoCost::new(dir.path());
        std::fs::write(ioc.qos_path(), "").unwrap();
        std::fs::write(ioc.model_path(), "").unwrap();

        let nvme = Tuning::for_disk("nvme0n1", Some(false));
        assert!(nvme.qos.contains("rpct=95.00 rlat=10000 wpct=95.00 wlat=40000"), "{}", nvme.qos);
        assert!(nvme.model.as_deref().unwrap().starts_with("ctrl=user model=linear rbps="));
        let (prev_qos, prev_model) = ioc.enable_tuned("259:0", &nvme).unwrap();
        assert_eq!((prev_qos, prev_model.as_deref()), (None, Some("259:0 ctrl=auto")));
        let written = std::fs::read_to_string(ioc.qos_path()).unwrap();
        assert!(written.starts_with("259:0 enable=1 ctrl=user rpct=95.00"), "{written}");
        assert!(std::fs::read_to_string(ioc.model_path()).unwrap().starts_with("259:0 ctrl=user model=linear"));

        // Already enabled (by the admin, or by us before a crash): nothing is rewritten.
        std::fs::write(ioc.model_path(), "untouched").unwrap();
        let (prev, model) = ioc.enable_tuned("259:0", &nvme).unwrap();
        assert!(prev.unwrap().contains("enable=1") && model.is_none());
        assert_eq!(std::fs::read_to_string(ioc.model_path()).unwrap(), "untouched");

        // Spinning disks keep the kernel's model and get looser targets; SATA SSDs sit between.
        let hdd = Tuning::for_disk("sda", Some(true));
        assert!(hdd.model.is_none() && hdd.qos.contains("rlat=150000") && hdd.qos.contains("min=10.00"));
        assert!(Tuning::for_disk("sda", Some(false)).qos.contains("rlat=20000"));
    }

    #[test]
    fn model_restore_commands() {
        let auto = "259:0 ctrl=auto model=linear rbps=3102524156 rseqiops=724816 rrandiops=778122 wbps=1 wseqiops=2";
        assert_eq!(restore_model_command("259:0", auto), "259:0 ctrl=auto");
        assert_eq!(restore_model_command("259:0", "259:0 ctrl=auto"), "259:0 ctrl=auto");
        assert_eq!(
            restore_model_command("259:0", "259:0 ctrl=user model=linear rbps=5 rseqiops=6"),
            "259:0 ctrl=user model=linear rbps=5 rseqiops=6"
        );
    }
}
