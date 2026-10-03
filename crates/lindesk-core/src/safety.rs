//! Safety caps: the last word on every value lindesk writes, whoever asked for it.
//!
//! Principle: never make interactive work dangerous, and never starve anything. So
//! there are no realtime classes, boosts are bounded, throttles have floors, and
//! critical workloads can't be throttled at all.

use serde::{Deserialize, Serialize};

use lindesk_plugin_api::{IoLimit, QosClass, Setting};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SafetyCaps {
    pub cpu_weight_min: u64,
    pub cpu_weight_max: u64,
    pub io_weight_min: u64,
    pub io_weight_max: u64,
    /// Upper bound for `cpu.uclamp.min` (percent). 50% ≈ 512/1024.
    pub uclamp_min_max: f32,
    /// Lower bound for `cpu.uclamp.max` (percent).
    pub uclamp_max_floor: f32,
    /// `io.max` bandwidth floor per direction, bytes/s.
    pub io_bps_floor: u64,
    /// `io.max` IOPS floor per direction.
    pub io_iops_floor: u64,
    /// Absolute `memory.high` floor, bytes.
    pub memory_high_floor: u64,
    /// `memory.high` may not go below this fraction of the target's current usage.
    pub memory_high_ratio_floor: f64,
    /// `memory.low` may not exceed this fraction of total RAM.
    pub memory_low_max_ratio: f64,
    /// Most negative niceness lindesk will set.
    pub nice_min: i8,
}

impl Default for SafetyCaps {
    fn default() -> Self {
        SafetyCaps {
            cpu_weight_min: 1,
            cpu_weight_max: 1000,
            io_weight_min: 1,
            io_weight_max: 1000,
            uclamp_min_max: 50.0,
            uclamp_max_floor: 20.0,
            io_bps_floor: 8 * 1024 * 1024,
            io_iops_floor: 100,
            memory_high_floor: 256 * 1024 * 1024,
            memory_high_ratio_floor: 0.5,
            memory_low_max_ratio: 0.25,
            nice_min: -5,
        }
    }
}

/// Facts about the target needed to judge a setting.
#[derive(Clone, Debug, Default)]
pub struct ClampContext {
    pub class: Option<QosClass>,
    /// Current memory usage of the target, bytes.
    pub mem_current: Option<u64>,
    /// Total system RAM, bytes.
    pub mem_total: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Accepted unchanged.
    Ok(Setting),
    /// Accepted after clamping; carries the original for provenance.
    Clamped { setting: Setting, from: Setting },
    /// Refused outright.
    Rejected(String),
}

impl SafetyCaps {
    pub fn check(&self, s: &Setting, ctx: &ClampContext) -> Verdict {
        if ctx.class == Some(QosClass::Critical) && is_throttle(s) {
            return Verdict::Rejected(format!("{s} would throttle a critical workload"));
        }
        let clamped = match s.clone() {
            Setting::CpuWeight { value } => {
                Setting::CpuWeight { value: value.clamp(self.cpu_weight_min, self.cpu_weight_max) }
            }
            Setting::IoWeight { value } => {
                Setting::IoWeight { value: value.clamp(self.io_weight_min, self.io_weight_max) }
            }
            Setting::UclampMin { percent } => {
                Setting::UclampMin { percent: clamp_pct(percent, 0.0, self.uclamp_min_max) }
            }
            Setting::UclampMax { percent } => {
                Setting::UclampMax { percent: clamp_pct(percent, self.uclamp_max_floor, 100.0) }
            }
            Setting::IoMax { device, limit } => {
                let f = |v: Option<u64>, floor: u64| v.map(|v| v.max(floor));
                Setting::IoMax {
                    device,
                    limit: IoLimit {
                        rbps: f(limit.rbps, self.io_bps_floor),
                        wbps: f(limit.wbps, self.io_bps_floor),
                        riops: f(limit.riops, self.io_iops_floor),
                        wiops: f(limit.wiops, self.io_iops_floor),
                    },
                }
            }
            Setting::MemoryHigh { bytes: Some(b) } => {
                let rel = ctx.mem_current.map_or(0, |c| (c as f64 * self.memory_high_ratio_floor) as u64);
                Setting::MemoryHigh { bytes: Some(b.max(self.memory_high_floor).max(rel)) }
            }
            Setting::MemoryLow { bytes } => match ctx.mem_total {
                Some(t) => Setting::MemoryLow { bytes: bytes.min((t as f64 * self.memory_low_max_ratio) as u64) },
                None => Setting::MemoryLow { bytes },
            },
            Setting::Nice { value } => Setting::Nice { value: value.clamp(self.nice_min, 19) },
            Setting::IoPrio { class, level } => Setting::IoPrio { class, level: level.min(7) },
            other @ (Setting::CpuIdle { .. } | Setting::MemoryHigh { bytes: None }) => other,
        };
        if &clamped == s { Verdict::Ok(clamped) } else { Verdict::Clamped { setting: clamped, from: s.clone() } }
    }
}

fn clamp_pct(p: f32, lo: f32, hi: f32) -> f32 {
    if p.is_nan() { lo } else { p.clamp(lo, hi) }
}

/// Settings that reduce a workload's resources below the defaults.
pub fn is_throttle(s: &Setting) -> bool {
    match s {
        Setting::CpuWeight { value } | Setting::IoWeight { value } => *value < 100,
        Setting::CpuIdle { value } => *value,
        Setting::UclampMax { percent } => *percent < 100.0,
        Setting::IoMax { limit, .. } => !limit.is_unlimited(),
        Setting::MemoryHigh { bytes } => bytes.is_some(),
        Setting::Nice { value } => *value > 0,
        Setting::IoPrio { class, .. } => *class == lindesk_plugin_api::IoPrioClass::Idle,
        Setting::UclampMin { .. } | Setting::MemoryLow { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> SafetyCaps {
        SafetyCaps::default()
    }

    #[test]
    fn boosts_are_bounded() {
        let v = caps().check(&Setting::UclampMin { percent: 100.0 }, &ClampContext::default());
        assert_eq!(
            v,
            Verdict::Clamped {
                setting: Setting::UclampMin { percent: 50.0 },
                from: Setting::UclampMin { percent: 100.0 }
            }
        );
        let v = caps().check(&Setting::CpuWeight { value: 10000 }, &ClampContext::default());
        assert!(matches!(v, Verdict::Clamped { setting: Setting::CpuWeight { value: 1000 }, .. }));
    }

    #[test]
    fn throttles_have_floors() {
        let s = Setting::IoMax {
            device: "8:0".into(),
            limit: IoLimit { wbps: Some(1), riops: Some(1), ..Default::default() },
        };
        match caps().check(&s, &ClampContext::default()) {
            Verdict::Clamped { setting: Setting::IoMax { limit, .. }, .. } => {
                assert_eq!(limit.wbps, Some(8 * 1024 * 1024));
                assert_eq!(limit.riops, Some(100));
                assert_eq!(limit.rbps, None);
            }
            v => panic!("{v:?}"),
        }
        let ctx = ClampContext { mem_current: Some(4 << 30), ..Default::default() };
        match caps().check(&Setting::MemoryHigh { bytes: Some(1 << 20) }, &ctx) {
            Verdict::Clamped { setting: Setting::MemoryHigh { bytes: Some(b) }, .. } => assert_eq!(b, 2 << 30),
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn critical_is_untouchable_but_boostable() {
        let ctx = ClampContext { class: Some(QosClass::Critical), ..Default::default() };
        assert!(matches!(caps().check(&Setting::CpuIdle { value: true }, &ctx), Verdict::Rejected(_)));
        assert!(matches!(caps().check(&Setting::CpuWeight { value: 50 }, &ctx), Verdict::Rejected(_)));
        assert!(matches!(caps().check(&Setting::CpuWeight { value: 200 }, &ctx), Verdict::Ok(_)));
        assert!(matches!(caps().check(&Setting::MemoryLow { bytes: 1 << 30 }, &ctx), Verdict::Ok(_)));
    }

    #[test]
    fn nan_and_negative_nice() {
        assert!(matches!(
            caps().check(&Setting::UclampMax { percent: f32::NAN }, &ClampContext::default()),
            Verdict::Clamped { setting: Setting::UclampMax { percent }, .. } if percent == 20.0
        ));
        assert!(matches!(
            caps().check(&Setting::Nice { value: -20 }, &ClampContext::default()),
            Verdict::Clamped { setting: Setting::Nice { value: -5 }, .. }
        ));
    }
}
