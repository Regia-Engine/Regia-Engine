// SPDX-License-Identifier: MPL-2.0
//! Integer hashes for seed mixing (`docs/specs/62-procedural-content-graph.md` § 6.2).
//!
//! [`pcg_hash`] is the PCG output hash (Jarzynski and Olano, "Hash Functions for GPU Rendering",
//! JCGT 2020): one LCG step followed by the PCG RXS-M-XS output permutation. [`mix`] combines two
//! words into one. Both are pure integer arithmetic, so every platform gives the same bits. The copy
//! in `regia_terrain::grass::select` has the same output (a test in `regia_procgen` pins that).

/// The PCG output hash of one 32-bit word.
#[inline]
#[must_use]
pub fn pcg_hash(v: u32) -> u32 {
    let state = v.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let word = ((state >> ((state >> 28) + 4)) ^ state).wrapping_mul(277_803_737);
    (word >> 22) ^ word
}

/// Combines two words into one: `pcg_hash(a ^ pcg_hash(b))`. Not symmetric: `mix(a, b)` and
/// `mix(b, a)` differ.
#[inline]
#[must_use]
pub fn mix(a: u32, b: u32) -> u32 {
    pcg_hash(a ^ pcg_hash(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recorded vectors (computed independently from the formula in spec 62 § 6.2).
    #[test]
    fn pcg_hash_matches_recorded_vectors() {
        assert_eq!(pcg_hash(0), 129_708_002);
        assert_eq!(pcg_hash(1), 2_831_084_092);
        assert_eq!(pcg_hash(2), 2_055_130_248);
        assert_eq!(pcg_hash(0xdead_beef), 1_730_779_506);
        assert_eq!(pcg_hash(u32::MAX), 3_861_530_882);
        assert_eq!(pcg_hash(12_345), 4_099_845_390);
    }

    #[test]
    fn mix_matches_recorded_vectors_and_is_ordered() {
        assert_eq!(mix(0, 0), 817_759_070);
        assert_eq!(mix(1, 2), 2_112_726_488);
        assert_eq!(mix(2, 1), 3_752_912_895);
        assert_eq!(mix(0xdead_beef, 7), 3_865_353_526);
        assert_eq!(mix(u32::MAX, u32::MAX), 3_094_549_908);
        assert_ne!(mix(1, 2), mix(2, 1));
    }
}
