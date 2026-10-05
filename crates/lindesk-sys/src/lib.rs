//! Linux system access for lindesk.
//!
//! Everything that reads or writes kernel interfaces lives here: PSI ([`psi`]), cgroup v2 and
//! v1 attribute files ([`cgroupfs`]), `/proc` ([`procfs`]), block devices ([`blockdev`]),
//! iocost ([`iocost`]), raw syscalls such as `ioprio_set` and `sched_setattr`
//! ([`syscalls`]), the capability probe ([`probe`]) and its human-readable report
//! ([`doctor`]), the hardware profile ([`hardware`]), the start-up/install requirement
//! checks ([`requirements`]), swap analysis ([`swap`]) and the latency calibration
//! ([`calibrate`]).
//!
//! No function reads global state implicitly. Filesystem access goes through explicit roots
//! ([`Roots`]) and desktop environment variables through an explicit [`Env`], so every
//! module can be tested against fixture trees without root. With the `fixtures` feature,
//! [`fixture`] exposes the builder used by this crate's own tests.

pub mod blockdev;
pub mod calibrate;
pub mod cgroupfs;
pub mod doctor;
#[cfg(any(test, feature = "fixtures"))]
pub mod fixture;
pub mod hardware;
pub mod iocost;
pub mod probe;
pub mod procfs;
pub mod psi;
pub mod requirements;
pub mod roots;
pub mod swap;
pub mod syscalls;
pub mod telemetry;

mod util;

pub use probe::{Backend, Probe, SupportLevel, probe};
pub use roots::{Env, Roots};
