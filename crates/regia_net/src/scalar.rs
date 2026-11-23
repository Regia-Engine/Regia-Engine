// SPDX-License-Identifier: MPL-2.0
//! Scalar math in one fixed evaluation order (spec 71 § 12).
//!
//! The replication codec, the lag-compensation poses and blends, and the interpolation recovery
//! decay must round bit-identically on x86_64, aarch64 and every other target. glam's SIMD
//! horizontal sums do not guarantee that: its SSE2 dot4 keeps the `(x + z) + (y + w)` grouping
//! while NEON's `vaddvq_f32` reduces pairwise, so the same quaternion normalizes to different
//! bits on the two architectures; and `libm` transcendentals (`powf`) are not required to agree
//! across glibc, the Windows CRT and bionic. Everything here is plain IEEE `f32`/`f64`
//! arithmetic written in a single order: no horizontal sums, no fused multiply-add, no libm.

#[cfg(feature = "ecs")]
use glam::Quat;

/// Scalar normalization of a 4-vector in left-to-right order (no SIMD horizontal sums, no fused
/// multiply-add), so every platform rounds identically (spec 71 § 12). `None` for zero or
/// non-finite input.
pub(crate) fn normalize4(v: [f32; 4]) -> Option<[f32; 4]> {
    if !v.iter().all(|c| c.is_finite()) {
        return None;
    }
    let len2 = v[0] * v[0] + v[1] * v[1] + v[2] * v[2] + v[3] * v[3];
    if len2 <= 0.0 || !len2.is_finite() {
        return None;
    }
    let len = len2.sqrt();
    Some([v[0] / len, v[1] / len, v[2] / len, v[3] / len])
}

#[cfg(feature = "ecs")]
/// Scalar quaternion dot in left-to-right order. Only used for sign decisions (the nlerp
/// short-path flip in `lagcomp::geom::blend` and `interp::nlerp`), where glam's SIMD dot4 could
/// otherwise flip the sign on one architecture only.
pub(crate) fn quat_dot(a: Quat, b: Quat) -> f32 {
    a.x * b.x + a.y * b.y + a.z * b.z + a.w * b.w
}

#[cfg(feature = "ecs")]
/// `0.5f64.powf(r)` without libm, so every platform returns the same bits: the recovery-offset
/// decay of `SnapshotBuffer::sample` runs on every client frame, and a libc-dependent factor
/// would make proxies drift apart across platforms even with identical snapshots. Splits `r`
/// into an integer part `n` and a fraction `f`, approximates `2^-f` by a degree-6 Taylor
/// expansion evaluated at `f / 16` and squared four times (relative error below 1e-12, far
/// under `f32` epsilon when the caller narrows), and scales by the exact dyadic `2^-n`.
///
/// `r` is expected to be a non-negative milliseconds ratio (`frame_ms / half_life`); `r <= 0`
/// is 1.0 and a large `r` underflows to 0.0. A NaN ratio (an infinite frame over an infinite
/// half-life) does not decay either (corr-low, ADR-5789).
pub(crate) fn half_decay(r: f64) -> f64 {
    if r <= 0.0 || r.is_nan() {
        return 1.0;
    }
    if r >= 1075.0 {
        // 2^-1075 is below the smallest subnormal: the true value rounds to zero.
        return 0.0;
    }
    let n = r.floor();
    let f = r - n;
    let g = f * (1.0 / 16.0);
    let x = -std::f64::consts::LN_2 * g;
    let mut p = 1.0
        + x * (1.0 + x * (0.5 + x * (1.0 / 6.0 + x * (1.0 / 24.0 + x * (1.0 / 120.0 + x * (1.0 / 720.0))))));
    p *= p;
    p *= p;
    p *= p;
    p *= p;
    let a = n.min(512.0);
    let b = n - a;
    p * pow2_neg(a) * pow2_neg(b)
}

#[cfg(feature = "ecs")]
/// The exact dyadic `2^-e` for `0 <= e <= 1022` (a power of two is exactly representable). `e =
/// 1023` would need the subnormal `2^-1023`, which this bit formula cannot form (`(1023 - 1023)
/// << 52` is the bit pattern of 0.0), so the range stops one below.
fn pow2_neg(e: f64) -> f64 {
    debug_assert!((0.0..=1022.0).contains(&e));
    f64::from_bits(((1023.0 - e) as u64) << 52)
}
