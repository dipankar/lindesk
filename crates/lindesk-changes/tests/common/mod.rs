#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lindesk_changes::*;
use lindesk_core::clock::SimClock;
use lindesk_plugin_api::{CpuFeatures, Event, Hardware};
use serde_json::Value;

pub const T0: i64 = 1_800_000_000_000;

/// The shipped lindesk.toml: lots of comments to preserve.
pub fn shipped_config() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/config/lindesk.toml")).unwrap()
}

pub struct Fx {
    pub tmp: tempfile::TempDir,
    pub roots: Roots,
    pub paths: ConfigPaths,
    pub clock: SimClock,
    pub events: Arc<Mutex<Vec<Event>>>,
}

fn put(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

impl Fx {
    pub fn new() -> Fx {
        // tmpfs when available: the store fsyncs every record, which is slow on a busy disk.
        let tmp = tempfile::Builder::new()
            .prefix("lindesk-changes-")
            .tempdir_in("/dev/shm")
            .or_else(|_| tempfile::tempdir())
            .unwrap();
        let root = tmp.path().join("root");
        let roots = Roots::under(&root);
        for (k, v) in [
            ("swappiness", "60"),
            ("page-cluster", "3"),
            ("watermark_scale_factor", "10"),
            ("vfs_cache_pressure", "100"),
        ] {
            put(&roots.proc.join("sys/vm").join(k), &format!("{v}\n"));
        }
        put(&roots.proc.join("sys/vm/overcommit_memory"), "0\n");
        put(
            &roots.proc.join("crypto"),
            "name         : lzo-rle\ndriver       : lzo-rle-scomp\n\nname         : zstd\ndriver       : zstd-generic\n\nname         : lz4\n",
        );
        put(&roots.sys.join("block/sda/queue/scheduler"), "[mq-deadline] kyber bfq none\n");
        put(&roots.sys.join("dev/block/8:0/uevent"), "MAJOR=8\nMINOR=0\nDEVNAME=sda\nDEVTYPE=disk\n");
        put(&roots.sys.join("block/nvme0n1/queue/scheduler"), "[none] mq-deadline\n");
        put(&roots.sys.join("dev/block/259:0/uevent"), "MAJOR=259\nMINOR=0\nDEVNAME=nvme0n1\n");
        let z = roots.sys.join("module/zswap/parameters");
        put(&z.join("enabled"), "N\n");
        put(&z.join("compressor"), "lzo-rle\n");
        put(&z.join("max_pool_percent"), "20\n");
        for i in 0..4 {
            let d = roots.sys.join(format!("devices/system/cpu/cpufreq/policy{i}"));
            put(&d.join("energy_performance_preference"), "balance_performance\n");
            put(&d.join("scaling_governor"), "powersave\n");
        }
        std::fs::create_dir_all(&roots.etc).unwrap();
        let cfg = tmp.path().join("config");
        let paths =
            ConfigPaths { user: Some(cfg.join("user/lindesk.toml")), system: Some(cfg.join("system/lindesk.toml")) };
        put(paths.system.as_ref().unwrap(), &shipped_config());
        Fx { tmp, roots, paths, clock: SimClock::new(T0), events: Arc::new(Mutex::new(Vec::new())) }
    }

    pub fn store_dir(&self, scope: Scope) -> PathBuf {
        self.tmp.path().join(format!("changes-{scope}"))
    }

    pub fn applier(&self) -> RealApplier {
        RealApplier::new(self.roots.clone(), self.paths.clone())
    }

    pub fn store_with(&self, scope: Scope, applier: Arc<dyn Applier>) -> ChangeStore {
        let ev = self.events.clone();
        ChangeStore::open(self.store_dir(scope), scope)
            .unwrap()
            .with_applier(applier)
            .with_clock(Arc::new(self.clock.clone()))
            .on_event(move |e| ev.lock().unwrap().push(e.clone()))
    }

    pub fn store(&self, scope: Scope) -> ChangeStore {
        self.store_with(scope, Arc::new(self.applier()))
    }

    pub fn hardware(&self) -> Hardware {
        Hardware {
            features: CpuFeatures {
                epp_available: ["default", "performance", "balance_performance", "balance_power", "power"]
                    .map(String::from)
                    .to_vec(),
                governors_available: vec!["performance".into(), "powersave".into()],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    pub fn ctx(&self, scope: Scope, origin: Origin) -> GuardContext {
        GuardContext::new(scope, origin, self.roots.clone(), self.paths.clone(), self.hardware())
            .with_profiles(["balanced", "latency", "battery", "gaming"])
    }

    pub fn sysctl(&self, k: &str) -> String {
        std::fs::read_to_string(self.roots.proc.join("sys/vm").join(k)).unwrap().trim().to_string()
    }

    pub fn sys(&self, rel: &str) -> String {
        std::fs::read_to_string(self.roots.sys.join(rel)).unwrap().trim().to_string()
    }

    pub fn system_config(&self) -> String {
        std::fs::read_to_string(self.paths.system.as_ref().unwrap()).unwrap()
    }

    pub fn event_names(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| match e {
                Event::Notice { name, .. } => name.clone(),
                _ => "?".into(),
            })
            .collect()
    }

    /// Plan and apply, panicking on rejections.
    pub fn apply(
        &self,
        store: &ChangeStore,
        origin: Origin,
        ops: Vec<ChangeOp>,
        ttl: Option<std::time::Duration>,
    ) -> Result<ChangeSet> {
        let plan = store.plan(&ops, &self.ctx(store.scope(), origin));
        assert!(plan.is_ok(), "{:?}", plan.rejections);
        store.apply(&plan, Author::new(1000, origin), "test", ttl)
    }
}

pub fn sysctl(key: &str, value: &str) -> ChangeOp {
    ChangeOp::Sysctl { key: key.into(), value: value.into(), persist: false }
}

pub fn set(key: &str, value: Value) -> ChangeOp {
    ChangeOp::ConfigSet { scope: Scope::System, key: key.into(), value }
}

/// An applier that fails (or "crashes" by panicking) on the n-th write.
pub struct Faulty {
    pub inner: RealApplier,
    pub writes: AtomicUsize,
    pub fail_at: usize,
    /// Panic instead of returning an error (simulated crash). With `write_first`, the
    /// write happens before the panic (crash between the write and the WAL update).
    pub panic: bool,
    pub write_first: bool,
}

impl Faulty {
    pub fn new(inner: RealApplier, fail_at: usize, panic: bool, write_first: bool) -> Self {
        Faulty { inner, writes: AtomicUsize::new(0), fail_at, panic, write_first }
    }
}

impl Applier for Faulty {
    fn read(&self, op: &ChangeOp) -> Result<Value> {
        self.inner.read(op)
    }

    fn write(&self, op: &ChangeOp, v: &Value) -> Result<()> {
        let n = self.writes.fetch_add(1, Ordering::SeqCst);
        if n == self.fail_at {
            if self.write_first {
                self.inner.write(op, v)?;
            }
            if self.panic {
                panic!("simulated crash during write {n}");
            }
            return Err(ChangeError::Apply("simulated EBUSY".into()));
        }
        self.inner.write(op, v)
    }

    fn config_path(&self, scope: Scope) -> Option<PathBuf> {
        self.inner.config_path(scope)
    }
}
