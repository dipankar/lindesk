//! Safety caps: sizes, memory and disk budgets.
//!
//! The bench runs on real workstations, so every load is bounded:
//!
//! * memory pressure is a fraction of `MemAvailable` at start, by default at most 50% and
//!   never more than [`HARD_MAX_MEM_FRAC`] (75%), whatever the user asks for. While it runs,
//!   the load releases memory if `MemAvailable` drops below [`mem_floor`];
//! * disk writes never exceed the write cap, and the cap is shrunk to half of the free
//!   space left after a fixed reserve ([`DISK_RESERVE`]).

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const KIB: u64 = 1024;
pub const MIB: u64 = 1024 * KIB;
pub const GIB: u64 = 1024 * MIB;

/// Memory pressure can never take more than this fraction of `MemAvailable`.
pub const HARD_MAX_MEM_FRAC: f64 = 0.75;
/// Default `--max-mem-frac`.
pub const DEFAULT_MAX_MEM_FRAC: f64 = 0.5;
/// Free space that the bench always leaves on the scratch filesystem.
pub const DISK_RESERVE: u64 = 5 * GIB;

/// Parses `4G`, `512MiB`, `1.5GiB`, `64k`, `1000000`. All units are binary.
pub fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim();
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().with_context(|| format!("bad size '{s}'"))?;
    let mult = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => KIB,
        "m" | "mb" | "mib" => MIB,
        "g" | "gb" | "gib" => GIB,
        "t" | "tb" | "tib" => 1024 * GIB,
        u => bail!("bad size unit '{u}' in '{s}' (use K, M, G or T)"),
    };
    if !(n.is_finite() && n >= 0.0) {
        bail!("bad size '{s}'");
    }
    Ok((n * mult as f64) as u64)
}

/// `4.0 GiB`, `512 MiB`, `12 KiB`, `100 B`.
pub fn fmt_bytes(b: u64) -> String {
    let f = b as f64;
    if b >= GIB {
        format!("{:.1} GiB", f / GIB as f64)
    } else if b >= MIB {
        format!("{:.0} MiB", f / MIB as f64)
    } else if b >= KIB {
        format!("{:.0} KiB", f / KIB as f64)
    } else {
        format!("{b} B")
    }
}

/// How much memory pressure to create.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemSpec {
    /// Absolute size.
    Bytes(u64),
    /// Percentage of `MemAvailable` at start.
    Percent(f64),
}

impl MemSpec {
    pub fn parse(s: &str) -> Result<MemSpec> {
        match s.trim().strip_suffix('%') {
            Some(p) => {
                let p: f64 = p.trim().parse().with_context(|| format!("bad percentage '{s}'"))?;
                if !(0.0..=100.0).contains(&p) {
                    bail!("memory percentage must be 0..100, got '{s}'");
                }
                Ok(MemSpec::Percent(p))
            }
            None => Ok(MemSpec::Bytes(parse_size(s)?)),
        }
    }
}

impl std::fmt::Display for MemSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemSpec::Bytes(b) => f.write_str(&fmt_bytes(*b)),
            MemSpec::Percent(p) => write!(f, "{p}% of MemAvailable"),
        }
    }
}

/// The parts of `/proc/meminfo` the bench needs, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemInfo {
    pub total: u64,
    pub available: u64,
}

impl MemInfo {
    pub fn parse(s: &str) -> Option<MemInfo> {
        let field = |name: &str| {
            s.lines().find_map(|l| {
                let rest = l.strip_prefix(name)?.strip_prefix(':')?;
                let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
                Some(kb * KIB)
            })
        };
        Some(MemInfo { total: field("MemTotal")?, available: field("MemAvailable")? })
    }

    pub fn read() -> Result<MemInfo> {
        let s = std::fs::read_to_string("/proc/meminfo").context("reading /proc/meminfo")?;
        MemInfo::parse(&s).context("parsing /proc/meminfo")
    }
}

/// Resolved memory budget.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemPlan {
    pub requested: u64,
    /// What will actually be allocated.
    pub effective: u64,
    pub available_at_start: u64,
    /// The fraction cap that applied (after clamping to [`HARD_MAX_MEM_FRAC`]).
    pub max_frac: f64,
    /// The fraction cap was hit (`requested` > `max_frac` of MemAvailable).
    pub clamped: bool,
    /// The load releases memory when `MemAvailable` drops below this.
    pub floor: u64,
}

/// Turns a [`MemSpec`] into a budget that respects the caps.
pub fn plan_mem(spec: MemSpec, mi: &MemInfo, max_frac: f64) -> MemPlan {
    let max_frac = if max_frac.is_finite() { max_frac.clamp(0.0, HARD_MAX_MEM_FRAC) } else { DEFAULT_MAX_MEM_FRAC };
    let requested = match spec {
        MemSpec::Bytes(b) => b,
        MemSpec::Percent(p) => (mi.available as f64 * p / 100.0) as u64,
    };
    let limit = (mi.available as f64 * max_frac) as u64;
    // Whole 2 MiB units, so the allocation maps cleanly onto huge pages if THP is on.
    let effective = requested.min(limit) / (2 * MIB) * (2 * MIB);
    MemPlan {
        requested,
        effective,
        available_at_start: mi.available,
        max_frac,
        clamped: requested > limit,
        floor: mem_floor(mi),
    }
}

/// `MemAvailable` below which the memory load gives memory back: 2% of RAM, at least 512 MiB.
pub fn mem_floor(mi: &MemInfo) -> u64 {
    (mi.total / 50).max(512 * MIB)
}

/// Resolved disk budget.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiskPlan {
    pub requested: u64,
    pub effective: u64,
    pub free_at_start: u64,
    pub clamped: bool,
}

/// Caps a write budget at half the free space left after [`DISK_RESERVE`].
pub fn plan_disk(requested: u64, free: u64) -> DiskPlan {
    let limit = free.saturating_sub(DISK_RESERVE) / 2;
    let effective = requested.min(limit) / MIB * MIB;
    DiskPlan { requested, effective, free_at_start: free, clamped: requested > limit }
}

/// Free bytes (for unprivileged users) on the filesystem holding `path`.
pub fn free_space(path: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a valid out-pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("statvfs {}", path.display()));
    }
    Ok(st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMINFO: &str = "MemTotal:       65574028 kB\nMemFree:         1234567 kB\n\
                           MemAvailable:   44490784 kB\nBuffers:          123 kB\n";

    #[test]
    fn sizes() {
        assert_eq!(parse_size("4G").unwrap(), 4 * GIB);
        assert_eq!(parse_size("512MiB").unwrap(), 512 * MIB);
        assert_eq!(parse_size("1.5gib").unwrap(), 3 * GIB / 2);
        assert_eq!(parse_size("64k").unwrap(), 64 * KIB);
        assert_eq!(parse_size("1000").unwrap(), 1000);
        assert!(parse_size("4X").is_err());
        assert!(parse_size("G").is_err());
        assert_eq!(fmt_bytes(4 * GIB), "4.0 GiB");
        assert_eq!(fmt_bytes(512 * MIB), "512 MiB");
    }

    #[test]
    fn meminfo_parses() {
        let mi = MemInfo::parse(MEMINFO).unwrap();
        assert_eq!(mi.total, 65574028 * KIB);
        assert_eq!(mi.available, 44490784 * KIB);
        assert!(MemInfo::parse("MemTotal: 1 kB\n").is_none());
    }

    #[test]
    fn mem_caps_from_fake_meminfo() {
        let mi = MemInfo::parse(MEMINFO).unwrap();
        let avail = mi.available as f64;

        // 40% is under the default 50% cap: honoured.
        let p = plan_mem(MemSpec::Percent(40.0), &mi, DEFAULT_MAX_MEM_FRAC);
        assert!(!p.clamped);
        assert!((p.effective as f64 - avail * 0.4).abs() <= (2 * MIB) as f64);

        // 90% is clamped to the default 50%.
        let p = plan_mem(MemSpec::Percent(90.0), &mi, DEFAULT_MAX_MEM_FRAC);
        assert!(p.clamped);
        assert!(p.effective as f64 <= avail * 0.5 && p.effective as f64 > avail * 0.5 - (2 * MIB) as f64);

        // A user cap above 75% is itself clamped to 75%.
        let p = plan_mem(MemSpec::Bytes(1 << 50), &mi, 0.99);
        assert_eq!(p.max_frac, HARD_MAX_MEM_FRAC);
        assert!(p.effective as f64 <= avail * 0.75);
        assert!(p.clamped);

        // Absolute sizes are rounded down to 2 MiB.
        let p = plan_mem(MemSpec::Bytes(GIB + 3 * MIB), &mi, 0.5);
        assert_eq!(p.effective, GIB + 2 * MIB);

        // Nonsense fractions fall back to the default.
        assert_eq!(plan_mem(MemSpec::Percent(10.0), &mi, f64::NAN).max_frac, DEFAULT_MAX_MEM_FRAC);
        assert_eq!(plan_mem(MemSpec::Percent(10.0), &mi, -1.0).effective, 0);

        // Floor: 2% of 62.5 GiB > 512 MiB.
        assert_eq!(p.floor, mi.total / 50);
        let small = MemInfo { total: 8 * GIB, available: 2 * GIB };
        assert_eq!(mem_floor(&small), 512 * MIB);
    }

    #[test]
    fn mem_spec_parse() {
        assert_eq!(MemSpec::parse("40%").unwrap(), MemSpec::Percent(40.0));
        assert_eq!(MemSpec::parse("2G").unwrap(), MemSpec::Bytes(2 * GIB));
        assert!(MemSpec::parse("140%").is_err());
    }

    #[test]
    fn disk_caps() {
        let d = plan_disk(4 * GIB, 72 * GIB);
        assert_eq!(d.effective, 4 * GIB);
        assert!(!d.clamped);
        let d = plan_disk(4 * GIB, 9 * GIB);
        assert_eq!(d.effective, 2 * GIB);
        assert!(d.clamped);
        assert_eq!(plan_disk(4 * GIB, 3 * GIB).effective, 0);
    }
}
