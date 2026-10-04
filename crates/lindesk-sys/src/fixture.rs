//! Builders for fake `/sys`, `/proc`, cgroup, `/etc`, `/usr`, `/boot` and `/run` trees.
//!
//! Available in this crate's tests and, with the `fixtures` feature, to other crates.
//! [`Fixture`] writes individual files, block devices and mounts under a root directory;
//! the preset functions ([`pop_os`], [`cgroup_v1`], [`no_psi`], [`zfs_root`],
//! [`full_bfq`]) build complete host shapes for probe and doctor tests, and
//! [`intel_hybrid_laptop`], [`arm_big_little`], [`hdd_desktop`], [`kvm_guest`],
//! [`amd_desktop`] and [`asahi_m1`] add hardware, cpufreq and swap profiles for
//! [`crate::hardware`], [`crate::swap`] and the requirement checks.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use crate::{Env, Roots};

/// Writes a fake system tree under `root`. Methods panic on I/O errors: this is test code.
#[derive(Clone, Debug)]
pub struct Fixture {
    root: PathBuf,
}

impl Fixture {
    /// A fixture rooted at `root` (usually a temporary directory).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Fixture { root: root.into() }
    }

    /// The fixture root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// [`Roots`] pointing into this fixture.
    pub fn roots(&self) -> Roots {
        Roots::under(&self.root)
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel.trim_start_matches('/'))
    }

    /// Write a file (creating parent directories).
    pub fn file(&self, rel: &str, contents: impl AsRef<[u8]>) -> &Self {
        let p = self.p(rel);
        fs::create_dir_all(p.parent().expect("file has a parent")).expect("create parent dirs");
        fs::write(&p, contents).expect("write fixture file");
        self
    }

    /// Create a directory (and parents).
    pub fn dir(&self, rel: &str) -> &Self {
        fs::create_dir_all(self.p(rel)).expect("create fixture dir");
        self
    }

    /// Remove a file or directory tree if it exists.
    pub fn remove(&self, rel: &str) -> &Self {
        let p = self.p(rel);
        if p.is_dir() {
            fs::remove_dir_all(&p).expect("remove fixture dir");
        } else if p.exists() {
            fs::remove_file(&p).expect("remove fixture file");
        }
        self
    }

    /// Create a symlink at `rel` pointing to the fixture path `target` (absolute link).
    pub fn symlink(&self, rel: &str, target: &str) -> &Self {
        let link = self.p(rel);
        fs::create_dir_all(link.parent().expect("link has a parent")).expect("create parent dirs");
        let _ = fs::remove_file(&link);
        symlink(self.p(target), &link).expect("create fixture symlink");
        self
    }

    fn register_block(&self, name: &str, dev: &str, dir: &str) {
        self.symlink(&format!("sys/dev/block/{dev}"), dir);
        self.symlink(&format!("sys/class/block/{name}"), dir);
    }

    fn block_dir(&self, name: &str) -> String {
        let canon = fs::canonicalize(self.p(&format!("sys/class/block/{name}"))).expect("known block device");
        let root = fs::canonicalize(&self.root).expect("fixture root");
        canon.strip_prefix(root).expect("inside fixture").to_string_lossy().into_owned()
    }

    /// A whole disk with a request queue. `scheduler` is the raw `queue/scheduler`
    /// contents, e.g. `"[none] mq-deadline kyber bfq"`.
    pub fn disk(&self, name: &str, dev: &str, scheduler: &str, rotational: bool) -> &Self {
        let dir = format!("sys/devices/fixture/block/{name}");
        self.file(&format!("{dir}/dev"), format!("{dev}\n"))
            .file(&format!("{dir}/queue/scheduler"), format!("{scheduler}\n"))
            .file(&format!("{dir}/queue/rotational"), if rotational { "1\n" } else { "0\n" })
            .symlink(&format!("sys/block/{name}"), &dir);
        self.register_block(name, dev, &dir);
        self
    }

    /// A partition of `disk`.
    pub fn partition(&self, disk: &str, name: &str, dev: &str, number: u32) -> &Self {
        let dir = format!("{}/{name}", self.block_dir(disk));
        self.file(&format!("{dir}/dev"), format!("{dev}\n")).file(&format!("{dir}/partition"), format!("{number}\n"));
        self.register_block(name, dev, &dir);
        self
    }

    /// A device-mapper device (`/dev/mapper/<dm_name>`) stacked on `slaves`.
    pub fn dm(&self, name: &str, dev: &str, dm_name: &str, slaves: &[&str]) -> &Self {
        let dir = format!("sys/devices/virtual/block/{name}");
        self.file(&format!("{dir}/dev"), format!("{dev}\n"))
            .file(&format!("{dir}/dm/name"), format!("{dm_name}\n"))
            .dir(&format!("{dir}/slaves"))
            .symlink(&format!("sys/block/{name}"), &dir);
        for s in slaves {
            let target = self.block_dir(s);
            self.symlink(&format!("{dir}/slaves/{s}"), &target);
        }
        self.register_block(name, dev, &dir);
        self
    }

    /// `/proc/self/mountinfo` from `(mount point, MAJ:MIN, fs type, source)` tuples.
    pub fn mounts(&self, mounts: &[(&str, &str, &str, &str)]) -> &Self {
        let s: String = mounts
            .iter()
            .enumerate()
            .map(|(i, (mp, dev, fs, src))| {
                format!("{} 1 {dev} / {mp} rw,relatime shared:{i} - {fs} {src} rw\n", 30 + i)
            })
            .collect();
        self.file("proc/self/mountinfo", s)
    }

    /// A process with a `comm` (for running-daemon detection).
    pub fn process(&self, pid: i32, comm: &str) -> &Self {
        self.file(&format!("proc/{pid}/comm"), format!("{comm}\n"))
    }

    /// Global PSI files under `/proc/pressure`.
    pub fn global_psi(&self) -> &Self {
        for r in ["cpu", "io", "memory"] {
            self.file(&format!("proc/pressure/{r}"), PSI);
        }
        self
    }

    /// `/sys/devices/system/cpu/online` and, when given, `smt/active`.
    pub fn cpus_online(&self, list: &str, smt_active: Option<bool>) -> &Self {
        self.file("sys/devices/system/cpu/online", format!("{list}\n"));
        if let Some(on) = smt_active {
            self.file("sys/devices/system/cpu/smt/active", if on { "1\n" } else { "0\n" });
        }
        self
    }

    /// One logical CPU: its core's sibling list (`core_cpus_list`), and optionally its
    /// `cpu_capacity` and `cpufreq/cpuinfo_max_freq` (kHz).
    pub fn cpu(&self, n: u32, siblings: &str, capacity: Option<u32>, max_khz: Option<u64>) -> &Self {
        let base = format!("sys/devices/system/cpu/cpu{n}");
        self.file(&format!("{base}/topology/core_cpus_list"), format!("{siblings}\n"));
        if let Some(c) = capacity {
            self.file(&format!("{base}/cpu_capacity"), format!("{c}\n"));
        }
        if let Some(k) = max_khz {
            self.file(&format!("{base}/cpufreq/cpuinfo_max_freq"), format!("{k}\n"));
        }
        self
    }

    /// `/proc/meminfo` with `MemTotal` and `SwapTotal` (kB).
    pub fn meminfo(&self, mem_kb: u64, swap_kb: u64) -> &Self {
        self.file(
            "proc/meminfo",
            format!("MemTotal:       {mem_kb} kB\nMemFree:        {} kB\nSwapTotal:      {swap_kb} kB\n", mem_kb / 2),
        )
    }

    /// `/proc/swaps` from `(file name, type, size kB)`.
    pub fn swaps(&self, entries: &[(&str, &str, u64)]) -> &Self {
        let mut s = String::from("Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n");
        for (i, (name, ty, kb)) in entries.iter().enumerate() {
            s.push_str(&format!("{name:<40}{ty}\t{kb}\t\t0\t\t{}\n", -(i as i32) - 1));
        }
        self.file("proc/swaps", s)
    }

    /// A `/sys/class/power_supply/<name>` entry with attribute files (`type`, `online`,
    /// `status`, `scope`, ...).
    pub fn power_supply(&self, name: &str, attrs: &[(&str, &str)]) -> &Self {
        for (k, v) in attrs {
            self.file(&format!("sys/class/power_supply/{name}/{k}"), format!("{v}\n"));
        }
        self
    }

    /// `/sys/class/dmi/id/<attr>` files (`chassis_type`, `sys_vendor`, `product_name`, ...).
    pub fn dmi(&self, attrs: &[(&str, &str)]) -> &Self {
        for (k, v) in attrs {
            self.file(&format!("sys/class/dmi/id/{k}"), format!("{v}\n"));
        }
        self
    }

    /// A cpufreq policy directory (`/sys/devices/system/cpu/cpufreq/policy<n>`) with
    /// attribute files (`scaling_driver`, `scaling_governor`, ...).
    pub fn cpufreq_policy(&self, n: u32, attrs: &[(&str, &str)]) -> &Self {
        for (k, v) in attrs {
            self.file(&format!("sys/devices/system/cpu/cpufreq/policy{n}/{k}"), format!("{v}\n"));
        }
        self
    }

    /// `/proc/sys/<key>` files, keys with dots or slashes (`vm.swappiness`, `kernel/arch`).
    pub fn sysctl(&self, entries: &[(&str, &str)]) -> &Self {
        for (k, v) in entries {
            self.file(&format!("proc/sys/{}", k.replace('.', "/")), format!("{v}\n"));
        }
        self
    }

    /// `/proc/vmstat` with the swap counters.
    pub fn vmstat(&self, pswpin: u64, pswpout: u64) -> &Self {
        self.file("proc/vmstat", format!("nr_free_pages 100000\npswpin {pswpin}\npswpout {pswpout}\npgfault 1\n"))
    }

    /// zswap module parameters.
    pub fn zswap(&self, enabled: bool, compressor: &str, max_pool_percent: u32) -> &Self {
        let d = "sys/module/zswap/parameters";
        self.file(&format!("{d}/enabled"), if enabled { "Y\n" } else { "N\n" })
            .file(&format!("{d}/compressor"), format!("{compressor}\n"))
            .file(&format!("{d}/max_pool_percent"), format!("{max_pool_percent}\n"))
            .file(&format!("{d}/zpool"), "zsmalloc\n")
            .file(&format!("{d}/shrinker_enabled"), "Y\n")
    }

    /// A zram device (`/sys/block/<name>`) with its compression algorithm.
    pub fn zram(&self, name: &str, dev: &str, algorithm: &str) -> &Self {
        self.disk(name, dev, "none", false).file(
            &format!("sys/block/{name}/comp_algorithm"),
            format!(
                "lzo-rle lzo lz4 {}\n",
                if algorithm.is_empty() { String::new() } else { format!("[{algorithm}]") }
            ),
        )
    }

    /// A cgroup v2 directory (relative to the cgroup mount) with attribute files.
    pub fn cgroup(&self, cg: &str, files: &[(&str, &str)]) -> &Self {
        let base = format!("sys/fs/cgroup/{}", cg.trim_matches('/'));
        self.dir(&base);
        for (name, contents) in files {
            self.file(&format!("{base}/{name}"), format!("{contents}\n"));
        }
        self
    }
}

const PSI: &str = "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";

/// Knobs for [`v2_host`].
#[derive(Clone, Debug)]
pub struct V2Host {
    pub release: &'static str,
    /// Kernel config contents written to `boot/config-<release>` (None = no config file).
    pub kconfig: Option<String>,
    pub psi: bool,
    pub uclamp: bool,
    /// `io.latency` files in child cgroups.
    pub io_latency: bool,
    /// `io.cost.qos` exists at the root.
    pub iocost: bool,
    /// Contents of `io.cost.qos`.
    pub iocost_qos: String,
    pub systemd: bool,
    /// Root controllers.
    pub controllers: &'static str,
    /// Controllers delegated to `user@1000.service`.
    pub user_controllers: &'static str,
}

impl Default for V2Host {
    fn default() -> Self {
        V2Host {
            release: "7.0.11-76070011-generic",
            kconfig: Some(kconfig(&[
                ("PSI", "y"),
                ("UCLAMP_TASK_GROUP", "y"),
                ("BLK_CGROUP_IOCOST", "y"),
                ("BLK_CGROUP_IOLATENCY", "n"),
                ("SCHED_CLASS_EXT", "y"),
            ])),
            psi: true,
            uclamp: true,
            io_latency: false,
            iocost: true,
            iocost_qos: String::new(),
            systemd: true,
            controllers: "cpuset cpu io memory hugetlb pids rdma misc dmem",
            user_controllers: "cpu io memory pids",
        }
    }
}

/// Render kernel config lines; value `"n"` becomes `# CONFIG_X is not set`.
pub fn kconfig(entries: &[(&str, &str)]) -> String {
    entries
        .iter()
        .map(|(k, v)| if *v == "n" { format!("# CONFIG_{k} is not set\n") } else { format!("CONFIG_{k}={v}\n") })
        .collect()
}

/// A systemd + cgroup v2 host without block devices, mounts or desktop specifics.
pub fn v2_host(f: &Fixture, h: &V2Host) {
    f.file("proc/sys/kernel/osrelease", format!("{}\n", h.release)).process(1, "systemd");
    if let Some(cfg) = &h.kconfig {
        f.file(&format!("boot/config-{}", h.release), cfg);
    }
    if h.systemd {
        f.dir("run/systemd/system").file("usr/lib/systemd/libsystemd-shared-255.so", "");
    }
    if h.psi {
        f.global_psi();
    }
    let psi_files: &[(&str, &str)] =
        if h.psi { &[("cpu.pressure", PSI), ("io.pressure", PSI), ("memory.pressure", PSI)] } else { &[] };
    let mut root = vec![("cgroup.controllers", h.controllers), ("cgroup.subtree_control", h.controllers)];
    if h.iocost {
        root.push(("io.cost.qos", h.iocost_qos.as_str()));
        root.push(("io.cost.model", ""));
    }
    root.extend_from_slice(psi_files);
    f.cgroup("/", &root);

    let has = |c: &str| h.controllers.split_whitespace().any(|x| x == c);
    for child in ["user.slice", "system.slice", "init.scope"] {
        let mut files = vec![("cgroup.controllers", h.controllers), ("cgroup.procs", "")];
        files.extend_from_slice(psi_files);
        if has("cpu") {
            files.extend([("cpu.weight", "100"), ("cpu.idle", "0"), ("cpu.stat", "usage_usec 0")]);
            if h.uclamp {
                files.extend([("cpu.uclamp.min", "0.00"), ("cpu.uclamp.max", "max")]);
            }
        }
        if has("io") {
            files.extend([("io.weight", "default 100"), ("io.max", ""), ("io.stat", "")]);
            if h.io_latency {
                files.push(("io.latency", ""));
            }
        }
        if has("memory") {
            files.extend([("memory.low", "0"), ("memory.high", "max"), ("memory.current", "0")]);
        }
        f.cgroup(child, &files);
    }
    f.cgroup("/user.slice/user-1000.slice/user@1000.service", &[("cgroup.controllers", h.user_controllers)]);
    f.file("sys/kernel/btf/vmlinux", "").file("sys/kernel/sched_ext/state", "disabled\n");
}

/// The development host: Pop!_OS 24.04, kernel 7.0.11, cgroup v2, no `io.latency`
/// (`CONFIG_BLK_CGROUP_IOLATENCY` not set), iocost available but disabled, NVMe on the
/// `none` scheduler, ext4 root, GNOME on Wayland, system76-scheduler installed but not
/// running. Expected level: IO-PARTIAL.
pub fn pop_os(root: &Path) -> (Roots, Env) {
    let f = Fixture::new(root);
    v2_host(&f, &V2Host::default());
    f.disk("nvme0n1", "259:0", "[none] mq-deadline kyber bfq", false)
        .partition("nvme0n1", "nvme0n1p1", "259:1", 1)
        .partition("nvme0n1", "nvme0n1p3", "259:3", 3)
        .mounts(&[
            ("/", "259:3", "ext4", "/dev/nvme0n1p3"),
            ("/sys/fs/cgroup", "0:33", "cgroup2", "cgroup2"),
            ("/boot/efi", "259:1", "vfat", "/dev/nvme0n1p1"),
        ])
        .file("usr/lib/systemd/system/com.system76.Scheduler.service", "[Unit]\n")
        .file("usr/lib/systemd/system/auto-cpufreq.service", "[Unit]\n")
        .process(2000, "gnome-shell");
    dev_host_hardware(&f);
    (f.roots(), gnome_wayland())
}

/// `/proc/cpuinfo` for `n` identical x86 CPUs.
pub fn x86_cpuinfo(n: u32, model: &str, flags: &str) -> String {
    (0..n)
        .map(|i| {
            format!(
                "processor\t: {i}\nvendor_id\t: GenuineIntel\nmodel name\t: {model}\nflags\t\t: fpu vme sse sse2 {flags}\n\n"
            )
        })
        .collect()
}

/// The development host's hardware: an i9-12950HX (8 P-cores with SMT + 8 E-cores, 24
/// threads) mini PC on mains, 62.5 GiB RAM, zram + a swap partition, PCIe NVMe.
pub fn dev_host_hardware(f: &Fixture) {
    f.cpus_online("0-23", Some(true));
    for c in 0..24 {
        let (sib, khz) = if c < 16 {
            (format!("{}-{}", c & !1, c | 1), if c < 4 { 5_000_000 } else { 4_900_000 })
        } else {
            (c.to_string(), 3_600_000)
        };
        f.cpu(c, &sib, Some(1024), Some(khz));
    }
    for p in 0..24 {
        f.cpufreq_policy(
            p,
            &[
                ("scaling_driver", "intel_pstate"),
                ("scaling_governor", "powersave"),
                ("energy_performance_preference", "balance_performance"),
                (
                    "energy_performance_available_preferences",
                    "default performance balance_performance balance_power power",
                ),
                ("scaling_available_governors", "performance powersave"),
            ],
        );
    }
    f.file("sys/devices/cpu_core/cpus", "0-15\n")
        .file("sys/devices/cpu_atom/cpus", "16-23\n")
        .file("proc/cpuinfo", x86_cpuinfo(24, "12th Gen Intel(R) Core(TM) i9-12950HX", ""))
        .meminfo(65_574_028, 20_970_996)
        .swaps(&[("/dev/dm-0", "partition", 4_193_784), ("/dev/zram0", "partition", 16_777_212)])
        .file("sys/module/zswap/parameters/enabled", "N\n")
        .file("sys/block/nvme0n1/device/transport", "pcie\n")
        .sysctl(&[
            ("kernel.arch", "x86_64"),
            ("vm.swappiness", "180"),
            ("vm.page-cluster", "0"),
            ("vm.watermark_scale_factor", "125"),
            ("vm.vfs_cache_pressure", "100"),
            ("vm.min_free_kbytes", "655740"),
        ])
        .file("sys/kernel/mm/lru_gen/enabled", "0x0007\n")
        .vmstat(1_971_010, 9_491_966)
        .zram("zram0", "251:0", "zstd")
        .file("sys/devices/system/node/online", "0\n")
        .cgroup(
            "/user.slice/user-1000.slice/user@1000.service/app.slice/app-firefox.scope",
            &[("memory.swap.current", "1073741824")],
        )
        .cgroup("/system.slice/docker.service", &[("memory.swap.current", "52428800")])
        .dmi(&[
            ("chassis_type", "3"),
            ("sys_vendor", "Micro Computer (HK) Tech Limited"),
            ("product_name", "EliteMini Series"),
        ]);
}

/// GNOME on Wayland as on the development host.
pub fn gnome_wayland() -> Env {
    Env::new()
        .with("XDG_CURRENT_DESKTOP", "ubuntu:GNOME")
        .with("XDG_SESSION_TYPE", "wayland")
        .with("WAYLAND_DISPLAY", "wayland-0")
        .with("DISPLAY", ":0")
        .with("DESKTOP_SESSION", "ubuntu")
}

/// A legacy cgroup v1 host (no unified hierarchy) with PSI, BFQ and ananicy-cpp running.
/// Expected level: CPU-ONLY, backend: cgroup v1.
pub fn cgroup_v1(root: &Path) -> (Roots, Env) {
    let f = Fixture::new(root);
    let release = "5.4.0-150-generic";
    f.file("proc/sys/kernel/osrelease", format!("{release}\n"))
        .file(&format!("boot/config-{release}"), kconfig(&[("PSI", "y"), ("BLK_CGROUP_IOLATENCY", "y")]))
        .dir("run/systemd/system")
        .global_psi()
        .file("sys/fs/cgroup/cpu,cpuacct/user.slice/cpu.shares", "1024\n")
        .file("sys/fs/cgroup/blkio/user.slice/blkio.bfq.weight", "default 100\n")
        .file("sys/fs/cgroup/memory/user.slice/memory.soft_limit_in_bytes", "9223372036854771712\n")
        .dir("sys/fs/cgroup/systemd/user.slice")
        .symlink("sys/fs/cgroup/cpu", "sys/fs/cgroup/cpu,cpuacct")
        .disk("sda", "8:0", "mq-deadline [bfq] none", true)
        .partition("sda", "sda1", "8:1", 1)
        .mounts(&[("/", "8:1", "ext4", "/dev/sda1"), ("/sys/fs/cgroup/cpu,cpuacct", "0:30", "cgroup", "cgroup")])
        .file("etc/systemd/system/ananicy-cpp.service", "[Unit]\n")
        .process(1, "systemd")
        .process(700, "ananicy-cpp");
    let env = Env::new().with("XDG_CURRENT_DESKTOP", "XFCE").with("XDG_SESSION_TYPE", "x11").with("DISPLAY", ":0");
    (f.roots(), env)
}

/// The development host's shape, but booted without PSI (`psi=0`).
/// Expected level: CPU-ONLY.
pub fn no_psi(root: &Path) -> (Roots, Env) {
    let (roots, env) = pop_os(root);
    let f = Fixture::new(root);
    f.remove("proc/pressure");
    for cg in ["", "user.slice", "system.slice", "init.scope"] {
        for r in ["cpu", "io", "memory"] {
            f.remove(&format!("sys/fs/cgroup/{cg}/{r}.pressure"));
        }
    }
    (roots, env)
}

/// Root on ZFS (no block device visible to cgroup writeback) with iocost enabled on the
/// pool's disk. Expected level: IO-PARTIAL because writeback attribution is limited.
pub fn zfs_root(root: &Path) -> (Roots, Env) {
    let f = Fixture::new(root);
    let h = V2Host { iocost_qos: "259:0 enable=1 ctrl=auto rpct=0.00 rlat=250000\n".into(), ..V2Host::default() };
    v2_host(&f, &h);
    f.disk("nvme0n1", "259:0", "[none] mq-deadline kyber bfq", false)
        .partition("nvme0n1", "nvme0n1p1", "259:1", 1)
        .partition("nvme0n1", "nvme0n1p2", "259:2", 2)
        .mounts(&[
            ("/", "0:27", "zfs", "rpool/ROOT/ubuntu_abc123"),
            ("/home", "0:28", "zfs", "rpool/USERDATA/home"),
            ("/boot/efi", "259:1", "vfat", "/dev/nvme0n1p1"),
        ]);
    (f.roots(), gnome_wayland())
}

/// Everything available: `io.latency`, BFQ on a SATA disk (root) and on NVMe behind a
/// LUKS `/home`, ext4 and xfs, KDE on X11, auto-cpufreq (not a conflict) running.
/// Expected level: FULL.
pub fn full_bfq(root: &Path) -> (Roots, Env) {
    let f = Fixture::new(root);
    let h = V2Host {
        release: "6.12.9-arch1-1",
        kconfig: Some(kconfig(&[("PSI", "y"), ("BLK_CGROUP_IOLATENCY", "y"), ("BLK_CGROUP_IOCOST", "y")])),
        io_latency: true,
        ..V2Host::default()
    };
    v2_host(&f, &h);
    f.disk("sda", "8:0", "mq-deadline kyber [bfq] none", true)
        .partition("sda", "sda2", "8:2", 2)
        .disk("nvme0n1", "259:0", "none mq-deadline kyber [bfq]", false)
        .partition("nvme0n1", "nvme0n1p2", "259:2", 2)
        .dm("dm-0", "254:0", "home", &["nvme0n1p2"])
        .mounts(&[("/", "8:2", "ext4", "/dev/sda2"), ("/home", "254:0", "xfs", "/dev/mapper/home")])
        .file("sys/kernel/sched_ext/state", "enabled\n")
        .file("sys/kernel/sched_ext/root/ops", "lavd\n")
        .file("usr/lib/systemd/system/auto-cpufreq.service", "[Unit]\n")
        .process(900, "auto-cpufreq");
    let env = Env::new()
        .with("XDG_CURRENT_DESKTOP", "KDE")
        .with("XDG_SESSION_TYPE", "x11")
        .with("DISPLAY", ":0")
        .with("DESKTOP_SESSION", "plasmax11");
    (f.roots(), env)
}

/// An Intel Core Ultra 7 155H laptop on battery: 6 P-cores with SMT + 10 E-cores (22
/// threads, Intel hybrid PMUs), 31.3 GiB RAM with zram, PCIe NVMe on `none`, GNOME on
/// Wayland. Mains adapter offline, BAT0 discharging, chassis "Notebook".
pub fn intel_hybrid_laptop(root: &Path) -> (Roots, Env) {
    let f = Fixture::new(root);
    v2_host(&f, &V2Host { release: "6.8.0-45-generic", ..V2Host::default() });
    f.disk("nvme0n1", "259:0", "[none] mq-deadline kyber bfq", false)
        .file("sys/block/nvme0n1/device/transport", "pcie\n")
        .partition("nvme0n1", "nvme0n1p2", "259:2", 2)
        .mounts(&[("/", "259:2", "ext4", "/dev/nvme0n1p2")])
        .cpus_online("0-21", Some(true));
    for c in 0..22 {
        let sib = if c < 12 { format!("{}-{}", c & !1, c | 1) } else { c.to_string() };
        f.cpu(c, &sib, Some(1024), Some(if c < 12 { 4_800_000 } else { 3_800_000 }));
    }
    f.file("sys/devices/cpu_core/cpus", "0-11\n")
        .file("sys/devices/cpu_atom/cpus", "12-21\n")
        .file("proc/cpuinfo", x86_cpuinfo(22, "Intel(R) Core(TM) Ultra 7 155H", ""))
        .meminfo(32_800_000, 8_388_604)
        .swaps(&[("/dev/zram0", "partition", 8_388_604)])
        .power_supply("AC", &[("type", "Mains"), ("online", "0")])
        .power_supply("BAT0", &[("type", "Battery"), ("status", "Discharging"), ("capacity", "64")])
        .power_supply("hidpp_battery_0", &[("type", "Battery"), ("scope", "Device"), ("status", "Discharging")])
        .dmi(&[("chassis_type", "10"), ("sys_vendor", "LENOVO"), ("product_name", "21KC")])
        .sysctl(&[("kernel.sched_itmt_enabled", "1"), ("vm.swappiness", "60"), ("vm.page-cluster", "3")])
        .zram("zram0", "251:0", "lz4")
        .vmstat(1200, 5400)
        .cpufreq_policy(
            0,
            &[
                ("scaling_driver", "intel_pstate"),
                ("scaling_governor", "powersave"),
                ("energy_performance_preference", "balance_power"),
                (
                    "energy_performance_available_preferences",
                    "default performance balance_performance balance_power power",
                ),
                ("scaling_available_governors", "performance powersave"),
            ],
        )
        .file("sys/devices/system/node/online", "0\n");
    (f.roots(), gnome_wayland())
}

/// An RK3588-style ARM board: 4× Cortex-A55 (capacity 414) + 4× Cortex-A76 (capacity 1024),
/// 3.8 GiB RAM, no swap, root on eMMC (`mmcblk0`, mq-deadline), no power supplies, sway.
pub fn arm_big_little(root: &Path) -> (Roots, Env) {
    let f = Fixture::new(root);
    let h = V2Host {
        release: "6.1.75-rockchip",
        kconfig: Some(kconfig(&[("PSI", "y"), ("UCLAMP_TASK_GROUP", "y"), ("BLK_CGROUP_IOCOST", "y")])),
        ..V2Host::default()
    };
    v2_host(&f, &h);
    f.disk("mmcblk0", "179:0", "[mq-deadline] kyber bfq none", false)
        .file("sys/block/mmcblk0/device/type", "MMC\n")
        .partition("mmcblk0", "mmcblk0p2", "179:2", 2)
        .mounts(&[("/", "179:2", "ext4", "/dev/mmcblk0p2")])
        .cpus_online("0-7", None);
    let mut cpuinfo = String::new();
    for c in 0..8u32 {
        let big = c >= 4;
        f.cpu(c, &c.to_string(), Some(if big { 1024 } else { 414 }), Some(if big { 2_400_000 } else { 1_800_000 }));
        let part = if big { "0xd0b" } else { "0xd05" };
        cpuinfo
            .push_str(&format!("processor\t: {c}\nBogoMIPS\t: 48.00\nCPU implementer\t: 0x41\nCPU part\t: {part}\n\n"));
    }
    f.file("proc/cpuinfo", cpuinfo)
        .meminfo(4_000_000, 0)
        .swaps(&[])
        .sysctl(&[("kernel.arch", "aarch64"), ("kernel.sched_energy_aware", "1"), ("vm.swappiness", "60")])
        .cpufreq_policy(0, &[("scaling_driver", "cpufreq-dt"), ("scaling_governor", "schedutil")])
        .cpufreq_policy(4, &[("scaling_driver", "cpufreq-dt"), ("scaling_governor", "schedutil")]);
    let env = Env::new().with("XDG_CURRENT_DESKTOP", "sway").with("WAYLAND_DISPLAY", "wayland-1");
    (f.roots(), env)
}

/// A desktop with a single SATA hard disk on the `none` scheduler (a bad fit for HDDs), a
/// 4-core i5-4590 without SMT, 15.6 GiB RAM with a 2 GiB swap partition, chassis "Desktop".
/// The only battery is a wireless mouse's (scope Device). Xfce on X11.
pub fn hdd_desktop(root: &Path) -> (Roots, Env) {
    let f = Fixture::new(root);
    v2_host(&f, &V2Host { release: "6.8.0-45-generic", ..V2Host::default() });
    f.disk("sda", "8:0", "[none] mq-deadline kyber bfq", true)
        .partition("sda", "sda2", "8:2", 2)
        .mounts(&[("/", "8:2", "ext4", "/dev/sda2")])
        .cpus_online("0-3", Some(false));
    for c in 0..4 {
        f.cpu(c, &c.to_string(), None, Some(3_700_000));
    }
    f.file("proc/cpuinfo", x86_cpuinfo(4, "Intel(R) Core(TM) i5-4590 CPU @ 3.30GHz", ""))
        .partition("sda", "sda3", "8:3", 3)
        .meminfo(16_340_000, 2_097_148)
        .swaps(&[("/dev/sda3", "partition", 2_097_148)])
        .sysctl(&[("vm.swappiness", "60"), ("vm.page-cluster", "3")])
        .vmstat(250_000, 400_000)
        .cpufreq_policy(
            0,
            &[
                ("scaling_driver", "intel_cpufreq"),
                ("scaling_governor", "powersave"),
                ("scaling_available_governors", "conservative ondemand userspace powersave performance schedutil"),
            ],
        )
        .power_supply("hidpp_battery_0", &[("type", "Battery"), ("scope", "Device"), ("status", "Discharging")])
        .dmi(&[("chassis_type", "3"), ("sys_vendor", "Dell Inc."), ("product_name", "OptiPlex 9020")]);
    let env = Env::new().with("XDG_CURRENT_DESKTOP", "XFCE").with("XDG_SESSION_TYPE", "x11").with("DISPLAY", ":0");
    (f.roots(), env)
}

/// A KVM guest: 8 vCPUs (`hypervisor` flag), 8 GiB RAM, a virtio disk `vda` that reports
/// `rotational=1`, QEMU DMI strings, systemd, no desktop session.
pub fn kvm_guest(root: &Path) -> (Roots, Env) {
    let f = Fixture::new(root);
    v2_host(&f, &V2Host { release: "6.8.0-45-generic", ..V2Host::default() });
    f.disk("vda", "252:0", "[none] mq-deadline", true)
        .partition("vda", "vda1", "252:1", 1)
        .mounts(&[("/", "252:1", "ext4", "/dev/vda1")])
        .cpus_online("0-7", None);
    for c in 0..8 {
        f.cpu(c, &c.to_string(), None, None);
    }
    f.file("proc/cpuinfo", x86_cpuinfo(8, "AMD EPYC 7763 64-Core Processor", "hypervisor")).meminfo(8_388_608, 0).dmi(
        &[
            ("chassis_type", "1"),
            ("sys_vendor", "QEMU"),
            ("product_name", "Standard PC (Q35 + ICH9, 2009)"),
            ("bios_vendor", "SeaBIOS"),
        ],
    );
    (f.roots(), Env::new())
}

/// An AMD Ryzen 9 7950X desktop on mains: 16 cores / 32 threads, amd-pstate-epp with
/// EPP `power` (a poor fit on AC), 2 NUMA-less nodes, 64 GiB RAM, a swap partition on the
/// NVMe with zswap enabled, swappiness 60 and page-cluster 3. KDE on Wayland.
pub fn amd_desktop(root: &Path) -> (Roots, Env) {
    let f = Fixture::new(root);
    v2_host(&f, &V2Host { release: "6.10.3-arch1-1", ..V2Host::default() });
    f.disk("nvme0n1", "259:0", "[none] mq-deadline kyber bfq", false)
        .file("sys/block/nvme0n1/device/transport", "pcie\n")
        .partition("nvme0n1", "nvme0n1p2", "259:2", 2)
        .partition("nvme0n1", "nvme0n1p3", "259:3", 3)
        .mounts(&[("/", "259:2", "btrfs", "/dev/nvme0n1p2")])
        .cpus_online("0-31", Some(true));
    for c in 0..32 {
        f.cpu(c, &format!("{},{}", c % 16, c % 16 + 16), None, Some(if c % 16 < 8 { 5_880_000 } else { 5_700_000 }));
    }
    let mut cpuinfo = x86_cpuinfo(32, "AMD Ryzen 9 7950X 16-Core Processor", "");
    cpuinfo = cpuinfo.replace("GenuineIntel", "AuthenticAMD");
    f.file("proc/cpuinfo", cpuinfo)
        .meminfo(65_000_000, 16_777_212)
        .swaps(&[("/dev/nvme0n1p3", "partition", 16_777_212)])
        .zswap(true, "zstd", 20)
        .sysctl(&[("kernel.arch", "x86_64"), ("vm.swappiness", "60"), ("vm.page-cluster", "3")])
        .file("sys/kernel/mm/lru_gen/enabled", "0x0000\n")
        .vmstat(10, 20)
        .file("sys/devices/system/node/online", "0\n")
        .cpufreq_policy(
            0,
            &[
                ("scaling_driver", "amd-pstate-epp"),
                ("scaling_governor", "powersave"),
                ("energy_performance_preference", "power"),
                (
                    "energy_performance_available_preferences",
                    "default performance balance_performance balance_power power",
                ),
                ("scaling_available_governors", "performance powersave"),
            ],
        )
        .dmi(&[("chassis_type", "3"), ("sys_vendor", "ASUS"), ("product_name", "ROG STRIX X670E-E")]);
    let env = Env::new()
        .with("XDG_CURRENT_DESKTOP", "KDE")
        .with("XDG_SESSION_TYPE", "wayland")
        .with("WAYLAND_DISPLAY", "wayland-0");
    (f.roots(), env)
}

/// An Apple M1 MacBook Air on Asahi Linux: 4 Icestorm (capacity 485) + 4 Firestorm (1024)
/// cores (implementer 0x61), EAS off (no energy model), apple-cpufreq/schedutil, 8 GiB RAM
/// with zram, NVMe (Apple ANS), on battery.
pub fn asahi_m1(root: &Path) -> (Roots, Env) {
    let f = Fixture::new(root);
    v2_host(&f, &V2Host { release: "6.9.12-404.asahi.fc40.aarch64+16k", ..V2Host::default() });
    f.disk("nvme0n1", "259:0", "[none] mq-deadline kyber bfq", false)
        .partition("nvme0n1", "nvme0n1p6", "259:6", 6)
        .mounts(&[("/", "259:6", "btrfs", "/dev/nvme0n1p6")])
        .cpus_online("0-7", None);
    let mut cpuinfo = String::new();
    for c in 0..8u32 {
        let big = c >= 4;
        f.cpu(c, &c.to_string(), Some(if big { 1024 } else { 485 }), Some(if big { 3_204_000 } else { 2_064_000 }));
        let part = if big { "0x023" } else { "0x022" };
        cpuinfo.push_str(&format!("processor\t: {c}\nCPU implementer\t: 0x61\nCPU part\t: {part}\n\n"));
    }
    f.file("proc/cpuinfo", cpuinfo)
        .meminfo(7_900_000, 7_900_000)
        .swaps(&[("/dev/zram0", "partition", 7_900_000)])
        .zram("zram0", "251:0", "lzo-rle")
        .sysctl(&[("kernel.arch", "aarch64"), ("kernel.sched_energy_aware", "0"), ("vm.swappiness", "180")])
        .cpufreq_policy(0, &[("scaling_driver", "apple-cpufreq"), ("scaling_governor", "schedutil")])
        .cpufreq_policy(4, &[("scaling_driver", "apple-cpufreq"), ("scaling_governor", "schedutil")])
        .power_supply("macsmc-ac", &[("type", "Mains"), ("online", "0")])
        .power_supply("macsmc-battery", &[("type", "Battery"), ("status", "Discharging")])
        .dmi(&[]);
    (f.roots(), gnome_wayland())
}
