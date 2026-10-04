//! Built-in sensor plugins.
//!
//! | Plugin | Type | Produces |
//! |---|---|---|
//! | `psi` | [`psi::PsiSensor`] | PSI samples for the system and for every watched cgroup, plus PSI trigger fds that wake the host early |
//! | `cgroup-stat` | [`cgroup_stat::CgroupStatSensor`] | per-target CPU, I/O and memory usage deltas from `cpu.stat`, `io.stat` and `memory.current` |
//!
//! Both read the files through `lindesk-sys` and take explicit roots, so tests run against
//! fixture trees.

pub mod cgroup_stat;
pub mod psi;

pub use cgroup_stat::CgroupStatSensor;
pub use psi::{ForegroundCgroup, PsiConfig, PsiSensor};

use lindesk_plugin_api::Sensor;

/// Plugin name of [`psi::PsiSensor`].
pub const PSI: &str = psi::NAME;
/// Plugin name of [`cgroup_stat::CgroupStatSensor`].
pub const CGROUP_STAT: &str = cgroup_stat::NAME;

/// Instantiates a built-in sensor by plugin name, reading the real host. `None` for unknown
/// names.
pub fn create(name: &str) -> Option<Box<dyn Sensor>> {
    match name {
        PSI => Some(Box::new(PsiSensor::new())),
        CGROUP_STAT => Some(Box::new(CgroupStatSensor::new())),
        _ => None,
    }
}
