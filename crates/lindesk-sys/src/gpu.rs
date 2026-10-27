//! Read-only DRM client accounting. Missing counters are unknown, never zero utilization.
use serde::Serialize;
use std::{collections::BTreeMap, fs::File, io::Read, path::Path};

#[derive(Clone, Debug, Default, Serialize)]
pub struct Client {
    pub driver: String,
    pub device: String,
    pub id: u64,
    pub engines_ns: BTreeMap<String, u64>,
    pub capacity: BTreeMap<String, u64>,
    pub resident_bytes: BTreeMap<String, u64>,
}

pub fn parse(text: &str) -> Option<Client> {
    let mut c = Client::default();
    let mut id = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match key {
            "drm-driver" => c.driver = value.into(),
            "drm-pdev" => c.device = value.into(),
            "drm-client-id" => id = value.parse().ok(),
            _ => {
                if let Some(engine) = key.strip_prefix("drm-engine-capacity-") {
                    if let Ok(n) = value.parse::<u64>()
                        && n > 0
                    {
                        c.capacity.insert(engine.into(), n);
                    }
                } else if let Some(engine) = key.strip_prefix("drm-engine-") {
                    let mut v = value.split_whitespace();
                    if let (Some(n), Some("ns")) = (v.next().and_then(|s| s.parse().ok()), v.next()) {
                        c.engines_ns.insert(engine.into(), n);
                    }
                } else if let Some(region) = key.strip_prefix("drm-resident-") {
                    let mut v = value.split_whitespace();
                    if let Some(n) = v.next().and_then(|s| s.parse::<u64>().ok()) {
                        let scale = match v.next() {
                            None | Some("B") => Some(1),
                            Some("KiB") => Some(1024),
                            Some("MiB") => Some(1024 * 1024),
                            _ => None,
                        };
                        if let Some(bytes) = scale.and_then(|s| n.checked_mul(s)) {
                            c.resident_bytes.insert(region.into(), bytes);
                        }
                    }
                }
            }
        }
    }
    c.id = id?;
    (!c.driver.is_empty()).then_some(c)
}

/// Deduplicate shared/duplicated descriptors using driver, device and DRM client id.
/// Bounds work to 1024 descriptors and 64KiB of text per descriptor.
pub fn clients(proc_root: &Path, pid: i32) -> std::io::Result<BTreeMap<String, Client>> {
    if pid <= 0 {
        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    }
    let mut clients = BTreeMap::new();
    for entry in std::fs::read_dir(proc_root.join(pid.to_string()).join("fdinfo"))?.take(1024).flatten() {
        let mut text = String::new();
        if let Ok(file) = File::open(entry.path())
            && file.take(65536).read_to_string(&mut text).is_ok()
            && let Some(c) = parse(&text)
        {
            clients.entry(format!("{}:{}:{}", c.driver, c.device, c.id)).or_insert(c);
        }
    }
    Ok(clients)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_supported_counters_and_rejects_unknown_units() {
        let c = parse("drm-driver: i915\ndrm-client-id: 42\ndrm-pdev: 0000:00:02.0\ndrm-engine-render: 100 ns\ndrm-engine-capacity-render: 2\ndrm-resident-local: 4 KiB\ndrm-resident-bad: 9 apples\n").unwrap();
        assert_eq!(c.engines_ns["render"], 100);
        assert_eq!(c.capacity["render"], 2);
        assert_eq!(c.resident_bytes["local"], 4096);
        assert!(!c.resident_bytes.contains_key("bad"));
        assert!(parse("pos: 0\n").is_none());
        assert!(parse("drm-driver: i915\n").is_none());
    }
    #[test]
    fn duplicate_fds_count_once() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("42/fdinfo");
        std::fs::create_dir_all(&f).unwrap();
        for n in 3..5 {
            std::fs::write(f.join(n.to_string()), "drm-driver: xe\ndrm-client-id: 7\n").unwrap();
        }
        assert_eq!(clients(d.path(), 42).unwrap().len(), 1);
    }
}
