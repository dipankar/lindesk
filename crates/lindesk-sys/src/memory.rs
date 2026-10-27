//! Effective memory protection is constrained by every ancestor, not just the leaf.
use crate::cgroupfs::CgroupFs;
use std::path::Path;

#[derive(Debug, serde::Serialize)]
pub struct Protection {
    pub configured: u64,
    /// An upper bound: competing siblings can further reduce the effective share.
    pub ancestor_ceiling: u64,
    pub limiting_cgroup: Option<String>,
}

pub fn protection(root: &Path, cgroup: &str) -> std::io::Result<Protection> {
    let fs = CgroupFs::new(root);
    let read = |cg: &str| fs.memory_low(cg).map(|v| v.unwrap_or(u64::MAX));
    let configured = read(cgroup)?;
    let mut out = Protection { configured, ancestor_ceiling: configured, limiting_cgroup: None };
    let mut path = cgroup.trim_end_matches('/');
    while let Some((parent, _)) = path.rsplit_once('/') {
        if parent.is_empty() {
            break;
        }
        let low = read(parent)?;
        if low < out.ancestor_ceiling {
            out.ancestor_ceiling = low;
            out.limiting_cgroup = Some(parent.to_string());
        }
        path = parent;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn zero_ancestor_blocks_leaf_even_with_recursive_protection() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("user.slice/app.slice/app.scope")).unwrap();
        for (p, v) in [("user.slice", "0"), ("user.slice/app.slice", "100"), ("user.slice/app.slice/app.scope", "64")] {
            std::fs::write(d.path().join(p).join("memory.low"), v).unwrap();
        }
        let a = protection(d.path(), "/user.slice/app.slice/app.scope").unwrap();
        assert_eq!((a.configured, a.ancestor_ceiling), (64, 0));
        assert_eq!(a.limiting_cgroup.as_deref(), Some("/user.slice"));
        std::fs::write(d.path().join("user.slice/memory.low"), "100").unwrap();
        assert_eq!(protection(d.path(), "/user.slice/app.slice/app.scope").unwrap().ancestor_ceiling, 64);
    }
}
