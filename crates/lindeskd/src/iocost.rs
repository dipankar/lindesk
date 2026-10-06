//! iocost management: enable the iocost controller on managed disks and put the previous
//! `io.cost.qos` lines back on shutdown and `--restore`.
//!
//! Without BFQ (`none`, `mq-deadline`, `kyber`: the defaults for NVMe and most SSDs)
//! `io.weight` does nothing, so with `[io] iocost = "auto"` the broker enables iocost on every
//! managed device that isn't on BFQ; BFQ devices (whose own `io.weight` works) are left
//! alone. `"on"` enables it on every managed device; `"off"` never touches it.
//!
//! Before writing, the previous line is persisted in `<state-dir>/iocost.json`, so a crash
//! between enabling and recording can't lose it. Restoring writes every saved line back and
//! deletes the file; restoring with no file is a no-op.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use lindesk_core::config::Toggle;
use lindesk_sys::blockdev::BlockDev;
use lindesk_sys::iocost::{IoCost, Tuning};
use serde::{Deserialize, Serialize};

/// File name inside the state directory.
pub const FILE: &str = "iocost.json";

/// Saved `io.cost.qos` lines by device (`MAJ:MIN`). `None`: the device had no line.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Saved {
    pub devices: BTreeMap<String, Option<String>>,
    /// Saved `io.cost.model` lines of the devices whose cost model was replaced.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, String>,
}

/// Why a device is (not) enabled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Enable,
    Skip(String),
}

/// What `[io] iocost` decides for one device.
pub fn decide(toggle: Toggle, dev: &BlockDev, already_enabled: bool) -> Decision {
    if already_enabled {
        return Decision::Skip("iocost already enabled".into());
    }
    match toggle {
        Toggle::Off => Decision::Skip("[io] iocost = \"off\"".into()),
        Toggle::On => Decision::Enable,
        // iocost is an rq-qos policy, independent of the elevator: with `none`,
        // `mq-deadline` or `kyber` it is the only thing that makes io.weight effective.
        // BFQ does its own weighting, and running both would double-control the device.
        Toggle::Auto => match dev.scheduler.as_deref() {
            Some("bfq") => Decision::Skip("on BFQ, io.weight already works".into()),
            Some(_) => Decision::Enable,
            None => Decision::Skip("no I/O scheduler information".into()),
        },
    }
}

/// Enables iocost and remembers how to undo it.
#[derive(Debug)]
pub struct IoCostManager {
    ioc: IoCost,
    path: PathBuf,
}

impl IoCostManager {
    pub fn new(cgroup_root: impl Into<PathBuf>, state_dir: &Path) -> Self {
        IoCostManager { ioc: IoCost::new(cgroup_root), path: state_dir.join(FILE) }
    }

    pub fn available(&self) -> bool {
        self.ioc.available()
    }

    pub fn load(&self) -> io::Result<Saved> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => serde_json::from_str(&s).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Saved::default()),
            Err(e) => Err(e),
        }
    }

    fn save(&self, saved: &Saved) -> io::Result<()> {
        if saved.devices.is_empty() && saved.models.is_empty() {
            return match std::fs::remove_file(&self.path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(saved).unwrap_or_default())?;
        std::fs::rename(&tmp, &self.path)
    }

    /// Enable iocost on `devices` according to `toggle`, with latency targets unless
    /// `targets` is off. Returns the devices enabled.
    pub fn enable(&self, toggle: Toggle, targets: Toggle, devices: &[BlockDev]) -> Vec<String> {
        let mut enabled = Vec::new();
        if toggle == Toggle::Off {
            return enabled;
        }
        if !self.available() {
            tracing::info!("iocost: io.cost.qos not present; nothing to enable");
            return enabled;
        }
        let mut saved = match self.load() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("iocost: ignoring unreadable {}: {e}", self.path.display());
                Saved::default()
            }
        };
        for dev in devices {
            let id = dev.dev.to_string();
            let line = match self.ioc.line(&id) {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!("iocost: reading {id}: {e}");
                    continue;
                }
            };
            match decide(toggle, dev, line.as_ref().is_some_and(|l| l.enabled)) {
                Decision::Skip(why) => tracing::info!("iocost: leaving {} ({id}) alone: {why}", dev.name),
                Decision::Enable => {
                    // An entry from an earlier (crashed) run keeps the true original.
                    saved.devices.entry(id.clone()).or_insert_with(|| line.map(|l| l.raw));
                    if let Err(e) = self.save(&saved) {
                        tracing::warn!("iocost: not enabling {id}: cannot record the original: {e}");
                        saved.devices.remove(&id);
                        continue;
                    }
                    let tuning = (targets != Toggle::Off).then(|| Tuning::for_disk(&dev.name, dev.rotational));
                    let done = match &tuning {
                        Some(t) => self.ioc.enable_tuned(&id, t).map(|(_, model)| model),
                        None => self.ioc.enable(&id).map(|_| None),
                    };
                    match done {
                        Ok(model) => {
                            match &tuning {
                                Some(t) => tracing::info!("iocost: enabled on {} ({id}) with {}", dev.name, t.qos),
                                None => tracing::info!("iocost: enabled on {} ({id}), no latency targets", dev.name),
                            }
                            if let Some(model) = model {
                                // As with the QoS line: an entry from a crashed run is the original.
                                saved.models.entry(id.clone()).or_insert(model);
                                if let Err(e) = self.save(&saved) {
                                    tracing::warn!("iocost: cannot record the original cost model of {id}: {e}");
                                }
                            }
                            enabled.push(id);
                        }
                        Err(e) => {
                            tracing::warn!("iocost: enabling on {id} failed: {e}");
                            saved.devices.remove(&id);
                            let _ = self.save(&saved);
                        }
                    }
                }
            }
        }
        enabled
    }

    /// Put every saved line back and forget them. Returns (restored, failed).
    pub fn restore(&self) -> (usize, usize) {
        let mut saved = match self.load() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("iocost: cannot read {}: {e}", self.path.display());
                return (0, 1);
            }
        };
        let (mut ok, mut failed) = (0, 0);
        for (dev, prev) in std::mem::take(&mut saved.devices) {
            match self.ioc.restore(&dev, prev.as_deref()) {
                Ok(()) => {
                    tracing::info!("iocost: restored {dev}");
                    ok += 1;
                }
                Err(e) => {
                    tracing::warn!("iocost: restoring {dev} failed: {e}");
                    saved.devices.insert(dev, prev);
                    failed += 1;
                }
            }
        }
        for (dev, prev) in std::mem::take(&mut saved.models) {
            if let Err(e) = self.ioc.restore_model(&dev, &prev) {
                tracing::warn!("iocost: restoring the cost model of {dev} failed: {e}");
                saved.models.insert(dev, prev);
                failed += 1;
            }
        }
        if let Err(e) = self.save(&saved) {
            tracing::warn!("iocost: updating {}: {e}", self.path.display());
        }
        (ok, failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_sys::blockdev::DevId;

    const LINE: &str = "259:0 enable=0 ctrl=auto rpct=0.00 rlat=250000 wpct=0.00 wlat=250000 min=1.00 max=10000.00";

    fn dev(id: (u32, u32), name: &str, sched: &str) -> BlockDev {
        BlockDev {
            dev: DevId::new(id.0, id.1),
            name: name.into(),
            scheduler: Some(sched.into()),
            schedulers: vec![],
            rotational: Some(false),
        }
    }

    #[test]
    fn decisions() {
        let nvme = dev((259, 0), "nvme0n1", "none");
        let hdd = dev((8, 0), "sda", "bfq");
        let dl = dev((8, 16), "sdb", "mq-deadline");
        assert_eq!(decide(Toggle::Auto, &nvme, false), Decision::Enable);
        assert!(matches!(decide(Toggle::Auto, &hdd, false), Decision::Skip(_)));
        assert_eq!(decide(Toggle::Auto, &dl, false), Decision::Enable, "iocost works under any elevator");
        assert_eq!(decide(Toggle::Auto, &dev((259, 0), "nvme0n1", "kyber"), false), Decision::Enable);
        assert_eq!(decide(Toggle::On, &hdd, false), Decision::Enable);
        assert!(matches!(decide(Toggle::Off, &nvme, false), Decision::Skip(_)));
        assert!(matches!(decide(Toggle::On, &nvme, true), Decision::Skip(_)));
    }

    #[test]
    fn enable_persist_restore_cycle() {
        let cg = crate::common::test_dir();
        let state = crate::common::test_dir();
        let qos = cg.path().join("io.cost.qos");
        std::fs::write(&qos, format!("{LINE}\n")).unwrap();
        let m = IoCostManager::new(cg.path(), state.path());
        let devs = [dev((259, 0), "nvme0n1", "none"), dev((8, 0), "sda", "bfq")];
        // A plain file is not the kernel's: each write replaces it with the command.
        assert_eq!(m.enable(Toggle::Auto, Toggle::Off, &devs), vec!["259:0".to_string()]);
        assert_eq!(std::fs::read_to_string(&qos).unwrap(), "259:0 enable=1");
        let saved = m.load().unwrap();
        assert_eq!(saved.devices.len(), 1);
        assert_eq!(saved.devices["259:0"].as_deref(), Some(LINE));
        let on_disk: Saved = serde_json::from_str(&std::fs::read_to_string(state.path().join(FILE)).unwrap()).unwrap();
        assert_eq!(on_disk, saved);

        // Enabling again (restart after a crash) keeps the original original.
        std::fs::write(&qos, "259:0 enable=0 ctrl=auto\n").unwrap();
        m.enable(Toggle::Auto, Toggle::Off, &devs);
        assert_eq!(m.load().unwrap().devices["259:0"].as_deref(), Some(LINE));

        assert_eq!(m.restore(), (1, 0));
        assert_eq!(std::fs::read_to_string(&qos).unwrap(), "259:0 enable=0 ctrl=auto");
        assert!(!state.path().join(FILE).exists(), "state file removed");
        assert_eq!(m.restore(), (0, 0), "idempotent");
    }

    #[test]
    fn no_iocost_or_off_does_nothing() {
        let cg = crate::common::test_dir();
        let state = crate::common::test_dir();
        let m = IoCostManager::new(cg.path(), state.path());
        let devs = [dev((259, 0), "nvme0n1", "none")];
        assert!(m.enable(Toggle::Auto, Toggle::Off, &devs).is_empty());
        std::fs::write(cg.path().join("io.cost.qos"), "").unwrap();
        assert!(m.enable(Toggle::Off, Toggle::Off, &devs).is_empty());
        assert!(!state.path().join(FILE).exists());
        assert_eq!(m.restore(), (0, 0));
    }

    #[test]
    fn device_without_line_restores_to_disabled() {
        let cg = crate::common::test_dir();
        let state = crate::common::test_dir();
        let qos = cg.path().join("io.cost.qos");
        std::fs::write(&qos, "").unwrap();
        let m = IoCostManager::new(cg.path(), state.path());
        assert_eq!(m.enable(Toggle::On, Toggle::Off, &[dev((8, 0), "sda", "bfq")]), vec!["8:0".to_string()]);
        assert_eq!(m.load().unwrap().devices["8:0"], None);
        assert_eq!(m.restore(), (1, 0));
        assert_eq!(std::fs::read_to_string(&qos).unwrap(), "8:0 enable=0");
    }

    #[test]
    fn latency_targets_and_the_cost_model_are_restored() {
        let cg = crate::common::test_dir();
        let state = crate::common::test_dir();
        let (qos, model) = (cg.path().join("io.cost.qos"), cg.path().join("io.cost.model"));
        std::fs::write(&qos, format!("{LINE}\n")).unwrap();
        let kernel_model = "259:0 ctrl=auto model=linear rbps=3102524156 rseqiops=724816 rrandiops=778122 wbps=1";
        std::fs::write(&model, format!("{kernel_model}\n")).unwrap();
        let m = IoCostManager::new(cg.path(), state.path());
        let devs = [dev((259, 0), "nvme0n1", "kyber")];

        assert_eq!(m.enable(Toggle::Auto, Toggle::Auto, &devs), vec!["259:0".to_string()]);
        let written = std::fs::read_to_string(&qos).unwrap();
        assert!(written.starts_with("259:0 enable=1 ctrl=user rpct=95.00 rlat=10000"), "{written}");
        assert!(std::fs::read_to_string(&model).unwrap().starts_with("259:0 ctrl=user model=linear rbps=488636629"));
        let saved = m.load().unwrap();
        assert_eq!(saved.devices["259:0"].as_deref(), Some(LINE));
        assert_eq!(saved.models["259:0"], kernel_model);

        assert_eq!(m.restore(), (1, 0));
        assert_eq!(std::fs::read_to_string(&qos).unwrap(), "259:0 enable=0 ctrl=auto");
        assert_eq!(std::fs::read_to_string(&model).unwrap(), "259:0 ctrl=auto");
        assert!(!state.path().join(FILE).exists());
    }
}
