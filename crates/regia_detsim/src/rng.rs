// SPDX-License-Identifier: MPL-2.0
//! A deterministic random number generator for lockstep code: [`DetRng`].
//!
//! Every peer that seeds a [`DetRng`] with the same `u64` draws the same numbers, on every target,
//! because the generator is integer only (wrapping adds, xors, shifts, rotations and one
//! multiplication) and the float helpers convert an integer that fits the mantissa exactly.
//!
//! - **Generator:** xoshiro256** (Blackman and Vigna, "Scrambled Linear Pseudorandom Number
//!   Generators", ACM TOMS 47(4), 2021), 256 bits of state, period 2^256 - 1. The state is filled from
//!   the seed by SplitMix64 (Steele, Lea and Flood, "Fast Splittable Pseudorandom Number Generators",
//!   OOPSLA 2014), as the xoshiro authors recommend; SplitMix64 never yields the all-zero state.
//! - **Streams:** [`DetRng::stream`] derives an independent generator per system (AI, loot, weather)
//!   from the generator's *key* and a system id, never from its draws, so a new draw in one system
//!   never moves another system's numbers.
//! - **Ranges:** [`DetRng::range_u32`] and [`DetRng::range_u64`] are exactly unbiased, by Lemire's
//!   multiply-and-reject method (D. Lemire, "Fast Random Integer Generation in an Interval", ACM
//!   TOMS 45(1), 2019). Never use `next_u32() % n`: it favors small values.
//! - **Floats:** [`DetRng::unit_f32`] takes 24 bits and [`DetRng::unit_f64`] 53 bits, so every value
//!   is an exact multiple of `2^-24` (`2^-53`) in `[0, 1)`, identical on every target.
//!
//! A change to any output of this module is a sim-version change (ADR-8012 A-4); the first outputs
//! are pinned by `det_rng_streams_are_independent_and_stable` and the golden `math_rng_fixed_vectors`.
//!
//! ```
//! use regia_detsim::rng::DetRng;
//!
//! let root = DetRng::seed_from(0x5eed);
//! let mut loot = root.stream(1);
//! let mut ai = root.stream(2);
//! let roll = loot.range_u32(6) + 1; // a fair d6
//! assert!((1..=6).contains(&roll));
//! let jitter = ai.unit_f32(); // in [0, 1), a multiple of 2^-24
//! assert!((0.0..1.0).contains(&jitter));
//! // The same seed and stream id give the same numbers.
//! assert_eq!(root.stream(1).next_u64(), DetRng::seed_from(0x5eed).stream(1).next_u64());
//! ```

use serde::{Deserialize, Serialize};

/// SplitMix64's increment: `2^64 / φ`, rounded to odd.
const SPLITMIX_GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;

/// Tag mixed into a system id before it is combined with a key (ASCII "DETSTRM1"), so a stream id
/// and a seed that happen to be the same number do not line up.
const STREAM_TAG: u64 = 0x4445_5453_5452_4d31;

/// SplitMix64's output function (Stafford's "Mix13" constants): a bijection on `u64`.
#[inline]
const fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Lemire's nearly divisionless map of a `bits`-wide uniform word onto `[0, bound)`.
///
/// `m = word * bound` is a `2 * bits` wide product; its high half is the candidate and its low half
/// `l` decides acceptance. Words with `l < 2^bits mod bound` are rejected (`None`), which leaves
/// exactly `floor(2^bits / bound)` accepted words per output value. The modulo is only computed when
/// `l < bound`, the rare case. Requires `1 <= bound < 2^bits`, `word < 2^bits`, `bits <= 64`.
///
/// One function for every width, so the exhaustive 16-bit test proves the code the 32- and 64-bit
/// draws run.
#[inline]
fn lemire(word: u64, bound: u64, bits: u32) -> Option<u64> {
    let m = u128::from(word) * u128::from(bound);
    let mask = (1u128 << bits) - 1;
    let low = m & mask;
    let bound = u128::from(bound);
    if low < bound {
        // (2^bits - bound) mod bound == 2^bits mod bound.
        let threshold = ((1u128 << bits) - bound) % bound;
        if low < threshold {
            return None;
        }
    }
    Some((m >> bits) as u64)
}

/// The deterministic generator: xoshiro256** with a stream key (see the module docs).
///
/// Not `Copy` on purpose: a silent copy replays the same numbers. Clone it when that is what you
/// mean. Serialized as `{ key, s }`; a deserialized all-zero state is rejected
/// (`det_rng_zero_state`), because xoshiro never leaves it. It is also a
/// [`crate::state::DetState`] (the key, then the four words), so a snapshot nests it with
/// `w.state(&rng)` / `r.state()?`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "DetRngParts", into = "DetRngParts")]
pub struct DetRng {
    key: u64,
    s: [u64; 4],
}

/// The serialized form of a [`DetRng`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetRngParts {
    /// The stream key [`DetRng::stream`] derives from.
    pub key: u64,
    /// The xoshiro256** state words.
    pub s: [u64; 4],
}

impl From<DetRng> for DetRngParts {
    fn from(r: DetRng) -> Self {
        DetRngParts { key: r.key, s: r.s }
    }
}

impl TryFrom<DetRngParts> for DetRng {
    type Error = &'static str;

    fn try_from(p: DetRngParts) -> Result<Self, Self::Error> {
        DetRng::from_state(p.key, p.s).ok_or("det_rng_zero_state")
    }
}

impl DetRng {
    /// A generator whose key is `seed`, its state filled by four SplitMix64 outputs from `seed`.
    pub const fn seed_from(seed: u64) -> DetRng {
        let mut x = seed;
        let mut s = [0u64; 4];
        let mut i = 0;
        while i < 4 {
            x = x.wrapping_add(SPLITMIX_GAMMA);
            s[i] = mix64(x);
            i += 1;
        }
        // Four consecutive SplitMix64 outputs are four distinct values (a bijection of distinct
        // counters), so at most one is zero and the state is never all zero.
        DetRng { key: seed, s }
    }

    /// The independent stream `system_id` of this generator.
    ///
    /// Its key is `mix64(key ^ mix64(system_id ^ 0x4445_5453_5452_4d31))` and its state is
    /// [`DetRng::seed_from`] of that key. It depends only on this generator's key and `system_id`,
    /// never on how many numbers were drawn, so streams can be made at any time in any order. Distinct
    /// ids of one generator give distinct keys (both mixes are bijections); streams of streams work
    /// the same way. Two streams are independent in the usual sense for a 2^256 generator: their
    /// sequences overlap only with negligible probability.
    pub const fn stream(&self, system_id: u64) -> DetRng {
        DetRng::seed_from(mix64(self.key ^ mix64(system_id ^ STREAM_TAG)))
    }

    /// A generator from a saved key and state; `None` for the all-zero state.
    pub const fn from_state(key: u64, s: [u64; 4]) -> Option<DetRng> {
        if s[0] | s[1] | s[2] | s[3] == 0 {
            None
        } else {
            Some(DetRng { key, s })
        }
    }

    /// The stream key.
    pub const fn key(&self) -> u64 {
        self.key
    }

    /// The xoshiro256** state words (for a hash of the sim state or a save).
    pub const fn state(&self) -> [u64; 4] {
        self.s
    }

    /// The next 64 bits (xoshiro256**, the reference algorithm).
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let s = &mut self.s;
        let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }

    /// The next 32 bits: the high half of [`DetRng::next_u64`].
    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// A uniform value in `[0, bound)`, exactly unbiased (Lemire). `bound == 0` means the full
    /// `2^32` range (the same as [`DetRng::next_u32`]).
    ///
    /// Draws one [`DetRng::next_u32`] and, with probability `(2^32 mod bound) / 2^32` (below
    /// `bound / 2^32`), draws again.
    #[inline]
    pub fn range_u32(&mut self, bound: u32) -> u32 {
        if bound == 0 {
            return self.next_u32();
        }
        loop {
            if let Some(v) = lemire(u64::from(self.next_u32()), u64::from(bound), 32) {
                return v as u32;
            }
        }
    }

    /// A uniform value in `[0, bound)`, exactly unbiased (Lemire over 64-bit words). `bound == 0`
    /// means the full `2^64` range.
    #[inline]
    pub fn range_u64(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return self.next_u64();
        }
        loop {
            if let Some(v) = lemire(self.next_u64(), bound, 64) {
                return v;
            }
        }
    }

    /// A uniform value in `[lo, hi)`, unbiased; `lo` when the range is empty (`hi <= lo`).
    #[inline]
    pub fn range_i32(&mut self, lo: i32, hi: i32) -> i32 {
        if hi <= lo {
            return lo;
        }
        // The span is at most 2^32 - 1, so it fits a u32 and is never 0 here.
        let span = hi.wrapping_sub(lo) as u32;
        lo.wrapping_add(self.range_u32(span) as i32)
    }

    /// A uniform `f32` in `[0, 1)`: the top 24 bits of [`DetRng::next_u32`] times `2^-24`. Both steps
    /// are exact, so the value is the same on every target.
    #[inline]
    pub fn unit_f32(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 * (1.0 / 16_777_216.0)
    }

    /// A uniform `f64` in `[0, 1)`: the top 53 bits of [`DetRng::next_u64`] times `2^-53` (exact).
    #[inline]
    pub fn unit_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / 9_007_199_254_740_992.0)
    }

    /// Shuffle `items` uniformly (Fisher-Yates from the back, each swap index by
    /// [`DetRng::range_u64`]).
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.range_u64(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }
}

/// Saves the stream key, then the four xoshiro256** state words (five `u64`s, 40 bytes).
///
/// The key is part of the state: [`DetRng::stream`] derives from it, so a resume that kept only the
/// four words would draw the same numbers from `next_u64` but hand out different streams (review
/// det-core-review-c R1). `load` refuses the all-zero state (`bad_value`, `det_rng_zero_state`), as
/// the serde form does.
impl crate::state::DetState for DetRng {
    const STATE_TAG: [u8; 8] = *b"detrng\0\0";
    const STATE_VERSION: u32 = 1;
    fn save(&self, w: &mut crate::state::DetWriter) {
        w.u64(self.key);
        for word in self.s {
            w.u64(word);
        }
    }
    fn load(r: &mut crate::state::DetReader<'_>) -> Result<Self, crate::state::DetStateError> {
        let key = r.u64()?;
        let s = [r.u64()?, r.u64()?, r.u64()?, r.u64()?];
        DetRng::from_state(key, s).ok_or_else(|| {
            crate::state::DetStateError::bad_value("det_rng_zero_state: all four state words are zero")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_u32_is_unbiased_for_small_bounds() {
        // A reduced generator: every 16-bit word exactly once (a perfectly uniform 16-bit source).
        // Through the same `lemire` that `range_u32` and `range_u64` run, every value of [0, bound)
        // must come out exactly floor(2^16 / bound) times, and exactly 2^16 mod bound words are
        // rejected.
        let mut bounds: Vec<u64> = (1..=300).collect();
        bounds.extend([1000, 4095, 4096, 4097, 21_845, 32_767, 32_768, 32_769, 65_535]);
        for bound in bounds {
            let mut counts = vec![0u32; bound as usize];
            let mut rejected = 0u64;
            for word in 0..(1u64 << 16) {
                match lemire(word, bound, 16) {
                    Some(v) => counts[v as usize] += 1,
                    None => rejected += 1,
                }
            }
            let each = ((1u64 << 16) / bound) as u32;
            assert!(counts.iter().all(|&c| c == each), "bound {bound}: {counts:?}");
            assert_eq!(rejected, (1u64 << 16) % bound, "bound {bound}");
        }
        // The plain modulo map is biased for the same source (the reason for Lemire).
        let mut low = 0u32;
        let mut high = 0u32;
        for word in 0..(1u32 << 16) {
            if word.is_multiple_of(3) {
                low += 1;
            } else if word % 3 == 2 {
                high += 1;
            }
        }
        assert_ne!(low, high);
    }

    #[test]
    fn lemire_32_and_64_bit_widths_stay_in_range() {
        for (word, bound) in [(u64::from(u32::MAX), 7u64), (0, 7), (123_456_789, 1), (5, 3)] {
            if let Some(v) = lemire(word, bound, 32) {
                assert!(v < bound);
            }
        }
        for (word, bound) in [(u64::MAX, u64::MAX), (u64::MAX, 3), (0, u64::MAX), (1, 2)] {
            if let Some(v) = lemire(word, bound, 64) {
                assert!(v < bound);
            }
        }
        // The largest word maps to the top value.
        assert_eq!(lemire(u64::from(u32::MAX), 10, 32), Some(9));
    }
}
