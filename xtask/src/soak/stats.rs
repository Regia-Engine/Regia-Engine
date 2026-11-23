// SPDX-License-Identifier: MPL-2.0
//! Frame-time statistics for soak reports.
//!
//! Percentiles use the nearest-rank definition (Hyndman & Fan, "Sample Quantiles in Statistical
//! Packages", 1996 — definition 2), which is order-statistics based and needs no interpolation:
//! for `p` and `n` samples, the rank is `ceil(p/100 · n)`, 1-based. See `docs/provenance.md`.

/// Summary statistics of a list of samples (all in the unit the caller uses).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Percentiles {
    /// Median (p50).
    pub p50: f64,
    /// 95th percentile.
    pub p95: f64,
    /// 99th percentile.
    pub p99: f64,
    /// Largest sample.
    pub max: f64,
    /// Arithmetic mean.
    pub mean: f64,
}

/// Nearest-rank percentiles of `samples`, or `None` when it is empty.
///
/// Non-finite samples sort unpredictably, so callers must never pass them; frame times and
/// memory readings are finite by construction.
pub fn percentiles(samples: &[f64]) -> Option<Percentiles> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    let nearest_rank = |p: f64| -> f64 {
        let rank = ((p / 100.0) * n as f64).ceil();
        let rank = rank.clamp(1.0, n as f64) as usize;
        sorted[rank - 1]
    };
    let mean = sorted.iter().sum::<f64>() / n as f64;
    Some(Percentiles {
        p50: nearest_rank(50.0),
        p95: nearest_rank(95.0),
        p99: nearest_rank(99.0),
        max: sorted[n - 1],
        mean,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn empty_has_no_percentiles() {
        assert!(percentiles(&[]).is_none());
    }

    #[test]
    fn single_sample() {
        let p = percentiles(&[7.5]).expect("one sample");
        assert!(close(p.p50, 7.5));
        assert!(close(p.p99, 7.5));
        assert!(close(p.max, 7.5));
        assert!(close(p.mean, 7.5));
    }

    #[test]
    fn hundred_samples_one_to_hundred() {
        let samples: Vec<f64> = (1..=100).map(f64::from).collect();
        let p = percentiles(&samples).expect("percentiles");
        assert!(close(p.p50, 50.0), "p50 was {}", p.p50);
        assert!(close(p.p95, 95.0), "p95 was {}", p.p95);
        assert!(close(p.p99, 99.0), "p99 was {}", p.p99);
        assert!(close(p.max, 100.0));
        assert!(close(p.mean, 50.5));
    }

    #[test]
    fn median_of_odd_count_is_the_middle_element() {
        let p = percentiles(&[3.0, 1.0, 2.0]).expect("percentiles");
        assert!(close(p.p50, 2.0));
        // Nearest rank of 95% of 3 is ceil(2.85) = 3 → the largest sample.
        assert!(close(p.p95, 3.0));
        assert!(close(p.max, 3.0));
    }

    #[test]
    fn small_sample_clamps_ranks_into_range() {
        let p = percentiles(&[10.0, 20.0]).expect("percentiles");
        assert!(close(p.p50, 10.0));
        assert!(close(p.p95, 20.0));
        assert!(close(p.p99, 20.0));
    }

    #[test]
    fn one_outlier_moves_p99_but_not_p50() {
        let mut samples: Vec<f64> = vec![10.0; 100];
        samples[37] = 5000.0;
        let p = percentiles(&samples).expect("percentiles");
        assert!(close(p.p50, 10.0));
        assert!(close(p.p99, 10.0), "one sample in a hundred must stay under p99");
        assert!(close(p.max, 5000.0));
    }
}
