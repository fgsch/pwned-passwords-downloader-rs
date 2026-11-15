// Copyright (c) 2026 Federico G. Schwindt <fgsch@lodoss.net>
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const LATENCY_BUCKETS_MS: [u64; 9] = [1, 5, 10, 25, 50, 100, 250, 500, u64::MAX];

#[derive(Debug)]
pub struct LatencyStats {
    count: AtomicU64,
    total_nanos: AtomicU64,
    max_nanos: AtomicU64,
    min_nanos: AtomicU64,
    buckets: [AtomicU64; LATENCY_BUCKETS_MS.len()],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencySnapshot {
    pub count: u64,
    pub average_ms: f64,
    pub p50_upper_bound_ms: u64,
    pub p95_upper_bound_ms: u64,
    pub min_ms: f64,
    pub max_ms: f64,
}

impl LatencyStats {
    pub fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            total_nanos: AtomicU64::new(0),
            max_nanos: AtomicU64::new(0),
            min_nanos: AtomicU64::new(u64::MAX),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    pub fn record(&self, duration: Duration) {
        let nanos = duration.as_nanos().min(u64::MAX as u128) as u64;
        let bucket = LATENCY_BUCKETS_MS
            .iter()
            .position(|upper_bound| duration <= Duration::from_millis(*upper_bound))
            .unwrap_or(LATENCY_BUCKETS_MS.len() - 1);

        self.count.fetch_add(1, Ordering::Relaxed);
        self.total_nanos.fetch_add(nanos, Ordering::Relaxed);
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        self.max_nanos.fetch_max(nanos, Ordering::Relaxed);
        self.min_nanos.fetch_min(nanos, Ordering::Relaxed);
    }

    /// Returns a best-effort snapshot; concurrent recording may make fields
    /// reflect slightly different points in time. Snapshot after work stops
    /// for consistent final metrics.
    pub fn snapshot(&self) -> Option<LatencySnapshot> {
        let count = self.count.load(Ordering::Relaxed);
        if count == 0 {
            return None;
        }

        Some(LatencySnapshot {
            count,
            average_ms: self.total_nanos.load(Ordering::Relaxed) as f64
                / count as f64
                / 1_000_000.0,
            p50_upper_bound_ms: self.percentile_upper_bound(count, 50),
            p95_upper_bound_ms: self.percentile_upper_bound(count, 95),
            min_ms: self.min_nanos.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            max_ms: self.max_nanos.load(Ordering::Relaxed) as f64 / 1_000_000.0,
        })
    }

    fn percentile_upper_bound(&self, count: u64, percentile: u64) -> u64 {
        let target = (u128::from(count) * u128::from(percentile)).div_ceil(100);
        let mut seen = 0_u128;
        for (index, upper_bound) in LATENCY_BUCKETS_MS.iter().enumerate() {
            seen += u128::from(self.buckets[index].load(Ordering::Relaxed));
            if seen >= target {
                return *upper_bound;
            }
        }
        u64::MAX
    }
}

impl Default for LatencyStats {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reports_minimum_average_percentiles_and_maximum() {
        let stats = LatencyStats::new();
        stats.record(Duration::from_millis(2));
        stats.record(Duration::from_millis(20));
        stats.record(Duration::from_millis(600));

        let snapshot = stats.snapshot().unwrap();
        assert_eq!(snapshot.count, 3);
        assert!((snapshot.average_ms - 207.333).abs() < 0.001);
        assert_eq!(snapshot.min_ms, 2.0);
        assert_eq!(snapshot.p50_upper_bound_ms, 25);
        assert_eq!(snapshot.p95_upper_bound_ms, u64::MAX);
        assert_eq!(snapshot.max_ms, 600.0);
    }

    #[test]
    fn snapshot_is_empty_without_samples() {
        assert_eq!(LatencyStats::new().snapshot(), None);
    }

    #[test]
    fn percentile_bounds_include_exact_limits_but_not_fractional_excess() {
        for bound_ms in [1, 5, 10, 25, 50, 100, 250, 500] {
            let exact = LatencyStats::new();
            let duration = Duration::from_millis(bound_ms);
            exact.record(duration);
            let snapshot = exact.snapshot().unwrap();
            assert_eq!(snapshot.p50_upper_bound_ms, bound_ms);
            assert_eq!(snapshot.p95_upper_bound_ms, bound_ms);

            let fractional = LatencyStats::new();
            fractional.record(duration + Duration::from_nanos(1));
            let snapshot = fractional.snapshot().unwrap();
            assert!(snapshot.p50_upper_bound_ms > bound_ms);
            assert!(snapshot.p95_upper_bound_ms > bound_ms);
        }
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let stats = LatencyStats::new();
        for _ in 0..19 {
            stats.record(Duration::from_millis(1));
        }
        stats.record(Duration::from_millis(501));
        let snapshot = stats.snapshot().unwrap();
        assert_eq!(snapshot.p50_upper_bound_ms, 1);
        assert_eq!(snapshot.p95_upper_bound_ms, 1);

        stats.record(Duration::from_millis(501));
        assert_eq!(stats.snapshot().unwrap().p95_upper_bound_ms, u64::MAX);
    }
}
