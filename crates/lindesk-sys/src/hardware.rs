//! Hardware profile: CPU topology, memory, storage, power and virtualization.
//!
//! [`detect`] reads only sysfs and procfs under [`Roots`] and never fails: anything it can't
//! read stays at its default (`0`, empty, [`StorageKind::Unknown`], [`PowerSource::Unknown`]),
//! which policies treat as "unknown hardware" and answer with their historical constants.
//!
//! # Sources
//!
//! | Field | Source |
//! |---|---|
//! | `cpus` | `/sys/devices/system/cpu/online` (else `processor` lines in `/proc/cpuinfo`) |
//! | `cores`, `smt` | `cpuN/topology/core_cpus_list` (or `thread_siblings_list`), `cpu/smt/active` |
//! | `cpu_model` | `/proc/cpuinfo` `model name`, else ARM `CPU part` names and `Hardware` |
//! | hybrid split | Intel `/sys/devices/cpu_core/cpus` + `cpu_atom/cpus`; else distinct `cpuN/cpu_capacity` (top = performance); else exactly two `cpufreq/cpuinfo_max_freq` clusters ≥ 15% apart |
//! | `mem_total`, `swap_total` | `/proc/meminfo` |
//! | `compressed_swap` | swap configured and a zram device in `/proc/swaps`, or `/sys/module/zswap/parameters/enabled` = `Y` |
//! | `storage` | the managed disks, see [`storage_kind`] and [`nominal_bps`] |
//! | `power`, `portable` | `/sys/class/power_supply/*`, `/sys/class/dmi/id/chassis_type` |
//! | `virtualization` | WSL kernel release, container markers, `/sys/hypervisor/type`, the cpuinfo `hypervisor` flag and DMI vendor strings |
//! | `arch` | `/proc/sys/kernel/arch`, else `uname(2)` (real host only), else inferred from cpuinfo |
//! | `vendor` | cpuinfo `vendor_id` (x86), else ARM `CPU implementer` (0x41 Arm, 0x61 Apple, 0x51 Qualcomm, 0xc0 Ampere) |
//! | `features.eas`, `features.itmt` | `/proc/sys/kernel/sched_energy_aware`, `/proc/sys/kernel/sched_itmt_enabled` |
//! | `features.cpufreq_*` | the first `/sys/devices/system/cpu/cpufreq/policy*`: `scaling_driver`, `scaling_governor`, `energy_performance_preference` and the `*_available_*` lists |
//! | `features.numa_nodes` | `/sys/devices/system/node/online` |
//!
//! `latency` is never detected here: it comes from [`crate::calibrate`] (stored JSON).

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use lindesk_plugin_api::{CpuFeatures, CpuVendor, Hardware, PowerSource, StorageDevice, StorageKind};

use crate::Roots;
use crate::blockdev::{self, BlockDev};
use crate::util::read_lossy;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

/// Detect the hardware of the host described by `roots`. Storage covers the disks behind
/// [`crate::probe::PROBED_PATHS`], in the same order as [`crate::Probe::devices`].
pub fn detect(roots: &Roots) -> Hardware {
    let devices = blockdev::devices_for(roots, crate::probe::PROBED_PATHS);
    let refs: Vec<&BlockDev> = devices.iter().collect();
    detect_for(roots, &refs)
}

/// [`detect`] for an already-resolved list of managed disks (what [`crate::probe()`] uses).
pub fn detect_for(roots: &Roots, devices: &[&BlockDev]) -> Hardware {
    let cpuinfo = read_lossy(&roots.proc.join("cpuinfo")).unwrap_or_default();
    let cpu = cpu_topology(roots, &cpuinfo);
    let meminfo = read_lossy(&roots.proc.join("meminfo")).unwrap_or_default();
    let mem_total = meminfo_bytes(&meminfo, "MemTotal").unwrap_or(0);
    let swap_total = meminfo_bytes(&meminfo, "SwapTotal").unwrap_or(0);
    let (power, battery_present) = power(roots);
    let chassis_portable = read_lossy(&roots.sys.join("class/dmi/id/chassis_type"))
        .and_then(|s| s.parse::<u32>().ok())
        .is_some_and(|t| PORTABLE_CHASSIS.contains(&t));
    Hardware {
        cpus: cpu.cpus.len() as u32,
        cores: cpu.cores,
        smt: cpu.smt,
        performance_cpus: cpu.performance,
        efficiency_cpus: cpu.efficiency,
        cpu_model: cpu_model(&cpuinfo),
        mem_total,
        swap_total,
        compressed_swap: swap_total > 0 && compressed_swap(roots),
        storage: devices.iter().map(|d| storage_device(&roots.sys, d)).collect(),
        power,
        portable: chassis_portable || battery_present,
        virtualization: virtualization(roots, &cpuinfo),
        arch: arch(roots, &cpuinfo),
        vendor: vendor(&cpuinfo),
        features: features(roots),
        latency: None,
    }
}

// ── architecture, vendor, scheduler/cpufreq features ────────────────────────────────────

/// `x86_64`, `aarch64`, ... from `/proc/sys/kernel/arch`; on the real host (`/proc`) the
/// `uname(2)` machine field; otherwise a guess from cpuinfo (fixtures).
fn arch(roots: &Roots, cpuinfo: &str) -> String {
    if let Some(a) = read_lossy(&roots.proc.join("sys/kernel/arch")).filter(|a| !a.is_empty()) {
        return a;
    }
    if roots.proc == Path::new("/proc")
        && let Some(m) = uname_machine()
    {
        return m;
    }
    if cpuinfo_values(cpuinfo, "vendor_id").next().is_some() {
        "x86_64".into()
    } else if cpuinfo_values(cpuinfo, "CPU implementer").next().is_some() {
        "aarch64".into()
    } else if cpuinfo_values(cpuinfo, "isa").next().is_some_and(|i| i.starts_with("rv64")) {
        "riscv64".into()
    } else {
        String::new()
    }
}

fn uname_machine() -> Option<String> {
    // SAFETY: `uname` fills the zeroed struct; the fields are NUL-terminated C strings.
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 {
        return None;
    }
    let bytes: Vec<u8> = u.machine.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8(bytes).ok().filter(|s| !s.is_empty())
}

/// CPU vendor from `vendor_id` (x86) or the ARM `CPU implementer` code.
pub fn vendor(cpuinfo: &str) -> CpuVendor {
    if let Some(v) = cpuinfo_values(cpuinfo, "vendor_id").next() {
        return match v {
            "GenuineIntel" => CpuVendor::Intel,
            "AuthenticAMD" | "HygonGenuine" => CpuVendor::Amd,
            _ => CpuVendor::Other,
        };
    }
    match cpuinfo_values(cpuinfo, "CPU implementer").next().map(str::to_ascii_lowercase).as_deref() {
        Some("0x41") => CpuVendor::Arm,
        Some("0x61") => CpuVendor::Apple,
        Some("0x51") => CpuVendor::Qualcomm,
        Some("0xc0") => CpuVendor::Ampere,
        Some(_) => CpuVendor::Other,
        None => CpuVendor::Unknown,
    }
}

/// Lower-case vendor name (`intel`, `amd`, `arm`, `apple`, ...), as profiles match it.
pub fn vendor_name(v: CpuVendor) -> &'static str {
    match v {
        CpuVendor::Intel => "intel",
        CpuVendor::Amd => "amd",
        CpuVendor::Arm => "arm",
        CpuVendor::Apple => "apple",
        CpuVendor::Qualcomm => "qualcomm",
        CpuVendor::Ampere => "ampere",
        CpuVendor::Other => "other",
        CpuVendor::Unknown => "unknown",
    }
}

fn features(roots: &Roots) -> CpuFeatures {
    let flag = |rel: &str| read_lossy(&roots.proc.join(rel)).is_some_and(|v| v == "1");
    let base = roots.sys.join("devices/system/cpu/cpufreq");
    // The lowest-numbered policy (policy0, policy1, ..., not lexical policy10).
    let policy = fs::read_dir(&base)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| {
            let n: u32 = e.file_name().to_str()?.strip_prefix("policy")?.parse().ok()?;
            Some((n, e.path()))
        })
        .min_by_key(|(n, _)| *n)
        .map(|(_, p)| p);
    let attr = |f: &str| policy.as_ref().and_then(|p| read_lossy(&p.join(f))).filter(|v| !v.is_empty());
    let list = |f: &str| attr(f).map(|v| v.split_whitespace().map(str::to_string).collect()).unwrap_or_default();
    let numa_nodes = read_lossy(&roots.sys.join("devices/system/node/online"))
        .and_then(|s| parse_cpulist(&s))
        .map_or(0, |n| n.len() as u32);
    CpuFeatures {
        eas: flag("sys/kernel/sched_energy_aware"),
        itmt: flag("sys/kernel/sched_itmt_enabled"),
        cpufreq_driver: attr("scaling_driver"),
        governor: attr("scaling_governor"),
        epp: attr("energy_performance_preference"),
        epp_available: list("energy_performance_available_preferences"),
        governors_available: list("scaling_available_governors"),
        numa_nodes,
    }
}

// ── CPU ─────────────────────────────────────────────────────────────────────────────────

/// Parse a kernel CPU list (`0-3,8,10-11`). Malformed input yields `None`.
pub fn parse_cpulist(s: &str) -> Option<Vec<u32>> {
    let s = s.trim();
    let mut out = BTreeSet::new();
    if s.is_empty() {
        return Some(Vec::new());
    }
    for part in s.split(',') {
        let part = part.trim();
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (u32, u32) = (a.parse().ok()?, b.parse().ok()?);
                if b < a || b - a > 65_536 {
                    return None;
                }
                out.extend(a..=b);
            }
            None => {
                out.insert(part.parse().ok()?);
            }
        }
    }
    Some(out.into_iter().collect())
}

struct CpuTopology {
    cpus: Vec<u32>,
    cores: u32,
    smt: bool,
    performance: Vec<u32>,
    efficiency: Vec<u32>,
}

fn cpu_topology(roots: &Roots, cpuinfo: &str) -> CpuTopology {
    let base = roots.sys.join("devices/system/cpu");
    let cpus = read_lossy(&base.join("online")).and_then(|s| parse_cpulist(&s)).unwrap_or_else(|| {
        let n = cpuinfo.lines().filter(|l| key_of(l) == Some("processor")).count() as u32;
        (0..n).collect()
    });
    let cores: BTreeSet<String> = cpus
        .iter()
        .map(|c| {
            let topo = base.join(format!("cpu{c}/topology"));
            read_lossy(&topo.join("core_cpus_list"))
                .or_else(|| read_lossy(&topo.join("thread_siblings_list")))
                .and_then(|s| parse_cpulist(&s))
                .filter(|l| !l.is_empty())
                .map_or_else(|| format!("cpu{c}"), |l| format!("{l:?}"))
        })
        .collect();
    let cores = cores.len() as u32;
    let smt = match read_lossy(&base.join("smt/active")).as_deref() {
        Some("1") => true,
        Some("0") => false,
        _ => cores < cpus.len() as u32,
    };
    let (performance, efficiency) = hybrid_split(roots, &cpus).unwrap_or_default();
    CpuTopology { cpus, cores, smt, performance, efficiency }
}

/// Performance and efficiency CPUs on a hybrid CPU, `None` on a uniform one.
fn hybrid_split(roots: &Roots, online: &[u32]) -> Option<(Vec<u32>, Vec<u32>)> {
    let on: BTreeSet<u32> = online.iter().copied().collect();
    let keep = |v: Vec<u32>| -> Vec<u32> { v.into_iter().filter(|c| on.contains(c)).collect() };
    let nonempty = |(p, e): (Vec<u32>, Vec<u32>)| (!p.is_empty() && !e.is_empty()).then_some((p, e));

    // Intel hybrid PMUs.
    let pmu = |n: &str| read_lossy(&roots.sys.join(format!("devices/{n}/cpus"))).and_then(|s| parse_cpulist(&s));
    if let (Some(p), Some(e)) = (pmu("cpu_core"), pmu("cpu_atom"))
        && let Some(split) = nonempty((keep(p), keep(e)))
    {
        return Some(split);
    }

    let base = roots.sys.join("devices/system/cpu");
    let per_cpu = |rel: &str| -> Option<Vec<(u32, u64)>> {
        online.iter().map(|c| read_lossy(&base.join(format!("cpu{c}/{rel}")))?.parse().ok().map(|v| (*c, v))).collect()
    };

    // Asymmetric CPU capacity (arm64 big.LITTLE/DynamIQ, and anything else that exports it).
    if let Some(caps) = per_cpu("cpu_capacity").filter(|v| !v.is_empty()) {
        let top = caps.iter().map(|(_, v)| *v).max().unwrap_or(0);
        let p: Vec<u32> = caps.iter().filter(|(_, v)| *v == top).map(|(c, _)| *c).collect();
        let e: Vec<u32> = caps.iter().filter(|(_, v)| *v != top).map(|(c, _)| *c).collect();
        if let Some(split) = nonempty((p, e)) {
            return Some(split);
        }
    }

    // Fallback: two clearly separated max-frequency clusters.
    let freqs = per_cpu("cpufreq/cpuinfo_max_freq").filter(|v| !v.is_empty())?;
    let distinct: BTreeSet<u64> = freqs.iter().map(|(_, f)| *f).filter(|f| *f > 0).collect();
    let distinct: Vec<u64> = distinct.into_iter().collect();
    let mut clusters: Vec<Vec<u64>> = Vec::new();
    for f in distinct {
        match clusters.last_mut() {
            Some(c) if (f as f64) < *c.last().expect("non-empty") as f64 * FREQ_GAP => c.push(f),
            _ => clusters.push(vec![f]),
        }
    }
    if clusters.len() != 2 {
        return None;
    }
    let split_at = clusters[1][0];
    let p = freqs.iter().filter(|(_, f)| *f >= split_at).map(|(c, _)| *c).collect();
    let e = freqs.iter().filter(|(_, f)| *f < split_at).map(|(c, _)| *c).collect();
    nonempty((p, e))
}

/// Max-frequency clusters must be at least this far apart to count as hybrid (P/E), so the
/// few-percent spread of "favoured cores" on uniform CPUs is not mistaken for one.
const FREQ_GAP: f64 = 1.15;

fn key_of(line: &str) -> Option<&str> {
    line.split_once(':').map(|(k, _)| k.trim())
}

fn cpuinfo_values<'a>(cpuinfo: &'a str, key: &str) -> impl Iterator<Item = &'a str> {
    cpuinfo.lines().filter_map(move |l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == key).then(|| v.trim())
    })
}

/// Core names by (`CPU implementer`, `CPU part`).
const ARM_PARTS: &[(&str, &str, &str)] = &[
    ("0x41", "0xd03", "Cortex-A53"),
    ("0x41", "0xd04", "Cortex-A35"),
    ("0x41", "0xd05", "Cortex-A55"),
    ("0x41", "0xd07", "Cortex-A57"),
    ("0x41", "0xd08", "Cortex-A72"),
    ("0x41", "0xd09", "Cortex-A73"),
    ("0x41", "0xd0a", "Cortex-A75"),
    ("0x41", "0xd0b", "Cortex-A76"),
    ("0x41", "0xd0c", "Neoverse-N1"),
    ("0x41", "0xd0d", "Cortex-A77"),
    ("0x41", "0xd40", "Neoverse-V1"),
    ("0x41", "0xd41", "Cortex-A78"),
    ("0x41", "0xd44", "Cortex-X1"),
    ("0x41", "0xd46", "Cortex-A510"),
    ("0x41", "0xd47", "Cortex-A710"),
    ("0x41", "0xd48", "Cortex-X2"),
    ("0x41", "0xd49", "Neoverse-N2"),
    ("0x41", "0xd4d", "Cortex-A715"),
    ("0x41", "0xd4e", "Cortex-X3"),
    ("0x41", "0xd4f", "Neoverse-V2"),
    ("0x41", "0xd80", "Cortex-A520"),
    ("0x41", "0xd81", "Cortex-A720"),
    ("0x41", "0xd82", "Cortex-X4"),
    ("0x61", "0x022", "Icestorm"),
    ("0x61", "0x023", "Firestorm"),
    ("0x61", "0x024", "Icestorm"),
    ("0x61", "0x025", "Firestorm"),
    ("0x61", "0x028", "Icestorm"),
    ("0x61", "0x029", "Firestorm"),
    ("0x61", "0x032", "Blizzard"),
    ("0x61", "0x033", "Avalanche"),
    ("0x51", "0x001", "Oryon"),
    ("0xc0", "0xac3", "AmpereOne"),
];

/// The CPU's marketing name, tidied (`(R)`/`(TM)` dropped, whitespace collapsed).
fn cpu_model(cpuinfo: &str) -> String {
    let tidy = |s: &str| s.replace("(R)", "").replace("(TM)", "").split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(m) = cpuinfo_values(cpuinfo, "model name").find(|v| !v.is_empty()) {
        return tidy(m);
    }
    let mut parts: Vec<String> = Vec::new();
    for block in cpuinfo.split("\n\n") {
        let Some(p) = cpuinfo_values(block, "CPU part").next().map(str::to_ascii_lowercase) else { continue };
        let imp = cpuinfo_values(block, "CPU implementer").next().map(str::to_ascii_lowercase).unwrap_or_default();
        let name = ARM_PARTS
            .iter()
            .find(|(i, id, _)| *i == imp && *id == p)
            .map_or_else(|| format!("CPU part {p}"), |(_, _, n)| n.to_string());
        if !parts.contains(&name) {
            parts.push(name);
        }
    }
    let hw = cpuinfo_values(cpuinfo, "Hardware").next().filter(|v| !v.is_empty()).map(tidy);
    match (hw, parts.is_empty()) {
        (Some(hw), true) => hw,
        (Some(hw), false) => format!("{hw} ({})", parts.join(" + ")),
        (None, false) => parts.join(" + "),
        (None, true) => String::new(),
    }
}

// ── memory ──────────────────────────────────────────────────────────────────────────────

fn meminfo_bytes(meminfo: &str, key: &str) -> Option<u64> {
    let v = cpuinfo_values(meminfo, key).next()?;
    let mut it = v.split_whitespace();
    let n: u64 = it.next()?.parse().ok()?;
    Some(match it.next() {
        Some("kB") => n * KIB,
        _ => n,
    })
}

fn compressed_swap(roots: &Roots) -> bool {
    let zram = read_lossy(&roots.proc.join("swaps"))
        .is_some_and(|s| s.lines().skip(1).filter_map(|l| l.split_whitespace().next()).any(|f| f.contains("/zram")));
    let zswap = read_lossy(&roots.sys.join("module/zswap/parameters/enabled"))
        .is_some_and(|v| matches!(v.as_str(), "Y" | "y" | "1"));
    zram || zswap
}

// ── storage ─────────────────────────────────────────────────────────────────────────────

/// Rough sustained sequential bandwidth by device kind, bytes/s. Used to scale I/O throttles
/// (the `threshold` policy's AIMD floor and step); not a measurement.
///
/// | Kind | MiB/s |
/// |---|---|
/// | NVMe | 2000 |
/// | SSD (SATA/SAS) | 500 |
/// | HDD | 150 |
/// | eMMC | 250 |
/// | SD card | 50 |
/// | Virtual (virtio, Xen, VMware, ...) | 500 |
/// | Network (nbd, rbd, iSCSI, NVMe-oF) | 100 |
/// | Unknown | 300 |
pub fn nominal_bps(kind: StorageKind) -> u64 {
    MIB * match kind {
        StorageKind::Nvme => 2000,
        StorageKind::Ssd => 500,
        StorageKind::Hdd => 150,
        StorageKind::Emmc => 250,
        StorageKind::Sd => 50,
        StorageKind::Virtual => 500,
        StorageKind::Network => 100,
        StorageKind::Unknown => 300,
    }
}

/// Short human label: `NVMe`, `SSD`, `HDD`, `eMMC`, `SD`, `virtual`, `network`, `disk`.
pub fn kind_label(kind: StorageKind) -> &'static str {
    match kind {
        StorageKind::Nvme => "NVMe",
        StorageKind::Ssd => "SSD",
        StorageKind::Hdd => "HDD",
        StorageKind::Emmc => "eMMC",
        StorageKind::Sd => "SD",
        StorageKind::Virtual => "virtual",
        StorageKind::Network => "network",
        StorageKind::Unknown => "disk",
    }
}

/// Virtual-disk vendors reported in `device/vendor` (SCSI emulation in hypervisors).
const VIRTUAL_VENDORS: &[&str] = &["QEMU", "VMware", "VBOX", "Msft", "Xen", "Red Hat"];

/// Classify a whole disk, first match wins:
///
/// 1. `nbd*`, `rbd*`, or an iSCSI session in the sysfs path → Network;
///    `nvme*` over `tcp`/`rdma`/`fc` (`device/transport`) → Network
/// 2. `nvme*`, or `device/transport` = `pcie` → Nvme
/// 3. `vd*`, `xvd*`, `zram*`, `loop*`, a virtio/Xen sysfs path, or a hypervisor
///    `device/vendor` (QEMU, VMware, VBOX, Msft, ...) → Virtual (checked before rotational,
///    because virtio disks often report `rotational=1`)
/// 4. `mmcblk*`: `device/type` `SD` → Sd, otherwise (`MMC`) → Emmc
/// 5. `queue/rotational` = 1 → Hdd, = 0 → Ssd, unreadable → Unknown
pub fn storage_kind(sys: &Path, name: &str, rotational: Option<bool>) -> StorageKind {
    let dir = sys.join("block").join(name);
    let dir = if dir.exists() { dir } else { sys.join("class/block").join(name) };
    let device = dir.join("device");
    let transport = read_lossy(&device.join("transport"));
    let sys_canon = fs::canonicalize(sys).unwrap_or_else(|_| sys.to_path_buf());
    let path = fs::canonicalize(&dir)
        .ok()
        .and_then(|p| p.strip_prefix(&sys_canon).ok().map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| name.starts_with(p));

    if starts(&["nbd", "rbd"]) || path.contains("/session") {
        return StorageKind::Network;
    }
    if starts(&["nvme"]) || transport.as_deref() == Some("pcie") {
        return match transport.as_deref() {
            Some("tcp" | "rdma" | "fc") => StorageKind::Network,
            _ => StorageKind::Nvme,
        };
    }
    let vendor = read_lossy(&device.join("vendor")).unwrap_or_default();
    if starts(&["vd", "xvd", "zram", "loop"])
        || path.contains("/virtio")
        || path.contains("/xen")
        || path.contains("/vbd-")
        || VIRTUAL_VENDORS.iter().any(|v| vendor.eq_ignore_ascii_case(v) || vendor.starts_with(v))
    {
        return StorageKind::Virtual;
    }
    if starts(&["mmcblk"]) {
        return match read_lossy(&device.join("type")).as_deref() {
            Some("SD") => StorageKind::Sd,
            _ => StorageKind::Emmc,
        };
    }
    match rotational {
        Some(true) => StorageKind::Hdd,
        Some(false) => StorageKind::Ssd,
        None => StorageKind::Unknown,
    }
}

fn storage_device(sys: &Path, d: &BlockDev) -> StorageDevice {
    let kind = storage_kind(sys, &d.name, d.rotational);
    StorageDevice {
        dev: d.dev.to_string(),
        name: d.name.clone(),
        kind,
        scheduler: d.scheduler.clone().unwrap_or_default(),
        nominal_bps: nominal_bps(kind),
    }
}

// ── power ───────────────────────────────────────────────────────────────────────────────

/// DMI chassis types of portable machines: Portable, Laptop, Notebook, Sub Notebook,
/// Tablet, Convertible, Detachable.
pub const PORTABLE_CHASSIS: &[u32] = &[8, 9, 10, 14, 30, 31, 32];

/// The power source, and whether a system battery exists.
///
/// Online Mains/USB supply → AC. A system battery (`scope` not `Device`) discharging → Battery.
/// A system battery charging/full/not charging → AC. No system battery (desktops, or only
/// peripheral batteries such as a wireless mouse) → AC. Anything else → Unknown.
fn power(roots: &Roots) -> (PowerSource, bool) {
    let dir = roots.sys.join("class/power_supply");
    let Ok(rd) = fs::read_dir(&dir) else { return (PowerSource::Ac, false) };
    let mut mains_online = false;
    let mut batteries = Vec::new();
    for e in rd.filter_map(Result::ok) {
        let p = e.path();
        let ty = read_lossy(&p.join("type")).unwrap_or_default();
        let mains = matches!(ty.as_str(), "Mains" | "USB" | "USB_C" | "USB_PD" | "USB_PD_DRP");
        if mains && read_lossy(&p.join("online")).as_deref() == Some("1") {
            mains_online = true;
        }
        // Peripheral batteries (scope "Device": mice, headsets) don't power the machine.
        if ty == "Battery" && read_lossy(&p.join("scope")).as_deref() != Some("Device") {
            batteries.push(read_lossy(&p.join("status")).unwrap_or_default());
        }
    }
    let present = !batteries.is_empty();
    let source = if mains_online || batteries.is_empty() {
        PowerSource::Ac
    } else if batteries.iter().any(|s| s == "Discharging") {
        PowerSource::Battery
    } else if batteries.iter().any(|s| matches!(s.as_str(), "Charging" | "Full" | "Not charging")) {
        PowerSource::Ac
    } else {
        PowerSource::Unknown
    };
    (source, present)
}

// ── virtualization ──────────────────────────────────────────────────────────────────────

/// Names [`Hardware::virtualization`] uses for containers (as opposed to VMs).
pub const CONTAINERS: &[&str] =
    &["docker", "podman", "lxc", "lxc-libvirt", "systemd-nspawn", "container-other", "oci", "rkt", "openvz", "wsl1"];

/// True if `virt` (a [`Hardware::virtualization`] value) names a container, where cgroup
/// and I/O control are usually restricted by the container runtime.
pub fn is_container(virt: &str) -> bool {
    CONTAINERS.contains(&virt)
}

/// The filesystem root `/` under these roots (the parent of `/etc`), for markers such as
/// `/.dockerenv` that live outside every explicit root.
fn fs_root(roots: &Roots) -> &Path {
    roots.etc.parent().unwrap_or(Path::new("/"))
}

fn virtualization(roots: &Roots, cpuinfo: &str) -> Option<String> {
    let release = read_lossy(&roots.proc.join("sys/kernel/osrelease")).unwrap_or_default().to_ascii_lowercase();
    if release.contains("microsoft") || release.contains("wsl") {
        return Some("wsl".into());
    }

    // Containers: systemd's marker, runtime marker files, PID 1's environment.
    if let Some(c) = read_lossy(&roots.run.join("systemd/container")).filter(|s| !s.is_empty()) {
        return Some(normalize_container(&c));
    }
    if fs_root(roots).join(".dockerenv").exists() {
        return Some("docker".into());
    }
    if roots.run.join(".containerenv").exists() {
        return Some("podman".into());
    }
    if let Ok(env) = fs::read(roots.proc.join("1/environ"))
        && let Some(c) = env
            .split(|b| *b == 0)
            .filter_map(|kv| std::str::from_utf8(kv).ok()?.strip_prefix("container="))
            .find(|v| !v.is_empty())
    {
        return Some(normalize_container(c));
    }

    // Virtual machines.
    if let Some(h) = read_lossy(&roots.sys.join("hypervisor/type")).filter(|s| !s.is_empty()) {
        return Some(h);
    }
    let flag = cpuinfo_values(cpuinfo, "flags").next().is_some_and(|f| f.split_whitespace().any(|x| x == "hypervisor"));
    let dmi = |f: &str| read_lossy(&roots.sys.join("class/dmi/id").join(f)).unwrap_or_default();
    let (vendor, product, bios, board) =
        (dmi("sys_vendor"), dmi("product_name"), dmi("bios_vendor"), dmi("board_vendor"));
    let any = |needle: &str| [&vendor, &product, &bios, &board].iter().any(|s| s.contains(needle));
    let found = if any("QEMU") || any("KVM") {
        Some(if flag || product.contains("KVM") { "kvm" } else { "qemu" })
    } else if any("VMware") {
        Some("vmware")
    } else if any("innotek") || any("VirtualBox") {
        Some("virtualbox")
    } else if vendor.contains("Microsoft") && product.contains("Virtual Machine") {
        Some("hyperv")
    } else if any("Xen") {
        Some("xen")
    } else if any("Parallels") {
        Some("parallels")
    } else if any("Bochs") {
        Some("bochs")
    } else if flag && vendor.contains("Amazon") {
        Some("amazon")
    } else if flag && vendor.contains("Google") {
        Some("google")
    } else if flag {
        Some("vm")
    } else {
        None
    };
    found.map(str::to_string)
}

fn normalize_container(c: &str) -> String {
    match c.trim() {
        "systemd-nspawn" | "lxc" | "lxc-libvirt" | "docker" | "podman" | "rkt" | "openvz" | "oci" => c.trim().into(),
        _ => "container-other".into(),
    }
}

// ── formatting ──────────────────────────────────────────────────────────────────────────

/// `62.5 GiB`, `8 GiB`, `512 MiB`.
pub fn fmt_bytes(b: u64) -> String {
    if b >= GIB {
        let g = b as f64 / GIB as f64;
        let s = format!("{g:.1}");
        format!("{} GiB", s.strip_suffix(".0").unwrap_or(&s))
    } else {
        format!("{} MiB", b / MIB)
    }
}

/// Performance cores on a hybrid CPU, assuming efficiency cores have one thread each
/// (true for Intel E-cores and ARM LITTLE cores).
fn performance_cores(hw: &Hardware) -> u32 {
    let p = hw.performance_cpus.len() as u32;
    let rest = hw.cores.saturating_sub(hw.efficiency_cpus.len() as u32);
    if hw.cores > 0 && rest > 0 && rest <= p { rest } else { p }
}

/// `AMD Ryzen 9 7950X — 16 cores / 32 threads`, or on hybrid CPUs
/// `Intel Core Ultra 7 155H — hybrid: 6P (12 threads) + 10E`.
pub fn describe_cpu(hw: &Hardware) -> String {
    let model = if hw.cpu_model.is_empty() { "unknown CPU" } else { hw.cpu_model.as_str() };
    if hw.cpus == 0 {
        return model.to_string();
    }
    if hw.is_hybrid() {
        let pc = performance_cores(hw);
        let pt = hw.performance_cpus.len() as u32;
        let p = if pc == pt { format!("{pc}P") } else { format!("{pc}P ({pt} threads)") };
        return format!("{model} — hybrid: {p} + {}E", hw.efficiency_cpus.len());
    }
    if hw.cores > 0 && hw.cores != hw.cpus {
        format!("{model} — {} cores / {} threads", hw.cores, hw.cpus)
    } else {
        format!("{model} — {} cores", hw.cpus)
    }
}

/// `62.5 GiB RAM, 8 GiB swap (compressed)`.
pub fn describe_memory(hw: &Hardware) -> String {
    let ram = if hw.mem_total == 0 { "unknown RAM".to_string() } else { format!("{} RAM", fmt_bytes(hw.mem_total)) };
    if hw.swap_total == 0 {
        format!("{ram}, no swap")
    } else {
        let z = if hw.compressed_swap { " (compressed)" } else { "" };
        format!("{ram}, {} swap{z}", fmt_bytes(hw.swap_total))
    }
}

/// `nvme0n1 NVMe (bfq)`.
pub fn describe_storage(s: &StorageDevice) -> String {
    let sched = if s.scheduler.is_empty() { String::new() } else { format!(" ({})", s.scheduler) };
    format!("{} {}{sched}", s.name, kind_label(s.kind))
}

/// `AC (laptop)`, `battery`, `AC`.
pub fn describe_power(hw: &Hardware) -> String {
    let src = match hw.power {
        PowerSource::Ac => "AC",
        PowerSource::Battery => "battery",
        PowerSource::Unknown => "unknown",
    };
    if hw.portable { format!("{src} (laptop)") } else { src.to_string() }
}

/// `x86_64, intel; intel_pstate/powersave, EPP balance_performance; ITMT; 1 NUMA node`.
pub fn describe_features(hw: &Hardware) -> String {
    let f = &hw.features;
    let mut groups = Vec::new();
    let arch = if hw.arch.is_empty() { "unknown arch" } else { hw.arch.as_str() };
    groups.push(format!("{arch}, {}", vendor_name(hw.vendor)));
    let mut freq = Vec::new();
    match (&f.cpufreq_driver, &f.governor) {
        (Some(d), Some(g)) => freq.push(format!("{d}/{g}")),
        (Some(d), None) => freq.push(d.clone()),
        (None, Some(g)) => freq.push(g.clone()),
        (None, None) => freq.push("no cpufreq".into()),
    }
    if let Some(e) = &f.epp {
        freq.push(format!("EPP {e}"));
    }
    groups.push(freq.join(", "));
    let mut sched = Vec::new();
    if f.eas {
        sched.push("EAS".to_string());
    }
    if f.itmt {
        sched.push("ITMT".to_string());
    }
    if f.numa_nodes > 0 {
        sched.push(format!("{} NUMA node{}", f.numa_nodes, if f.numa_nodes == 1 { "" } else { "s" }));
    }
    if !sched.is_empty() {
        groups.push(sched.join(", "));
    }
    groups.join("; ")
}

/// One line for daemon start-up logs, e.g.
/// `32×CPU (16C, SMT), 62.5 GiB RAM, nvme0n1 NVMe, AC`.
pub fn summary_line(hw: &Hardware) -> String {
    let mut cpu_bits = Vec::new();
    if hw.cores > 0 {
        cpu_bits.push(format!("{}C", hw.cores));
    }
    if hw.smt {
        cpu_bits.push("SMT".to_string());
    }
    if hw.is_hybrid() {
        cpu_bits.push(format!("hybrid {}P+{}E", performance_cores(hw), hw.efficiency_cpus.len()));
    }
    let cpus = if hw.cpus == 0 { "?".to_string() } else { hw.cpus.to_string() };
    let cpus = if hw.arch.is_empty() { cpus } else { format!("{} {cpus}", hw.arch) };
    let mut parts = vec![if cpu_bits.is_empty() {
        format!("{cpus}×CPU")
    } else {
        format!("{cpus}×CPU ({})", cpu_bits.join(", "))
    }];
    parts.push(if hw.mem_total == 0 { "? RAM".into() } else { format!("{} RAM", fmt_bytes(hw.mem_total)) });
    if hw.storage.is_empty() {
        parts.push("no managed disk".into());
    }
    parts.extend(hw.storage.iter().map(|s| format!("{} {}", s.name, kind_label(s.kind))));
    parts.push(match hw.power {
        PowerSource::Ac => "AC".into(),
        PowerSource::Battery => "battery".into(),
        PowerSource::Unknown => "power unknown".into(),
    });
    if let Some(v) = &hw.virtualization {
        parts.push(v.clone());
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{self, Fixture};
    use crate::probe::probe;
    use crate::{Env, Probe};

    fn run(build: fn(&Path) -> (Roots, Env)) -> (tempfile::TempDir, Probe) {
        let d = tempfile::tempdir().unwrap();
        let (roots, env) = build(d.path());
        let p = probe(&roots, &env);
        (d, p)
    }

    #[test]
    fn cpulist() {
        assert_eq!(parse_cpulist("0-3,8,10-11"), Some(vec![0, 1, 2, 3, 8, 10, 11]));
        assert_eq!(parse_cpulist("0\n"), Some(vec![0]));
        assert_eq!(parse_cpulist(""), Some(vec![]));
        assert_eq!(parse_cpulist("3-1"), None);
        assert_eq!(parse_cpulist("a"), None);
    }

    #[test]
    fn dev_host_hybrid_desktop() {
        let (_d, p) = run(fixture::pop_os);
        let hw = &p.hardware;
        assert_eq!((hw.cpus, hw.cores, hw.smt), (24, 16, true));
        assert_eq!(hw.performance_cpus, (0..16).collect::<Vec<_>>());
        assert_eq!(hw.efficiency_cpus, (16..24).collect::<Vec<_>>());
        assert_eq!(hw.cpu_model, "12th Gen Intel Core i9-12950HX");
        assert_eq!(describe_cpu(hw), "12th Gen Intel Core i9-12950HX — hybrid: 8P (16 threads) + 8E");
        assert_eq!(hw.mem_total, 65_574_028 * KIB);
        assert!(hw.compressed_swap);
        assert_eq!(hw.storage.len(), 1);
        assert_eq!(hw.storage[0].kind, StorageKind::Nvme);
        assert_eq!(hw.storage[0].scheduler, "none");
        assert_eq!(hw.storage[0].nominal_bps, 2000 * MIB);
        assert_eq!((hw.power, hw.portable), (PowerSource::Ac, false));
        assert_eq!(hw.virtualization, None);
        assert_eq!(summary_line(hw), "x86_64 24×CPU (16C, SMT, hybrid 8P+8E), 62.5 GiB RAM, nvme0n1 NVMe, AC");
        assert_eq!(p.host_caps().hardware.storage[0].dev, "259:0");
    }

    #[test]
    fn intel_hybrid_laptop_on_battery() {
        let (_d, p) = run(fixture::intel_hybrid_laptop);
        let hw = &p.hardware;
        assert_eq!((hw.cpus, hw.cores, hw.smt), (22, 16, true));
        assert_eq!(hw.performance_cpus, (0..12).collect::<Vec<_>>());
        assert_eq!(hw.efficiency_cpus, (12..22).collect::<Vec<_>>());
        assert_eq!(describe_cpu(hw), "Intel Core Ultra 7 155H — hybrid: 6P (12 threads) + 10E");
        assert_eq!((hw.power, hw.portable), (PowerSource::Battery, true));
        assert!(hw.compressed_swap, "zram");
        assert_eq!(hw.storage[0].kind, StorageKind::Nvme);
        assert_eq!(describe_storage(&hw.storage[0]), "nvme0n1 NVMe (none)");
        assert_eq!(describe_power(hw), "battery (laptop)");
        assert_eq!(hw.virtualization, None);
        assert_eq!(summary_line(hw), "x86_64 22×CPU (16C, SMT, hybrid 6P+10E), 31.3 GiB RAM, nvme0n1 NVMe, battery");
    }

    #[test]
    fn arm_big_little_board() {
        let (_d, p) = run(fixture::arm_big_little);
        let hw = &p.hardware;
        assert_eq!((hw.cpus, hw.cores, hw.smt), (8, 8, false));
        assert_eq!(hw.performance_cpus, [4, 5, 6, 7]);
        assert_eq!(hw.efficiency_cpus, [0, 1, 2, 3]);
        assert_eq!(hw.cpu_model, "Cortex-A55 + Cortex-A76");
        assert_eq!(describe_cpu(hw), "Cortex-A55 + Cortex-A76 — hybrid: 4P + 4E");
        assert_eq!(hw.storage[0].kind, StorageKind::Emmc);
        assert_eq!(hw.storage[0].nominal_bps, 250 * MIB);
        assert_eq!((hw.power, hw.portable), (PowerSource::Ac, false), "no supplies = mains");
        assert!(hw.mem_total < 4 * GIB);
        assert!(!hw.compressed_swap && hw.swap_total == 0);
        assert_eq!(describe_memory(hw), "3.8 GiB RAM, no swap");
    }

    #[test]
    fn hdd_desktop() {
        let (_d, p) = run(fixture::hdd_desktop);
        let hw = &p.hardware;
        assert_eq!((hw.cpus, hw.cores, hw.smt), (4, 4, false));
        assert!(!hw.is_hybrid(), "single frequency cluster");
        assert_eq!(hw.cpu_model, "Intel Core i5-4590 CPU @ 3.30GHz");
        assert_eq!(describe_cpu(hw), "Intel Core i5-4590 CPU @ 3.30GHz — 4 cores");
        assert_eq!(hw.storage[0].kind, StorageKind::Hdd);
        assert_eq!(hw.storage[0].scheduler, "none");
        assert_eq!(hw.power, PowerSource::Ac);
        assert!(!hw.portable, "a wireless mouse battery does not make a laptop");
        assert!(!hw.compressed_swap);
        assert_eq!(describe_memory(hw), "15.6 GiB RAM, 2 GiB swap");
    }

    #[test]
    fn kvm_guest() {
        let (_d, p) = run(fixture::kvm_guest);
        let hw = &p.hardware;
        assert_eq!(hw.virtualization.as_deref(), Some("kvm"));
        assert_eq!(hw.storage[0].kind, StorageKind::Virtual, "virtio before rotational=1");
        assert_eq!((hw.cpus, hw.cores, hw.smt), (8, 8, false));
        assert!(!hw.is_hybrid());
        assert_eq!(summary_line(hw), "x86_64 8×CPU (8C), 8 GiB RAM, vda virtual, AC, kvm");
    }

    #[test]
    fn storage_kinds() {
        let d = tempfile::tempdir().unwrap();
        let f = Fixture::new(d.path());
        f.disk("sda", "8:0", "[mq-deadline] none", false)
            .disk("sdb", "8:16", "[mq-deadline] none", true)
            .file("sys/block/sdb/device/vendor", "QEMU    \n")
            .disk("mmcblk1", "179:0", "[none]", false)
            .file("sys/block/mmcblk1/device/type", "SD\n")
            .disk("nbd0", "43:0", "[none]", false)
            .disk("nvme1n1", "259:5", "[none]", false)
            .file("sys/block/nvme1n1/device/transport", "tcp\n");
        let sys = f.roots().sys;
        assert_eq!(storage_kind(&sys, "sda", Some(false)), StorageKind::Ssd);
        assert_eq!(storage_kind(&sys, "sdb", Some(true)), StorageKind::Virtual);
        assert_eq!(storage_kind(&sys, "mmcblk1", Some(false)), StorageKind::Sd);
        assert_eq!(storage_kind(&sys, "nbd0", Some(false)), StorageKind::Network);
        assert_eq!(storage_kind(&sys, "nvme1n1", Some(false)), StorageKind::Network);
        assert_eq!(storage_kind(&sys, "sdz", None), StorageKind::Unknown);
        assert_eq!(nominal_bps(StorageKind::Hdd), 150 * MIB);
    }

    #[test]
    fn freq_cluster_fallback_and_favoured_cores() {
        let d = tempfile::tempdir().unwrap();
        let f = Fixture::new(d.path());
        f.file("sys/devices/system/cpu/online", "0-5\n");
        for c in 0..6 {
            let khz = if c < 2 { 3_200_000 } else { 2_000_000 };
            f.file(&format!("sys/devices/system/cpu/cpu{c}/cpufreq/cpuinfo_max_freq"), format!("{khz}\n"));
        }
        let hw = detect(&f.roots());
        assert_eq!((hw.performance_cpus, hw.efficiency_cpus), (vec![0, 1], vec![2, 3, 4, 5]));

        // Favoured cores a few percent apart are one cluster.
        for c in 0..6 {
            let khz = if c < 2 { 5_700_000 } else { 5_500_000 };
            f.file(&format!("sys/devices/system/cpu/cpu{c}/cpufreq/cpuinfo_max_freq"), format!("{khz}\n"));
        }
        assert!(!detect(&f.roots()).is_hybrid());

        // Three clusters are ambiguous: not hybrid by frequency.
        for (c, khz) in [(0, 1_000_000), (1, 2_000_000), (2, 3_000_000)] {
            f.file(&format!("sys/devices/system/cpu/cpu{c}/cpufreq/cpuinfo_max_freq"), format!("{khz}\n"));
        }
        assert!(!detect(&f.roots()).is_hybrid());
    }

    #[test]
    fn power_states() {
        let d = tempfile::tempdir().unwrap();
        let f = Fixture::new(d.path());
        f.power_supply("AC", &[("type", "Mains"), ("online", "1")])
            .power_supply("BAT0", &[("type", "Battery"), ("status", "Discharging")]);
        let hw = detect(&f.roots());
        assert_eq!((hw.power, hw.portable), (PowerSource::Ac, true), "mains online wins");
        f.power_supply("AC", &[("type", "Mains"), ("online", "0")]);
        assert_eq!(detect(&f.roots()).power, PowerSource::Battery);
        f.power_supply("BAT0", &[("type", "Battery"), ("status", "Full")]);
        assert_eq!(detect(&f.roots()).power, PowerSource::Ac);
        f.power_supply("BAT0", &[("type", "Battery"), ("status", "Unknown")]);
        assert_eq!(detect(&f.roots()).power, PowerSource::Unknown);
    }

    #[test]
    fn containers_and_wsl() {
        let d = tempfile::tempdir().unwrap();
        let f = Fixture::new(d.path());
        f.dir("etc");
        assert_eq!(detect(&f.roots()).virtualization, None);
        f.file("proc/1/environ", b"PATH=/bin\0container=lxc\0");
        assert_eq!(detect(&f.roots()).virtualization.as_deref(), Some("lxc"));
        f.file("run/.containerenv", "");
        assert_eq!(detect(&f.roots()).virtualization.as_deref(), Some("podman"));
        f.file(".dockerenv", "");
        assert_eq!(detect(&f.roots()).virtualization.as_deref(), Some("docker"));
        assert!(is_container("docker") && !is_container("kvm"));
        f.file("proc/sys/kernel/osrelease", "5.15.153.1-microsoft-standard-WSL2\n");
        assert_eq!(detect(&f.roots()).virtualization.as_deref(), Some("wsl"));
    }

    #[test]
    fn arch_vendor_and_features() {
        let (_d, p) = run(fixture::pop_os);
        let hw = &p.hardware;
        assert_eq!((hw.arch.as_str(), hw.vendor), ("x86_64", CpuVendor::Intel));
        assert_eq!(hw.features.cpufreq_driver.as_deref(), Some("intel_pstate"));
        assert_eq!(hw.features.governor.as_deref(), Some("powersave"));
        assert_eq!(hw.features.epp.as_deref(), Some("balance_performance"));
        assert_eq!(hw.features.epp_available.len(), 5);
        assert_eq!(hw.features.governors_available, ["performance", "powersave"]);
        assert_eq!((hw.features.numa_nodes, hw.features.eas, hw.features.itmt), (1, false, false));
        assert_eq!(
            describe_features(hw),
            "x86_64, intel; intel_pstate/powersave, EPP balance_performance; 1 NUMA node"
        );

        let (_d, p) = run(fixture::intel_hybrid_laptop);
        assert!(p.hardware.features.itmt);
        assert_eq!(p.hardware.arch, "x86_64", "inferred from cpuinfo without /proc/sys/kernel/arch");

        let (_d, p) = run(fixture::amd_desktop);
        let hw = &p.hardware;
        assert_eq!((hw.vendor, hw.cores, hw.cpus, hw.is_hybrid()), (CpuVendor::Amd, 16, 32, false));
        assert_eq!(hw.cpu_model, "AMD Ryzen 9 7950X 16-Core Processor");
        assert_eq!(hw.features.cpufreq_driver.as_deref(), Some("amd-pstate-epp"));
        assert_eq!(hw.features.epp.as_deref(), Some("power"));
        assert!(hw.compressed_swap, "zswap on");

        let (_d, p) = run(fixture::arm_big_little);
        let hw = &p.hardware;
        assert_eq!((hw.arch.as_str(), hw.vendor, hw.features.eas), ("aarch64", CpuVendor::Arm, true));
        assert_eq!(describe_features(hw), "aarch64, arm; cpufreq-dt/schedutil; EAS");

        let (_d, p) = run(fixture::asahi_m1);
        let hw = &p.hardware;
        assert_eq!((hw.arch.as_str(), hw.vendor, hw.features.eas), ("aarch64", CpuVendor::Apple, false));
        assert_eq!(hw.cpu_model, "Icestorm + Firestorm");
        assert_eq!((hw.performance_cpus.clone(), hw.efficiency_cpus.clone()), (vec![4, 5, 6, 7], vec![0, 1, 2, 3]));
        assert_eq!(hw.power, PowerSource::Battery);
        assert_eq!(vendor_name(hw.vendor), "apple");
        assert_eq!(hw.latency, None, "latency comes from calibration, never detection");
    }

    #[test]
    fn microsoft_laptops_are_not_vms() {
        let d = tempfile::tempdir().unwrap();
        let f = Fixture::new(d.path());
        f.dmi(&[("sys_vendor", "Microsoft Corporation"), ("product_name", "Surface Laptop 5")]);
        assert_eq!(detect(&f.roots()).virtualization, None);
        f.dmi(&[("product_name", "Virtual Machine")]);
        assert_eq!(detect(&f.roots()).virtualization.as_deref(), Some("hyperv"));
    }

    #[test]
    fn empty_tree_is_unknown_hardware() {
        let d = tempfile::tempdir().unwrap();
        let hw = detect(&Roots::under(d.path()));
        assert_eq!(hw.cpus, 0);
        assert_eq!(hw.power, PowerSource::Ac, "no power_supply class: assume mains");
        assert!(hw.storage.is_empty());
        assert_eq!(summary_line(&hw), "?×CPU, ? RAM, no managed disk, AC");
        assert_eq!(fmt_bytes(512 * MIB), "512 MiB");
    }

    #[test]
    #[ignore = "reads the real host"]
    fn real_host() {
        let hw = detect(&Roots::host());
        eprintln!("{}", serde_json::to_string_pretty(&hw).unwrap());
        eprintln!("{}", summary_line(&hw));
        eprintln!("{}", describe_cpu(&hw));
    }
}
