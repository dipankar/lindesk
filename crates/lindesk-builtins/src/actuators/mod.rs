//! Built-in actuator plugins: the only code that changes the system.
//!
//! | Plugin | Type | Targets | Mechanism |
//! |---|---|---|---|
//! | `systemd` | [`systemd::SystemdActuator`] | `Target::Unit` of one manager | `SetUnitProperties(runtime=true)` over D-Bus; uclamp through the unit's cgroup |
//! | `cgroup-v2` | [`cgroupv2::CgroupV2Actuator`] | `Target::Cgroup` | cgroup v2 attribute files (hosts without systemd) |
//! | `cgroup-v1` | [`cgroupv1::CgroupV1Actuator`] | `Target::Cgroup` | closest cgroup v1 equivalents |
//! | `process` | [`process::ProcessActuator`] | `Target::Process` | `setpriority`, `ioprio_set`, `sched_setattr` (uclamp) on every thread |
//! | `relay` | [`relay::RelayActuator`] | `Target::Unit` of user managers (broker only) | records into a [`relay::RelayTable`]; the session agent applies |
//!
//! [`for_backend`] picks the set for the backend the probe chose.
//!
//! # Conventions
//!
//! * Knobs an actuator can't express return [`PluginError::Unsupported`], never panic.
//! * `read` returns the value to restore later; `Ok(None)` means "not set / default", and
//!   `reset(.., None)` puts that default back.
//! * A target that no longer exists fails `read`/`apply` with the error from [`gone`]
//!   (recognised by [`is_gone`]), so the daemon can call `Engine::forget_target`. `reset`
//!   on a vanished target succeeds: there is nothing left to restore.

pub mod cgroupv1;
pub mod cgroupv2;
pub mod process;
pub mod relay;
pub mod systemd;
pub mod timer_slack;

use std::fmt;
use std::io;

use lindesk_plugin_api::{Actuator, Knob, PluginError, Target};
use lindesk_sys::{Backend, Roots};
use serde::{Deserialize, Serialize};

pub use cgroupv1::CgroupV1Actuator;
pub use cgroupv2::CgroupV2Actuator;
pub use process::ProcessActuator;
pub use relay::{RelayActuator, RelayTable};
pub use systemd::SystemdActuator;
pub use timer_slack::TimerSlackActuator;

/// Plugin name of [`systemd::SystemdActuator`].
pub const SYSTEMD: &str = systemd::NAME;
/// Plugin name of [`cgroupv2::CgroupV2Actuator`].
pub const CGROUP_V2: &str = cgroupv2::NAME;
/// Plugin name of [`cgroupv1::CgroupV1Actuator`].
pub const CGROUP_V1: &str = cgroupv1::NAME;
/// Plugin name of [`process::ProcessActuator`].
pub const PROCESS: &str = process::NAME;
/// Plugin name of [`relay::RelayActuator`].
pub const RELAY: &str = relay::NAME;
/// Plugin name of [`timer_slack::TimerSlackActuator`].
pub const TIMER_SLACK: &str = timer_slack::NAME;

/// Prefix of the [`PluginError::Failed`] message that marks a vanished target.
pub const GONE: &str = "target gone";

/// Which systemd manager an actuator (or environment) talks to.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Bus {
    /// The system manager on the system bus (`lindeskd`).
    #[default]
    System,
    /// The calling user's manager on the session bus (`lindesk-session`).
    User,
}

impl fmt::Display for Bus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Bus::System => "system",
            Bus::User => "user",
        })
    }
}

/// The error for a target that no longer exists (unit unloaded, cgroup removed, process
/// exited).
pub fn gone(target: &Target) -> PluginError {
    PluginError::Gone(target.to_string())
}

/// True for errors produced by [`gone`]: the engine should forget the target.
pub fn is_gone(e: &PluginError) -> bool {
    matches!(e, PluginError::Gone(_))
}

/// The actuators for a backend, acting on the real host. `bus` selects the systemd manager
/// (only used by [`Backend::Systemd`]). Every set includes the per-process actuator, so
/// `Target::Process` intents always have a home.
pub fn for_backend(backend: Backend, bus: Bus) -> Vec<Box<dyn Actuator>> {
    for_backend_at(backend, bus, &Roots::host())
}

/// [`for_backend`] with explicit filesystem roots.
pub fn for_backend_at(backend: Backend, bus: Bus, roots: &Roots) -> Vec<Box<dyn Actuator>> {
    let process: Box<dyn Actuator> = Box::new(ProcessActuator::with_proc_root(&roots.proc));
    match backend {
        Backend::Systemd => {
            vec![Box::new(SystemdActuator::new(bus).with_cgroup_root(&roots.cgroup)), process]
        }
        Backend::CgroupV2Raw => vec![Box::new(CgroupV2Actuator::with_cgroup_root(&roots.cgroup)), process],
        Backend::CgroupV1 => vec![Box::new(CgroupV1Actuator::with_cgroup_root(&roots.cgroup)), process],
        Backend::PerProcess => vec![process],
    }
}

/// Every cgroup knob (`IoMax` listed with an empty device, as [`lindesk_plugin_api::ActuatorCaps`] expects).
pub(crate) fn cgroup_knobs() -> Vec<Knob> {
    vec![
        Knob::CpuWeight,
        Knob::CpuIdle,
        Knob::UclampMin,
        Knob::UclampMax,
        Knob::IoWeight,
        Knob::IoMax(String::new()),
        Knob::MemoryLow,
        Knob::MemoryHigh,
    ]
}

pub(crate) fn unsupported_target(plugin: &str, target: &Target) -> PluginError {
    PluginError::Unsupported(format!("{plugin}: cannot act on {target}"))
}

pub(crate) fn unsupported_knob(plugin: &str, knob: &Knob, target: &Target) -> PluginError {
    PluginError::Unsupported(format!("{plugin}: {knob} is not supported on {target}"))
}

/// Map an I/O error from writing or reading `knob` on `target`.
pub(crate) fn io_error(plugin: &str, knob: &Knob, target: &Target, e: io::Error) -> PluginError {
    match e.kind() {
        io::ErrorKind::Unsupported => PluginError::Unsupported(format!("{plugin}: {e}")),
        io::ErrorKind::NotFound => {
            PluginError::Unsupported(format!("{plugin}: {knob} is not available on {target} ({e})"))
        }
        _ => PluginError::Failed(format!("{plugin}: {knob} on {target}: {e}")),
    }
}

/// A temporary directory for fixture trees, on tmpfs when available.
///
/// `lindesk-sys` writes attribute files with `O_TRUNC`, which on ext4 (`auto_da_alloc`)
/// forces writeback of the freshly written fixture file and can stall for seconds on a
/// busy disk.
#[cfg(test)]
pub(crate) fn test_dir() -> tempfile::TempDir {
    ["/dev/shm", "/run/user"]
        .iter()
        .map(std::path::Path::new)
        .filter(|d| d.is_dir())
        .find_map(|d| {
            let d = if d.ends_with("user") { d.join(rustix::process::getuid().as_raw().to_string()) } else { d.into() };
            tempfile::tempdir_in(d).ok()
        })
        .unwrap_or_else(|| tempfile::tempdir().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backends_pick_actuators() {
        let names = |b| for_backend(b, Bus::User).iter().map(|a| a.info().name).collect::<Vec<_>>();
        assert_eq!(names(Backend::Systemd), ["systemd", "proc"]);
        assert_eq!(names(Backend::CgroupV2Raw), ["cgroup-v2-raw", "proc"]);
        assert_eq!(names(Backend::CgroupV1), ["cgroup-v1", "proc"]);
        assert_eq!(names(Backend::PerProcess), ["proc"]);
    }

    #[test]
    fn gone_errors() {
        let e = gone(&Target::system_unit("x.service"));
        assert!(is_gone(&e));
        assert_eq!(e.to_string(), "target gone: system:x.service");
        assert!(!is_gone(&PluginError::Failed("nope".into())));
        assert!(!is_gone(&PluginError::Unsupported(GONE.into())));
    }

    #[test]
    fn bus_serde() {
        assert_eq!(serde_json::from_str::<Bus>("\"user\"").unwrap(), Bus::User);
        assert_eq!(serde_json::to_string(&Bus::System).unwrap(), "\"system\"");
    }
}
