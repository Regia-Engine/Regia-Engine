// SPDX-License-Identifier: MPL-2.0
//! A fast hasher for the renderer's per-frame maps keyed by `u64` ids and content keys.
//!
//! `std`'s default SipHash is DoS-resistant, which these maps do not need: their keys come
//! from the engine (stable object ids, content hashes computed once at load). Each word is
//! folded in with a rotate, xor and a multiply by an odd 64-bit constant (the "Fx" hash of
//! the Firefox and rustc hash tables), which spreads a key into the high bits the table's
//! control bytes use. Iteration order changes with the hasher; every renderer map that
//! uses it is either looked up by key or sorted before its order matters.

use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;
use std::hash::{BuildHasherDefault, Hasher};

/// Multiplier of the Fx hash (odd, from the golden ratio's bit pattern).
const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// Fx-style word hasher. Not for keys an attacker controls.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct KeyHasher {
    hash: u64,
}

impl KeyHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for KeyHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for word in words {
            self.add(u64::from_le_bytes(*word));
        }
        if !rest.is_empty() {
            let mut word = [0u8; 8];
            word[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(word));
        }
    }

    #[inline]
    fn write_u8(&mut self, value: u8) {
        self.add(u64::from(value));
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.add(u64::from(value));
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.add(value);
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.add(value as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

/// `BuildHasher` of [`KeyHasher`].
pub(crate) type KeyBuild = BuildHasherDefault<KeyHasher>;
/// A `HashMap` with [`KeyHasher`].
pub(crate) type KeyMap<K, V> = HashMap<K, V, KeyBuild>;
/// A `HashSet` with [`KeyHasher`].
#[cfg(test)]
pub(crate) type KeySet<K> = HashSet<K, KeyBuild>;

/// An empty [`KeyMap`] with room for `capacity` entries. The frame keeps its maps between
/// frames instead (`frame::scratch`, ADR-385), so only the tests size one up front.
#[cfg(test)]
pub(crate) fn key_map_with_capacity<K, V>(capacity: usize) -> KeyMap<K, V> {
    HashMap::with_capacity_and_hasher(capacity, KeyBuild::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::BuildHasher;

    #[test]
    fn sequential_ids_spread_into_the_high_bits() {
        // hashbrown keys its control bytes on the top 7 bits; small sequential ids must
        // not all land on one control value.
        let build = KeyBuild::default();
        let tops: KeySet<u64> = (0..1000u64).map(|id| build.hash_one(id) >> 57).collect();
        assert!(tops.len() > 100, "only {} distinct top-7-bit values", tops.len());
    }

    #[test]
    fn map_round_trips() {
        let mut map: KeyMap<u64, u32> = key_map_with_capacity(4);
        for id in 0..10_000u64 {
            map.insert(id.wrapping_mul(0x9e37_79b9_7f4a_7c15), id as u32);
        }
        for id in 0..10_000u64 {
            assert_eq!(
                map.get(&id.wrapping_mul(0x9e37_79b9_7f4a_7c15)),
                Some(&(id as u32))
            );
        }
    }

    #[test]
    fn byte_writes_cover_the_tail() {
        let build = KeyBuild::default();
        assert_ne!(build.hash_one([1u8, 2, 3]), build.hash_one([1u8, 2, 4]));
        assert_ne!(build.hash_one("rock_a"), build.hash_one("rock_b"));
    }
}
