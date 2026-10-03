//! cgroup v2 attribute files: parsers, typed settings, usage counters and tree helpers.
//!
//! Cgroup paths are relative to the cgroup mount (`/user.slice/app.slice/x.scope`; a
//! leading `/` is optional, `..` is refused). The [`v1`] submodule covers the closest
//! equivalents on legacy hierarchies.

pub mod v1;

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};

use lindesk_plugin_api::{IoLimit, IoPrioClass, Knob, Resource, Setting, TimestampMs, Usage};
use serde::{Deserialize, Serialize};

use crate::psi::PsiReading;
use crate::util::{self, check_attr_name, check_device, invalid_data, invalid_input, read_opt, read_trimmed, under};

/// Access to a cgroup v2 hierarchy mounted at `root`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CgroupFs {
    root: PathBuf,
}

impl CgroupFs {
    /// A hierarchy mounted at `root` (usually `/sys/fs/cgroup`).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        CgroupFs { root: root.into() }
    }

    /// The mount point.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory of a cgroup.
    pub fn dir(&self, cg: &str) -> io::Result<PathBuf> {
        under(&self.root, cg)
    }

    /// Path of one attribute file of a cgroup.
    pub fn attr_path(&self, cg: &str, name: &str) -> io::Result<PathBuf> {
        check_attr_name(name)?;
        Ok(self.dir(cg)?.join(name))
    }

    /// True if the cgroup directory exists.
    pub fn exists(&self, cg: &str) -> bool {
        self.dir(cg).is_ok_and(|d| d.is_dir())
    }

    /// True if the cgroup has the attribute file (i.e. the controller is enabled for it).
    pub fn has_attr(&self, cg: &str, name: &str) -> bool {
        self.attr_path(cg, name).is_ok_and(|p| p.exists())
    }

    /// Read an attribute, trailing whitespace stripped.
    pub fn read_attr(&self, cg: &str, name: &str) -> io::Result<String> {
        read_trimmed(&self.attr_path(cg, name)?)
    }

    /// Read an attribute; a missing file is `Ok(None)`.
    pub fn read_attr_opt(&self, cg: &str, name: &str) -> io::Result<Option<String>> {
        read_opt(&self.attr_path(cg, name)?)
    }

    /// Write an attribute with a single `write(2)`.
    pub fn write_attr(&self, cg: &str, name: &str, value: &str) -> io::Result<()> {
        util::write_attr(&self.attr_path(cg, name)?, value)
    }

    /// Controllers available to the cgroup (`cgroup.controllers`).
    pub fn controllers(&self, cg: &str) -> io::Result<Vec<String>> {
        Ok(split_words(&self.read_attr(cg, "cgroup.controllers")?))
    }

    /// Controllers enabled for the cgroup's children (`cgroup.subtree_control`).
    pub fn subtree_control(&self, cg: &str) -> io::Result<Vec<String>> {
        Ok(split_words(&self.read_attr(cg, "cgroup.subtree_control")?))
    }

    /// Enable controllers for the cgroup's children (writes `+cpu +io ...`).
    pub fn enable_controllers(&self, cg: &str, controllers: &[&str]) -> io::Result<()> {
        if controllers.iter().any(|c| c.is_empty() || c.contains(char::is_whitespace)) {
            return Err(invalid_input("invalid controller name"));
        }
        let v: Vec<String> = controllers.iter().map(|c| format!("+{c}")).collect();
        self.write_attr(cg, "cgroup.subtree_control", &v.join(" "))
    }

    /// Child cgroups, as full relative paths (`/user.slice/user-1000.slice`), sorted.
    pub fn children(&self, cg: &str) -> io::Result<Vec<String>> {
        let base = cg.trim_end_matches('/');
        Ok(util::subdirs(&self.dir(cg)?)?.into_iter().map(|n| format!("{base}/{n}")).collect())
    }

    /// Processes directly in the cgroup (`cgroup.procs`).
    pub fn procs(&self, cg: &str) -> io::Result<Vec<i32>> {
        parse_pid_list(&self.read_attr(cg, "cgroup.procs")?)
    }

    /// Move a process into the cgroup (writes `cgroup.procs`).
    pub fn move_pid(&self, cg: &str, pid: i32) -> io::Result<()> {
        if pid <= 0 {
            return Err(invalid_input(format!("invalid pid {pid}")));
        }
        self.write_attr(cg, "cgroup.procs", &pid.to_string())
    }

    /// Create a cgroup (and any missing parents).
    pub fn create(&self, cg: &str) -> io::Result<()> {
        std::fs::create_dir_all(self.dir(cg)?)
    }

    /// Remove an empty cgroup.
    pub fn remove(&self, cg: &str) -> io::Result<()> {
        std::fs::remove_dir(self.dir(cg)?)
    }

    /// `cpu.stat`.
    pub fn cpu_stat(&self, cg: &str) -> io::Result<CpuStat> {
        Ok(CpuStat::parse(&self.read_attr(cg, "cpu.stat")?))
    }

    /// `io.stat`, keyed by `MAJ:MIN`.
    pub fn io_stat(&self, cg: &str) -> io::Result<BTreeMap<String, IoStat>> {
        Ok(parse_io_stat(&self.read_attr(cg, "io.stat")?))
    }

    /// `memory.current`, bytes.
    pub fn memory_current(&self, cg: &str) -> io::Result<u64> {
        parse_u64(&self.read_attr(cg, "memory.current")?)
    }

    /// `memory.stat` as a flat map, or `None` when the memory controller is not enabled.
    pub fn memory_stat(&self, cg: &str) -> io::Result<Option<BTreeMap<String, u64>>> {
        Ok(self.read_attr_opt(cg, "memory.stat")?.map(|s| parse_flat_keyed(&s)))
    }

    /// `io.max` limits keyed by `MAJ:MIN`. Devices without a line are unlimited.
    pub fn io_max(&self, cg: &str) -> io::Result<BTreeMap<String, IoLimit>> {
        parse_io_max(&self.read_attr(cg, "io.max")?)
    }

    /// `io.weight`.
    pub fn io_weight(&self, cg: &str) -> io::Result<IoWeight> {
        IoWeight::parse(&self.read_attr(cg, "io.weight")?)
    }

    /// `cpu.weight`.
    pub fn cpu_weight(&self, cg: &str) -> io::Result<u64> {
        parse_u64(&self.read_attr(cg, "cpu.weight")?)
    }

    /// `cpu.idle`.
    pub fn cpu_idle(&self, cg: &str) -> io::Result<bool> {
        Ok(parse_u64(&self.read_attr(cg, "cpu.idle")?)? != 0)
    }

    /// `cpu.uclamp.min`, percent.
    pub fn uclamp_min(&self, cg: &str) -> io::Result<f32> {
        parse_uclamp(&self.read_attr(cg, "cpu.uclamp.min")?)
    }

    /// `cpu.uclamp.max`, percent (`max` = 100).
    pub fn uclamp_max(&self, cg: &str) -> io::Result<f32> {
        parse_uclamp(&self.read_attr(cg, "cpu.uclamp.max")?)
    }

    /// `memory.low`, bytes; `None` = `max`.
    pub fn memory_low(&self, cg: &str) -> io::Result<Option<u64>> {
        parse_max_u64(&self.read_attr(cg, "memory.low")?)
    }

    /// `memory.high`, bytes; `None` = `max`.
    pub fn memory_high(&self, cg: &str) -> io::Result<Option<u64>> {
        parse_max_u64(&self.read_attr(cg, "memory.high")?)
    }

    /// A cgroup's `<resource>.pressure`.
    pub fn pressure(&self, cg: &str, resource: Resource) -> io::Result<PsiReading> {
        PsiReading::read(&self.attr_path(cg, &format!("{}.pressure", resource.as_str()))?)
    }

    /// Current value of a knob.
    ///
    /// `Ok(None)` means the attribute file does not exist (controller not enabled for this
    /// cgroup, or the root cgroup). Per-process knobs (`Nice`, `IoPrio`) are `Unsupported`.
    /// For `IoMax(dev)`, a device without a line in `io.max` reads as unlimited.
    pub fn read_setting(&self, cg: &str, knob: &Knob) -> io::Result<Option<Setting>> {
        let file = knob_file(knob).ok_or_else(|| unsupported(knob))?;
        let Some(raw) = self.read_attr_opt(cg, file)? else { return Ok(None) };
        parse_setting(knob, &raw).map(Some)
    }

    /// Apply a setting by writing the matching attribute file in the kernel's format.
    pub fn write_setting(&self, cg: &str, setting: &Setting) -> io::Result<()> {
        let (file, value) = format_setting(setting)?;
        self.write_attr(cg, file, &value)
    }

    /// Cumulative counters for a cgroup. `interval_ms` is 0; feed snapshots to
    /// [`UsageTracker`] for per-interval values. Missing controller files count as zero;
    /// a missing cgroup is `NotFound`.
    pub fn usage(&self, cg: &str) -> io::Result<Usage> {
        let dir = self.dir(cg)?;
        if !dir.is_dir() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("cgroup {cg} does not exist")));
        }
        let mut u = Usage::default();
        if let Some(s) = self.read_attr_opt(cg, "cpu.stat")? {
            u.cpu_usage_us = CpuStat::parse(&s).usage_usec;
        }
        if let Some(s) = self.read_attr_opt(cg, "io.stat")? {
            for st in parse_io_stat(&s).values() {
                u.io_rbytes += st.rbytes;
                u.io_wbytes += st.wbytes;
                u.io_rios += st.rios;
                u.io_wios += st.wios;
            }
        }
        if let Some(s) = self.read_attr_opt(cg, "memory.current")? {
            u.mem_current = parse_u64(&s)?;
        }
        Ok(u)
    }
}

fn unsupported(knob: &Knob) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, format!("{knob} is a per-process knob, not a cgroup attribute"))
}

fn split_words(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

/// Parse a newline-separated pid list (`cgroup.procs`, `tasks`).
pub fn parse_pid_list(s: &str) -> io::Result<Vec<i32>> {
    s.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().parse().map_err(|_| invalid_data(format!("bad pid '{l}'"))))
        .collect()
}

/// Parse an unsigned integer attribute.
pub fn parse_u64(s: &str) -> io::Result<u64> {
    s.trim().parse().map_err(|_| invalid_data(format!("expected an integer, got '{}'", s.trim())))
}

/// Parse a value that may be `max` (→ `None`).
pub fn parse_max_u64(s: &str) -> io::Result<Option<u64>> {
    match s.trim() {
        "max" => Ok(None),
        v => parse_u64(v).map(Some),
    }
}

/// Parse `cpu.uclamp.{min,max}`: `max` or a percentage with two decimals.
pub fn parse_uclamp(s: &str) -> io::Result<f32> {
    match s.trim() {
        "max" => Ok(100.0),
        v => v
            .parse::<f32>()
            .ok()
            .filter(|p| (0.0..=100.0).contains(p))
            .ok_or_else(|| invalid_data(format!("bad uclamp value '{v}'"))),
    }
}

/// Format a uclamp percentage. `cpu.uclamp.max` at 100% is written as `max`.
pub fn format_uclamp(percent: f32, is_max: bool) -> io::Result<String> {
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        return Err(invalid_input(format!("uclamp percent {percent} out of 0..=100")));
    }
    if is_max && percent >= 100.0 { Ok("max".into()) } else { Ok(format!("{percent:.2}")) }
}

/// Parse flat-keyed files (`cpu.stat`, `memory.stat`): `key value` per line. Lines whose
/// value isn't an integer are skipped.
pub fn parse_flat_keyed(s: &str) -> BTreeMap<String, u64> {
    s.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let k = it.next()?;
            let v = it.next()?.parse().ok()?;
            Some((k.to_string(), v))
        })
        .collect()
}

/// `cpu.stat`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct CpuStat {
    pub usage_usec: u64,
    pub user_usec: u64,
    pub system_usec: u64,
    pub nr_periods: u64,
    pub nr_throttled: u64,
    pub throttled_usec: u64,
}

impl CpuStat {
    /// Parse `cpu.stat` contents; absent keys are zero.
    pub fn parse(s: &str) -> Self {
        let m = parse_flat_keyed(s);
        let g = |k: &str| m.get(k).copied().unwrap_or(0);
        CpuStat {
            usage_usec: g("usage_usec"),
            user_usec: g("user_usec"),
            system_usec: g("system_usec"),
            nr_periods: g("nr_periods"),
            nr_throttled: g("nr_throttled"),
            throttled_usec: g("throttled_usec"),
        }
    }
}

/// One device line of `io.stat`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoStat {
    pub rbytes: u64,
    pub wbytes: u64,
    pub rios: u64,
    pub wios: u64,
    pub dbytes: u64,
    pub dios: u64,
}

/// Parse `io.stat`: `MAJ:MIN rbytes=.. wbytes=.. rios=.. wios=.. dbytes=.. dios=..`.
/// Unknown keys (e.g. iocost debug stats) are ignored.
pub fn parse_io_stat(s: &str) -> BTreeMap<String, IoStat> {
    let mut out = BTreeMap::new();
    for line in s.lines() {
        let mut it = line.split_whitespace();
        let Some(dev) = it.next() else { continue };
        if check_device(dev).is_err() {
            continue;
        }
        let mut st = IoStat::default();
        for kv in it {
            let Some((k, v)) = kv.split_once('=') else { continue };
            let Ok(v) = v.parse::<u64>() else { continue };
            match k {
                "rbytes" => st.rbytes = v,
                "wbytes" => st.wbytes = v,
                "rios" => st.rios = v,
                "wios" => st.wios = v,
                "dbytes" => st.dbytes = v,
                "dios" => st.dios = v,
                _ => {}
            }
        }
        out.insert(dev.to_string(), st);
    }
    out
}

/// Parse `io.max`: `MAJ:MIN rbps=N|max wbps=.. riops=.. wiops=..`.
pub fn parse_io_max(s: &str) -> io::Result<BTreeMap<String, IoLimit>> {
    let mut out = BTreeMap::new();
    for line in s.lines() {
        let mut it = line.split_whitespace();
        let Some(dev) = it.next() else { continue };
        check_device(dev).map_err(|_| invalid_data(format!("bad io.max line '{line}'")))?;
        let mut lim = IoLimit::default();
        for kv in it {
            let (k, v) = kv.split_once('=').ok_or_else(|| invalid_data(format!("bad io.max field '{kv}'")))?;
            let v = parse_max_u64(v)?;
            match k {
                "rbps" => lim.rbps = v,
                "wbps" => lim.wbps = v,
                "riops" => lim.riops = v,
                "wiops" => lim.wiops = v,
                _ => {}
            }
        }
        out.insert(dev.to_string(), lim);
    }
    Ok(out)
}

/// Format one `io.max` line. All four keys are always written so a limit that was lifted
/// really goes back to `max`.
pub fn format_io_max(device: &str, limit: &IoLimit) -> io::Result<String> {
    check_device(device)?;
    let v = |o: Option<u64>| o.map_or_else(|| "max".to_string(), |v| v.to_string());
    Ok(format!(
        "{device} rbps={} wbps={} riops={} wiops={}",
        v(limit.rbps),
        v(limit.wbps),
        v(limit.riops),
        v(limit.wiops)
    ))
}

/// `io.weight` (also `io.bfq.weight`): a default plus per-device overrides.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct IoWeight {
    pub default: u64,
    pub devices: BTreeMap<String, u64>,
}

impl Default for IoWeight {
    fn default() -> Self {
        IoWeight { default: 100, devices: BTreeMap::new() }
    }
}

impl IoWeight {
    /// Parse `default N` / `MAJ:MIN N` lines. A bare number (v1 `blkio.weight`) is the
    /// default.
    pub fn parse(s: &str) -> io::Result<Self> {
        let mut w = IoWeight::default();
        for line in s.lines() {
            let mut it = line.split_whitespace();
            match (it.next(), it.next()) {
                (Some("default"), Some(v)) => w.default = parse_u64(v)?,
                (Some(dev), Some(v)) => {
                    check_device(dev).map_err(|_| invalid_data(format!("bad io.weight line '{line}'")))?;
                    w.devices.insert(dev.to_string(), parse_u64(v)?);
                }
                (Some(v), None) => w.default = parse_u64(v)?,
                (None, _) => {}
            }
        }
        Ok(w)
    }
}

/// The attribute file backing a cgroup knob, or `None` for per-process knobs.
pub fn knob_file(knob: &Knob) -> Option<&'static str> {
    Some(match knob {
        Knob::CpuWeight => "cpu.weight",
        Knob::CpuIdle => "cpu.idle",
        Knob::UclampMin => "cpu.uclamp.min",
        Knob::UclampMax => "cpu.uclamp.max",
        Knob::IoWeight => "io.weight",
        Knob::IoMax(_) => "io.max",
        Knob::MemoryLow => "memory.low",
        Knob::MemoryHigh => "memory.high",
        Knob::Nice | Knob::IoPrio => return None,
    })
}

/// Parse the raw contents of the knob's attribute file into a [`Setting`].
pub fn parse_setting(knob: &Knob, raw: &str) -> io::Result<Setting> {
    Ok(match knob {
        Knob::CpuWeight => Setting::CpuWeight { value: parse_u64(raw)? },
        Knob::CpuIdle => Setting::CpuIdle { value: parse_u64(raw)? != 0 },
        Knob::UclampMin => Setting::UclampMin { percent: parse_uclamp(raw)? },
        Knob::UclampMax => Setting::UclampMax { percent: parse_uclamp(raw)? },
        Knob::IoWeight => Setting::IoWeight { value: IoWeight::parse(raw)?.default },
        Knob::IoMax(dev) => {
            check_device(dev)?;
            let limit = parse_io_max(raw)?.remove(dev).unwrap_or_default();
            Setting::IoMax { device: dev.clone(), limit }
        }
        Knob::MemoryLow => Setting::MemoryLow { bytes: parse_max_u64(raw)?.unwrap_or(u64::MAX) },
        Knob::MemoryHigh => Setting::MemoryHigh { bytes: parse_max_u64(raw)? },
        Knob::Nice | Knob::IoPrio => return Err(unsupported(knob)),
    })
}

/// The attribute file and exact value to write for a cgroup setting.
///
/// * `cpu.weight` / `io.weight` must be 1..=10000 (`io.weight` is written as `default N`);
/// * uclamp is written as `NN.NN`, `cpu.uclamp.max` at 100% as `max`;
/// * `io.max` always carries all four keys, `max` for unlimited;
/// * `memory.low = u64::MAX` and `memory.high = None` are written as `max`.
pub fn format_setting(setting: &Setting) -> io::Result<(&'static str, String)> {
    let weight = |v: u64| {
        if (1..=10_000).contains(&v) { Ok(v) } else { Err(invalid_input(format!("weight {v} out of 1..=10000"))) }
    };
    let file = knob_file(&setting.knob()).ok_or_else(|| unsupported(&setting.knob()))?;
    let value = match setting {
        Setting::CpuWeight { value } => weight(*value)?.to_string(),
        Setting::CpuIdle { value } => u8::from(*value).to_string(),
        Setting::UclampMin { percent } => format_uclamp(*percent, false)?,
        Setting::UclampMax { percent } => format_uclamp(*percent, true)?,
        Setting::IoWeight { value } => format!("default {}", weight(*value)?),
        Setting::IoMax { device, limit } => format_io_max(device, limit)?,
        Setting::MemoryLow { bytes } => {
            if *bytes == u64::MAX {
                "max".into()
            } else {
                bytes.to_string()
            }
        }
        Setting::MemoryHigh { bytes } => bytes.map_or_else(|| "max".to_string(), |b| b.to_string()),
        Setting::Nice { .. } | Setting::IoPrio { .. } => return Err(unsupported(&setting.knob())),
    };
    Ok((file, value))
}

/// The kernel's default value for a knob, i.e. what "no lindesk override" looks like.
///
/// cgroup knobs: `cpu.weight` 100, `cpu.idle` 0, `cpu.uclamp.min` 0%, `cpu.uclamp.max`
/// max (100%), `io.weight` 100, `io.max` unlimited, `memory.low` 0, `memory.high` max.
/// Per-process knobs: nice 0, and ioprio best-effort level 4 (what the kernel derives for
/// nice 0 when no class is set).
pub fn default_setting(knob: &Knob) -> Option<Setting> {
    Some(match knob {
        Knob::CpuWeight => Setting::CpuWeight { value: 100 },
        Knob::CpuIdle => Setting::CpuIdle { value: false },
        Knob::UclampMin => Setting::UclampMin { percent: 0.0 },
        Knob::UclampMax => Setting::UclampMax { percent: 100.0 },
        Knob::IoWeight => Setting::IoWeight { value: 100 },
        Knob::IoMax(dev) => Setting::IoMax { device: dev.clone(), limit: IoLimit::default() },
        Knob::MemoryLow => Setting::MemoryLow { bytes: 0 },
        Knob::MemoryHigh => Setting::MemoryHigh { bytes: None },
        Knob::Nice => Setting::Nice { value: 0 },
        Knob::IoPrio => Setting::IoPrio { class: IoPrioClass::BestEffort, level: 4 },
    })
}

/// Turns successive cumulative [`Usage`] snapshots into per-interval deltas.
///
/// Keyed by an arbitrary string (usually the cgroup path). Counters that go backwards
/// (cgroup recreated) count from zero. `mem_current` is a gauge and is passed through.
#[derive(Debug, Default)]
pub struct UsageTracker {
    prev: HashMap<String, (TimestampMs, Usage)>,
}

impl UsageTracker {
    /// An empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a snapshot taken at `ts`. The first snapshot for a key (or one with an older
    /// timestamp) only sets the baseline and returns `None`.
    pub fn update(&mut self, key: &str, ts: TimestampMs, cur: Usage) -> Option<Usage> {
        let (pts, prev) = self.prev.insert(key.to_string(), (ts, cur.clone()))?;
        if ts < pts {
            return None;
        }
        let d = |p: u64, c: u64| if c >= p { c - p } else { c };
        Some(Usage {
            cpu_usage_us: d(prev.cpu_usage_us, cur.cpu_usage_us),
            io_rbytes: d(prev.io_rbytes, cur.io_rbytes),
            io_wbytes: d(prev.io_wbytes, cur.io_wbytes),
            io_rios: d(prev.io_rios, cur.io_rios),
            io_wios: d(prev.io_wios, cur.io_wios),
            mem_current: cur.mem_current,
            interval_ms: u32::try_from(ts - pts).unwrap_or(u32::MAX),
        })
    }

    /// Drop a key.
    pub fn forget(&mut self, key: &str) {
        self.prev.remove(key);
    }

    /// Keep only keys for which `keep` returns true.
    pub fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.prev.retain(|k, _| keep(k));
    }

    /// Number of tracked keys.
    pub fn len(&self) -> usize {
        self.prev.len()
    }

    /// True when nothing is tracked.
    pub fn is_empty(&self) -> bool {
        self.prev.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, CgroupFs) {
        let dir = tempfile::tempdir().unwrap();
        let cg = dir.path().join("user.slice/app.slice");
        fs::create_dir_all(&cg).unwrap();
        let files = [
            ("cgroup.controllers", "cpu io memory pids\n"),
            ("cgroup.subtree_control", "cpu io\n"),
            ("cgroup.procs", "12\n345\n"),
            (
                "cpu.stat",
                "usage_usec 655679095635\nuser_usec 608216531866\nsystem_usec 47462563769\nnice_usec 1\n\
                 core_sched.force_idle_usec 0\nnr_periods 0\nnr_throttled 3\nthrottled_usec 7\n",
            ),
            (
                "io.stat",
                "7:17 rbytes=5120 wbytes=0 rios=2 wios=0 dbytes=0 dios=0\n\
                 252:0 rbytes=4218327040 wbytes=17104797696 rios=1029865 wios=4157425 dbytes=0 dios=0\n",
            ),
            ("memory.current", "123456789\n"),
            ("memory.stat", "anon 100\nfile 200\n"),
            ("io.max", "259:0 rbps=2097152 wbps=max riops=max wiops=120\n"),
            ("io.weight", "default 100\n259:0 200\n"),
            ("cpu.weight", "100\n"),
            ("cpu.idle", "0\n"),
            ("cpu.uclamp.min", "0.00\n"),
            ("cpu.uclamp.max", "max\n"),
            ("memory.low", "0\n"),
            ("memory.high", "max\n"),
            (
                "io.pressure",
                "some avg10=1.00 avg60=0.00 avg300=0.00 total=10\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=5\n",
            ),
        ];
        for (f, c) in files {
            fs::write(cg.join(f), c).unwrap();
        }
        fs::create_dir_all(cg.join("a.scope")).unwrap();
        fs::create_dir_all(cg.join("b.scope")).unwrap();
        let fs = CgroupFs::new(dir.path());
        (dir, fs)
    }

    const CG: &str = "/user.slice/app.slice";

    #[test]
    fn parsers() {
        let (_d, fs) = fixture();
        assert_eq!(fs.controllers(CG).unwrap(), ["cpu", "io", "memory", "pids"]);
        assert_eq!(fs.subtree_control(CG).unwrap(), ["cpu", "io"]);
        assert_eq!(fs.procs(CG).unwrap(), [12, 345]);
        let cs = fs.cpu_stat(CG).unwrap();
        assert_eq!(cs.usage_usec, 655679095635);
        assert_eq!(cs.nr_throttled, 3);
        let io = fs.io_stat(CG).unwrap();
        assert_eq!(io["252:0"].wbytes, 17104797696);
        assert_eq!(io["7:17"].rios, 2);
        assert_eq!(fs.memory_current(CG).unwrap(), 123456789);
        assert_eq!(fs.memory_stat(CG).unwrap().unwrap()["file"], 200);
        assert_eq!(fs.memory_stat("/user.slice").unwrap(), None);
        let max = fs.io_max(CG).unwrap();
        assert_eq!(max["259:0"], IoLimit { rbps: Some(2097152), wbps: None, riops: None, wiops: Some(120) });
        let w = fs.io_weight(CG).unwrap();
        assert_eq!(w.default, 100);
        assert_eq!(w.devices["259:0"], 200);
        assert_eq!(fs.cpu_weight(CG).unwrap(), 100);
        assert!(!fs.cpu_idle(CG).unwrap());
        assert_eq!(fs.uclamp_min(CG).unwrap(), 0.0);
        assert_eq!(fs.uclamp_max(CG).unwrap(), 100.0);
        assert_eq!(fs.memory_low(CG).unwrap(), Some(0));
        assert_eq!(fs.memory_high(CG).unwrap(), None);
        assert_eq!(fs.pressure(CG, Resource::Io).unwrap().some.total_us, 10);
        assert_eq!(fs.children(CG).unwrap(), ["/user.slice/app.slice/a.scope", "/user.slice/app.slice/b.scope"]);
        assert_eq!(fs.children("/").unwrap(), ["/user.slice"]);
        assert!(fs.exists(CG) && !fs.exists("/nope"));
        assert!(fs.has_attr(CG, "io.max") && !fs.has_attr("/user.slice", "io.max"));
        assert!(fs.read_attr(CG, "../x").is_err());
        assert!(fs.read_attr("/user.slice/../..", "cpu.weight").is_err());
    }

    #[test]
    fn small_parsers() {
        assert_eq!(parse_uclamp("50.00").unwrap(), 50.0);
        assert_eq!(parse_uclamp("max").unwrap(), 100.0);
        assert!(parse_uclamp("101").is_err());
        assert!(parse_uclamp("abc").is_err());
        assert_eq!(parse_max_u64("max").unwrap(), None);
        assert_eq!(parse_max_u64("42\n").unwrap(), Some(42));
        assert!(parse_max_u64("-1").is_err());
        assert_eq!(IoWeight::parse("default 100\n").unwrap().default, 100);
        assert_eq!(IoWeight::parse("500\n").unwrap().default, 500);
        assert!(IoWeight::parse("bogus 1").is_err());
        assert!(parse_io_max("259:0 rbps=x").is_err());
        assert!(parse_io_max("").unwrap().is_empty());
        assert!(parse_pid_list("1\nx\n").is_err());
        assert_eq!(parse_io_stat("garbage\n8:0 rbytes=1 cost.usage=5\n")["8:0"].rbytes, 1);
    }

    #[test]
    fn setting_formats() {
        let f = |s: Setting| format_setting(&s).unwrap();
        assert_eq!(f(Setting::CpuWeight { value: 250 }), ("cpu.weight", "250".into()));
        assert_eq!(f(Setting::CpuIdle { value: true }), ("cpu.idle", "1".into()));
        assert_eq!(f(Setting::UclampMin { percent: 50.0 }), ("cpu.uclamp.min", "50.00".into()));
        assert_eq!(f(Setting::UclampMin { percent: 100.0 }), ("cpu.uclamp.min", "100.00".into()));
        assert_eq!(f(Setting::UclampMax { percent: 100.0 }), ("cpu.uclamp.max", "max".into()));
        assert_eq!(f(Setting::UclampMax { percent: 33.333 }), ("cpu.uclamp.max", "33.33".into()));
        assert_eq!(f(Setting::IoWeight { value: 10 }), ("io.weight", "default 10".into()));
        assert_eq!(
            f(Setting::IoMax {
                device: "259:0".into(),
                limit: IoLimit { rbps: None, wbps: Some(8388608), riops: None, wiops: Some(100) }
            }),
            ("io.max", "259:0 rbps=max wbps=8388608 riops=max wiops=100".into())
        );
        assert_eq!(
            f(Setting::IoMax { device: "8:0".into(), limit: IoLimit::default() }),
            ("io.max", "8:0 rbps=max wbps=max riops=max wiops=max".into())
        );
        assert_eq!(f(Setting::MemoryLow { bytes: 1 << 30 }), ("memory.low", "1073741824".into()));
        assert_eq!(f(Setting::MemoryLow { bytes: u64::MAX }), ("memory.low", "max".into()));
        assert_eq!(f(Setting::MemoryHigh { bytes: None }), ("memory.high", "max".into()));
        assert_eq!(f(Setting::MemoryHigh { bytes: Some(4096) }), ("memory.high", "4096".into()));

        for bad in [
            Setting::CpuWeight { value: 0 },
            Setting::CpuWeight { value: 10_001 },
            Setting::IoWeight { value: 0 },
            Setting::UclampMin { percent: -1.0 },
            Setting::UclampMax { percent: f32::NAN },
            Setting::IoMax { device: "sda".into(), limit: IoLimit::default() },
            Setting::Nice { value: 1 },
            Setting::IoPrio { class: IoPrioClass::Idle, level: 0 },
        ] {
            assert!(format_setting(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn read_write_settings_roundtrip() {
        let (_d, fs) = fixture();
        let all = [
            Setting::CpuWeight { value: 300 },
            Setting::CpuIdle { value: true },
            Setting::UclampMin { percent: 25.5 },
            Setting::UclampMax { percent: 100.0 },
            Setting::UclampMax { percent: 60.0 },
            Setting::IoWeight { value: 50 },
            Setting::IoMax {
                device: "259:0".into(),
                limit: IoLimit { rbps: Some(1), wbps: Some(2), riops: Some(3), wiops: None },
            },
            Setting::MemoryLow { bytes: 1 << 20 },
            Setting::MemoryLow { bytes: u64::MAX },
            Setting::MemoryHigh { bytes: Some(1 << 29) },
            Setting::MemoryHigh { bytes: None },
        ];
        for s in all {
            fs.write_setting(CG, &s).unwrap();
            assert_eq!(fs.read_setting(CG, &s.knob()).unwrap(), Some(s.clone()), "{s}");
        }
        // Another device has no io.max line: unlimited.
        assert_eq!(
            fs.read_setting(CG, &Knob::IoMax("8:0".into())).unwrap(),
            Some(Setting::IoMax { device: "8:0".into(), limit: IoLimit::default() })
        );
        // Controller not enabled → None. Per-process knobs → Unsupported.
        assert_eq!(fs.read_setting("/user.slice", &Knob::CpuWeight).unwrap(), None);
        assert_eq!(fs.read_setting(CG, &Knob::Nice).unwrap_err().kind(), io::ErrorKind::Unsupported);
        // Writing to a missing attribute fails rather than creating a file.
        assert!(fs.write_setting("/user.slice", &Setting::CpuWeight { value: 5 }).is_err());
        assert!(!fs.has_attr("/user.slice", "cpu.weight"));
    }

    #[test]
    fn defaults() {
        assert_eq!(default_setting(&Knob::CpuWeight), Some(Setting::CpuWeight { value: 100 }));
        assert_eq!(default_setting(&Knob::UclampMax), Some(Setting::UclampMax { percent: 100.0 }));
        assert_eq!(default_setting(&Knob::MemoryHigh), Some(Setting::MemoryHigh { bytes: None }));
        let d = default_setting(&Knob::IoMax("8:0".into())).unwrap();
        assert_eq!(format_setting(&d).unwrap().1, "8:0 rbps=max wbps=max riops=max wiops=max");
        // Every cgroup default is writable as-is.
        for k in [
            Knob::CpuWeight,
            Knob::CpuIdle,
            Knob::UclampMin,
            Knob::UclampMax,
            Knob::IoWeight,
            Knob::MemoryLow,
            Knob::MemoryHigh,
        ] {
            format_setting(&default_setting(&k).unwrap()).unwrap();
        }
    }

    #[test]
    fn tree_helpers() {
        let (_d, fs) = fixture();
        fs.create("/lindesk.slice/batch").unwrap();
        assert!(fs.exists("/lindesk.slice/batch"));
        fs.remove("/lindesk.slice/batch").unwrap();
        assert!(!fs.exists("/lindesk.slice/batch"));
        fs.move_pid(CG, 4242).unwrap();
        assert_eq!(fs.procs(CG).unwrap(), [4242]);
        assert!(fs.move_pid(CG, 0).is_err());
        fs.enable_controllers(CG, &["cpu", "io", "memory"]).unwrap();
        assert_eq!(fs.read_attr(CG, "cgroup.subtree_control").unwrap(), "+cpu +io +memory");
        assert!(fs.enable_controllers(CG, &["cpu io"]).is_err());
    }

    #[test]
    fn usage_and_tracker() {
        let (_d, fs) = fixture();
        let u = fs.usage(CG).unwrap();
        assert_eq!(u.cpu_usage_us, 655679095635);
        assert_eq!(u.io_rbytes, 4218327040 + 5120);
        assert_eq!(u.io_wios, 4157425);
        assert_eq!(u.mem_current, 123456789);
        assert_eq!(u.interval_ms, 0);
        // Missing controller files count as zero; missing cgroup is an error.
        assert_eq!(fs.usage("/user.slice").unwrap().cpu_usage_us, 0);
        assert_eq!(fs.usage("/nope").unwrap_err().kind(), io::ErrorKind::NotFound);

        let mut t = UsageTracker::new();
        let snap = |cpu, rb, mem| Usage { cpu_usage_us: cpu, io_rbytes: rb, mem_current: mem, ..Default::default() };
        assert!(t.update(CG, 1000, snap(100, 1000, 5)).is_none());
        let d = t.update(CG, 2000, snap(600, 1500, 7)).unwrap();
        assert_eq!((d.cpu_usage_us, d.io_rbytes, d.mem_current, d.interval_ms), (500, 500, 7, 1000));
        // Reset: counters restart from zero.
        let d = t.update(CG, 3000, snap(50, 10, 1)).unwrap();
        assert_eq!((d.cpu_usage_us, d.io_rbytes), (50, 10));
        assert!(t.update(CG, 2500, snap(60, 10, 1)).is_none());
        assert_eq!(t.len(), 1);
        t.retain(|k| k != CG);
        assert!(t.is_empty());
    }
}
