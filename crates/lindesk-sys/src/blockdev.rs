//! Block devices behind mount points, and their I/O schedulers.
//!
//! cgroup I/O controls (`io.max`, `io.weight`, iocost) apply to whole disks, so a mount
//! point is resolved through `/proc/self/mountinfo` to its `MAJ:MIN`, then partitions are
//! walked up to their disk (`/sys/dev/block/M:m/partition` → parent directory) and
//! device-mapper/md devices down to their members (`slaves/`).

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Roots;
use crate::util::{invalid_data, read_lossy, read_trimmed};

/// A device number, displayed and serialised as `MAJ:MIN`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DevId {
    pub major: u32,
    pub minor: u32,
}

impl DevId {
    /// A device number.
    pub fn new(major: u32, minor: u32) -> Self {
        DevId { major, minor }
    }

    /// Anonymous devices (major 0) back virtual filesystems and btrfs/ZFS subvolumes.
    pub fn is_anonymous(self) -> bool {
        self.major == 0
    }
}

impl fmt::Display for DevId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.major, self.minor)
    }
}

impl FromStr for DevId {
    type Err = io::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (a, b) = s.trim().split_once(':').ok_or_else(|| invalid_data(format!("bad device number '{s}'")))?;
        let p = |x: &str| x.parse::<u32>().map_err(|_| invalid_data(format!("bad device number '{s}'")));
        Ok(DevId { major: p(a)?, minor: p(b)? })
    }
}

impl Serialize for DevId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DevId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

/// One line of `/proc/self/mountinfo`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MountEntry {
    pub dev: DevId,
    pub mount_point: PathBuf,
    pub fs_type: String,
    /// Mount source, e.g. `/dev/nvme0n1p3` or `rpool/ROOT/ubuntu`.
    pub source: String,
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse `/proc/self/mountinfo`. Malformed lines are skipped.
pub fn parse_mountinfo(s: &str) -> Vec<MountEntry> {
    s.lines()
        .filter_map(|line| {
            let (pre, post) = line.split_once(" - ")?;
            let pre: Vec<&str> = pre.split(' ').collect();
            let mut post = post.split(' ');
            Some(MountEntry {
                dev: pre.get(2)?.parse().ok()?,
                mount_point: PathBuf::from(unescape(pre.get(4)?)),
                fs_type: post.next()?.to_string(),
                source: unescape(post.next()?),
            })
        })
        .collect()
}

/// Mounts of the process whose `/proc/self` is under `roots.proc`.
pub fn mounts(roots: &Roots) -> io::Result<Vec<MountEntry>> {
    Ok(parse_mountinfo(&fs::read_to_string(roots.self_mountinfo())?))
}

/// The mount that contains `path` (longest matching mount point; later mounts shadow
/// earlier ones on the same point). `path` is compared lexically, not canonicalised.
pub fn mount_for<'a>(mounts: &'a [MountEntry], path: &Path) -> Option<&'a MountEntry> {
    let comps = |p: &Path| p.components().filter(|c| matches!(c, Component::Normal(_))).count();
    mounts.iter().filter(|m| path.starts_with(&m.mount_point)).max_by_key(|m| comps(&m.mount_point)).and_then(|best| {
        // Among mounts on the same point, the last one wins.
        mounts.iter().rev().find(|m| m.mount_point == best.mount_point)
    })
}

/// A whole block device and its queue settings.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BlockDev {
    pub dev: DevId,
    /// Kernel name, e.g. `nvme0n1`.
    pub name: String,
    /// Active I/O scheduler (`none`, `mq-deadline`, `bfq`, `kyber`), if the device has a
    /// request queue scheduler.
    pub scheduler: Option<String>,
    /// All schedulers the device offers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedulers: Vec<String>,
    /// `queue/rotational`.
    pub rotational: Option<bool>,
}

impl BlockDev {
    /// True if BFQ is the active scheduler (so `io.weight`/ioprio work without iocost).
    pub fn is_bfq(&self) -> bool {
        self.scheduler.as_deref() == Some("bfq")
    }
}

/// Parse `queue/scheduler` (`none mq-deadline kyber [bfq]`) into (active, available).
pub fn parse_scheduler(s: &str) -> (Option<String>, Vec<String>) {
    let mut active = None;
    let mut all = Vec::new();
    for tok in s.split_whitespace() {
        if let Some(inner) = tok.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
            active = Some(inner.to_string());
            all.push(inner.to_string());
        } else {
            all.push(tok.to_string());
        }
    }
    if active.is_none() && all.len() == 1 {
        active = all.first().cloned();
    }
    (active, all)
}

fn dev_dir(sys_root: &Path, dev: DevId) -> io::Result<PathBuf> {
    fs::canonicalize(sys_root.join("dev/block").join(dev.to_string()))
}

fn read_dev(dir: &Path) -> io::Result<DevId> {
    read_trimmed(&dir.join("dev"))?.parse()
}

/// Describe a block device from sysfs.
pub fn block_dev(sys_root: &Path, dev: DevId) -> io::Result<BlockDev> {
    let dir = dev_dir(sys_root, dev)?;
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
    let (scheduler, schedulers) =
        read_lossy(&dir.join("queue/scheduler")).map(|s| parse_scheduler(&s)).unwrap_or_default();
    let rotational = read_lossy(&dir.join("queue/rotational")).map(|s| s == "1");
    Ok(BlockDev { dev, name, scheduler, schedulers, rotational })
}

/// Whole disks backing `dev`: a partition resolves to its disk, a dm/md device to the
/// disks of its members (recursively); anything else is itself.
pub fn whole_disks(sys_root: &Path, dev: DevId) -> io::Result<Vec<DevId>> {
    let mut out = BTreeSet::new();
    walk(sys_root, dev, 0, &mut out)?;
    Ok(out.into_iter().collect())
}

fn walk(sys_root: &Path, dev: DevId, depth: u32, out: &mut BTreeSet<DevId>) -> io::Result<()> {
    if depth > 16 {
        return Err(invalid_data("block device stacking too deep"));
    }
    let dir = dev_dir(sys_root, dev)?;
    if dir.join("partition").exists() {
        let parent = dir.parent().ok_or_else(|| invalid_data("partition without parent"))?;
        return walk(sys_root, read_dev(parent)?, depth + 1, out);
    }
    let mut slaves: Vec<_> = match fs::read_dir(dir.join("slaves")) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => Vec::new(),
    };
    slaves.sort();
    if slaves.is_empty() {
        out.insert(dev);
        return Ok(());
    }
    for s in slaves {
        walk(sys_root, read_dev(&s)?, depth + 1, out)?;
    }
    Ok(())
}

/// Find the device number for a mount source like `/dev/sda2` or `/dev/mapper/root`.
/// Used when mountinfo reports an anonymous device (btrfs).
pub fn dev_from_source(sys_root: &Path, source: &str) -> Option<DevId> {
    let name = source.strip_prefix("/dev/")?;
    if let Some(mapped) = name.strip_prefix("mapper/") {
        let rd = fs::read_dir(sys_root.join("block")).ok()?;
        return rd.filter_map(Result::ok).find_map(|e| {
            let p = e.path();
            (read_lossy(&p.join("dm/name")).as_deref() == Some(mapped)).then(|| read_dev(&p).ok()).flatten()
        });
    }
    let base = Path::new(name).file_name()?;
    read_dev(&sys_root.join("class/block").join(base)).ok()
}

/// Whole disks behind a mount. Empty for virtual filesystems and pools without a block
/// device (ZFS).
pub fn mount_devices(sys_root: &Path, m: &MountEntry) -> Vec<DevId> {
    let dev = if m.dev.is_anonymous() { dev_from_source(sys_root, &m.source) } else { Some(m.dev) };
    dev.and_then(|d| whole_disks(sys_root, d).ok()).unwrap_or_default()
}

/// The whole disks behind the filesystems containing `paths`, deduplicated, in order of
/// first appearance. Unresolvable paths are skipped.
pub fn devices_for<P: AsRef<Path>>(roots: &Roots, paths: &[P]) -> Vec<BlockDev> {
    let Ok(ms) = mounts(roots) else { return Vec::new() };
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for p in paths {
        let Some(m) = mount_for(&ms, p.as_ref()) else { continue };
        for d in mount_devices(&roots.sys, m) {
            if seen.insert(d)
                && let Ok(b) = block_dev(&roots.sys, d)
            {
                out.push(b);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Fixture;

    const MOUNTINFO: &str = "\
35 2 259:3 / / rw,noatime shared:1 - ext4 /dev/nvme0n1p3 rw,errors=remount-ro
39 28 0:33 / /sys/fs/cgroup rw,nosuid shared:10 - cgroup2 cgroup2 rw,nsdelegate
110 35 259:1 / /boot/efi rw,relatime shared:78 - vfat /dev/nvme0n1p1 rw
120 35 252:1 / /home rw,relatime shared:79 - xfs /dev/mapper/home rw
121 35 0:44 / /mnt/my\\040disk rw,relatime shared:80 - btrfs /dev/sda2 rw
";

    fn host() -> (tempfile::TempDir, Roots) {
        let d = tempfile::tempdir().unwrap();
        let f = Fixture::new(d.path());
        f.disk("nvme0n1", "259:0", "[none] mq-deadline kyber bfq", false);
        f.partition("nvme0n1", "nvme0n1p1", "259:1", 1);
        f.partition("nvme0n1", "nvme0n1p3", "259:3", 3);
        f.partition("nvme0n1", "nvme0n1p4", "259:4", 4);
        f.disk("sda", "8:0", "mq-deadline kyber [bfq] none", true);
        f.partition("sda", "sda2", "8:2", 2);
        f.dm("dm-1", "252:1", "home", &["nvme0n1p4", "sda2"]);
        f.file("proc/self/mountinfo", MOUNTINFO);
        let r = f.roots();
        (d, r)
    }

    #[test]
    fn devid() {
        let d: DevId = "259:3".parse().unwrap();
        assert_eq!(d, DevId::new(259, 3));
        assert_eq!(d.to_string(), "259:3");
        assert!("259".parse::<DevId>().is_err());
        assert_eq!(serde_json::to_string(&d).unwrap(), "\"259:3\"");
        assert_eq!(serde_json::from_str::<DevId>("\"8:0\"").unwrap(), DevId::new(8, 0));
    }

    #[test]
    fn mountinfo_parsing() {
        let ms = parse_mountinfo(MOUNTINFO);
        assert_eq!(ms.len(), 5);
        assert_eq!(ms[0].dev, DevId::new(259, 3));
        assert_eq!(ms[0].fs_type, "ext4");
        assert_eq!(ms[4].mount_point, PathBuf::from("/mnt/my disk"));
        assert_eq!(mount_for(&ms, Path::new("/home/u/x")).unwrap().fs_type, "xfs");
        assert_eq!(mount_for(&ms, Path::new("/homework")).unwrap().mount_point, PathBuf::from("/"));
        assert_eq!(mount_for(&ms, Path::new("/boot/efi")).unwrap().fs_type, "vfat");
        assert_eq!(mount_for(&ms, Path::new("/")).unwrap().source, "/dev/nvme0n1p3");
        // A later mount on the same point shadows the earlier one.
        let shadow = format!("{MOUNTINFO}200 35 0:99 / /home rw - tmpfs tmpfs rw\n");
        assert_eq!(mount_for(&parse_mountinfo(&shadow), Path::new("/home")).unwrap().fs_type, "tmpfs");
        assert_eq!(unescape("a\\040b\\134"), "a b\\");
    }

    #[test]
    fn scheduler_parsing() {
        assert_eq!(parse_scheduler("[none] mq-deadline kyber bfq").0.as_deref(), Some("none"));
        let (a, all) = parse_scheduler("none mq-deadline kyber [bfq]");
        assert_eq!(a.as_deref(), Some("bfq"));
        assert_eq!(all, ["none", "mq-deadline", "kyber", "bfq"]);
        assert_eq!(parse_scheduler("none").0.as_deref(), Some("none"));
        assert_eq!(parse_scheduler("").0, None);
    }

    #[test]
    fn resolve_partitions_and_dm() {
        let (_d, roots) = host();
        assert_eq!(whole_disks(&roots.sys, DevId::new(259, 3)).unwrap(), [DevId::new(259, 0)]);
        assert_eq!(whole_disks(&roots.sys, DevId::new(259, 0)).unwrap(), [DevId::new(259, 0)]);
        assert_eq!(whole_disks(&roots.sys, DevId::new(252, 1)).unwrap(), [DevId::new(8, 0), DevId::new(259, 0)]);
        assert!(whole_disks(&roots.sys, DevId::new(1, 1)).is_err());
        let b = block_dev(&roots.sys, DevId::new(259, 0)).unwrap();
        assert_eq!(b.name, "nvme0n1");
        assert_eq!(b.scheduler.as_deref(), Some("none"));
        assert_eq!(b.rotational, Some(false));
        let s = block_dev(&roots.sys, DevId::new(8, 0)).unwrap();
        assert!(s.is_bfq() && s.rotational == Some(true));
        assert_eq!(dev_from_source(&roots.sys, "/dev/mapper/home"), Some(DevId::new(252, 1)));
        assert_eq!(dev_from_source(&roots.sys, "/dev/sda2"), Some(DevId::new(8, 2)));
        assert_eq!(dev_from_source(&roots.sys, "rpool/ROOT"), None);
    }

    #[test]
    fn devices_for_mounts() {
        let (_d, roots) = host();
        let devs = devices_for(&roots, &["/", "/home", "/mnt/my disk/x", "/sys/fs/cgroup"]);
        let names: Vec<_> = devs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["nvme0n1", "sda"]);
        assert!(devices_for(&Roots::under("/nonexistent"), &["/"]).is_empty());
    }

    #[test]
    #[ignore = "reads the host's /proc and /sys"]
    fn real_root_device() {
        let devs = devices_for(&Roots::host(), &["/"]);
        assert!(!devs.is_empty());
        eprintln!("{devs:#?}");
    }
}
