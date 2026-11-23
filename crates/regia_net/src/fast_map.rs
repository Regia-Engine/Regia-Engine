// SPDX-License-Identifier: MPL-2.0
//! The fixed-seed hash map of the server send loop and of the ECS-free interest grid (spec 72
//! § 14, spec 120 § 8.6). Core: no ECS (moved out of `dense` so `crate::interest` and
//! `crate::external_replication` build without the `ecs` feature).

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// A hash for small keys the server makes itself (ids, slots, grid cells), never for anything a
/// client chooses: the send loop hashes per (connection, record), and a fixed multiplicative mix
/// costs a fraction of the default. The fixed seed also makes iteration order the same every run.
#[derive(Debug, Clone, Copy, Default)]
pub struct FastHasher(u64);

/// Odd 64-bit constant of multiplicative hashing: 2^64 over the golden ratio (Knuth, TAOCP 6.4).
const FAST_HASH_K: u64 = 0x9e37_79b9_7f4a_7c15;

impl FastHasher {
    #[inline]
    fn mix(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(FAST_HASH_K);
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn finish(&self) -> u64 {
        // The multiply leaves its best bits at the top; tables index with both ends.
        self.0 ^ (self.0 >> 32)
    }

    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for w in words {
            self.mix(u64::from_le_bytes(*w));
        }
        for b in rest {
            self.mix(u64::from(*b));
        }
    }

    #[inline]
    fn write_u16(&mut self, v: u16) {
        self.mix(u64::from(v));
    }

    #[inline]
    fn write_u32(&mut self, v: u32) {
        self.mix(u64::from(v));
    }

    #[inline]
    fn write_i32(&mut self, v: i32) {
        self.mix(u64::from(v as u32));
    }

    #[inline]
    fn write_u64(&mut self, v: u64) {
        self.mix(v);
    }

    #[inline]
    fn write_usize(&mut self, v: usize) {
        self.mix(v as u64);
    }
}

/// A `HashMap` with [`FastHasher`], for keys the server makes itself.
pub type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FastHasher>>;
