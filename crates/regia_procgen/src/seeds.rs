// SPDX-License-Identifier: MPL-2.0
//! Seed derivation (spec 62 § 6.2). Every random draw in a graph goes through these helpers so that
//! per-element randomness depends only on the element's seed, the node seed and a fixed draw slot.

use regia_core::id::Guid;
use regia_core::math::Rng;
use regia_core::math::hash::mix;

/// The default generator seed from its entity guid: `mix(low 32, high 32)` of the guid's first
/// u64 (its high 64 bits: the first 16 hex digits of the text form).
pub fn gen_seed_from_guid(guid: Guid) -> u32 {
    let first = (guid.0 >> 64) as u64;
    mix(first as u32, (first >> 32) as u32)
}

/// `tier_seed = mix(gen_seed, tier_index)`.
pub fn tier_seed(gen_seed: u32, tier_index: u32) -> u32 {
    mix(gen_seed, tier_index)
}

/// `node_seed = mix(tier_seed, seed_salt)`; the salt defaults to the node id. No cell term, so a
/// point gets the same draws whichever cell computes it.
pub fn node_seed(tier_seed: u32, seed_salt: u32) -> u32 {
    mix(tier_seed, seed_salt)
}

/// `cell_key = mix(i, j)` of a cell (0 for untiled runs, by definition of the caller).
pub fn cell_key(i: i32, j: i32) -> u32 {
    mix(i as u32, j as u32)
}

/// The millimeter-grid quantization `q(v) = round(v · 1000) as i32 as u32`.
pub fn q(v: f32) -> u32 {
    (v * 1000.0).round() as i32 as u32
}

/// The seed of a point a sampler creates at `p`: `mix(mix(mix(q(x), q(y)), q(z)), tier_seed)`.
pub fn point_seed(p: [f32; 3], tier_seed: u32) -> u32 {
    mix(mix(mix(q(p[0]), q(p[1])), q(p[2])), tier_seed)
}

/// The random stream for draw slot `slot` of an element with seed `element_seed` in a node.
pub fn element_rng(element_seed: u32, node_seed: u32, slot: u32) -> Rng {
    Rng::with_stream(
        (u64::from(element_seed) << 32) | u64::from(node_seed),
        u64::from(slot),
    )
}

/// The PCG32 multiplier of `regia_core::math::Rng` (the inlined [`draw`] must match it).
const PCG_MULTIPLIER: u64 = 6_364_136_223_846_793_005;

/// One PCG32 output from state `old` (XSH-RR), as `regia_core::math::Rng::next_u32`.
#[inline(always)]
fn pcg_output(old: u64) -> u32 {
    let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
    xorshifted.rotate_right((old >> 59) as u32)
}

/// One uniform draw in `[0, 1)` (f64, 53 bits) for an element and slot: exactly
/// `element_rng(element_seed, node_seed, slot).f64()` (a test checks it), inlined so per-element
/// loops do not pay a cross-crate call per draw (spec 63 § 15 budgets).
#[inline]
pub fn draw(element_seed: u32, node_seed: u32, slot: u32) -> f64 {
    let seed = (u64::from(element_seed) << 32) | u64::from(node_seed);
    let inc = (u64::from(slot) << 1) | 1;
    // `Rng::with_stream`: state 0, one step, add the seed, one step.
    let s = inc.wrapping_add(seed);
    let s = s.wrapping_mul(PCG_MULTIPLIER).wrapping_add(inc);
    // `Rng::f64`: two outputs, high word first, top 53 bits.
    let hi = u64::from(pcg_output(s));
    let s = s.wrapping_mul(PCG_MULTIPLIER).wrapping_add(inc);
    let lo = u64::from(pcg_output(s));
    // Through `i64`: the value fits 53 bits, so the conversion is exact either way, and a signed one is
    // a single instruction on baseline x86-64.
    (((hi << 32) | lo) >> 11) as i64 as f64 * (1.0 / (1u64 << 53) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_seed_depends_on_position_and_tier_only() {
        let a = point_seed([1.0, 2.0, 3.0], 7);
        assert_eq!(a, point_seed([1.0, 2.0, 3.0], 7));
        assert_eq!(
            a,
            point_seed([1.0002, 2.0, 3.0], 7),
            "sub-millimeter offsets share a seed"
        );
        assert_ne!(a, point_seed([1.002, 2.0, 3.0], 7));
        assert_ne!(a, point_seed([1.0, 2.0, 3.0], 8));
    }

    #[test]
    fn draws_are_per_slot_and_repeatable() {
        let a = draw(11, 22, 0);
        assert_eq!(a, draw(11, 22, 0));
        assert_ne!(a, draw(11, 22, 1));
        assert_ne!(a, draw(12, 22, 0));
        assert!((0.0..1.0).contains(&a));
    }

    #[test]
    fn the_inlined_draw_equals_the_core_rng_draw() {
        // Edge seeds, slots and a sweep of mixed inputs: every bit must match.
        let mut cases = vec![(0, 0, 0), (u32::MAX, u32::MAX, u32::MAX), (1, 0, 8), (0, 1, 3)];
        let mut x = 0x9e37_79b9u32;
        for _ in 0..20_000 {
            x = mix(x, 17);
            cases.push((x, mix(x, 3), x % 64));
        }
        for (e, n, slot) in cases {
            assert_eq!(
                draw(e, n, slot).to_bits(),
                element_rng(e, n, slot).f64().to_bits(),
                "element {e} node {n} slot {slot}"
            );
        }
    }

    #[test]
    fn guid_seed_uses_the_first_u64() {
        let g1 = Guid(0x0123_4567_89ab_cdef_0000_0000_0000_0000);
        let g2 = Guid(0x0123_4567_89ab_cdef_ffff_ffff_ffff_ffff);
        assert_eq!(gen_seed_from_guid(g1), gen_seed_from_guid(g2));
        assert_eq!(gen_seed_from_guid(g1), mix(0x89ab_cdef, 0x0123_4567));
    }
}
