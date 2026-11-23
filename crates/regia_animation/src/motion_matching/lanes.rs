// SPDX-License-Identifier: MPL-2.0
//! The canonical motion-matching distance (`docs/specs/52-motion-matching.md` § 2.7, item 1).
//!
//! Every search method (the brute-force scan, the k-d tree, `Auto`, top-k and the debug report)
//! computes a squared distance with this one function, so they return bit-identical costs. The
//! sum runs over eight independent `f32` lane accumulators: lane `j` accumulates dimensions
//! `j, j + 8, j + 16, …` in increasing order with a separate multiply and add (never a fused
//! multiply-add), as if the vector were padded with zeros to a multiple of eight. The lanes are
//! then reduced as `((l0 + l4) + (l2 + l6)) + ((l1 + l5) + (l3 + l7))`.
//!
//! The layout is what lets the compiler vectorize the loop (one SSE2 register pair or two NEON
//! registers per eight lanes) without changing a single bit of the result on any platform:
//! padding lanes add `(0 − 0)² = +0`, which leaves a non-negative accumulator unchanged, so the
//! padding is implicit and the rows are stored unpadded. [`reference_distance`] is the plain
//! scalar statement of the same order, kept for the tests.

// Source: the summation order is this engine's own contract (spec 52 § 2.7); IEEE 754 binary32
// addition and multiplication are correctly rounded, so a fixed operation order gives the same
// bits everywhere. Original code; nothing ported.

/// Lanes of the canonical distance.
pub const LANES: usize = 8;

/// The padded length of a `dims`-float vector: the next multiple of [`LANES`].
#[must_use]
pub fn padded_dims(dims: usize) -> usize {
    dims.div_ceil(LANES) * LANES
}

/// Reduce the eight lane accumulators in the canonical order.
#[inline]
fn reduce(l: &[f32; LANES]) -> f32 {
    ((l[0] + l[4]) + (l[2] + l[6])) + ((l[1] + l[5]) + (l[3] + l[7]))
}

/// The canonical squared distance between `a` and `b` (the shorter length counts; the search
/// only calls it with equal lengths).
#[inline]
#[must_use]
pub fn lane_distance(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let (a_chunks, a_tail) = a.as_chunks::<LANES>();
    let (b_chunks, b_tail) = b.as_chunks::<LANES>();
    let mut lanes = [0.0f32; LANES];
    for (x, y) in a_chunks.iter().zip(b_chunks) {
        for j in 0..LANES {
            let d = x[j] - y[j];
            let sq = d * d;
            lanes[j] += sq;
        }
    }
    for (j, (x, y)) in a_tail.iter().zip(b_tail).enumerate() {
        let d = x - y;
        let sq = d * d;
        lanes[j] += sq;
    }
    reduce(&lanes)
}

/// The canonical distance with an early out: returns `None` as soon as the lanes summed so far
/// already exceed `limit` (each lane only grows, and correctly rounded addition is monotone, so
/// the full distance would exceed it too). With `limit = +inf` it equals [`lane_distance`].
#[inline]
#[must_use]
pub fn lane_distance_bounded(a: &[f32], b: &[f32], limit: f32) -> Option<f32> {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let (a_chunks, a_tail) = a.as_chunks::<LANES>();
    let (b_chunks, b_tail) = b.as_chunks::<LANES>();
    let mut lanes = [0.0f32; LANES];
    for (index, (x, y)) in a_chunks.iter().zip(b_chunks).enumerate() {
        for j in 0..LANES {
            let d = x[j] - y[j];
            let sq = d * d;
            lanes[j] += sq;
        }
        // Check every second chunk: often enough to cut long rows, rare enough to stay cheap.
        if index % 2 == 1 && reduce(&lanes) > limit {
            return None;
        }
    }
    for (j, (x, y)) in a_tail.iter().zip(b_tail).enumerate() {
        let d = x - y;
        let sq = d * d;
        lanes[j] += sq;
    }
    let total = reduce(&lanes);
    (total <= limit || limit.is_nan()).then_some(total)
}

/// The scalar statement of the canonical order: lane by lane, dimension by dimension, no
/// chunking. The tests compare [`lane_distance`] against it bit for bit.
#[must_use]
pub fn reference_distance(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let padded = padded_dims(n);
    let mut lanes = [0.0f32; LANES];
    for (j, lane) in lanes.iter_mut().enumerate() {
        let mut d = j;
        while d < padded {
            let (x, y) = if d < n { (a[d], b[d]) } else { (0.0, 0.0) };
            let diff = x - y;
            let sq = diff * diff;
            *lane += sq;
            d += LANES;
        }
    }
    reduce(&lanes)
}

#[cfg(test)]
mod tests {
    use regia_core::math::Rng;

    use super::*;

    fn random(rng: &mut Rng, n: usize) -> Vec<f32> {
        (0..n).map(|_| (rng.f32() * 100.0 - 50.0) * rng.f32()).collect()
    }

    #[test]
    fn lane_distance_equals_the_scalar_reference_bit_for_bit() {
        let mut rng = Rng::new(52);
        for dims in (1..=97).chain([128, 191, 192]) {
            for _ in 0..40 {
                let a = random(&mut rng, dims);
                let b = random(&mut rng, dims);
                let fast = lane_distance(&a, &b);
                assert_eq!(
                    fast.to_bits(),
                    reference_distance(&a, &b).to_bits(),
                    "dims {dims}"
                );
                assert_eq!(
                    lane_distance_bounded(&a, &b, f32::INFINITY).map(f32::to_bits),
                    Some(fast.to_bits()),
                    "unbounded early-out equals the plain distance (dims {dims})"
                );
            }
        }
    }

    #[test]
    fn explicit_zero_padding_changes_nothing() {
        let mut rng = Rng::new(7);
        for dims in [1, 3, 9, 27, 33, 47] {
            let a = random(&mut rng, dims);
            let b = random(&mut rng, dims);
            let mut pa = a.clone();
            let mut pb = b.clone();
            pa.resize(padded_dims(dims), 0.0);
            pb.resize(padded_dims(dims), 0.0);
            assert_eq!(lane_distance(&a, &b).to_bits(), lane_distance(&pa, &pb).to_bits());
        }
        assert_eq!(padded_dims(0), 0);
        assert_eq!(padded_dims(1), 8);
        assert_eq!(padded_dims(27), 32);
        assert_eq!(padded_dims(48), 48);
    }

    #[test]
    fn the_lane_order_is_the_canonical_one() {
        // Values chosen so a different summation order gives a different f32.
        let a: Vec<f32> = (0..16)
            .map(|i| if i % 3 == 0 { 1.0e4 } else { 1.0e-3 * i as f32 })
            .collect();
        let b = vec![0.0f32; 16];
        let mut lanes = [0.0f32; LANES];
        for (d, v) in a.iter().enumerate() {
            lanes[d % LANES] += v * v;
        }
        let expected =
            ((lanes[0] + lanes[4]) + (lanes[2] + lanes[6])) + ((lanes[1] + lanes[5]) + (lanes[3] + lanes[7]));
        assert_eq!(lane_distance(&a, &b).to_bits(), expected.to_bits());
    }

    #[test]
    fn early_out_never_drops_a_row_within_the_limit() {
        let mut rng = Rng::new(11);
        for _ in 0..500 {
            let dims = 1 + (rng.next_u32() % 96) as usize;
            let a = random(&mut rng, dims);
            let b = random(&mut rng, dims);
            let full = lane_distance(&a, &b);
            assert_eq!(lane_distance_bounded(&a, &b, full), Some(full));
            assert_eq!(lane_distance_bounded(&a, &b, full * 0.5), None);
        }
    }
}
