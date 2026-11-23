// SPDX-License-Identifier: MPL-2.0
//! The order a probe's rays are traced in (ADR-862).
//!
//! A probe traces one spherical Fibonacci set per sample (Keinert et al. 2015). In index order
//! the set walks down in latitude while every step turns the azimuth by the golden angle
//! (137.5°), so the 8, 32 or 64 invocations a GPU runs in lockstep shoot rays all around the
//! sphere and their traversals diverge. [`coherent_ray_order`] sorts the set along a Morton
//! curve over its octahedral map (Morton 1966; Cigolle et al. 2014), so consecutive invocations
//! trace neighbouring directions from the same origin: the coherence that makes camera rays
//! cheaper than diffuse rays on a GPU (Aila & Laine 2009). The kernels look a slot's Fibonacci
//! index up in this table and key the slot's random stream by that index, so a probe's rays
//! and their paths are the same as in index order; only the order of the sums changes.

use regia_asset::baked_gi::oct_encode;

/// Spherical Fibonacci direction `i` of `n`, Y up (`fibonacci_dir` in `pt.wgsl`).
#[must_use]
pub fn fibonacci_direction(i: u32, n: u32) -> [f32; 3] {
    let golden = std::f32::consts::PI * (3.0 - 5.0f32.sqrt());
    let y = 1.0 - (2.0 * i as f32 + 1.0) / n.max(1) as f32;
    let r = (1.0 - y * y).max(0.0).sqrt();
    let theta = golden * i as f32;
    [r * theta.cos(), y, r * theta.sin()]
}

/// Spreads the low 16 bits of `v` to the even bits of a `u32`.
fn spread_bits(v: u32) -> u32 {
    let mut x = v & 0xffff;
    x = (x | (x << 8)) & 0x00ff_00ff;
    x = (x | (x << 4)) & 0x0f0f_0f0f;
    x = (x | (x << 2)) & 0x3333_3333;
    (x | (x << 1)) & 0x5555_5555
}

/// Morton (Z-order) code of a point in `[0, 1]²`, 16 bits per axis.
#[must_use]
pub fn morton_2d(uv: [f32; 2]) -> u32 {
    let q = |f: f32| (f.clamp(0.0, 1.0) * 65535.0 + 0.5) as u32;
    spread_bits(q(uv[0])) | (spread_bits(q(uv[1])) << 1)
}

/// The coherent trace order of an `n`-ray set: `order[slot]` is the Fibonacci index slot
/// `slot` traces. A permutation of `0..n`, sorted by the Morton code of each direction's
/// octahedral coordinates (ties by index, so it is the same on every machine).
#[must_use]
pub fn coherent_ray_order(n: u32) -> Vec<u32> {
    let mut keyed: Vec<(u32, u32)> = (0..n)
        .map(|i| (morton_2d(oct_encode(fibonacci_direction(i, n))), i))
        .collect();
    keyed.sort_unstable();
    keyed.into_iter().map(|(_, i)| i).collect()
}

/// Mean angle (radians) between two rays of the same lockstep group, over the groups of
/// `group` consecutive slots of `order` (an `n`-ray set): how far apart the rays one GPU
/// wave traces are. Lower is more coherent.
#[must_use]
pub fn lockstep_spread(order: &[u32], n: u32, group: usize) -> f32 {
    let group = group.max(2);
    let mut total = 0.0f64;
    let mut groups = 0usize;
    for chunk in order.chunks(group) {
        let dirs: Vec<[f32; 3]> = chunk.iter().map(|&i| fibonacci_direction(i, n)).collect();
        let mut sum = 0.0f64;
        let mut pairs = 0usize;
        for (a, da) in dirs.iter().enumerate() {
            for db in &dirs[a + 1..] {
                let dot = da[0] * db[0] + da[1] * db[1] + da[2] * db[2];
                sum += f64::from(dot.clamp(-1.0, 1.0).acos());
                pairs += 1;
            }
        }
        if pairs > 0 {
            total += sum / pairs as f64;
            groups += 1;
        }
    }
    if groups == 0 {
        0.0
    } else {
        (total / groups as f64) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_order_is_a_permutation_and_stable() {
        for n in [1u32, 2, 7, 64, 100, 256, 4096] {
            let order = coherent_ray_order(n);
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..n).collect::<Vec<_>>(), "n = {n}");
            assert_eq!(order, coherent_ray_order(n), "deterministic, n = {n}");
        }
        assert!(coherent_ray_order(0).is_empty());
    }

    /// The default 256-ray set: a 32-wide wave's rays are 68° apart on average in index order
    /// and 44° in the coherent order; an 8-wide one (lavapipe) 74° and 29° (measured when the
    /// order was designed, ADR-862). The bounds leave room for f32 rounding of the keys.
    #[test]
    fn coherent_order_keeps_lockstep_rays_together() {
        let n = 256;
        let identity: Vec<u32> = (0..n).collect();
        let coherent = coherent_ray_order(n);
        for (group, bound) in [(8usize, 0.5f32), (32, 0.75), (64, 0.9)] {
            let before = lockstep_spread(&identity, n, group);
            let after = lockstep_spread(&coherent, n, group);
            assert!(
                after < bound * before,
                "group {group}: {:.1}° → {:.1}°",
                before.to_degrees(),
                after.to_degrees()
            );
        }
        // Larger sets get more coherent still.
        let n = 1024;
        let identity: Vec<u32> = (0..n).collect();
        let ratio = lockstep_spread(&coherent_ray_order(n), n, 32) / lockstep_spread(&identity, n, 32);
        assert!(ratio < 0.5, "{ratio}");
    }

    #[test]
    fn fibonacci_directions_are_unit_and_cover_both_poles() {
        let n = 64;
        let dirs: Vec<[f32; 3]> = (0..n).map(|i| fibonacci_direction(i, n)).collect();
        for d in &dirs {
            let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
            assert!((len - 1.0).abs() < 1e-5, "{d:?}");
        }
        assert!(dirs[0][1] > 0.95 && dirs[63][1] < -0.95);
    }

    #[test]
    fn morton_interleaves_axes() {
        assert_eq!(morton_2d([0.0, 0.0]), 0);
        assert_eq!(morton_2d([1.0, 1.0]), u32::MAX);
        assert_eq!(morton_2d([1.0, 0.0]), 0x5555_5555);
        assert_eq!(morton_2d([0.0, 1.0]), 0xaaaa_aaaa);
    }
}
