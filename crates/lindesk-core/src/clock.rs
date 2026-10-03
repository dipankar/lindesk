//! Time source. Real daemons use [`SystemClock`]; tests and `lindeskctl simulate` drive a
//! [`SimClock`] so policies behave identically on replayed data.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use lindesk_plugin_api::TimestampMs;

pub trait Clock: Send + Sync {
    fn now(&self) -> TimestampMs;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> TimestampMs {
        now_ms()
    }
}

pub fn now_ms() -> TimestampMs {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[derive(Clone, Debug, Default)]
pub struct SimClock(Arc<AtomicI64>);

impl SimClock {
    pub fn new(start: TimestampMs) -> Self {
        SimClock(Arc::new(AtomicI64::new(start)))
    }

    pub fn set(&self, t: TimestampMs) {
        self.0.store(t, Ordering::SeqCst);
    }

    pub fn advance(&self, ms: i64) -> TimestampMs {
        self.0.fetch_add(ms, Ordering::SeqCst) + ms
    }
}

impl Clock for SimClock {
    fn now(&self) -> TimestampMs {
        self.0.load(Ordering::SeqCst)
    }
}
