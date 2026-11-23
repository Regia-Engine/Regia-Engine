// SPDX-License-Identifier: MPL-2.0
//! Seasons for instanced scatter classes (ADR-3146, `book/agents/scatter-forest.md`).
//!
//! A class carries one [`ScatterSeason`]: a tint on its leaf materials and a leaf density. Which
//! instances keep their leaves is the rule of `regia_framework::season::leaf_present`: a fixed rank per
//! instance against the density, so the set is the same in every run and shrinks monotonically as the
//! density falls. `regia_framework` depends on this crate, not the other way round, so the rule is
//! written here again and a test in `regia_framework` compares the two over hashes and densities.
//!
//! Nothing per instance is rewritten on a season change. The mesh path decides per frame, in the loop
//! that builds the frame's draws anyway; the impostor path holds each instance's [`leaf_rank`] on the
//! GPU from the upload on, and a season change writes the prototype table (`impostor_common.wgsl`,
//! `imp_leaf_present`). A tint changes the uniform of the class's own material copies: the renderer
//! keeps their textures and bind groups under the same key.

use serde::Serialize;

/// What one leaf rank step is worth: a rank is 24 bits, `rank / 2^24` is the instance's place in `[0, 1)`.
pub const LEAF_RANK_SCALE: f32 = 16_777_216.0;

/// The instance's place in the leaf order, 24 bits: the splitmix64 finalizer of `instance_hash`
/// (Steele, Lea & Flood 2014), top bits. An instance with a lower rank keeps its leaves longer.
#[must_use]
pub fn leaf_rank(instance_hash: u64) -> u32 {
    let mut x = instance_hash.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    (x >> 40) as u32
}

/// Whether an instance of rank `rank` keeps its leaves at `density`. `imp_leaf_present` in
/// `impostor_common.wgsl` is this comparison: the rank and `2^-24` are exact in `f32`, so both sides
/// compare the same two floats.
#[must_use]
pub fn rank_present(rank: u32, density: f32) -> bool {
    if !density.is_finite() || density <= 0.0 {
        return false;
    }
    (rank as f32) / LEAF_RANK_SCALE < density.min(1.0)
}

/// Whether the instance with hash `instance_hash` keeps its leaves at `density`: the same answer as
/// `regia_framework::season::leaf_present`. An instance present at a density is present at every
/// higher one.
#[must_use]
pub fn leaf_present(instance_hash: u64, density: f32) -> bool {
    rank_present(leaf_rank(instance_hash), density)
}

/// One class's season ([`super::field::ScatterField::set_class_season`]): sanitized on construction, so
/// it holds no NaN and compares as a value.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ScatterSeason {
    tint: [f32; 3],
    density: f32,
}

impl Eq for ScatterSeason {}

impl ScatterSeason {
    /// Largest tint channel a season keeps.
    pub const MAX_TINT: f32 = 8.0;

    /// A season that multiplies the leaf materials' base colour by `tint` (linear RGB; a non-finite
    /// channel reads 1, the rest clamp to `0..=`[`Self::MAX_TINT`]) and keeps the leaves of the fraction
    /// `density` of the instances (clamped to `0..=1`; non-finite reads 0, as in `leaf_present`).
    #[must_use]
    pub fn new(tint: [f32; 3], density: f32) -> Self {
        Self {
            tint: tint.map(|c| {
                if c.is_finite() {
                    c.clamp(0.0, Self::MAX_TINT)
                } else {
                    1.0
                }
            }),
            density: if density.is_finite() {
                density.clamp(0.0, 1.0)
            } else {
                0.0
            },
        }
    }

    /// The tint on the class's leaf materials.
    #[must_use]
    pub fn tint(&self) -> [f32; 3] {
        self.tint
    }

    /// The fraction of the class's instances that keep their leaves.
    #[must_use]
    pub fn density(&self) -> f32 {
        self.density
    }

    /// Whether the season changes nothing: no tint, every instance in leaf.
    #[must_use]
    pub fn is_neutral(&self) -> bool {
        self.tint == [1.0; 3] && self.density >= 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rank_is_24_bits_and_uniform() {
        let n = 1u64 << 16;
        let mut quarters = [0u32; 4];
        for hash in 0..n {
            let rank = leaf_rank(hash);
            assert!(rank < 1 << 24);
            quarters[(rank >> 22) as usize] += 1;
        }
        for count in quarters {
            let share = f64::from(count) / n as f64;
            assert!((share - 0.25).abs() < 0.01, "{quarters:?}");
        }
    }

    #[test]
    fn the_set_shrinks_monotonically_with_the_density() {
        for hash in 0..4096u64 {
            let mut last = true;
            for step in (0..=64).rev() {
                let now = leaf_present(hash, step as f32 / 64.0);
                assert!(last || !now, "hash {hash} came back as the density fell");
                last = now;
            }
        }
    }

    #[test]
    fn the_ends_are_all_and_none() {
        for hash in 0..4096u64 {
            assert!(leaf_present(hash, 1.0));
            assert!(leaf_present(hash, 7.0), "a density past 1 reads 1");
            assert!(!leaf_present(hash, 0.0));
            assert!(!leaf_present(hash, -1.0));
            assert!(!leaf_present(hash, f32::NAN));
        }
    }

    #[test]
    fn a_season_is_sanitized_and_neutral_only_when_it_changes_nothing() {
        let s = ScatterSeason::new([f32::NAN, -1.0, 99.0], f32::INFINITY);
        assert_eq!(s.tint(), [1.0, 0.0, ScatterSeason::MAX_TINT]);
        assert_eq!(s.density(), 0.0);
        assert!(ScatterSeason::new([1.0; 3], 1.0).is_neutral());
        assert!(ScatterSeason::new([1.0; 3], 3.0).is_neutral());
        assert!(!ScatterSeason::new([1.0; 3], 0.5).is_neutral());
        assert!(!ScatterSeason::new([1.0, 0.5, 1.0], 1.0).is_neutral());
    }
}
