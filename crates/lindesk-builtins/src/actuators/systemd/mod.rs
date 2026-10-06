//! The `systemd` actuator: unit properties through the systemd manager over D-Bus.
//!
//! Targets are `Target::Unit`s of one manager, chosen by the config `{"bus": "system"}`
//! (lindeskd) or `{"bus": "user"}` (lindesk-session). A user actuator only accepts
//! `Manager::User(<own uid>)`. Changes go through
//! `org.freedesktop.systemd1.Manager.SetUnitProperties(name, runtime = true, a(sv))` on one
//! reusable connection ([`SystemdClient`]); each write is a single round trip (about 1 ms).
//!
//! | Knob | systemd property | Notes |
//! |---|---|---|
//! | `CpuWeight` | `CPUWeight` (`t`) | |
//! | `CpuIdle` | `CPUWeight = 0` | systemd's `idle` weight; see below |
//! | `IoWeight` | `IOWeight` (`t`) | |
//! | `IoMax(dev)` | `IO{Read,Write}{Bandwidth,IOPS}Max` (`a(st)`) | one entry per device, `/dev/block/MAJ:MIN` |
//! | `MemoryLow` | `MemoryLow` (`t`) | |
//! | `MemoryHigh` | `MemoryHigh` (`t`) | `None` = `infinity` = `u64::MAX` |
//! | `UclampMin/Max` | none | written to `cpu.uclamp.{min,max}` in the unit's `ControlGroup` |
//! | `Nice`, `IoPrio` | none | `Unsupported` (per-process knobs) |
//!
//! # What systemd 255 does on the wire (verified on the development host)
//!
//! * **Unset** weights (`CPUWeight`, `IOWeight`, `StartupCPUWeight`, ...) read as
//!   `u64::MAX` (`CGROUP_WEIGHT_INVALID`); writing `u64::MAX` unsets them again, exactly
//!   like `systemctl set-property --runtime X CPUWeight=` (the drop-in then holds an empty
//!   `CPUWeight=` assignment, which resets to the default rather than leaving a stale value).
//! * **`CPUWeight=idle`** is `0` on the bus (`CGROUP_WEIGHT_IDLE`). The cgroup then gets
//!   `cpu.weight = 1` and `cpu.idle = 1`; any later non-idle weight writes `cpu.idle = 0`
//!   again. So `CpuIdle` maps to `CPUWeight = 0`, and the actuator composes the two knobs:
//!   while `CpuIdle` is on the unit stays idle and an applied `CpuWeight` takes effect
//!   when idle is released.
//! * With no CPU property set, systemd does **not enable the cpu controller** for the unit,
//!   so `cpu.weight`, `cpu.idle` and `cpu.uclamp.*` don't exist. To apply uclamp the
//!   actuator pins `CPUWeight = 100` (the kernel default, so behaviour is unchanged) while a
//!   clamp is held, and unsets it again afterwards. systemd leaves `cpu.uclamp.*` alone when
//!   it rewrites `cpu.weight`/`cpu.idle`.
//! * **`MemoryHigh`** unset is `u64::MAX` (`infinity`), **`MemoryLow`** unset is `0`.
//! * **I/O limits** are `a(st)` lists of `(device path, value)`. Writing a device with
//!   `u64::MAX` removes that device's limit; when all four limits of a device are infinite
//!   systemd drops the device's `io.max` line and its property entry. Other devices are
//!   untouched. An **empty array clears every device** for that property. The drop-in is
//!   rewritten from the remaining list each time, so no stale lines remain.
//!
//! # Reading and restoring
//!
//! `read` uses the unit's D-Bus properties, because they round-trip exactly with `reset`:
//! an unset weight reads as `Ok(None)` and `reset(None)` writes the unset value back, so the
//! unit returns to its configured default (systemd shows `[not set]` again). Uclamp is read
//! from the cgroup files (`Ok(None)` when the controller is off).
//!
//! A unit that is not loaded fails `read`/`apply` with [`super::gone`]; `reset` on it
//! succeeds.

pub mod client;

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::Duration;

use lindesk_plugin_api::{
    Actuator, ActuatorCaps, InitContext, IoLimit, Knob, Manager, Plugin, PluginError, PluginInfo, PluginKind,
    PluginResult, Setting, Target,
};
use lindesk_sys::Roots;
use lindesk_sys::cgroupfs::{CgroupFs, default_setting, knob_file};
use serde::{Deserialize, Serialize};
use zbus::zvariant::Value;

pub use client::{SystemdClient, SystemdError};

use super::{Bus, cgroup_knobs, gone, is_gone, unsupported_knob, unsupported_target};

/// Plugin name.
pub const NAME: &str = "systemd";

/// systemd's "not set" for weights and `infinity` for limits.
pub const UNSET: u64 = u64::MAX;
/// systemd's `CPUWeight=idle` on the bus.
pub const CPU_WEIGHT_IDLE: u64 = 0;
/// The kernel's default weight, used to keep the cpu controller enabled for uclamp.
const DEFAULT_WEIGHT: u64 = 100;

/// `IoLimit` field order ↔ systemd properties.
const IO_LIMIT_PROPS: [&str; 4] = ["IOReadBandwidthMax", "IOWriteBandwidthMax", "IOReadIOPSMax", "IOWriteIOPSMax"];

/// Configuration of the `systemd` actuator.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SystemdConfig {
    /// Which manager to drive.
    pub bus: Bus,
}

/// lindesk's view of a unit's `CPUWeight`, shared by the `CpuWeight`, `CpuIdle` and uclamp
/// knobs (see the [module docs](self)).
#[derive(Clone, Debug, Default, PartialEq)]
struct CpuState {
    /// Non-idle `CPUWeight` before lindesk touched it (`UNSET` = not set).
    base_weight: Option<u64>,
    /// Whether the unit was `CPUWeight=idle` before lindesk touched it.
    base_idle: Option<bool>,
    /// Applied `CpuIdle`.
    idle: Option<bool>,
    /// Applied `CpuWeight`.
    weight: Option<u64>,
    /// Applied `UclampMin`, `UclampMax`.
    uclamp: [bool; 2],
}

impl CpuState {
    fn base_known(&self) -> bool {
        self.base_weight.is_some() || self.base_idle.is_some()
    }

    fn set_base_from_raw(&mut self, raw: u64) {
        if raw == CPU_WEIGHT_IDLE {
            self.base_idle = Some(true);
        } else {
            self.base_idle = Some(false);
            self.base_weight = Some(raw);
        }
    }

    /// The `CPUWeight` value the unit should have.
    fn effective(&self) -> u64 {
        if self.idle.or(self.base_idle).unwrap_or(false) {
            return CPU_WEIGHT_IDLE;
        }
        if let Some(w) = self.weight {
            return w;
        }
        match self.base_weight {
            Some(w) if w != UNSET => w,
            _ if self.uclamp.iter().any(|u| *u) => DEFAULT_WEIGHT,
            _ => UNSET,
        }
    }

    /// Nothing of lindesk's is applied any more.
    fn released(&self) -> bool {
        self.idle.is_none() && self.weight.is_none() && !self.uclamp.iter().any(|u| *u)
    }
}

/// The built-in `systemd` actuator. See the [module docs](self).
#[derive(Debug)]
pub struct SystemdActuator {
    bus: Bus,
    uid: u32,
    client: SystemdClient,
    fs: CgroupFs,
    /// Unit → `ControlGroup`, for uclamp. Invalidated on errors.
    cgroups: HashMap<String, String>,
    /// (unit, `MAJ:MIN`) → device path systemd already uses for that device.
    io_paths: HashMap<(String, String), String>,
    cpu: HashMap<String, CpuState>,
}

impl SystemdActuator {
    /// An actuator for `bus` acting on the host's cgroup mount. Connects on first use.
    pub fn new(bus: Bus) -> Self {
        let uid = rustix::process::getuid().as_raw();
        SystemdActuator {
            bus,
            uid,
            client: SystemdClient::new(bus, uid),
            fs: CgroupFs::new(Roots::host().cgroup),
            cgroups: HashMap::new(),
            io_paths: HashMap::new(),
            cpu: HashMap::new(),
        }
    }

    /// Use another cgroup mount for the uclamp files.
    pub fn with_cgroup_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.fs = CgroupFs::new(root);
        self
    }

    /// The manager this actuator drives.
    pub fn bus(&self) -> Bus {
        self.bus
    }

    /// Open the D-Bus connection now instead of on the first call.
    pub fn connect(&mut self) -> PluginResult<()> {
        self.client.connect().map(|_| ()).map_err(|e| PluginError::Failed(format!("{NAME}: {e}")))
    }

    /// True if `manager` is the one this actuator drives.
    pub fn manages(&self, manager: &Manager) -> bool {
        match (self.bus, manager) {
            (Bus::System, Manager::System) => true,
            (Bus::User, Manager::User(uid)) => *uid == self.uid,
            _ => false,
        }
    }

    /// The `ControlGroup` of a unit, relative to the cgroup mount (cached).
    pub fn control_group(&mut self, target: &Target) -> PluginResult<String> {
        let (unit, iface) = self.unit(target)?;
        self.cgroup_of(target, unit, iface)
    }

    fn unit<'t>(&self, target: &'t Target) -> PluginResult<(&'t str, &'static str)> {
        match target {
            Target::Unit { manager, name } if self.manages(manager) => {
                let iface = client::unit_interface(name)
                    .ok_or_else(|| PluginError::Unsupported(format!("{NAME}: {name} has no cgroup properties")))?;
                Ok((name, iface))
            }
            _ => Err(unsupported_target(NAME, target)),
        }
    }

    fn sd_error(&mut self, target: &Target, unit: &str, e: SystemdError) -> PluginError {
        self.cgroups.remove(unit);
        match e {
            SystemdError::NoSuchUnit(_) => {
                self.cpu.remove(unit);
                self.io_paths.retain(|(u, _), _| u != unit);
                gone(target)
            }
            e => PluginError::Failed(format!("{NAME} ({} manager): {e}", self.bus)),
        }
    }

    fn get_u64(&mut self, target: &Target, unit: &str, iface: &str, prop: &str) -> PluginResult<u64> {
        self.client.property_u64(unit, iface, prop).map_err(|e| self.sd_error(target, unit, e))
    }

    fn set(&mut self, target: &Target, unit: &str, props: &[(&str, Value<'_>)]) -> PluginResult<()> {
        self.client.set_properties(unit, props).map_err(|e| self.sd_error(target, unit, e))
    }

    fn cgroup_of(&mut self, target: &Target, unit: &str, iface: &str) -> PluginResult<String> {
        if let Some(cg) = self.cgroups.get(unit) {
            return Ok(cg.clone());
        }
        let cg =
            self.client.property_string(unit, iface, "ControlGroup").map_err(|e| self.sd_error(target, unit, e))?;
        if cg.is_empty() {
            // Loaded but not running: no cgroup to write to.
            return Err(gone(target));
        }
        self.cgroups.insert(unit.to_string(), cg.clone());
        Ok(cg)
    }

    // ------------------------------------------------------------------ CPUWeight / idle

    fn ensure_cpu_base(&mut self, target: &Target, unit: &str, iface: &str) -> PluginResult<()> {
        if self.cpu.get(unit).is_some_and(CpuState::base_known) {
            return Ok(());
        }
        let raw = self.get_u64(target, unit, iface, "CPUWeight")?;
        self.cpu.entry(unit.to_string()).or_default().set_base_from_raw(raw);
        Ok(())
    }

    /// Write the composed `CPUWeight` and drop the state once lindesk holds nothing.
    fn write_cpu(&mut self, target: &Target, unit: &str) -> PluginResult<()> {
        let st = self.cpu.get(unit).cloned().unwrap_or_default();
        self.set(target, unit, &[("CPUWeight", Value::U64(st.effective()))])?;
        if st.released() {
            self.cpu.remove(unit);
        }
        Ok(())
    }

    /// Update the CPU state with `f` and write `CPUWeight` if `always` or if the
    /// effective value changed.
    fn update_cpu(
        &mut self,
        target: &Target,
        unit: &str,
        always: bool,
        f: impl FnOnce(&mut CpuState),
    ) -> PluginResult<()> {
        let st = self.cpu.entry(unit.to_string()).or_default();
        let before = st.effective();
        f(st);
        if always || st.effective() != before {
            self.write_cpu(target, unit)
        } else {
            if st.released() {
                self.cpu.remove(unit);
            }
            Ok(())
        }
    }

    // ------------------------------------------------------------------------ uclamp

    fn read_uclamp(&mut self, target: &Target, unit: &str, iface: &str, knob: &Knob) -> PluginResult<Option<Setting>> {
        let cg = self.cgroup_of(target, unit, iface)?;
        self.fs.read_setting(&cg, knob).map_err(|e| PluginError::Failed(format!("{NAME}: {knob} of {target}: {e}")))
    }

    fn write_uclamp(&mut self, target: &Target, unit: &str, iface: &str, setting: &Setting) -> PluginResult<()> {
        let knob = setting.knob();
        let file = knob_file(&knob).expect("uclamp knobs are cgroup attributes");
        let idx = usize::from(knob == Knob::UclampMax);
        let mut cg = self.cgroup_of(target, unit, iface)?;
        // Keep the cpu controller enabled while a clamp is held.
        self.ensure_cpu_base(target, unit, iface)?;
        self.update_cpu(target, unit, false, |st| st.uclamp[idx] = true)?;
        if !self.wait_for_attr(&cg, file) {
            // Maybe the unit was restarted into a new cgroup.
            self.cgroups.remove(unit);
            cg = self.cgroup_of(target, unit, iface)?;
            if !self.wait_for_attr(&cg, file) {
                self.update_cpu(target, unit, false, |st| st.uclamp[idx] = false)?;
                return Err(PluginError::Unsupported(format!(
                    "{NAME}: {file} is not available for {target} (no uclamp support or cpu controller)"
                )));
            }
        }
        self.fs.write_setting(&cg, setting).map_err(|e| {
            self.cgroups.remove(unit);
            PluginError::Failed(format!("{NAME}: writing {file} of {target}: {e}"))
        })
    }

    /// systemd realises cgroup changes from its event loop, so a freshly enabled controller's
    /// files can take a moment to appear.
    fn wait_for_attr(&self, cg: &str, file: &str) -> bool {
        for _ in 0..10 {
            if self.fs.has_attr(cg, file) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        self.fs.has_attr(cg, file)
    }

    fn reset_uclamp(
        &mut self,
        target: &Target,
        unit: &str,
        iface: &str,
        knob: &Knob,
        original: Option<&Setting>,
    ) -> PluginResult<()> {
        let file = knob_file(knob).expect("uclamp knobs are cgroup attributes");
        let cg = self.cgroup_of(target, unit, iface)?;
        if self.fs.has_attr(&cg, file) {
            let setting = original.cloned().or_else(|| default_setting(knob)).expect("cgroup knob");
            match self.fs.write_setting(&cg, &setting) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(PluginError::Failed(format!("{NAME}: restoring {file} of {target}: {e}")));
                }
                _ => {}
            }
        }
        let idx = usize::from(*knob == Knob::UclampMax);
        self.update_cpu(target, unit, false, |st| st.uclamp[idx] = false)
    }

    // ------------------------------------------------------------------------- io.max

    /// The unit's cgroup when its `io.max` can be written directly.
    ///
    /// `io.max` is the one setting lindesk changes at a high rate (the adaptive limit moves
    /// every second on every yielding unit while RED). Through systemd that is a D-Bus round
    /// trip plus a drop-in file per change, on PID 1, exactly when the machine is busiest;
    /// a write to the cgroup file is microseconds and involves nobody else. systemd only
    /// rewrites `io.max` for devices it has limits configured for, so a direct write to a
    /// unit without `IO*Max` properties stays put. The systemd route remains for units whose
    /// `io.max` file doesn't exist yet (the io controller isn't enabled there) and for units
    /// that were already set through it.
    fn io_max_direct(&mut self, target: &Target, unit: &str, iface: &str, dev: &str) -> Option<String> {
        if self.io_paths.contains_key(&(unit.to_string(), dev.to_string())) {
            return None;
        }
        let cg = self.cgroup_of(target, unit, iface).ok()?;
        self.fs.has_attr(&cg, "io.max").then_some(cg)
    }

    /// Current limits for `dev` and the device path systemd already uses for it.
    fn read_io_limit(
        &mut self,
        target: &Target,
        unit: &str,
        iface: &str,
        dev: &str,
    ) -> PluginResult<(Option<String>, IoLimit)> {
        let mut path = None;
        let mut vals = [None; 4];
        for (slot, prop) in vals.iter_mut().zip(IO_LIMIT_PROPS) {
            let list =
                self.client.property_device_values(unit, iface, prop).map_err(|e| self.sd_error(target, unit, e))?;
            for (p, v) in list {
                if device_of(&p).as_deref() == Some(dev) {
                    *slot = (v != UNSET).then_some(v);
                    path.get_or_insert(p);
                }
            }
        }
        let [rbps, wbps, riops, wiops] = vals;
        Ok((path, IoLimit { rbps, wbps, riops, wiops }))
    }

    fn write_io_limit(
        &mut self,
        target: &Target,
        unit: &str,
        iface: &str,
        dev: &str,
        limit: &IoLimit,
    ) -> PluginResult<()> {
        check_device(dev).map_err(|m| PluginError::Failed(format!("{NAME}: {m}")))?;
        let key = (unit.to_string(), dev.to_string());
        let path = match self.io_paths.get(&key) {
            Some(p) => p.clone(),
            None => {
                let (existing, _) = self.read_io_limit(target, unit, iface, dev)?;
                let p = existing.unwrap_or_else(|| format!("/dev/block/{dev}"));
                self.io_paths.insert(key, p.clone());
                p
            }
        };
        let vals = [limit.rbps, limit.wbps, limit.riops, limit.wiops];
        let props: Vec<(&str, Value<'_>)> = IO_LIMIT_PROPS
            .iter()
            .zip(vals)
            .map(|(name, v)| (*name, Value::from(vec![(path.clone(), v.unwrap_or(UNSET))])))
            .collect();
        self.set(target, unit, &props)
    }

    fn write(&mut self, target: &Target, unit: &str, iface: &str, setting: &Setting) -> PluginResult<()> {
        match setting {
            Setting::CpuWeight { value } => {
                self.ensure_cpu_base(target, unit, iface)?;
                self.update_cpu(target, unit, true, |st| st.weight = Some(*value))
            }
            Setting::CpuIdle { value } => {
                self.ensure_cpu_base(target, unit, iface)?;
                self.update_cpu(target, unit, true, |st| st.idle = Some(*value))
            }
            Setting::IoWeight { value } => self.set(target, unit, &[("IOWeight", Value::U64(*value))]),
            Setting::MemoryLow { bytes } => self.set(target, unit, &[("MemoryLow", Value::U64(*bytes))]),
            Setting::MemoryHigh { bytes } => {
                self.set(target, unit, &[("MemoryHigh", Value::U64(bytes.unwrap_or(UNSET)))])
            }
            Setting::IoMax { device, limit } => match self.io_max_direct(target, unit, iface, device) {
                Some(cg) => self.fs.write_setting(&cg, setting).map_err(|e| {
                    self.cgroups.remove(unit);
                    PluginError::Failed(format!("{NAME}: writing io.max of {target}: {e}"))
                }),
                None => self.write_io_limit(target, unit, iface, device, limit),
            },
            Setting::UclampMin { .. } | Setting::UclampMax { .. } => self.write_uclamp(target, unit, iface, setting),
            Setting::Nice { .. } | Setting::IoPrio { .. } => Err(unsupported_knob(NAME, &setting.knob(), target)),
        }
    }
}

/// `MAJ:MIN` of a device path: parsed from `/dev/block/MAJ:MIN`, else from `stat(2)`.
fn device_of(path: &str) -> Option<String> {
    if let Some(d) = path.strip_prefix("/dev/block/")
        && check_device(d).is_ok()
    {
        return Some(d.to_string());
    }
    let rdev = std::fs::metadata(path).ok()?.rdev();
    Some(format!("{}:{}", rustix::fs::major(rdev), rustix::fs::minor(rdev)))
}

fn check_device(dev: &str) -> Result<(), String> {
    match dev.split_once(':') {
        Some((a, b))
            if !a.is_empty()
                && !b.is_empty()
                && a.bytes().all(|c| c.is_ascii_digit())
                && b.bytes().all(|c| c.is_ascii_digit()) =>
        {
            Ok(())
        }
        _ => Err(format!("invalid block device '{dev}' (expected MAJ:MIN)")),
    }
}

impl Plugin for SystemdActuator {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Actuator],
            description: "Unit properties via systemd SetUnitProperties(runtime); uclamp via the unit's cgroup".into(),
        }
    }

    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
        let cfg: SystemdConfig = if ctx.config.is_null() {
            SystemdConfig { bus: self.bus }
        } else {
            serde_json::from_value(ctx.config.clone()).map_err(|e| PluginError::Failed(format!("{NAME}: {e}")))?
        };
        if cfg.bus != self.bus {
            self.bus = cfg.bus;
            self.client = SystemdClient::new(cfg.bus, self.uid);
        }
        self.cgroups.clear();
        self.io_paths.clear();
        self.cpu.clear();
        Ok(())
    }
}

impl Actuator for SystemdActuator {
    fn caps(&self) -> ActuatorCaps {
        ActuatorCaps { knobs: cgroup_knobs(), targets: vec!["unit".into()] }
    }

    fn read(&mut self, target: &Target, knob: &Knob) -> PluginResult<Option<Setting>> {
        let (unit, iface) = self.unit(target)?;
        Ok(match knob {
            Knob::CpuWeight | Knob::CpuIdle => {
                let raw = self.get_u64(target, unit, iface, "CPUWeight")?;
                let st = self.cpu.entry(unit.to_string()).or_default();
                if !st.base_known() {
                    st.set_base_from_raw(raw);
                }
                if *knob == Knob::CpuIdle {
                    Some(Setting::CpuIdle { value: raw == CPU_WEIGHT_IDLE })
                } else {
                    (raw != UNSET && raw != CPU_WEIGHT_IDLE).then_some(Setting::CpuWeight { value: raw })
                }
            }
            Knob::IoWeight => {
                let raw = self.get_u64(target, unit, iface, "IOWeight")?;
                (raw != UNSET).then_some(Setting::IoWeight { value: raw })
            }
            Knob::MemoryLow => Some(Setting::MemoryLow { bytes: self.get_u64(target, unit, iface, "MemoryLow")? }),
            Knob::MemoryHigh => {
                let raw = self.get_u64(target, unit, iface, "MemoryHigh")?;
                Some(Setting::MemoryHigh { bytes: (raw != UNSET).then_some(raw) })
            }
            Knob::IoMax(dev) => {
                check_device(dev).map_err(|m| PluginError::Failed(format!("{NAME}: {m}")))?;
                if let Some(cg) = self.io_max_direct(target, unit, iface, dev) {
                    return self
                        .fs
                        .read_setting(&cg, knob)
                        .map_err(|e| PluginError::Failed(format!("{NAME}: reading io.max of {target}: {e}")));
                }
                let (_, limit) = self.read_io_limit(target, unit, iface, dev)?;
                Some(Setting::IoMax { device: dev.clone(), limit })
            }
            Knob::UclampMin | Knob::UclampMax => self.read_uclamp(target, unit, iface, knob)?,
            Knob::Nice | Knob::IoPrio => return Err(unsupported_knob(NAME, knob, target)),
        })
    }

    fn apply(&mut self, target: &Target, setting: &Setting) -> PluginResult<()> {
        let (unit, iface) = self.unit(target)?;
        self.write(target, unit, iface, setting)
    }

    fn reset(&mut self, target: &Target, knob: &Knob, original: Option<&Setting>) -> PluginResult<()> {
        let (unit, iface) = self.unit(target)?;
        let r = match knob {
            Knob::CpuWeight => {
                let base = match original {
                    Some(Setting::CpuWeight { value }) => *value,
                    _ => UNSET,
                };
                self.update_cpu(target, unit, true, |st| {
                    st.weight = None;
                    st.base_weight = Some(base);
                })
            }
            Knob::CpuIdle => {
                let base = matches!(original, Some(Setting::CpuIdle { value: true }));
                self.update_cpu(target, unit, true, |st| {
                    st.idle = None;
                    st.base_idle = Some(base);
                })
            }
            Knob::UclampMin | Knob::UclampMax => self.reset_uclamp(target, unit, iface, knob, original),
            Knob::Nice | Knob::IoPrio => return Err(unsupported_knob(NAME, knob, target)),
            Knob::IoWeight | Knob::IoMax(_) | Knob::MemoryLow | Knob::MemoryHigh => {
                let setting = match original {
                    Some(s) => s.clone(),
                    None => match knob {
                        // "Not set" for weights; the kernel defaults for the rest.
                        Knob::IoWeight => {
                            return self.set(target, unit, &[("IOWeight", Value::U64(UNSET))]).or_else(ok_if_gone);
                        }
                        _ => default_setting(knob).expect("cgroup knob"),
                    },
                };
                self.write(target, unit, iface, &setting)
            }
        };
        // A unit that is gone has nothing left to restore.
        r.or_else(ok_if_gone)
    }
}

fn ok_if_gone(e: PluginError) -> PluginResult<()> {
    if is_gone(&e) { Ok(()) } else { Err(e) }
}

#[cfg(test)]
mod tests;
