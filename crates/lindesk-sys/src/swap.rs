//! Swap analysis: what swap exists, how it is tuned, how hard it is used, and what to change.
//!
//! [`analyze`] reads `/proc/swaps`, the zram/zswap state, the `vm.*` sysctls that shape
//! reclaim, MGLRU, the swap counters in `/proc/vmstat` and `memory.swap.current` of every
//! unit, and turns them into [`SwapFinding`]s. Each finding carries **proposed change ops**
//! in the `ChangeOp` JSON shape of `docs/CONTROL-PLANE.md` §4 (for example
//! `{"op":"sysctl","key":"vm.swappiness","value":"10"}`), so `lindeskctl`, the MCP server
//! or an agent can plan them through the guardrailed change store. lindesk never creates
//! or removes swap itself; that advice has no ops.

use std::path::Path;
use std::time::{Duration, Instant};

use lindesk_plugin_api::{Hardware, StorageKind};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::Roots;
use crate::blockdev::{self, DevId};
use crate::doctor::Severity;
use crate::hardware::{fmt_bytes, storage_kind};
use crate::util::{read_lossy, subdirs};

const KIB: u64 = 1024;
const GIB: u64 = 1024 * 1024 * 1024;

/// What backs a swap area.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SwapKind {
    /// Compressed RAM block device.
    Zram,
    Partition,
    File,
}

/// One line of `/proc/swaps`, resolved.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SwapDevice {
    pub path: String,
    pub kind: SwapKind,
    /// Whole disk behind a partition or file (`MAJ:MIN`), when resolvable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backing_dev: Option<String>,
    /// Kind of that disk (zram: none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backing: Option<StorageKind>,
    /// zram compression algorithm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compressor: Option<String>,
    pub size_bytes: u64,
    pub used_bytes: u64,
    pub priority: i32,
}

/// zswap module parameters.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Zswap {
    pub enabled: bool,
    pub compressor: Option<String>,
    pub zpool: Option<String>,
    pub max_pool_percent: Option<u32>,
    pub shrinker_enabled: Option<bool>,
}

/// Reclaim-related sysctls (`None` when unreadable).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct VmTunables {
    pub swappiness: Option<u32>,
    pub page_cluster: Option<u32>,
    pub watermark_scale_factor: Option<u32>,
    pub vfs_cache_pressure: Option<u32>,
    pub min_free_kbytes: Option<u64>,
}

/// Multi-gen LRU (`/sys/kernel/mm/lru_gen/enabled`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Mglru {
    /// Raw bitmask, e.g. `0x0007`.
    pub raw: String,
    /// Bit 0 set: MGLRU is in use.
    pub enabled: bool,
}

/// Swap counters from `/proc/vmstat` (pages since boot) and when they were read.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct VmCounters {
    /// Milliseconds on a monotonic clock local to this process; only differences matter.
    pub at_ms: u64,
    pub pswpin: u64,
    pub pswpout: u64,
}

/// Swap-in/out rates over an interval, pages/s.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct SwapRates {
    pub interval_ms: u64,
    pub pswpin_per_s: f64,
    pub pswpout_per_s: f64,
}

/// A unit (cgroup) and how much of it is swapped out.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UnitSwap {
    /// Cgroup path relative to the cgroup root, e.g. `/system.slice/docker.service`.
    pub cgroup: String,
    pub swap_bytes: u64,
}

/// One piece of advice, with the change ops that would implement it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SwapFinding {
    pub severity: Severity,
    pub title: String,
    pub detail: String,
    /// `ChangeOp` JSON values (CONTROL-PLANE.md §4); empty for advice lindesk can't apply.
    #[serde(default)]
    pub proposed_ops: Vec<serde_json::Value>,
}

/// Everything [`analyze`] found.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct SwapReport {
    pub mem_total: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub devices: Vec<SwapDevice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zswap: Option<Zswap>,
    pub vm: VmTunables,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mglru: Option<Mglru>,
    pub counters: VmCounters,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rates: Option<SwapRates>,
    /// Top 10 units by `memory.swap.current`.
    pub top_units: Vec<UnitSwap>,
    pub findings: Vec<SwapFinding>,
}

/// Options for [`analyze_with`].
#[derive(Clone, Debug, Default)]
pub struct SwapOptions {
    /// Take a second `/proc/vmstat` reading this long after the first (blocks) for rates.
    pub interval: Option<Duration>,
    /// Or compute rates against an earlier reading (e.g. the previous report's `counters`).
    pub previous: Option<VmCounters>,
}

/// Swap-in rate (pages/s) above which swapping is called thrashing.
pub const THRASH_PAGES_PER_S: f64 = 1000.0;

/// Analyze swap without blocking (counter totals only, no rates).
pub fn analyze(roots: &Roots, hw: &Hardware) -> SwapReport {
    analyze_with(roots, hw, &SwapOptions::default())
}

fn epoch() -> Instant {
    use std::sync::OnceLock;
    static T0: OnceLock<Instant> = OnceLock::new();
    *T0.get_or_init(Instant::now)
}

/// Read the swap counters now.
pub fn counters(roots: &Roots) -> VmCounters {
    let vmstat = read_lossy(&roots.proc.join("vmstat")).unwrap_or_default();
    let get = |k: &str| {
        vmstat.lines().find_map(|l| {
            let (key, v) = l.split_once(' ')?;
            (key == k).then(|| v.trim().parse().ok()).flatten()
        })
    };
    VmCounters {
        at_ms: u64::try_from(epoch().elapsed().as_millis()).unwrap_or(u64::MAX),
        pswpin: get("pswpin").unwrap_or(0),
        pswpout: get("pswpout").unwrap_or(0),
    }
}

/// Rates between two readings (`None` if no time passed or the counters went backwards).
pub fn rates(before: &VmCounters, after: &VmCounters) -> Option<SwapRates> {
    let dt = after.at_ms.checked_sub(before.at_ms).filter(|d| *d > 0)?;
    let per_s = |a: u64, b: u64| b.checked_sub(a).map(|d| d as f64 * 1000.0 / dt as f64);
    Some(SwapRates {
        interval_ms: dt,
        pswpin_per_s: per_s(before.pswpin, after.pswpin)?,
        pswpout_per_s: per_s(before.pswpout, after.pswpout)?,
    })
}

/// [`analyze`] with rate options.
pub fn analyze_with(roots: &Roots, hw: &Hardware, opts: &SwapOptions) -> SwapReport {
    let meminfo = read_lossy(&roots.proc.join("meminfo")).unwrap_or_default();
    let mem = |k: &str| {
        meminfo.lines().find_map(|l| {
            let (key, v) = l.split_once(':')?;
            (key.trim() == k).then(|| v.split_whitespace().next()?.parse::<u64>().ok().map(|n| n * KIB)).flatten()
        })
    };
    let first = counters(roots);
    let (counters_now, rates) = match (opts.interval, opts.previous) {
        (Some(d), _) => {
            std::thread::sleep(d);
            let second = counters(roots);
            (second, rates(&first, &second))
        }
        (None, Some(prev)) => (first, rates(&prev, &first)),
        (None, None) => (first, None),
    };
    let devices = swap_devices(roots);
    let mut r = SwapReport {
        mem_total: mem("MemTotal").unwrap_or(hw.mem_total),
        swap_total: mem("SwapTotal").unwrap_or(hw.swap_total),
        swap_used: devices.iter().map(|d| d.used_bytes).sum(),
        devices,
        zswap: zswap(roots),
        vm: vm_tunables(roots),
        mglru: read_lossy(&roots.sys.join("kernel/mm/lru_gen/enabled")).map(|raw| {
            let bits = u32::from_str_radix(raw.trim_start_matches("0x"), 16).unwrap_or(0);
            Mglru { enabled: bits & 1 == 1, raw }
        }),
        counters: counters_now,
        rates,
        top_units: top_units(roots, 10),
        findings: Vec::new(),
    };
    if r.swap_total == 0 {
        r.swap_total = r.devices.iter().map(|d| d.size_bytes).sum();
    }
    r.findings = findings(&r);
    r
}

fn parse_u<T: std::str::FromStr>(s: Option<String>) -> Option<T> {
    s.and_then(|v| v.trim().parse().ok())
}

fn vm_tunables(roots: &Roots) -> VmTunables {
    let vm = |k: &str| read_lossy(&roots.proc.join("sys/vm").join(k));
    VmTunables {
        swappiness: parse_u(vm("swappiness")),
        page_cluster: parse_u(vm("page-cluster")),
        watermark_scale_factor: parse_u(vm("watermark_scale_factor")),
        vfs_cache_pressure: parse_u(vm("vfs_cache_pressure")),
        min_free_kbytes: parse_u(vm("min_free_kbytes")),
    }
}

fn zswap(roots: &Roots) -> Option<Zswap> {
    let d = roots.sys.join("module/zswap/parameters");
    let enabled = read_lossy(&d.join("enabled"))?;
    let yes = |v: &str| matches!(v, "Y" | "y" | "1");
    Some(Zswap {
        enabled: yes(&enabled),
        compressor: read_lossy(&d.join("compressor")),
        zpool: read_lossy(&d.join("zpool")),
        max_pool_percent: parse_u(read_lossy(&d.join("max_pool_percent"))),
        shrinker_enabled: read_lossy(&d.join("shrinker_enabled")).map(|v| yes(&v)),
    })
}

/// The active algorithm in a zram `comp_algorithm` (`lzo lz4 [zstd]`).
fn zram_algorithm(roots: &Roots, name: &str) -> Option<String> {
    let s = read_lossy(&roots.sys.join("block").join(name).join("comp_algorithm"))?;
    blockdev::parse_scheduler(&s).0
}

/// Parse `/proc/swaps` and resolve each area's backing disk.
pub fn swap_devices(roots: &Roots) -> Vec<SwapDevice> {
    let Some(text) = read_lossy(&roots.proc.join("swaps")) else { return Vec::new() };
    let mounts = blockdev::mounts(roots).unwrap_or_default();
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let path = it.next()?.replace("\\040", " ");
            let ty = it.next()?;
            let size_kb: u64 = it.next()?.parse().ok()?;
            let used_kb: u64 = it.next()?.parse().ok()?;
            let priority: i32 = it.next()?.parse().ok()?;
            let name = path.strip_prefix("/dev/").map(|n| n.rsplit('/').next().unwrap_or(n).to_string());
            let is_zram = name.as_deref().is_some_and(|n| n.starts_with("zram"));
            let kind = if is_zram {
                SwapKind::Zram
            } else if ty == "file" {
                SwapKind::File
            } else {
                SwapKind::Partition
            };
            let backing_disk: Option<DevId> = match kind {
                SwapKind::Zram => None,
                SwapKind::Partition => blockdev::dev_from_source(&roots.sys, &path)
                    .and_then(|d| blockdev::whole_disks(&roots.sys, d).ok())
                    .and_then(|v| v.into_iter().next()),
                SwapKind::File => blockdev::mount_for(&mounts, Path::new(&path))
                    .and_then(|m| blockdev::mount_devices(&roots.sys, m).into_iter().next()),
            };
            let backing = backing_disk
                .and_then(|d| blockdev::block_dev(&roots.sys, d).ok())
                .map(|b| (b.dev.to_string(), storage_kind(&roots.sys, &b.name, b.rotational)));
            Some(SwapDevice {
                compressor: if is_zram { name.as_deref().and_then(|n| zram_algorithm(roots, n)) } else { None },
                path,
                kind,
                backing_dev: backing.as_ref().map(|b| b.0.clone()),
                backing: backing.map(|b| b.1),
                size_bytes: size_kb * KIB,
                used_bytes: used_kb * KIB,
                priority,
            })
        })
        .collect()
}

/// The `n` units (`*.service`, `*.scope`) with the most swapped-out memory.
pub fn top_units(roots: &Roots, n: usize) -> Vec<UnitSwap> {
    let mut out = Vec::new();
    walk(&roots.cgroup, "", 0, &mut out);
    out.sort_by(|a, b| b.swap_bytes.cmp(&a.swap_bytes).then_with(|| a.cgroup.cmp(&b.cgroup)));
    out.truncate(n);
    out
}

fn walk(root: &Path, rel: &str, depth: u32, out: &mut Vec<UnitSwap>) {
    if depth > 10 {
        return;
    }
    let dir = root.join(rel.trim_start_matches('/'));
    for child in subdirs(&dir).unwrap_or_default() {
        let cg = format!("{rel}/{child}");
        let is_unit = |n: &str| n.ends_with(".service") || n.ends_with(".scope");
        // Units that contain other units (user@UID.service) would double-count their children.
        let container =
            || subdirs(&dir.join(&child)).unwrap_or_default().iter().any(|c| is_unit(c) || c.ends_with(".slice"));
        if is_unit(&child) && !container() {
            let v: Option<u64> = parse_u(read_lossy(&dir.join(&child).join("memory.swap.current")));
            if let Some(b) = v.filter(|b| *b > 0) {
                out.push(UnitSwap { cgroup: cg.clone(), swap_bytes: b });
            }
        }
        walk(root, &cg, depth + 1, out);
    }
}

fn sysctl_op(key: &str, value: impl ToString) -> serde_json::Value {
    json!({ "op": "sysctl", "key": key, "value": value.to_string() })
}

fn finding(severity: Severity, title: &str, detail: String, ops: Vec<serde_json::Value>) -> SwapFinding {
    SwapFinding { severity, title: title.into(), detail, proposed_ops: ops }
}

/// The advice for a report (also run by [`analyze`]).
pub fn findings(r: &SwapReport) -> Vec<SwapFinding> {
    let mut out = Vec::new();
    let swappiness = r.vm.swappiness;
    let zswap_on = r.zswap.as_ref().is_some_and(|z| z.enabled);
    let zram: Vec<&SwapDevice> = r.devices.iter().filter(|d| d.kind == SwapKind::Zram).collect();
    let disk: Vec<&SwapDevice> = r.devices.iter().filter(|d| d.kind != SwapKind::Zram).collect();
    let hdd: Vec<&SwapDevice> = disk.iter().copied().filter(|d| d.backing == Some(StorageKind::Hdd)).collect();

    if r.devices.is_empty() {
        if r.mem_total > 0 && r.mem_total <= 8 * GIB {
            out.push(finding(
                Severity::Warn,
                "No swap with little RAM",
                format!(
                    "{} RAM and no swap: under memory pressure the kernel can only drop page cache and then \
                     OOM-kill. Enable zram (e.g. install zram-generator / systemd-zram-setup with zram-size = \
                     ram / 2); lindesk does not create swap.",
                    fmt_bytes(r.mem_total)
                ),
                Vec::new(),
            ));
        }
        return out;
    }

    if !hdd.is_empty() && swappiness.is_some_and(|s| s > 10) {
        let names: Vec<&str> = hdd.iter().map(|d| d.path.as_str()).collect();
        let mut ops = vec![sysctl_op("vm.swappiness", 10)];
        if !zswap_on && zram.is_empty() {
            ops.push(json!({ "op": "zswap", "enabled": true, "compressor": "zstd", "max_pool_percent": 20 }));
        }
        out.push(finding(
            Severity::Warn,
            "Swap on a hard disk",
            format!(
                "{} is on a rotational disk (page-in costs milliseconds) with vm.swappiness {}: swapping \
                 anonymous memory stalls the desktop. Lower swappiness, and put a compressed cache (zswap) or \
                 zram in front of it.",
                names.join(", "),
                swappiness.unwrap_or(0)
            ),
            ops,
        ));
    }

    let fast_only = disk.iter().all(|d| {
        matches!(d.backing, Some(StorageKind::Nvme | StorageKind::Ssd | StorageKind::Emmc | StorageKind::Virtual))
    });
    if let Some(pc) = r.vm.page_cluster.filter(|pc| *pc > 0)
        && (!zram.is_empty() || (!disk.is_empty() && fast_only))
    {
        let what = if zram.is_empty() { "solid-state swap" } else { "zram" };
        out.push(finding(
            Severity::Info,
            "Swap readahead on fast swap",
            format!(
                "vm.page-cluster {pc} reads {} pages per swap-in; on {what} there is no seek to amortise, so \
                 readahead only adds latency and wastes memory.",
                1u32 << pc.min(10)
            ),
            vec![sysctl_op("vm.page-cluster", 0)],
        ));
    }

    if !zram.is_empty() && disk.is_empty() && swappiness.is_some_and(|s| s < 100) {
        out.push(finding(
            Severity::Info,
            "Low swappiness with zram only",
            format!(
                "vm.swappiness {} with only zram swap: swapping to compressed RAM is cheaper than dropping \
                 file cache, so values above 100 (the kernel docs suggest up to 200) keep the desktop's \
                 working set cached.",
                swappiness.unwrap_or(0)
            ),
            vec![sysctl_op("vm.swappiness", 180)],
        ));
    }

    if zswap_on && !zram.is_empty() {
        out.push(finding(
            Severity::Info,
            "zswap in front of zram",
            "zswap compresses pages that then land in zram and get compressed again; disable one of them.".into(),
            vec![json!({ "op": "zswap", "enabled": false })],
        ));
    }

    if let Some(m) = r.mglru.as_ref().filter(|m| !m.enabled) {
        out.push(finding(
            Severity::Info,
            "MGLRU off",
            format!(
                "Multi-gen LRU is available but disabled ({}); it reclaims more accurately under pressure \
                 (echo y > /sys/kernel/mm/lru_gen/enabled).",
                m.raw
            ),
            Vec::new(),
        ));
    }

    if let Some(rt) = r.rates.filter(|rt| rt.pswpin_per_s > THRASH_PAGES_PER_S) {
        out.push(finding(
            Severity::Warn,
            "Heavy swap-in",
            format!(
                "{:.0} pages/s swapped in (and {:.0}/s out) over {} ms: the working set does not fit in RAM; \
                 lindesk's memory.high on batch work helps, but closing memory hogs helps more.",
                rt.pswpin_per_s, rt.pswpout_per_s, rt.interval_ms
            ),
            Vec::new(),
        ));
    }

    let used_pct = (r.swap_used * 100).checked_div(r.swap_total).unwrap_or(0);
    if used_pct >= 90 {
        out.push(finding(
            Severity::Warn,
            "Swap nearly full",
            format!("{used_pct}% of {} swap is in use; the OOM killer is close.", fmt_bytes(r.swap_total)),
            Vec::new(),
        ));
    }
    out
}

/// `zram0 zram/zstd 16 GiB (3.4 GiB used, prio 1000)`.
pub fn describe_device(d: &SwapDevice) -> String {
    let name = d.path.rsplit('/').next().unwrap_or(&d.path);
    let what = match (d.kind, &d.compressor, d.backing) {
        (SwapKind::Zram, Some(c), _) => format!("zram/{c}"),
        (SwapKind::Zram, None, _) => "zram".into(),
        (k, _, b) => {
            let k = if k == SwapKind::File { "file" } else { "partition" };
            match b {
                Some(b) => format!("{k} on {}", crate::hardware::kind_label(b)),
                None => k.to_string(),
            }
        }
    };
    format!("{name} {what} {} ({} used, prio {})", fmt_bytes(d.size_bytes), fmt_bytes(d.used_bytes), d.priority)
}

/// `swappiness 180, page-cluster 0, watermark_scale_factor 125, vfs_cache_pressure 100, min_free 640 MiB`.
pub fn describe_tunables(v: &VmTunables) -> String {
    let o = |x: Option<u32>| x.map_or("?".to_string(), |v| v.to_string());
    format!(
        "swappiness {}, page-cluster {}, watermark_scale_factor {}, vfs_cache_pressure {}, min_free {}",
        o(v.swappiness),
        o(v.page_cluster),
        o(v.watermark_scale_factor),
        o(v.vfs_cache_pressure),
        v.min_free_kbytes.map_or("?".to_string(), |k| fmt_bytes(k * KIB)),
    )
}

/// Remove the per-process timestamp so two reports can be compared (tests).
#[doc(hidden)]
pub fn without_clock(mut r: SwapReport) -> SwapReport {
    r.counters.at_ms = 0;
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::hardware::detect;
    use crate::{Env, probe};

    fn report(build: fn(&Path) -> (Roots, Env)) -> (tempfile::TempDir, SwapReport) {
        let d = tempfile::tempdir().unwrap();
        let (roots, _) = build(d.path());
        let hw = detect(&roots);
        (d, analyze(&roots, &hw))
    }

    fn titles(r: &SwapReport) -> Vec<&str> {
        r.findings.iter().map(|f| f.title.as_str()).collect()
    }

    #[test]
    fn hdd_swap_with_swappiness_60() {
        let (_d, r) = report(fixture::hdd_desktop);
        assert_eq!(r.devices.len(), 1);
        let s = &r.devices[0];
        assert_eq!(
            (s.kind, s.backing, s.backing_dev.as_deref()),
            (SwapKind::Partition, Some(StorageKind::Hdd), Some("8:0"))
        );
        assert_eq!(describe_device(s), "sda3 partition on HDD 2 GiB (0 MiB used, prio -1)");
        assert_eq!(r.vm.swappiness, Some(60));
        let f = r.findings.iter().find(|f| f.title == "Swap on a hard disk").unwrap();
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.proposed_ops[0], json!({"op": "sysctl", "key": "vm.swappiness", "value": "10"}));
        assert_eq!(f.proposed_ops[1]["op"], "zswap");
        assert!(!titles(&r).contains(&"Swap readahead on fast swap"), "page-cluster helps on HDDs");
        assert_eq!((r.counters.pswpin, r.counters.pswpout), (250_000, 400_000));
        assert!(r.rates.is_none());
    }

    #[test]
    fn zram_with_page_cluster_3() {
        let (_d, r) = report(fixture::intel_hybrid_laptop);
        let z = &r.devices[0];
        assert_eq!((z.kind, z.compressor.as_deref(), z.backing), (SwapKind::Zram, Some("lz4"), None));
        let f = r.findings.iter().find(|f| f.title == "Swap readahead on fast swap").unwrap();
        assert_eq!(f.proposed_ops, [json!({"op": "sysctl", "key": "vm.page-cluster", "value": "0"})]);
        assert!(f.detail.contains("reads 8 pages"));
        let f = r.findings.iter().find(|f| f.title == "Low swappiness with zram only").unwrap();
        assert_eq!(f.proposed_ops[0]["value"], "180");
    }

    #[test]
    fn no_swap_with_8_gib() {
        let (_d, r) = report(fixture::kvm_guest);
        assert!(r.devices.is_empty());
        assert_eq!(titles(&r), ["No swap with little RAM"]);
        assert!(r.findings[0].proposed_ops.is_empty(), "lindesk does not create swap");
        assert!(r.findings[0].detail.contains("zram"));
    }

    #[test]
    fn zswap_with_nvme_swap() {
        let (_d, r) = report(fixture::amd_desktop);
        let z = r.zswap.as_ref().unwrap();
        assert!(z.enabled);
        assert_eq!((z.compressor.as_deref(), z.max_pool_percent), (Some("zstd"), Some(20)));
        let s = &r.devices[0];
        assert_eq!((s.kind, s.backing), (SwapKind::Partition, Some(StorageKind::Nvme)));
        assert!(titles(&r).contains(&"Swap readahead on fast swap"));
        assert!(!titles(&r).contains(&"Swap on a hard disk"));
        assert_eq!(r.mglru, Some(Mglru { raw: "0x0000".into(), enabled: false }));
        assert!(titles(&r).contains(&"MGLRU off"));
    }

    #[test]
    fn dev_host_is_well_tuned_and_ranks_units() {
        let (_d, r) = report(fixture::pop_os);
        assert!(r.findings.is_empty(), "{:#?}", r.findings);
        assert_eq!(r.vm.swappiness, Some(180));
        assert!(r.mglru.as_ref().unwrap().enabled);
        assert_eq!(
            r.top_units,
            [
                UnitSwap {
                    cgroup: "/user.slice/user-1000.slice/user@1000.service/app.slice/app-firefox.scope".into(),
                    swap_bytes: 1 << 30
                },
                UnitSwap { cgroup: "/system.slice/docker.service".into(), swap_bytes: 50 << 20 },
            ]
        );
        assert_eq!(r.devices.len(), 2, "dm-0 partition + zram0");
        assert_eq!(r.devices[1].compressor.as_deref(), Some("zstd"));
        assert_eq!(
            describe_tunables(&r.vm),
            "swappiness 180, page-cluster 0, watermark_scale_factor 125, vfs_cache_pressure 100, min_free 640 MiB"
        );
    }

    #[test]
    fn container_units_are_not_double_counted() {
        let d = tempfile::tempdir().unwrap();
        let (roots, _) = fixture::pop_os(d.path());
        crate::fixture::Fixture::new(d.path())
            .cgroup("/user.slice/user-1000.slice/user@1000.service", &[("memory.swap.current", "2147483648")]);
        let top = top_units(&roots, 10);
        assert!(top.iter().all(|u| !u.cgroup.ends_with("user@1000.service")), "{top:?}");
        assert_eq!(top.len(), 2);
    }

    #[test]
    fn rates_and_thrashing() {
        let d = tempfile::tempdir().unwrap();
        let (roots, _) = fixture::hdd_desktop(d.path());
        let hw = detect(&roots);
        let prev = VmCounters { at_ms: counters(&roots).at_ms.saturating_sub(1000), pswpin: 240_000, pswpout: 399_000 };
        let r = analyze_with(&roots, &hw, &SwapOptions { previous: Some(prev), ..Default::default() });
        let rt = r.rates.unwrap();
        assert!(rt.pswpin_per_s > THRASH_PAGES_PER_S, "{rt:?}");
        assert!(titles(&r).contains(&"Heavy swap-in"));
        let r =
            analyze_with(&roots, &hw, &SwapOptions { interval: Some(Duration::from_millis(20)), ..Default::default() });
        assert_eq!(r.rates.unwrap().pswpin_per_s, 0.0);
        assert!(rates(&VmCounters::default(), &VmCounters::default()).is_none());
        // Probe carries the report.
        let p = probe(&roots, &Env::new());
        assert_eq!(p.swap.devices.len(), 1);
    }

    #[test]
    #[ignore = "reads the real host"]
    fn real_host() {
        let roots = Roots::host();
        let hw = detect(&roots);
        let r =
            analyze_with(&roots, &hw, &SwapOptions { interval: Some(Duration::from_secs(1)), ..Default::default() });
        eprintln!("{}", serde_json::to_string_pretty(&r).unwrap());
    }
}
