//! Capability probe: what this host's kernel, cgroup setup, disks and desktop support.
//!
//! [`probe`] never fails: anything it can't read is reported as missing. The result is
//! plain data ([`Probe`], serde-serialisable) from which lindesk derives the overall
//! [`SupportLevel`], the actuator [`Backend`] and the plugin-facing [`HostCaps`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::Path;

use lindesk_plugin_api::HostCaps;
use serde::{Deserialize, Serialize};

use crate::blockdev::{self, BlockDev, DevId};
use crate::cgroupfs::CgroupFs;
use crate::cgroupfs::v1::V1Fs;
use crate::iocost::{IoCost, parse_qos};
use crate::util::{read_lossy, subdirs};
use crate::{Env, Roots};

/// How cgroups are mounted.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CgroupMode {
    /// Unified hierarchy only (cgroup v2).
    V2,
    /// v1 controllers plus a controller-less v2 mount at `unified/`.
    Hybrid,
    /// Legacy v1 hierarchies only.
    V1,
    /// No cgroup filesystem found.
    None,
}

/// The systemd service manager.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemdInfo {
    /// systemd is PID 1 (`/run/systemd/system` exists).
    pub running: bool,
    /// Version from the `libsystemd-shared-<version>.so` file name, when found.
    pub version: Option<String>,
}

/// A `user@UID.service` manager and the controllers delegated to it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UserManager {
    pub uid: u32,
    pub controllers: Vec<String>,
}

/// Pressure Stall Information availability.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PsiSupport {
    /// `/proc/pressure/*`.
    pub global: bool,
    /// Per-cgroup `*.pressure` files.
    pub cgroup: bool,
}

/// `io.latency` (blk-iolatency) state.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IoLatency {
    /// `io.latency` files exist in child cgroups.
    Available,
    /// Built into the kernel, but not usable here (io controller not enabled, or v1).
    NotEnabled,
    /// `CONFIG_BLK_CGROUP_IOLATENCY` is not set.
    NotBuilt,
    /// No `io.latency` file and no kernel config to tell why.
    Unknown,
}

/// iocost state on one device.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct IoCostDevice {
    pub dev: DevId,
    pub name: String,
    pub enabled: bool,
}

/// iocost availability and per-device state for the probed devices.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct IoCostProbe {
    /// `io.cost.qos` exists at the cgroup root.
    pub available: bool,
    pub devices: Vec<IoCostDevice>,
}

/// sched_ext state.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct SchedExt {
    /// `/sys/kernel/sched_ext` exists.
    pub available: bool,
    /// Contents of `state` (`disabled`, `enabled`, ...).
    pub state: Option<String>,
    /// The loaded scheduler (`root/ops`), e.g. `lavd`.
    pub ops: Option<String>,
}

impl SchedExt {
    /// A BPF scheduler is loaded.
    pub fn enabled(&self) -> bool {
        self.state.as_deref() == Some("enabled")
    }
}

/// Whether buffered writes on a filesystem are attributed to the dirtying cgroup.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Writeback {
    /// ext2/3/4, btrfs, f2fs, xfs.
    Supported,
    /// Anything else (ZFS, NFS, FUSE, ...): writeback is charged to the root cgroup.
    Limited,
    /// The mount could not be found.
    Unknown,
}

impl Writeback {
    /// Writeback support of a filesystem type.
    pub fn for_fs(fs_type: &str) -> Self {
        match fs_type {
            "ext2" | "ext3" | "ext4" | "btrfs" | "f2fs" | "xfs" => Writeback::Supported,
            _ => Writeback::Limited,
        }
    }
}

/// A path lindesk cares about (`/`, `/home`) and what's under it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MountProbe {
    /// The probed path.
    pub path: String,
    /// The mount point containing it.
    pub mount_point: String,
    pub fs_type: String,
    pub source: String,
    pub writeback: Writeback,
    /// Whole disks behind the filesystem.
    pub devices: Vec<BlockDev>,
}

/// Desktop family, for picking a foreground adapter.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DesktopKind {
    Gnome,
    Kde,
    Cosmic,
    Other,
    Unknown,
}

/// Display server protocol of the session.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionType {
    Wayland,
    X11,
    Tty,
    Unknown,
}

/// The desktop session the probe ran in.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Desktop {
    pub kind: DesktopKind,
    pub session: SessionType,
    /// Raw `XDG_CURRENT_DESKTOP` (or `DESKTOP_SESSION`).
    pub name: Option<String>,
}

/// Another daemon that also tunes priorities or cgroups.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub name: String,
    /// The unit file found, if installed.
    pub unit: Option<String>,
    pub installed: bool,
    pub running: bool,
}

/// Overall responsiveness level lindesk can deliver on this host.
///
/// Rules, first match wins:
/// 1. **Minimal**: no cgroup filesystem, or no CPU controller (v2 `cpu` / v1 `cpu`).
///    Only per-process nice/ioprio/uclamp are available.
/// 2. **CpuOnly**: cgroup v1 or hybrid (no `io.max`/iocost/`io.latency`, no writeback
///    attribution); or no PSI (no stall feedback, so only static work-conserving CPU
///    weights are worth applying); or no v2 `io` controller.
/// 3. **Full**: cgroup v2 + PSI + `cpu` and `io` controllers + an effective
///    proportional/latency I/O control (`io.latency`, or every probed disk on BFQ or with
///    iocost enabled) + cgroup writeback supported on every probed filesystem.
/// 4. **IoPartial**: otherwise. I/O control is limited to `io.max` (iocost available but
///    disabled counts here until lindeskd enables it), or writeback is limited (ZFS, NFS…).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum SupportLevel {
    Full,
    IoPartial,
    CpuOnly,
    Minimal,
}

impl SupportLevel {
    /// `FULL`, `IO-PARTIAL`, `CPU-ONLY` or `MINIMAL`.
    pub fn as_str(self) -> &'static str {
        match self {
            SupportLevel::Full => "FULL",
            SupportLevel::IoPartial => "IO-PARTIAL",
            SupportLevel::CpuOnly => "CPU-ONLY",
            SupportLevel::Minimal => "MINIMAL",
        }
    }
}

impl fmt::Display for SupportLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which actuator implementation fits the host.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    /// Unit properties through systemd (system and user managers).
    Systemd,
    /// Direct cgroup v2 writes (no systemd).
    CgroupV2Raw,
    /// Legacy hierarchies.
    CgroupV1,
    /// nice/ioprio/uclamp per process.
    PerProcess,
}

impl Backend {
    /// The built-in actuator plugin name.
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Systemd => "systemd",
            Backend::CgroupV2Raw => "cgroup-v2-raw",
            Backend::CgroupV1 => "cgroup-v1",
            Backend::PerProcess => "proc",
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything the probe found.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Probe {
    /// `uname -r`.
    pub kernel: Option<String>,
    pub cgroup_mode: CgroupMode,
    /// Controllers at the v2 root (the `unified` mount in hybrid mode).
    pub root_controllers: Vec<String>,
    /// Controllers with a v1 hierarchy (v1/hybrid only).
    pub v1_controllers: Vec<String>,
    pub user_managers: Vec<UserManager>,
    pub systemd: SystemdInfo,
    pub psi: PsiSupport,
    /// `cpu.uclamp.min` exists in a child cgroup.
    pub uclamp: bool,
    pub io_latency: IoLatency,
    pub io_cost: IoCostProbe,
    /// `io.weight` actually does something on every probed disk (BFQ or iocost enabled).
    pub io_weight_effective: bool,
    /// cgroup v2 `io.max` is available.
    pub io_max: bool,
    pub sched_ext: SchedExt,
    /// `/sys/kernel/btf/vmlinux` exists (CO-RE BPF programs can load).
    pub btf: bool,
    /// `/` and `/home`.
    pub mounts: Vec<MountProbe>,
    pub desktop: Desktop,
    pub conflicts: Vec<Conflict>,
    /// Relevant kernel config options (`y`/`m`/`n`), when a config was found.
    pub kconfig: Option<BTreeMap<String, String>>,
}

/// Kernel config options recorded in [`Probe::kconfig`].
pub const KCONFIG_KEYS: &[&str] = &[
    "PSI",
    "UCLAMP_TASK_GROUP",
    "BLK_CGROUP_IOLATENCY",
    "BLK_CGROUP_IOCOST",
    "BFQ_GROUP_IOSCHED",
    "CGROUP_WRITEBACK",
    "SCHED_CLASS_EXT",
    "DEBUG_INFO_BTF",
];

/// Paths whose filesystems and disks are probed.
pub const PROBED_PATHS: &[&str] = &["/", "/home"];

impl Probe {
    /// True if the v2 root offers a controller.
    pub fn has_controller(&self, c: &str) -> bool {
        self.root_controllers.iter().any(|x| x == c)
    }

    fn has_cpu_controller(&self) -> bool {
        match self.cgroup_mode {
            CgroupMode::V2 => self.has_controller("cpu"),
            CgroupMode::V1 | CgroupMode::Hybrid => self.v1_controllers.iter().any(|c| c == "cpu"),
            CgroupMode::None => false,
        }
    }

    /// Unique whole disks behind the probed mounts.
    pub fn devices(&self) -> Vec<&BlockDev> {
        let mut seen = BTreeSet::new();
        self.mounts.iter().flat_map(|m| &m.devices).filter(|d| seen.insert(d.dev)).collect()
    }

    /// iocost is enabled on `dev`.
    pub fn iocost_enabled(&self, dev: DevId) -> bool {
        self.io_cost.devices.iter().any(|d| d.dev == dev && d.enabled)
    }

    /// `io.latency` is usable, or `io.weight` is effective on every probed disk.
    pub fn io_control_effective(&self) -> bool {
        self.io_latency == IoLatency::Available || self.io_weight_effective
    }

    /// Cgroup writeback is supported on every probed filesystem.
    pub fn writeback_ok(&self) -> bool {
        !self.mounts.is_empty() && self.mounts.iter().all(|m| m.writeback == Writeback::Supported)
    }

    /// The overall level (see [`SupportLevel`] for the rules).
    pub fn support_level(&self) -> SupportLevel {
        self.support().0
    }

    /// The overall level and the reasons it isn't higher.
    pub fn support(&self) -> (SupportLevel, Vec<String>) {
        let mut why = Vec::new();
        if self.cgroup_mode == CgroupMode::None {
            why.push("no cgroup filesystem mounted".to_string());
            return (SupportLevel::Minimal, why);
        }
        if !self.has_cpu_controller() {
            why.push("cgroup cpu controller not available".to_string());
            return (SupportLevel::Minimal, why);
        }
        if matches!(self.cgroup_mode, CgroupMode::V1 | CgroupMode::Hybrid) {
            why.push("cgroup v1: no io.max/iocost/io.latency and no writeback attribution".to_string());
        }
        if !self.psi.global {
            why.push("no PSI: no stall feedback, only static CPU weights".to_string());
        }
        if self.cgroup_mode == CgroupMode::V2 && !self.has_controller("io") {
            why.push("cgroup io controller not available".to_string());
        }
        if !why.is_empty() {
            return (SupportLevel::CpuOnly, why);
        }
        if !self.io_control_effective() {
            let iocost = if self.io_cost.available { "iocost disabled" } else { "no iocost" };
            why.push(format!("I/O control limited to io.max: no io.latency, {iocost}, no BFQ"));
        }
        for m in self.mounts.iter().filter(|m| m.writeback != Writeback::Supported) {
            why.push(format!("cgroup writeback limited on {} ({})", m.path, m.fs_type));
        }
        if self.mounts.is_empty() {
            why.push("could not resolve the filesystems behind / and /home".to_string());
        }
        if why.is_empty() { (SupportLevel::Full, why) } else { (SupportLevel::IoPartial, why) }
    }

    /// The actuator backend to use.
    pub fn backend(&self) -> Backend {
        match self.cgroup_mode {
            CgroupMode::None => Backend::PerProcess,
            CgroupMode::V1 | CgroupMode::Hybrid => {
                if self.has_cpu_controller() {
                    Backend::CgroupV1
                } else {
                    Backend::PerProcess
                }
            }
            CgroupMode::V2 => {
                if !["cpu", "io", "memory"].iter().any(|c| self.has_controller(c)) {
                    Backend::PerProcess
                } else if self.systemd.running {
                    Backend::Systemd
                } else {
                    Backend::CgroupV2Raw
                }
            }
        }
    }

    /// The summary handed to plugins.
    pub fn host_caps(&self) -> HostCaps {
        HostCaps {
            cgroup_v2: self.cgroup_mode == CgroupMode::V2,
            systemd: self.systemd.running,
            psi: self.psi.global,
            uclamp: self.uclamp,
            io_latency: self.io_latency == IoLatency::Available,
            io_cost: self.io_cost.available,
            io_weight_effective: self.io_weight_effective,
            io_max: self.io_max,
            sched_ext: self.sched_ext.available,
            bpf: self.btf,
            devices: self.devices().iter().map(|d| d.dev.to_string()).collect(),
        }
    }
}

/// Probe the host described by `roots` and `env`.
pub fn probe(roots: &Roots, env: &Env) -> Probe {
    let kernel = read_lossy(&roots.proc.join("sys/kernel/osrelease"));
    let kconfig = kernel.as_deref().and_then(|k| read_kconfig(roots, k));
    let cg = CgroupFs::new(&roots.cgroup);
    let v1 = V1Fs::new(&roots.cgroup);

    let cgroup_mode = if cg.has_attr("/", "cgroup.controllers") {
        CgroupMode::V2
    } else if cg.has_attr("/unified", "cgroup.controllers") {
        CgroupMode::Hybrid
    } else if !v1.controllers().is_empty() {
        CgroupMode::V1
    } else {
        CgroupMode::None
    };
    let root_controllers = match cgroup_mode {
        CgroupMode::V2 => cg.controllers("/").unwrap_or_default(),
        CgroupMode::Hybrid => cg.controllers("/unified").unwrap_or_default(),
        _ => Vec::new(),
    };
    let v1_controllers =
        if matches!(cgroup_mode, CgroupMode::V1 | CgroupMode::Hybrid) { v1.controllers() } else { Vec::new() };
    let v2 = cgroup_mode == CgroupMode::V2;
    let children = if v2 { probe_children(&cg) } else { Vec::new() };
    let any_child_has = |f: &str| children.iter().any(|c| cg.has_attr(c, f));

    let psi = PsiSupport {
        global: ["cpu", "io", "memory"].iter().all(|r| roots.proc.join("pressure").join(r).exists()),
        cgroup: any_child_has("cpu.pressure"),
    };
    let uclamp = if v2 {
        any_child_has("cpu.uclamp.min")
    } else {
        v1.hierarchy("cpu")
            .is_some_and(|h| subdirs(&h).unwrap_or_default().iter().any(|c| h.join(c).join("cpu.uclamp.min").exists()))
    };

    let io_latency = if any_child_has("io.latency") {
        IoLatency::Available
    } else {
        match &kconfig {
            Some(cfg) => match cfg.get("BLK_CGROUP_IOLATENCY").map(String::as_str) {
                Some("y" | "m") => IoLatency::NotEnabled,
                _ => IoLatency::NotBuilt,
            },
            None => IoLatency::Unknown,
        }
    };

    let mounts = probe_mounts(roots);
    let mut seen = BTreeSet::new();
    let devices: Vec<&BlockDev> = mounts.iter().flat_map(|m| &m.devices).filter(|d| seen.insert(d.dev)).collect();

    let ioc = IoCost::new(&roots.cgroup);
    let qos = read_lossy(&ioc.qos_path()).map(|s| parse_qos(&s)).unwrap_or_default();
    let io_cost = IoCostProbe {
        available: v2 && ioc.available(),
        devices: devices
            .iter()
            .map(|d| IoCostDevice {
                dev: d.dev,
                name: d.name.clone(),
                enabled: qos.iter().any(|l| l.device == d.dev.to_string() && l.enabled),
            })
            .collect(),
    };
    let io_weight_effective = !devices.is_empty()
        && devices.iter().all(|d| d.is_bfq() || io_cost.devices.iter().any(|c| c.dev == d.dev && c.enabled));

    let sched_dir = roots.sys.join("kernel/sched_ext");
    let sched_ext = SchedExt {
        available: sched_dir.is_dir(),
        state: read_lossy(&sched_dir.join("state")),
        ops: read_lossy(&sched_dir.join("root/ops")).filter(|s| !s.is_empty()),
    };

    Probe {
        kernel,
        cgroup_mode,
        user_managers: if v2 { user_managers(&cg) } else { Vec::new() },
        io_max: v2 && root_controllers.iter().any(|c| c == "io"),
        root_controllers,
        v1_controllers,
        systemd: SystemdInfo { running: roots.run.join("systemd/system").is_dir(), version: systemd_version(roots) },
        psi,
        uclamp,
        io_latency,
        io_weight_effective,
        io_cost,
        sched_ext,
        btf: roots.sys.join("kernel/btf/vmlinux").exists(),
        mounts,
        desktop: desktop(env),
        conflicts: conflicts(roots),
        kconfig,
    }
}

/// Well-known children first, then whatever else exists at the root.
fn probe_children(cg: &CgroupFs) -> Vec<String> {
    let mut out: Vec<String> = ["/user.slice", "/system.slice", "/init.scope"]
        .iter()
        .filter(|c| cg.exists(c))
        .map(|c| c.to_string())
        .collect();
    if out.is_empty() {
        out.extend(cg.children("/").unwrap_or_default().into_iter().take(4));
    }
    out
}

fn user_managers(cg: &CgroupFs) -> Vec<UserManager> {
    let mut out = Vec::new();
    for slice in cg.children("/user.slice").unwrap_or_default() {
        let Some(uid) = slice.rsplit('/').next().and_then(|n| n.strip_prefix("user-")?.strip_suffix(".slice")) else {
            continue;
        };
        let mgr = format!("{slice}/user@{uid}.service");
        if let (Ok(uid), Ok(controllers)) = (uid.parse(), cg.controllers(&mgr)) {
            out.push(UserManager { uid, controllers });
        }
    }
    out.sort_by_key(|u| u.uid);
    out
}

fn systemd_version(roots: &Roots) -> Option<String> {
    let dirs = ["lib/systemd", "lib64/systemd", "lib/x86_64-linux-gnu/systemd", "lib/aarch64-linux-gnu/systemd"];
    dirs.iter().find_map(|d| {
        fs::read_dir(roots.usr.join(d)).ok()?.filter_map(Result::ok).find_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let v = name.strip_prefix("libsystemd-shared-")?.strip_suffix(".so")?;
            let v = v.split('-').next()?;
            (!v.is_empty() && v.starts_with(|c: char| c.is_ascii_digit())).then(|| v.to_string())
        })
    })
}

/// Parse kernel config text into the [`KCONFIG_KEYS`] subset (`y`/`m`/`n`).
pub fn parse_kconfig(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let (key, val) = if let Some(rest) = line.strip_prefix("# CONFIG_").and_then(|r| r.strip_suffix(" is not set"))
        {
            (rest, "n")
        } else if let Some((k, v)) = line.strip_prefix("CONFIG_").and_then(|r| r.split_once('=')) {
            (k, v)
        } else {
            continue;
        };
        if KCONFIG_KEYS.contains(&key) {
            out.insert(key.to_string(), val.to_string());
        }
    }
    out
}

/// Kernel config from `/proc/config.gz`, else `/boot/config-<release>`.
fn read_kconfig(roots: &Roots, release: &str) -> Option<BTreeMap<String, String>> {
    let gz = roots.proc.join("config.gz");
    if let Ok(f) = fs::File::open(&gz) {
        let mut s = String::new();
        if flate2::read::GzDecoder::new(f).read_to_string(&mut s).is_ok() {
            return Some(parse_kconfig(&s));
        }
    }
    fs::read_to_string(roots.boot.join(format!("config-{release}"))).ok().map(|s| parse_kconfig(&s))
}

fn probe_mounts(roots: &Roots) -> Vec<MountProbe> {
    let Ok(ms) = blockdev::mounts(roots) else { return Vec::new() };
    PROBED_PATHS
        .iter()
        .filter_map(|p| {
            let m = blockdev::mount_for(&ms, Path::new(p))?;
            let devices = blockdev::mount_devices(&roots.sys, m)
                .into_iter()
                .filter_map(|d| blockdev::block_dev(&roots.sys, d).ok())
                .collect();
            Some(MountProbe {
                path: p.to_string(),
                mount_point: m.mount_point.to_string_lossy().into_owned(),
                fs_type: m.fs_type.clone(),
                source: m.source.clone(),
                writeback: Writeback::for_fs(&m.fs_type),
                devices,
            })
        })
        .collect()
}

/// Classify the desktop from environment variables.
pub fn desktop(env: &Env) -> Desktop {
    let name = env.get("XDG_CURRENT_DESKTOP").or_else(|| env.get("DESKTOP_SESSION")).map(str::to_string);
    let tokens: Vec<String> = ["XDG_CURRENT_DESKTOP", "XDG_SESSION_DESKTOP", "DESKTOP_SESSION"]
        .iter()
        .filter_map(|k| env.get(k))
        .flat_map(|v| v.split(':').map(str::to_ascii_lowercase).collect::<Vec<_>>())
        .collect();
    let has = |pred: &dyn Fn(&str) -> bool| tokens.iter().any(|t| pred(t));
    let kind = if has(&|t| t == "cosmic") {
        DesktopKind::Cosmic
    } else if has(&|t| t == "kde" || t.starts_with("plasma")) {
        DesktopKind::Kde
    } else if has(&|t| t.contains("gnome")) {
        DesktopKind::Gnome
    } else if tokens.is_empty() {
        DesktopKind::Unknown
    } else {
        DesktopKind::Other
    };
    let session = match env.get("XDG_SESSION_TYPE").map(str::to_ascii_lowercase).as_deref() {
        Some("wayland") => SessionType::Wayland,
        Some("x11") => SessionType::X11,
        Some("tty") => SessionType::Tty,
        _ if env.get("WAYLAND_DISPLAY").is_some() => SessionType::Wayland,
        _ if env.get("DISPLAY").is_some() => SessionType::X11,
        _ => SessionType::Unknown,
    };
    Desktop { kind, session, name }
}

/// Daemons that also adjust priorities, cgroups or CPU weights.
/// (auto-cpufreq only tunes frequency governors and is deliberately not listed.)
pub const KNOWN_CONFLICTS: &[(&str, &[&str], &[&str])] = &[
    ("system76-scheduler", &["com.system76.Scheduler.service", "system76-scheduler.service"], &["system76-scheduler"]),
    ("ananicy-cpp", &["ananicy-cpp.service"], &["ananicy-cpp"]),
    ("ananicy", &["ananicy.service"], &["ananicy"]),
    ("uresourced", &["uresourced.service"], &["uresourced"]),
    ("gamemode", &["gamemoded.service"], &["gamemoded"]),
];

fn conflicts(roots: &Roots) -> Vec<Conflict> {
    let unit_dirs = [
        roots.etc.join("systemd/system"),
        roots.etc.join("systemd/user"),
        roots.usr.join("lib/systemd/system"),
        roots.usr.join("lib/systemd/user"),
    ];
    let comms: BTreeSet<String> = crate::procfs::list_pids(&roots.proc)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| crate::procfs::comm(&roots.proc, p).ok())
        .collect();
    KNOWN_CONFLICTS
        .iter()
        .filter_map(|(name, units, procs)| {
            let unit = units.iter().find(|u| unit_dirs.iter().any(|d| d.join(u).exists())).map(|u| u.to_string());
            // comm is truncated to 15 bytes by the kernel.
            let running = procs.iter().any(|p| comms.contains(&p[..p.len().min(15)]));
            (unit.is_some() || running).then(|| Conflict {
                name: name.to_string(),
                installed: unit.is_some(),
                unit,
                running,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{self, Fixture};

    fn run(build: fn(&Path) -> (Roots, Env)) -> (tempfile::TempDir, Probe) {
        let d = tempfile::tempdir().unwrap();
        let (roots, env) = build(d.path());
        let p = probe(&roots, &env);
        (d, p)
    }

    #[test]
    fn this_host_shape() {
        let (_d, p) = run(fixture::pop_os);
        assert_eq!(p.kernel.as_deref(), Some("7.0.11-76070011-generic"));
        assert_eq!(p.cgroup_mode, CgroupMode::V2);
        assert!(p.has_controller("cpu") && p.has_controller("io"));
        assert_eq!(
            p.user_managers,
            [UserManager { uid: 1000, controllers: vec!["cpu".into(), "io".into(), "memory".into(), "pids".into()] }]
        );
        assert_eq!(p.systemd, SystemdInfo { running: true, version: Some("255".into()) });
        assert_eq!(p.psi, PsiSupport { global: true, cgroup: true });
        assert!(p.uclamp);
        assert_eq!(p.io_latency, IoLatency::NotBuilt);
        assert!(p.io_cost.available);
        assert_eq!(p.io_cost.devices.len(), 1);
        assert!(!p.io_cost.devices[0].enabled);
        assert!(!p.io_weight_effective);
        assert!(p.io_max);
        assert_eq!(p.sched_ext, SchedExt { available: true, state: Some("disabled".into()), ops: None });
        assert!(p.btf);
        assert_eq!(p.mounts.len(), 2);
        assert_eq!(p.mounts[0].fs_type, "ext4");
        assert_eq!(p.mounts[1].mount_point, "/", "/home lives on /");
        assert_eq!(p.mounts[0].devices[0].name, "nvme0n1");
        assert_eq!(p.mounts[0].devices[0].scheduler.as_deref(), Some("none"));
        assert_eq!(
            p.desktop,
            Desktop { kind: DesktopKind::Gnome, session: SessionType::Wayland, name: Some("ubuntu:GNOME".into()) }
        );
        assert_eq!(
            p.conflicts,
            [Conflict {
                name: "system76-scheduler".into(),
                unit: Some("com.system76.Scheduler.service".into()),
                installed: true,
                running: false
            }]
        );
        assert_eq!(p.kconfig.as_ref().unwrap()["BLK_CGROUP_IOLATENCY"], "n");

        let (level, why) = p.support();
        assert_eq!(level, SupportLevel::IoPartial);
        assert_eq!(why.len(), 1);
        assert!(why[0].contains("iocost disabled"), "{why:?}");
        assert_eq!(p.backend(), Backend::Systemd);
        let caps = p.host_caps();
        assert!(caps.cgroup_v2 && caps.psi && caps.io_cost && caps.io_max && caps.uclamp && caps.bpf && caps.sched_ext);
        assert!(!caps.io_latency && !caps.io_weight_effective);
        assert_eq!(caps.devices, ["259:0"]);
    }

    #[test]
    fn this_host_full_once_iocost_enabled() {
        let d = tempfile::tempdir().unwrap();
        let (roots, env) = fixture::pop_os(d.path());
        IoCost::new(&roots.cgroup).enable("259:0").unwrap();
        let p = probe(&roots, &env);
        assert!(p.iocost_enabled(DevId::new(259, 0)));
        assert!(p.io_weight_effective);
        assert_eq!(p.support_level(), SupportLevel::Full);
    }

    #[test]
    fn cgroup_v1_only() {
        let (_d, p) = run(fixture::cgroup_v1);
        assert_eq!(p.cgroup_mode, CgroupMode::V1);
        assert_eq!(p.v1_controllers, ["blkio", "cpu", "cpuacct", "memory"]);
        assert!(p.root_controllers.is_empty() && p.user_managers.is_empty());
        assert!(!p.io_max && !p.io_cost.available && !p.psi.cgroup);
        assert_eq!(p.io_latency, IoLatency::NotEnabled);
        assert!(p.io_weight_effective, "BFQ");
        assert_eq!(p.support_level(), SupportLevel::CpuOnly);
        assert_eq!(p.backend(), Backend::CgroupV1);
        assert!(!p.host_caps().cgroup_v2);
        assert_eq!(p.desktop.kind, DesktopKind::Other);
        assert_eq!(p.desktop.session, SessionType::X11);
        assert_eq!(p.conflicts.len(), 1);
        assert!(p.conflicts[0].running && p.conflicts[0].installed && p.conflicts[0].name == "ananicy-cpp");
    }

    #[test]
    fn no_psi() {
        let (_d, p) = run(fixture::no_psi);
        assert_eq!(p.psi, PsiSupport { global: false, cgroup: false });
        let (level, why) = p.support();
        assert_eq!(level, SupportLevel::CpuOnly);
        assert!(why[0].contains("no PSI"));
        assert!(!p.host_caps().psi);
    }

    #[test]
    fn zfs_root() {
        let (_d, p) = run(fixture::zfs_root);
        assert_eq!(p.mounts[0].fs_type, "zfs");
        assert_eq!(p.mounts[0].writeback, Writeback::Limited);
        assert!(p.mounts.iter().all(|m| m.devices.is_empty()));
        assert!(!p.io_weight_effective);
        let (level, why) = p.support();
        assert_eq!(level, SupportLevel::IoPartial);
        assert!(why.iter().any(|w| w.contains("writeback limited on / (zfs)")), "{why:?}");
        assert!(p.host_caps().devices.is_empty());
    }

    #[test]
    fn full_featured_bfq() {
        let (_d, p) = run(fixture::full_bfq);
        assert_eq!(p.io_latency, IoLatency::Available);
        assert!(p.io_weight_effective);
        let names: Vec<_> = p.devices().iter().map(|d| d.name.clone()).collect();
        assert_eq!(names, ["sda", "nvme0n1"]);
        assert_eq!(p.mounts[1].fs_type, "xfs");
        assert_eq!(p.sched_ext.ops.as_deref(), Some("lavd"));
        assert!(p.sched_ext.enabled());
        assert_eq!(p.desktop.kind, DesktopKind::Kde);
        assert!(p.conflicts.is_empty(), "auto-cpufreq is not a conflict");
        assert_eq!(p.support(), (SupportLevel::Full, vec![]));
        assert_eq!(p.host_caps().devices, ["8:0", "259:0"]);
    }

    #[test]
    fn hybrid_and_backends() {
        let d = tempfile::tempdir().unwrap();
        let f = Fixture::new(d.path());
        f.file("sys/fs/cgroup/unified/cgroup.controllers", "\n")
            .file("sys/fs/cgroup/cpu,cpuacct/cpu.shares", "1024\n")
            .global_psi();
        let p = probe(&f.roots(), &Env::new());
        assert_eq!(p.cgroup_mode, CgroupMode::Hybrid);
        assert_eq!(p.backend(), Backend::CgroupV1);
        assert_eq!(p.support_level(), SupportLevel::CpuOnly);
        assert_eq!(p.desktop.kind, DesktopKind::Unknown);

        // Nothing at all.
        let e = tempfile::tempdir().unwrap();
        let p = probe(&Roots::under(e.path()), &Env::new());
        assert_eq!(p.cgroup_mode, CgroupMode::None);
        assert_eq!(p.backend(), Backend::PerProcess);
        assert_eq!(p.support_level(), SupportLevel::Minimal);
        assert_eq!(p.kernel, None);
        assert_eq!(p.io_latency, IoLatency::Unknown);

        // v2 without systemd → raw backend.
        let g = tempfile::tempdir().unwrap();
        let (roots, env) = fixture::pop_os(g.path());
        Fixture::new(g.path()).remove("run/systemd");
        assert_eq!(probe(&roots, &env).backend(), Backend::CgroupV2Raw);
    }

    #[test]
    fn kconfig_from_config_gz() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        let d = tempfile::tempdir().unwrap();
        let (roots, env) = fixture::pop_os(d.path());
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(b"CONFIG_PSI=y\nCONFIG_BLK_CGROUP_IOLATENCY=y\n").unwrap();
        fs::write(roots.proc.join("config.gz"), gz.finish().unwrap()).unwrap();
        let p = probe(&roots, &env);
        assert_eq!(p.io_latency, IoLatency::NotEnabled);
        assert_eq!(p.kconfig.unwrap().len(), 2);
    }

    #[test]
    fn desktop_detection() {
        let d = |pairs: &[(&str, &str)]| desktop(&pairs.iter().copied().collect());
        assert_eq!(d(&[("XDG_CURRENT_DESKTOP", "pop:GNOME"), ("WAYLAND_DISPLAY", "w")]).kind, DesktopKind::Gnome);
        assert_eq!(d(&[("XDG_CURRENT_DESKTOP", "pop:GNOME"), ("WAYLAND_DISPLAY", "w")]).session, SessionType::Wayland);
        assert_eq!(d(&[("XDG_CURRENT_DESKTOP", "COSMIC")]).kind, DesktopKind::Cosmic);
        assert_eq!(d(&[("DESKTOP_SESSION", "plasma")]).kind, DesktopKind::Kde);
        assert_eq!(d(&[("XDG_CURRENT_DESKTOP", "sway"), ("XDG_SESSION_TYPE", "wayland")]).kind, DesktopKind::Other);
        assert_eq!(d(&[("DISPLAY", ":1")]).session, SessionType::X11);
        assert_eq!(d(&[("XDG_SESSION_TYPE", "tty")]).session, SessionType::Tty);
        assert_eq!(d(&[]).session, SessionType::Unknown);
    }

    #[test]
    fn serde_roundtrip() {
        let (_d, p) = run(fixture::full_bfq);
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"dev\":\"8:0\""));
        let back: Probe = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
        assert_eq!(serde_json::to_string(&SupportLevel::IoPartial).unwrap(), "\"io-partial\"");
    }

    #[test]
    #[ignore = "probes the real host"]
    fn real_host() {
        let p = probe(&Roots::host(), &Env::from_process());
        eprintln!("{}", crate::doctor::render_text(&p, false));
        assert!(p.kernel.is_some());
    }
}
