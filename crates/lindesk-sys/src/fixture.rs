//! Builders for fake `/sys`, `/proc`, cgroup, `/etc`, `/usr`, `/boot` and `/run` trees.
//!
//! Available in this crate's tests and, with the `fixtures` feature, to other crates.
//! [`Fixture`] writes individual files, block devices and mounts under a root directory;
//! the preset functions ([`pop_os`], [`cgroup_v1`], [`no_psi`], [`zfs_root`],
//! [`full_bfq`]) build complete host shapes for probe and doctor tests.

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
    (f.roots(), gnome_wayland())
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
