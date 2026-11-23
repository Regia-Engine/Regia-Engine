// SPDX-License-Identifier: MPL-2.0
//! Deterministic value noise used for tunnel detail, scatter-agnostic variation and tests.
//!
//! Lattice value noise: pseudo-random values at integer lattice points, interpolated with the
//! quintic fade curve (Ken Perlin, "Improving Noise", 2002). The lattice hash is the classic
//! integer avalanche hash attributed to Hugo Elias. Not used where cross-machine bit-exact
//! determinism is contractual (scatter placement draws its randomness from
//! [`regia_core::math::Rng`] instead); noise only feeds visual detail and test fields.

/// Quintic fade curve `6t⁵ − 15t⁴ + 10t³` (C² continuous).
fn fade(t: f32) -> f32 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

/// 32-bit integer avalanche hash (Hugo Elias' public-domain hash), mapped to `0..1`.
fn hash_u32(mut n: u32) -> f32 {
    n = n.wrapping_mul(0x9e37_79b9);
    n ^= n >> 16;
    n = n.wrapping_mul(0x85eb_ca6b);
    n ^= n >> 13;
    n = n.wrapping_mul(0xc2b2_ae35);
    n ^= n >> 16;
    (n >> 8) as f32 * (1.0 / 16_777_216.0)
}

/// Lattice cell and fade weight of one coordinate.
///
/// The weight is `x − floor(x)`, so it stays in `0..1` for every finite `x` (a value past
/// `i32` range saturates the cell index but not the weight). NaN and infinity read as 0.
fn cell(x: f32) -> (i32, f32) {
    let x = if x.is_finite() { x } else { 0.0 };
    let floor = x.floor();
    (floor as i32, fade((x - floor).clamp(0.0, 1.0)))
}

/// Hash of two integer coordinates plus a seed, in `0..1`.
pub fn hash2(x: i32, y: i32, seed: u32) -> f32 {
    let packed = (x as u32)
        .wrapping_mul(0x1f1f_1f1f)
        .wrapping_add((y as u32).wrapping_mul(0x2c1c_3c2c))
        .wrapping_add(seed.wrapping_mul(0x3bac_c7d2));
    hash_u32(packed)
}

/// Hash of three integer coordinates plus a seed, in `0..1`.
pub fn hash3(x: i32, y: i32, z: i32, seed: u32) -> f32 {
    let packed = (x as u32)
        .wrapping_mul(0x1f1f_1f1f)
        .wrapping_add((y as u32).wrapping_mul(0x2c1c_3c2c))
        .wrapping_add((z as u32).wrapping_mul(0x39d9_c39d))
        .wrapping_add(seed.wrapping_mul(0x3bac_c7d2));
    hash_u32(packed)
}

/// 2D lattice value noise in `0..1`.
pub fn value_noise2(x: f32, y: f32, seed: u32) -> f32 {
    let (xi, fx) = cell(x);
    let (yi, fy) = cell(y);
    let v00 = hash2(xi, yi, seed);
    let v10 = hash2(xi.wrapping_add(1), yi, seed);
    let v01 = hash2(xi, yi.wrapping_add(1), seed);
    let v11 = hash2(xi.wrapping_add(1), yi.wrapping_add(1), seed);
    let a = v00 + (v10 - v00) * fx;
    let b = v01 + (v11 - v01) * fx;
    a + (b - a) * fy
}

/// 3D lattice value noise in `0..1`.
pub fn value_noise3(x: f32, y: f32, z: f32, seed: u32) -> f32 {
    let (xi, fx) = cell(x);
    let (yi, fy) = cell(y);
    let (zi, fz) = cell(z);
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let c000 = hash3(xi, yi, zi, seed);
    let c100 = hash3(xi.wrapping_add(1), yi, zi, seed);
    let c010 = hash3(xi, yi.wrapping_add(1), zi, seed);
    let c110 = hash3(xi.wrapping_add(1), yi.wrapping_add(1), zi, seed);
    let c001 = hash3(xi, yi, zi.wrapping_add(1), seed);
    let c101 = hash3(xi.wrapping_add(1), yi, zi.wrapping_add(1), seed);
    let c011 = hash3(xi, yi.wrapping_add(1), zi.wrapping_add(1), seed);
    let c111 = hash3(xi.wrapping_add(1), yi.wrapping_add(1), zi.wrapping_add(1), seed);
    let x00 = lerp(c000, c100, fx);
    let x10 = lerp(c010, c110, fx);
    let x01 = lerp(c001, c101, fx);
    let x11 = lerp(c011, c111, fx);
    let y0 = lerp(x00, x10, fy);
    let y1 = lerp(x01, x11, fy);
    lerp(y0, y1, fz)
}

/// Two octaves of [`value_noise2`], still in `0..1` (normalised weights).
pub fn fbm2(x: f32, y: f32, seed: u32) -> f32 {
    (value_noise2(x, y, seed) * 0.6667) + (value_noise2(x * 2.03, y * 2.03, seed ^ 0x5bd1) * 0.3333)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_is_deterministic_and_bounded() {
        for i in 0..200 {
            let x = i as f32 * 0.37;
            let y = i as f32 * -0.11;
            assert_eq!(value_noise2(x, y, 7), value_noise2(x, y, 7));
            let n = value_noise2(x, y, 7);
            assert!((0.0..=1.0).contains(&n), "noise out of range: {n}");
            let n3 = value_noise3(x, y, x + y, 7);
            assert!((0.0..=1.0).contains(&n3), "noise3 out of range: {n3}");
        }
    }

    #[test]
    fn noise_hits_lattice_values_exactly() {
        // At integer coordinates the fade weights are zero: the value is the lattice hash.
        assert!((value_noise2(4.0, -2.0, 9) - hash2(4, -2, 9)).abs() < 1e-6);
        assert!((value_noise3(3.0, 1.0, 5.0, 9) - hash3(3, 1, 5, 9)).abs() < 1e-6);
    }

    #[test]
    fn noise_varies_across_space() {
        let a = value_noise2(0.5, 0.5, 3);
        let b = value_noise2(1.5, 0.5, 3);
        assert!((a - b).abs() > 1e-3, "adjacent cells should differ");
    }
}
