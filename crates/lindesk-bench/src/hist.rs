//! Log-bucketed latency histogram, HDR-style.
//!
//! Values (nanoseconds) below 64 are counted exactly. Above that, every power-of-two range
//! is split into 32 linear sub-buckets, so a reported percentile is within ~3% of the true
//! value. The whole `u64` range fits in 1920 counters (15 KiB), recording is a couple of
//! instructions, and two histograms merge by adding counters.

use serde::{Deserialize, Serialize};

const SUB_BITS: u32 = 5;
const SUB: u64 = 1 << SUB_BITS;
/// Values below this are their own bucket.
const LINEAR: u64 = 2 * SUB;
/// Number of buckets needed to cover `u64`.
pub const BUCKETS: usize = (64 - SUB_BITS as usize + 1) * SUB as usize;

/// Bucket index of a value.
pub fn index(v: u64) -> usize {
    if v < LINEAR {
        return v as usize;
    }
    let magnitude = 63 - v.leading_zeros();
    let shift = magnitude - SUB_BITS;
    (u64::from(shift) * SUB + (v >> shift)) as usize
}

/// Inclusive value range `(lowest, highest)` of a bucket.
pub fn bounds(idx: usize) -> (u64, u64) {
    let idx = idx as u64;
    if idx < LINEAR {
        return (idx, idx);
    }
    let shift = idx / SUB - 1;
    let sub = idx % SUB + SUB;
    let lo = sub << shift;
    (lo, lo + ((1u64 << shift) - 1))
}

/// A latency histogram in nanoseconds.
#[derive(Clone, Debug)]
pub struct Histogram {
    counts: Vec<u64>,
    count: u64,
    min: u64,
    max: u64,
    sum: u128,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    pub fn new() -> Self {
        Histogram { counts: vec![0; BUCKETS], count: 0, min: u64::MAX, max: 0, sum: 0 }
    }

    pub fn record(&mut self, v: u64) {
        self.counts[index(v)] += 1;
        self.count += 1;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        self.sum += u128::from(v);
    }

    #[cfg(test)]
    pub fn merge(&mut self, other: &Histogram) {
        for (a, b) in self.counts.iter_mut().zip(&other.counts) {
            *a += *b;
        }
        self.count += other.count;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.sum += other.sum;
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn max(&self) -> u64 {
        if self.count == 0 { 0 } else { self.max }
    }

    pub fn min(&self) -> u64 {
        if self.count == 0 { 0 } else { self.min }
    }

    pub fn mean(&self) -> f64 {
        if self.count == 0 { 0.0 } else { self.sum as f64 / self.count as f64 }
    }

    /// The value at percentile `p` (0..=100): the highest value equivalent to the bucket
    /// holding the sample of that rank, clamped to the observed min and max.
    pub fn percentile(&self, p: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        if p >= 100.0 {
            return self.max;
        }
        let rank = ((p.max(0.0) / 100.0) * self.count as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (i, &c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return bounds(i).1.clamp(self.min, self.max);
            }
        }
        self.max
    }

    /// Percentile summary in microseconds.
    pub fn summary(&self) -> HistSummary {
        let us = |ns: u64| (ns as f64 / 1000.0 * 10.0).round() / 10.0;
        HistSummary {
            count: self.count(),
            min_us: us(self.min()),
            mean_us: (self.mean() / 100.0).round() / 10.0,
            p50_us: us(self.percentile(50.0)),
            p90_us: us(self.percentile(90.0)),
            p95_us: us(self.percentile(95.0)),
            p99_us: us(self.percentile(99.0)),
            p999_us: us(self.percentile(99.9)),
            max_us: us(self.max()),
        }
    }
}

/// Percentiles of a [`Histogram`], in microseconds (0.1 µs resolution).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct HistSummary {
    pub count: u64,
    pub min_us: f64,
    pub mean_us: f64,
    pub p50_us: f64,
    pub p90_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
    pub p999_us: f64,
    pub max_us: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_and_bounds_agree() {
        let mut v: u64 = 1;
        let mut probes = vec![0u64, 1, 63, 64, 65, 127, 128, 1000, u64::MAX, u64::MAX - 1];
        while v < u64::MAX / 3 {
            probes.extend([v, v + 1, v.saturating_mul(3) / 2]);
            v += v / 5 * 2 + 1;
        }
        for v in probes {
            let i = index(v);
            assert!(i < BUCKETS, "{v} -> {i}");
            let (lo, hi) = bounds(i);
            assert!(lo <= v && v <= hi, "{v} not in bucket {i} = [{lo}, {hi}]");
            // Relative bucket width is at most 1/32.
            assert!((hi - lo) as f64 <= lo as f64 / SUB as f64 + 1.0);
        }
        // Buckets are contiguous.
        for i in 1..BUCKETS {
            assert_eq!(bounds(i - 1).1 + 1, bounds(i).0, "gap at {i}");
        }
        assert_eq!(bounds(BUCKETS - 1).1, u64::MAX);
    }

    #[test]
    fn percentiles_of_uniform_distribution() {
        let mut h = Histogram::new();
        for v in 1..=100_000u64 {
            h.record(v * 1000); // 1 µs .. 100 ms
        }
        let close = |got: u64, want: f64| {
            let err = (got as f64 - want).abs() / want;
            assert!(err < 0.035, "got {got}, want {want} (err {err})");
        };
        close(h.percentile(50.0), 50_000_000.0);
        close(h.percentile(99.0), 99_000_000.0);
        close(h.percentile(99.9), 99_900_000.0);
        assert_eq!(h.percentile(100.0), 100_000_000);
        assert_eq!(h.max(), 100_000_000);
        assert_eq!(h.min(), 1000);
        assert_eq!(h.count(), 100_000);
        close(h.mean() as u64, 50_000_500.0);
        let s = h.summary();
        assert_eq!(s.max_us, 100_000.0);
        assert!((s.p50_us - 50_000.0).abs() / 50_000.0 < 0.035);
    }

    #[test]
    fn tail_is_visible_and_merge_adds() {
        let mut a = Histogram::new();
        for _ in 0..990 {
            a.record(50_000);
        }
        let mut b = Histogram::new();
        for _ in 0..10 {
            b.record(20_000_000);
        }
        a.merge(&b);
        assert_eq!(a.count(), 1000);
        assert!(a.percentile(50.0) <= 51_600);
        assert!(a.percentile(99.0) >= 49_000 && a.percentile(99.0) <= 51_600);
        assert_eq!(a.percentile(99.5), 20_000_000);
        assert_eq!(a.max(), 20_000_000);
    }

    #[test]
    fn empty_histogram_is_zero() {
        let h = Histogram::new();
        assert_eq!(h.percentile(99.0), 0);
        assert_eq!(h.summary(), HistSummary::default());
    }
}
