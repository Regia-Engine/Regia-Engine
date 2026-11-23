// SPDX-License-Identifier: MPL-2.0
//! Seeded PCG32 generator for the synthetic sources and the network simulator
//! (`docs/specs/59-voice-chat.md` § 7). Bit-identical on every platform.

/// PCG-XSH-RR 64/32 (M. E. O'Neill, "PCG: A Family of Simple Fast Space-Efficient Statistically
/// Good Algorithms for Random Number Generation", 2014).
#[derive(Clone, Debug)]
pub struct Pcg32 {
    state: u64,
    inc: u64,
}

const MULT: u64 = 6_364_136_223_846_793_005;

impl Pcg32 {
    /// A generator for `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        let mut rng = Self {
            state: 0,
            inc: (0xDA3E_39CB_94B9_5BDBu64 << 1) | 1,
        };
        rng.next_u32();
        rng.state = rng.state.wrapping_add(seed);
        rng.next_u32();
        rng
    }

    /// The next 32 random bits.
    pub fn next_u32(&mut self) -> u32 {
        let old = self.state;
        self.state = old.wrapping_mul(MULT).wrapping_add(self.inc);
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        let rot = (old >> 59) as u32;
        xorshifted.rotate_right(rot)
    }

    /// A uniform value in `[0, 1)`.
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 / 16_777_216.0
    }

    /// A uniform integer in `0..=max`.
    pub fn next_up_to(&mut self, max: u32) -> u32 {
        if max == u32::MAX {
            return self.next_u32();
        }
        ((u64::from(self.next_u32()) * (u64::from(max) + 1)) >> 32) as u32
    }
}
