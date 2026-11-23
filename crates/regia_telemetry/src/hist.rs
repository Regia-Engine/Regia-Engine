// SPDX-License-Identifier: MPL-2.0
//! Fixed-bin frame-time histogram: percentiles without keeping samples (no allocation per frame).

/// Fine bins: 0–64 ms at 1/8 ms.
const FINE_BINS: usize = 512;
const FINE_STEP_MS: f32 = 0.125;
const FINE_END_MS: f32 = FINE_BINS as f32 * FINE_STEP_MS;
/// Coarse bins: 64–1024 ms at 4 ms.
const COARSE_BINS: usize = 240;
const COARSE_STEP_MS: f32 = 4.0;
const COARSE_END_MS: f32 = FINE_END_MS + COARSE_BINS as f32 * COARSE_STEP_MS;
/// Stall bins: 1024–5120 ms at 256 ms (so a percentile inside a stall is not the longest frame).
const STALL_BINS: usize = 16;
const STALL_STEP_MS: f32 = 256.0;
const STALL_END_MS: f32 = COARSE_END_MS + STALL_BINS as f32 * STALL_STEP_MS;
/// Plus one overflow bin (≥ 5120 ms).
const BINS: usize = FINE_BINS + COARSE_BINS + STALL_BINS + 1;

/// Upper edges (ms) of the frame-rate buckets in the summary's `fps_histogram`, fastest first:
/// ≥ 240, ≥ 144, ≥ 120, ≥ 90, ≥ 60, ≥ 45, ≥ 30, ≥ 20, ≥ 10 fps, then slower.
pub const FPS_EDGES_MS: [f32; 9] = [
    1000.0 / 240.0,
    1000.0 / 144.0,
    1000.0 / 120.0,
    1000.0 / 90.0,
    1000.0 / 60.0,
    1000.0 / 45.0,
    1000.0 / 30.0,
    1000.0 / 20.0,
    1000.0 / 10.0,
];

/// Names of the `fps_histogram` buckets, in [`FPS_EDGES_MS`] order plus the slowest.
pub const FPS_BUCKETS: [&str; 10] = [
    "ge_240", "ge_144", "ge_120", "ge_90", "ge_60", "ge_45", "ge_30", "ge_20", "ge_10", "lt_10",
];

/// Frame-time histogram with 1/8 ms resolution up to 64 ms and 4 ms up to 1 s.
#[derive(Clone)]
pub struct Histogram {
    bins: Box<[u32; BINS]>,
    fps: [u32; 10],
    count: u32,
    sum_ms: f64,
    max_ms: f32,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    /// An empty histogram (allocates its bins once).
    #[must_use]
    pub fn new() -> Self {
        Self {
            bins: Box::new([0; BINS]),
            fps: [0; 10],
            count: 0,
            sum_ms: 0.0,
            max_ms: 0.0,
        }
    }

    /// Adds one frame of `ms` milliseconds (negative, NaN and infinite values are ignored).
    pub fn add(&mut self, ms: f32) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        let bin = if ms < FINE_END_MS {
            (ms / FINE_STEP_MS) as usize
        } else if ms < COARSE_END_MS {
            FINE_BINS + ((ms - FINE_END_MS) / COARSE_STEP_MS) as usize
        } else if ms < STALL_END_MS {
            FINE_BINS + COARSE_BINS + ((ms - COARSE_END_MS) / STALL_STEP_MS) as usize
        } else {
            BINS - 1
        };
        let bin = bin.min(BINS - 1);
        self.bins[bin] = self.bins[bin].saturating_add(1);
        let bucket = FPS_EDGES_MS.iter().position(|&edge| ms <= edge).unwrap_or(9);
        self.fps[bucket] = self.fps[bucket].saturating_add(1);
        self.count = self.count.saturating_add(1);
        self.sum_ms += f64::from(ms);
        self.max_ms = self.max_ms.max(ms);
    }

    /// Frames added.
    #[must_use]
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Mean, ms (0 when empty).
    #[must_use]
    pub fn mean(&self) -> f32 {
        if self.count == 0 {
            0.0
        } else {
            (self.sum_ms / f64::from(self.count)) as f32
        }
    }

    /// Longest frame, ms.
    #[must_use]
    pub fn max(&self) -> f32 {
        self.max_ms
    }

    /// Frames per `fps_histogram` bucket ([`FPS_BUCKETS`]).
    #[must_use]
    pub fn fps_buckets(&self) -> [u32; 10] {
        self.fps
    }

    /// The `p`-th percentile (`0.0..=1.0`), ms: the upper edge of the bin holding it, capped
    /// at the longest frame (0 when empty).
    #[must_use]
    pub fn percentile(&self, p: f32) -> f32 {
        if self.count == 0 {
            return 0.0;
        }
        let rank = ((p.clamp(0.0, 1.0) * self.count as f32).ceil() as u32).max(1);
        let mut seen = 0u32;
        for (i, &n) in self.bins.iter().enumerate() {
            seen += n;
            if seen >= rank {
                let upper = if i < FINE_BINS {
                    (i + 1) as f32 * FINE_STEP_MS
                } else if i < FINE_BINS + COARSE_BINS {
                    FINE_END_MS + (i - FINE_BINS + 1) as f32 * COARSE_STEP_MS
                } else if i < FINE_BINS + COARSE_BINS + STALL_BINS {
                    COARSE_END_MS + (i - FINE_BINS - COARSE_BINS + 1) as f32 * STALL_STEP_MS
                } else {
                    self.max_ms
                };
                return upper.min(self.max_ms);
            }
        }
        self.max_ms
    }

    /// Empties the histogram (keeps its storage).
    pub fn clear(&mut self) {
        self.bins.fill(0);
        self.fps = [0; 10];
        self.count = 0;
        self.sum_ms = 0.0;
        self.max_ms = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_saturate_instead_of_overflowing() {
        let mut h = Histogram::new();
        h.add(16.6);
        h.count = u32::MAX;
        h.fps.fill(u32::MAX);
        h.bins.fill(u32::MAX);
        h.add(16.6);
        assert_eq!(h.count(), u32::MAX);
    }

    #[test]
    fn percentiles_within_a_bin() {
        let mut h = Histogram::new();
        for i in 0..1000 {
            h.add(if i < 990 { 16.6 } else { 40.0 });
        }
        h.add(300.0);
        assert!((h.percentile(0.5) - 16.625).abs() < 0.01, "{}", h.percentile(0.5));
        assert!((h.percentile(0.995) - 40.0).abs() <= 0.125);
        assert_eq!(h.max(), 300.0);
        assert_eq!(h.percentile(1.0), 300.0);
        assert_eq!(h.fps_buckets()[4], 990);
        assert_eq!(h.count(), 1001);
        h.add(5000.0);
        assert_eq!(h.percentile(1.0), 5000.0);
        h.clear();
        assert_eq!(h.percentile(0.5), 0.0);
    }

    #[test]
    fn non_finite_samples_are_ignored() {
        let mut h = Histogram::new();
        h.add(f32::INFINITY);
        h.add(f32::NEG_INFINITY);
        h.add(f32::NAN);
        assert_eq!(h.count(), 0);
        assert_eq!(h.max(), 0.0);
        h.add(16.0);
        assert_eq!(h.count(), 1);
    }

    #[test]
    fn percentile_inside_the_stall_range_is_not_the_max() {
        let mut h = Histogram::new();
        for _ in 0..989 {
            h.add(16.0);
        }
        for _ in 0..10 {
            h.add(1100.0);
        }
        h.add(4800.0);
        let p99 = h.percentile(0.99);
        assert_eq!(p99, 1280.0, "bin 0 of the stall range reports its upper edge");
        assert_eq!(h.percentile(1.0), 4800.0);
    }

    #[test]
    fn infinite_nan_and_negative_samples_leave_the_max_alone() {
        let mut h = Histogram::new();
        h.add(f32::INFINITY);
        h.add(f32::NAN);
        h.add(-1.0);
        h.add(10.0);
        assert_eq!(h.count(), 1);
        assert_eq!(h.max(), 10.0);
    }
}
