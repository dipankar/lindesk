//! Per-process controls: I/O priority, niceness and uclamp via `sched_setattr`.
//!
//! All of these act on a single thread id on Linux. To cover a whole process, apply to
//! every tid from [`crate::procfs::threads`]. Realtime classes are not representable:
//! [`IoPrioClass`] has no RT variant and the uclamp call keeps the scheduling policy
//! untouched (`SCHED_FLAG_KEEP_ALL`).

use std::io;

use lindesk_plugin_api::IoPrioClass;
use serde::{Deserialize, Serialize};

use crate::util::invalid_input;

/// `IOPRIO_WHO_PROCESS`.
const IOPRIO_WHO_PROCESS: libc::c_int = 1;
const IOPRIO_CLASS_SHIFT: u32 = 13;
const IOPRIO_PRIO_MASK: i32 = (1 << IOPRIO_CLASS_SHIFT) - 1;
const IOPRIO_CLASS_NONE: i32 = 0;
const IOPRIO_CLASS_RT: i32 = 1;
const IOPRIO_CLASS_BE: i32 = 2;
const IOPRIO_CLASS_IDLE: i32 = 3;

/// Keep the current scheduling policy.
pub const SCHED_FLAG_KEEP_POLICY: u64 = 0x08;
/// Keep the current scheduling parameters (nice, priority, deadline).
pub const SCHED_FLAG_KEEP_PARAMS: u64 = 0x10;
/// `KEEP_POLICY | KEEP_PARAMS`.
pub const SCHED_FLAG_KEEP_ALL: u64 = SCHED_FLAG_KEEP_POLICY | SCHED_FLAG_KEEP_PARAMS;
/// `sched_util_min` is valid.
pub const SCHED_FLAG_UTIL_CLAMP_MIN: u64 = 0x20;
/// `sched_util_max` is valid.
pub const SCHED_FLAG_UTIL_CLAMP_MAX: u64 = 0x40;
/// Largest utilisation value (100%).
pub const SCHED_CAPACITY_SCALE: u32 = 1024;
/// Passing this as a clamp value resets it to the system default (Linux 5.11+).
pub const UCLAMP_RESET: u32 = u32::MAX;

/// A thread's current I/O priority as reported by the kernel. Unlike [`IoPrioClass`] this
/// can describe realtime, because other software may have set it; lindesk never does.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "class", content = "level")]
pub enum IoPrio {
    /// No class set: best-effort with a level derived from nice.
    None,
    Realtime(u8),
    BestEffort(u8),
    Idle,
}

/// Encode an `ioprio` value. Levels are 0 (highest) ..= 7.
pub fn encode_ioprio(class: IoPrioClass, level: u8) -> io::Result<i32> {
    if level > 7 {
        return Err(invalid_input(format!("ioprio level {level} out of 0..=7")));
    }
    let (c, l) = match class {
        IoPrioClass::BestEffort => (IOPRIO_CLASS_BE, i32::from(level)),
        IoPrioClass::Idle => (IOPRIO_CLASS_IDLE, 0),
    };
    Ok((c << IOPRIO_CLASS_SHIFT) | l)
}

/// Decode a raw `ioprio` value.
pub fn decode_ioprio(raw: i32) -> IoPrio {
    let level = (raw & IOPRIO_PRIO_MASK).clamp(0, 7) as u8;
    match raw >> IOPRIO_CLASS_SHIFT {
        IOPRIO_CLASS_RT => IoPrio::Realtime(level),
        IOPRIO_CLASS_BE => IoPrio::BestEffort(level),
        IOPRIO_CLASS_IDLE => IoPrio::Idle,
        _ => IoPrio::None,
    }
}

fn check_pid(pid: i32) -> io::Result<libc::c_int> {
    if pid < 0 { Err(invalid_input(format!("invalid pid {pid}"))) } else { Ok(pid) }
}

/// `ioprio_get(IOPRIO_WHO_PROCESS, pid)`; pid 0 is the calling thread.
pub fn ioprio_get(pid: i32) -> io::Result<IoPrio> {
    let pid = check_pid(pid)?;
    // SAFETY: ioprio_get takes two integers and has no memory side effects.
    let rc = unsafe { libc::syscall(libc::SYS_ioprio_get, IOPRIO_WHO_PROCESS, pid) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(decode_ioprio(rc as i32))
}

/// `ioprio_set(IOPRIO_WHO_PROCESS, pid, class|level)`. Realtime is impossible by type.
pub fn ioprio_set(pid: i32, class: IoPrioClass, level: u8) -> io::Result<()> {
    let pid = check_pid(pid)?;
    let raw = encode_ioprio(class, level)?;
    ioprio_set_raw(pid, raw)
}

/// Clear the I/O priority class (back to "derived from nice"), the kernel default.
pub fn ioprio_reset(pid: i32) -> io::Result<()> {
    ioprio_set_raw(check_pid(pid)?, IOPRIO_CLASS_NONE << IOPRIO_CLASS_SHIFT)
}

fn ioprio_set_raw(pid: libc::c_int, raw: i32) -> io::Result<()> {
    debug_assert_ne!(raw >> IOPRIO_CLASS_SHIFT, IOPRIO_CLASS_RT, "lindesk never sets realtime I/O");
    // SAFETY: ioprio_set takes three integers and has no memory side effects.
    let rc = unsafe { libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, pid, raw) };
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Niceness of a thread (`getpriority(PRIO_PROCESS, pid)`); pid 0 is the caller.
pub fn get_nice(pid: i32) -> io::Result<i8> {
    let pid = check_pid(pid)?;
    // SAFETY: errno is thread-local; getpriority can legitimately return -1, so errno
    // must be cleared first and checked after.
    unsafe {
        *libc::__errno_location() = 0;
        let v = libc::getpriority(libc::PRIO_PROCESS, pid as libc::id_t);
        if v == -1 && *libc::__errno_location() != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(v as i8)
    }
}

/// Set a thread's niceness (-20..=19). Lowering it needs `CAP_SYS_NICE` or an
/// `RLIMIT_NICE` allowance.
pub fn set_nice(pid: i32, nice: i8) -> io::Result<()> {
    let pid = check_pid(pid)?;
    if !(-20..=19).contains(&nice) {
        return Err(invalid_input(format!("nice {nice} out of -20..=19")));
    }
    // SAFETY: plain integer syscall.
    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, libc::c_int::from(nice)) };
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// `struct sched_attr` (`SCHED_ATTR_SIZE_VER1`, 56 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SchedAttr {
    pub size: u32,
    pub sched_policy: u32,
    pub sched_flags: u64,
    pub sched_nice: i32,
    pub sched_priority: u32,
    pub sched_runtime: u64,
    pub sched_deadline: u64,
    pub sched_period: u64,
    pub sched_util_min: u32,
    pub sched_util_max: u32,
}

/// Size the kernel expects for [`SchedAttr`].
pub const SCHED_ATTR_SIZE: u32 = std::mem::size_of::<SchedAttr>() as u32;

/// Per-thread utilisation clamps, 0..=1024.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Uclamp {
    pub min: u32,
    pub max: u32,
}

/// Convert a percentage (0..=100) to a utilisation value (0..=1024), rounding.
pub fn percent_to_util(percent: f32) -> io::Result<u32> {
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        return Err(invalid_input(format!("uclamp percent {percent} out of 0..=100")));
    }
    Ok((percent / 100.0 * SCHED_CAPACITY_SCALE as f32).round() as u32)
}

/// Convert a utilisation value (0..=1024) to a percentage.
pub fn util_to_percent(util: u32) -> f32 {
    util.min(SCHED_CAPACITY_SCALE) as f32 * 100.0 / SCHED_CAPACITY_SCALE as f32
}

/// Build the `sched_attr` that changes only the given clamps, keeping policy and params.
/// `Some(UCLAMP_RESET)` resets a clamp to the default.
pub fn uclamp_attr(min: Option<u32>, max: Option<u32>) -> io::Result<SchedAttr> {
    for v in [min, max].into_iter().flatten() {
        if v > SCHED_CAPACITY_SCALE && v != UCLAMP_RESET {
            return Err(invalid_input(format!("uclamp value {v} out of 0..=1024")));
        }
    }
    if let (Some(lo), Some(hi)) = (min, max)
        && lo != UCLAMP_RESET
        && hi != UCLAMP_RESET
        && lo > hi
    {
        return Err(invalid_input(format!("uclamp min {lo} > max {hi}")));
    }
    let mut flags = SCHED_FLAG_KEEP_ALL;
    if min.is_some() {
        flags |= SCHED_FLAG_UTIL_CLAMP_MIN;
    }
    if max.is_some() {
        flags |= SCHED_FLAG_UTIL_CLAMP_MAX;
    }
    Ok(SchedAttr {
        size: SCHED_ATTR_SIZE,
        sched_policy: 0,
        sched_flags: flags,
        sched_util_min: min.unwrap_or(0),
        sched_util_max: max.unwrap_or(0),
        ..Default::default()
    })
}

/// `sched_getattr(pid)`; pid 0 is the caller.
pub fn sched_getattr(pid: i32) -> io::Result<SchedAttr> {
    let pid = check_pid(pid)?;
    let mut attr = SchedAttr::default();
    // SAFETY: `attr` is a valid, writable sched_attr of SCHED_ATTR_SIZE bytes.
    let rc = unsafe {
        libc::syscall(libc::SYS_sched_getattr, pid, &mut attr as *mut SchedAttr, SCHED_ATTR_SIZE, 0 as libc::c_uint)
    };
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(attr) }
}

/// `sched_setattr(pid, attr)`. Refuses anything that could change the scheduling policy:
/// lindesk only uses this for uclamp.
pub fn sched_setattr(pid: i32, attr: &SchedAttr) -> io::Result<()> {
    let pid = check_pid(pid)?;
    if attr.sched_flags & SCHED_FLAG_KEEP_ALL != SCHED_FLAG_KEEP_ALL {
        return Err(invalid_input("sched_setattr without SCHED_FLAG_KEEP_ALL is not allowed"));
    }
    // SAFETY: `attr` points to a valid sched_attr that outlives the call.
    let rc = unsafe { libc::syscall(libc::SYS_sched_setattr, pid, attr as *const SchedAttr, 0 as libc::c_uint) };
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// A thread's current uclamp values.
pub fn uclamp_get(pid: i32) -> io::Result<Uclamp> {
    let a = sched_getattr(pid)?;
    Ok(Uclamp { min: a.sched_util_min, max: a.sched_util_max })
}

/// Set a thread's uclamp min and/or max (0..=1024, or [`UCLAMP_RESET`]).
pub fn uclamp_set(pid: i32, min: Option<u32>, max: Option<u32>) -> io::Result<()> {
    sched_setattr(pid, &uclamp_attr(min, max)?)
}

/// Reset both clamps to the system defaults.
pub fn uclamp_reset(pid: i32) -> io::Result<()> {
    uclamp_set(pid, Some(UCLAMP_RESET), Some(UCLAMP_RESET))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioprio_encoding() {
        assert_eq!(encode_ioprio(IoPrioClass::BestEffort, 0).unwrap(), 2 << 13);
        assert_eq!(encode_ioprio(IoPrioClass::BestEffort, 7).unwrap(), (2 << 13) | 7);
        assert_eq!(encode_ioprio(IoPrioClass::Idle, 5).unwrap(), 3 << 13);
        assert!(encode_ioprio(IoPrioClass::BestEffort, 8).is_err());
        assert_eq!(decode_ioprio(0), IoPrio::None);
        assert_eq!(decode_ioprio((1 << 13) | 3), IoPrio::Realtime(3));
        assert_eq!(decode_ioprio((2 << 13) | 4), IoPrio::BestEffort(4));
        assert_eq!(decode_ioprio(3 << 13), IoPrio::Idle);
        // No input to encode_ioprio can produce the realtime class.
        for class in [IoPrioClass::BestEffort, IoPrioClass::Idle] {
            for level in 0..=7 {
                let raw = encode_ioprio(class, level).unwrap();
                assert!(!matches!(decode_ioprio(raw), IoPrio::Realtime(_)));
            }
        }
    }

    #[test]
    fn sched_attr_layout() {
        assert_eq!(SCHED_ATTR_SIZE, 56);
        assert_eq!(std::mem::offset_of!(SchedAttr, sched_util_min), 48);
        assert_eq!(SCHED_FLAG_KEEP_ALL, 0x18);
    }

    #[test]
    fn uclamp_attr_encoding() {
        let a = uclamp_attr(Some(512), None).unwrap();
        assert_eq!(a.size, 56);
        assert_eq!(a.sched_policy, 0);
        assert_eq!(a.sched_flags, SCHED_FLAG_KEEP_ALL | SCHED_FLAG_UTIL_CLAMP_MIN);
        assert_eq!(a.sched_util_min, 512);
        let a = uclamp_attr(Some(0), Some(1024)).unwrap();
        assert_eq!(a.sched_flags, 0x18 | 0x20 | 0x40);
        assert_eq!((a.sched_util_min, a.sched_util_max), (0, 1024));
        let r = uclamp_attr(Some(UCLAMP_RESET), Some(UCLAMP_RESET)).unwrap();
        assert_eq!(r.sched_util_min, u32::MAX);
        assert!(uclamp_attr(Some(1025), None).is_err());
        assert!(uclamp_attr(Some(600), Some(500)).is_err());
        assert!(sched_setattr(0, &SchedAttr { size: 56, ..Default::default() }).is_err());
    }

    #[test]
    fn percent_conversion() {
        assert_eq!(percent_to_util(50.0).unwrap(), 512);
        assert_eq!(percent_to_util(100.0).unwrap(), 1024);
        assert_eq!(percent_to_util(0.0).unwrap(), 0);
        assert!(percent_to_util(100.5).is_err());
        assert!(percent_to_util(f32::NAN).is_err());
        assert_eq!(util_to_percent(512), 50.0);
        assert_eq!(util_to_percent(2048), 100.0);
    }

    #[test]
    fn argument_validation() {
        assert!(set_nice(0, 20).is_err());
        assert!(set_nice(0, -21).is_err());
        assert!(get_nice(-1).is_err());
        assert!(ioprio_set(-5, IoPrioClass::Idle, 0).is_err());
    }

    /// Read-only calls against this test process.
    #[test]
    fn real_get_on_self() {
        let me = std::process::id() as i32;
        let n = get_nice(me).unwrap();
        assert!((-20..=19).contains(&n));
        let _ = ioprio_get(me).unwrap();
        let a = sched_getattr(me).unwrap();
        assert!(a.size >= 48 && a.size <= SCHED_ATTR_SIZE);
        let u = uclamp_get(me).unwrap();
        assert!(u.min <= u.max && u.max <= SCHED_CAPACITY_SCALE);
    }
}
