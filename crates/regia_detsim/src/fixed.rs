// SPDX-License-Identifier: MPL-2.0
//! Integer millimeters for lockstep geometry: [`Mm`] (`i32`), [`Mm2`] (`i64`, square millimeters
//! and products of millimeters), and the 2D integer [`dot`], [`cross`], [`len2`] and [`isqrt_u64`].
//!
//! Integer arithmetic is identical on every target by construction, which makes it the first choice
//! for lockstep state that must never drift: positions on a grid, portal edges, avoidance math,
//! edits (the integer policy of ADR-8012 A-4). An `i32` of millimeters spans ±2147 km.
//!
//! **Overflow is explicit.** There are no `+`/`-` operators on these types: Rust's integer operators
//! panic on overflow in debug builds and wrap in release builds, so a debug peer and a release peer
//! would part ways at the first overflow. Pick `checked_*` (an `Option`), `saturating_*` (clamped)
//! or `wrapping_*` (two's complement) instead; each gives the same answer in every build.
//!
//! **f32 conversion.** [`Mm::from_m_f32`] rounds the *exact* value of the `f32` times 1000, half
//! away from zero, with integer arithmetic on the float's bits (no float operation at all), so every
//! target gives the same millimeter. Note that it rounds the float you have, not the decimal you
//! typed: `0.0025_f32` is `0.00249999994...`, which is 2 mm, and `0.0035_f32` is
//! `0.00350000011...`, which is 4 mm. [`Mm::to_m_f32`] is one IEEE conversion and one IEEE division
//! (both correctly rounded on every target). The round trip `from_m_f32(to_m_f32(mm)) == mm` holds
//! for every `|mm| <= 2^23` (8.39 km).
//!
//! ```
//! use regia_detsim::fixed::{self, Mm};
//!
//! let a = Mm::from_m_f32(1.25).unwrap_or(Mm::ZERO);
//! assert_eq!(a, Mm(1250));
//! assert_eq!(Mm::from_m_f32(-0.0625), Some(Mm(-63))); // exactly -62.5 mm: half away from zero
//! assert_eq!(a.checked_add(Mm(i32::MAX)), None);
//! assert_eq!(a.saturating_add(Mm(i32::MAX)), Mm(i32::MAX));
//! assert_eq!(fixed::cross([1000, 0], [0, 1000]), 1_000_000);
//! assert_eq!(fixed::isqrt_u64(fixed::len2([3000, 4000])), 5000);
//! ```

use serde::{Deserialize, Serialize};

/// Millimeters in an `i32`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Mm(pub i32);

/// Square millimeters (or any product of two [`Mm`]) in an `i64`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Mm2(pub i64);

impl Mm {
    /// Zero millimeters.
    pub const ZERO: Mm = Mm(0);
    /// The largest value, `i32::MAX` mm.
    pub const MAX: Mm = Mm(i32::MAX);
    /// The smallest value, `i32::MIN` mm.
    pub const MIN: Mm = Mm(i32::MIN);

    /// The raw millimeters.
    pub const fn get(self) -> i32 {
        self.0
    }

    /// `self + rhs`, `None` on overflow.
    pub const fn checked_add(self, rhs: Mm) -> Option<Mm> {
        match self.0.checked_add(rhs.0) {
            Some(v) => Some(Mm(v)),
            None => None,
        }
    }

    /// `self - rhs`, `None` on overflow.
    pub const fn checked_sub(self, rhs: Mm) -> Option<Mm> {
        match self.0.checked_sub(rhs.0) {
            Some(v) => Some(Mm(v)),
            None => None,
        }
    }

    /// `self * k`, `None` on overflow.
    pub const fn checked_mul(self, k: i32) -> Option<Mm> {
        match self.0.checked_mul(k) {
            Some(v) => Some(Mm(v)),
            None => None,
        }
    }

    /// `self / k` rounded toward zero, `None` for `k == 0` or `MIN / -1`.
    pub const fn checked_div(self, k: i32) -> Option<Mm> {
        match self.0.checked_div(k) {
            Some(v) => Some(Mm(v)),
            None => None,
        }
    }

    /// `-self`, `None` for [`Mm::MIN`].
    pub const fn checked_neg(self) -> Option<Mm> {
        match self.0.checked_neg() {
            Some(v) => Some(Mm(v)),
            None => None,
        }
    }

    /// `self + rhs`, clamped to [`Mm::MIN`]..=[`Mm::MAX`].
    pub const fn saturating_add(self, rhs: Mm) -> Mm {
        Mm(self.0.saturating_add(rhs.0))
    }

    /// `self - rhs`, clamped.
    pub const fn saturating_sub(self, rhs: Mm) -> Mm {
        Mm(self.0.saturating_sub(rhs.0))
    }

    /// `self * k`, clamped.
    pub const fn saturating_mul(self, k: i32) -> Mm {
        Mm(self.0.saturating_mul(k))
    }

    /// `self + rhs`, two's complement wrap.
    pub const fn wrapping_add(self, rhs: Mm) -> Mm {
        Mm(self.0.wrapping_add(rhs.0))
    }

    /// `self - rhs`, two's complement wrap.
    pub const fn wrapping_sub(self, rhs: Mm) -> Mm {
        Mm(self.0.wrapping_sub(rhs.0))
    }

    /// `|self - rhs|` as an unsigned distance (never overflows).
    pub const fn abs_diff(self, rhs: Mm) -> u32 {
        self.0.abs_diff(rhs.0)
    }

    /// `self * rhs` in square millimeters. Exact: an `i32` product always fits an `i64`.
    pub const fn mul_wide(self, rhs: Mm) -> Mm2 {
        Mm2(self.0 as i64 * rhs.0 as i64)
    }

    /// Meters as `f32`: `mm as f32 / 1000.0`, two correctly rounded IEEE operations, so the same on
    /// every target (exact conversion for `|mm| <= 2^24`, then one rounding).
    pub fn to_m_f32(self) -> f32 {
        self.0 as f32 / 1000.0
    }

    /// Millimeters from meters: the exact value of `m * 1000` rounded half away from zero, computed
    /// with integers on the bits of `m`. `None` for NaN, infinities and results outside `i32`.
    /// `-0.0` gives `Mm(0)`.
    pub const fn from_m_f32(m: f32) -> Option<Mm> {
        match round_m_bits_to_mm(m.to_bits()) {
            MmRound::Value(v) => Some(Mm(v)),
            MmRound::Nan | MmRound::Overflow(_) => None,
        }
    }

    /// [`Mm::from_m_f32`] that clamps instead: NaN gives `Mm(0)`, values beyond `i32` (and the
    /// infinities) give [`Mm::MIN`] or [`Mm::MAX`].
    pub const fn from_m_f32_saturating(m: f32) -> Mm {
        match round_m_bits_to_mm(m.to_bits()) {
            MmRound::Value(v) => Mm(v),
            MmRound::Nan => Mm(0),
            MmRound::Overflow(true) => Mm::MIN,
            MmRound::Overflow(false) => Mm::MAX,
        }
    }
}

impl Mm2 {
    /// Zero.
    pub const ZERO: Mm2 = Mm2(0);

    /// The raw square millimeters.
    pub const fn get(self) -> i64 {
        self.0
    }

    /// `self + rhs`, `None` on overflow.
    pub const fn checked_add(self, rhs: Mm2) -> Option<Mm2> {
        match self.0.checked_add(rhs.0) {
            Some(v) => Some(Mm2(v)),
            None => None,
        }
    }

    /// `self - rhs`, `None` on overflow.
    pub const fn checked_sub(self, rhs: Mm2) -> Option<Mm2> {
        match self.0.checked_sub(rhs.0) {
            Some(v) => Some(Mm2(v)),
            None => None,
        }
    }

    /// `self + rhs`, clamped to the `i64` range.
    pub const fn saturating_add(self, rhs: Mm2) -> Mm2 {
        Mm2(self.0.saturating_add(rhs.0))
    }

    /// `self - rhs`, clamped.
    pub const fn saturating_sub(self, rhs: Mm2) -> Mm2 {
        Mm2(self.0.saturating_sub(rhs.0))
    }

    /// The side of a square of this area, rounded down: `floor(sqrt(self))` mm. `None` below zero
    /// and when the side does not fit [`Mm`] (areas of `2^62` mm² and more).
    pub const fn isqrt(self) -> Option<Mm> {
        if self.0 < 0 {
            return None;
        }
        let r = isqrt_u64(self.0 as u64);
        if r > i32::MAX as u64 {
            None
        } else {
            Some(Mm(r as i32))
        }
    }
}

/// `floor(sqrt(n))`, exact, integer only.
pub const fn isqrt_u64(n: u64) -> u64 {
    n.isqrt()
}

/// `a · b` of two 2D integer vectors (millimeters) in an `i64` (square millimeters).
///
/// Exact except for the single input `a == b == [i32::MIN, i32::MIN]`, whose true value `2^63` does
/// not fit and saturates to `i64::MAX`.
pub const fn dot(a: [i32; 2], b: [i32; 2]) -> i64 {
    (a[0] as i64 * b[0] as i64).saturating_add(a[1] as i64 * b[1] as i64)
}

/// `a × b` (the z of the 3D cross product, `a.x * b.y - a.y * b.x`) in an `i64`. Always exact: the
/// result lies in `[-2^63 + 2^31, 2^63 - 2^31]`.
pub const fn cross(a: [i32; 2], b: [i32; 2]) -> i64 {
    a[0] as i64 * b[1] as i64 - a[1] as i64 * b[0] as i64
}

/// `|a|²` in a `u64`. Always exact (at most `2^63`).
pub const fn len2(a: [i32; 2]) -> u64 {
    let x = a[0].unsigned_abs() as u64;
    let y = a[1].unsigned_abs() as u64;
    x * x + y * y
}

/// The outcome of rounding the bits of an `f32` of meters to millimeters.
enum MmRound {
    Value(i32),
    Nan,
    /// Out of the `i32` range; `true` when negative.
    Overflow(bool),
}

/// `round_half_away_from_zero(m * 1000)` from the bits of `m`, integer only.
///
/// An `f32` is `(-1)^sign * mant * 2^exp` with `mant < 2^24`; `mant * 1000 < 2^34` is exact in a
/// `u64`. A non-negative `exp` shifts it left (overflow checked); a negative one shifts it right,
/// and the bits shifted out decide the rounding: at or above one half rounds the magnitude up.
const fn round_m_bits_to_mm(bits: u32) -> MmRound {
    let negative = bits >> 31 != 0;
    let biased = ((bits >> 23) & 0xff) as i32;
    let frac = (bits & 0x007f_ffff) as u64;
    if biased == 0xff {
        return if frac == 0 {
            MmRound::Overflow(negative)
        } else {
            MmRound::Nan
        };
    }
    // Subnormals have no implicit bit and the exponent of the smallest normal.
    let (mant, exp) = if biased == 0 {
        (frac, -149)
    } else {
        (frac | 0x0080_0000, biased - 150)
    };
    let p = mant * 1000;
    let magnitude: u64 = if p == 0 {
        0
    } else if exp >= 0 {
        // Only normal floats get here (subnormals have exp -149), so mant >= 2^23 and the value is
        // at least 2^23 m = 8.4e9 mm: past the i32 range.
        return MmRound::Overflow(negative);
    } else {
        let s = (-exp) as u32;
        if s >= 40 {
            // p < 2^34, so p * 2^-40 < 2^-6: rounds to 0.
            0
        } else {
            let q = p >> s;
            let rem = p & ((1u64 << s) - 1);
            let half = 1u64 << (s - 1);
            if rem >= half { q + 1 } else { q }
        }
    };
    let limit = if negative { 1u64 << 31 } else { i32::MAX as u64 };
    if magnitude > limit {
        return MmRound::Overflow(negative);
    }
    let v = magnitude as i64;
    MmRound::Value((if negative { -v } else { v }) as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference: `m * 1000` is exact in f64 (24 + 10 bits), and `f64::round` rounds half away
    /// from zero.
    fn reference(m: f32) -> Option<i32> {
        if !m.is_finite() {
            return None;
        }
        let r = (f64::from(m) * 1000.0).round();
        if r >= f64::from(i32::MIN) && r <= f64::from(i32::MAX) {
            Some(r as i32)
        } else {
            None
        }
    }

    #[test]
    fn mm_round_trip_and_rounding_rule() {
        // Exact halves (dyadic meters): half away from zero, both signs.
        assert_eq!(Mm::from_m_f32(0.0625), Some(Mm(63))); // 62.5 mm
        assert_eq!(Mm::from_m_f32(-0.0625), Some(Mm(-63)));
        assert_eq!(Mm::from_m_f32(0.1875), Some(Mm(188))); // 187.5 mm
        assert_eq!(Mm::from_m_f32(-0.3125), Some(Mm(-313))); // -312.5 mm
        assert_eq!(Mm::from_m_f32(63.0 / 1024.0), Some(Mm(62))); // 61.5234375 mm
        // Decimals: the float's true value decides, not the digits typed.
        assert_eq!(Mm::from_m_f32(0.0025), Some(Mm(2))); // 2.49999994 mm
        assert_eq!(Mm::from_m_f32(0.0035), Some(Mm(4))); // 3.50000011 mm
        assert_eq!(Mm::from_m_f32(1.0 / 2048.0), Some(Mm(0))); // 0.48828125 mm
        assert_eq!(Mm::from_m_f32(1.0 / 1024.0), Some(Mm(1))); // 0.9765625 mm
        assert_eq!(Mm::from_m_f32(-1.0 / 1024.0), Some(Mm(-1)));
        assert_eq!(Mm::from_m_f32(1.25), Some(Mm(1250)));
        // Zeros, subnormals, specials.
        assert_eq!(Mm::from_m_f32(0.0), Some(Mm(0)));
        assert_eq!(Mm::from_m_f32(-0.0), Some(Mm(0)));
        assert_eq!(Mm::from_m_f32(f32::from_bits(1)), Some(Mm(0)));
        assert_eq!(Mm::from_m_f32(-f32::MIN_POSITIVE), Some(Mm(0)));
        assert_eq!(Mm::from_m_f32(f32::NAN), None);
        assert_eq!(Mm::from_m_f32(f32::INFINITY), None);
        assert_eq!(Mm::from_m_f32(f32::MAX), None);
        assert_eq!(Mm::from_m_f32_saturating(f32::NAN), Mm(0));
        assert_eq!(Mm::from_m_f32_saturating(f32::NEG_INFINITY), Mm::MIN);
        assert_eq!(Mm::from_m_f32_saturating(f32::MAX), Mm::MAX);
        assert_eq!(Mm::from_m_f32_saturating(-f32::MAX), Mm::MIN);
        assert_eq!(Mm::from_m_f32_saturating(-2_147_484.0), Mm::MIN);
        assert_eq!(Mm::from_m_f32_saturating(2_147_484.0), Mm::MAX);
        // The i32 edges (-2147483.648 m is -2^31 mm); f32 steps by 0.25 m there.
        assert_eq!(Mm::from_m_f32(-2_147_483.5), Some(Mm(-2_147_483_500)));
        assert_eq!(Mm::from_m_f32(-(2_147_483.5 + 0.25)), None);
        assert_eq!(Mm::from_m_f32(2_147_483.5), Some(Mm(2_147_483_500)));
        assert_eq!(Mm::from_m_f32(2_147_483.5 + 0.25), None);

        // The rule against the f64 reference: every f32 in [-4096, 4096] m with a stride, both
        // sides of the i32 edges bit by bit, and a stride over all 2^32 patterns.
        let mut checked = 0u32;
        let mut check = |m: f32| {
            assert_eq!(
                Mm::from_m_f32(m),
                reference(m).map(Mm),
                "{m:e} ({:#010x})",
                m.to_bits()
            );
            checked += 1;
        };
        for bits in (0..0x4580_0000u32).step_by(97) {
            check(f32::from_bits(bits));
            check(-f32::from_bits(bits));
        }
        for edge in [2_147_483.5_f32, -2_147_483.5_f32] {
            let b = edge.to_bits();
            for d in 0..64 {
                check(f32::from_bits(b - 32 + d));
            }
        }
        for bits in (0..=u32::MAX).step_by(65_521) {
            check(f32::from_bits(bits));
        }
        assert!(checked > 400_000, "{checked}");

        // Round trip for every |mm| <= 2^23.
        for mm in -(1i32 << 23)..=(1 << 23) {
            assert_eq!(Mm::from_m_f32(Mm(mm).to_m_f32()), Some(Mm(mm)), "{mm}");
        }
        // And it is not promised past that: the first failure above 2^23 exists.
        let first_bad =
            ((1i32 << 23)..(1 << 25)).find(|&mm| Mm::from_m_f32(Mm(mm).to_m_f32()) != Some(Mm(mm)));
        assert!(first_bad.is_some());
    }

    #[test]
    fn integer_ops_are_exact_or_explicit() {
        assert_eq!(Mm(5).checked_add(Mm(7)), Some(Mm(12)));
        assert_eq!(Mm::MAX.checked_add(Mm(1)), None);
        assert_eq!(Mm::MIN.checked_sub(Mm(1)), None);
        assert_eq!(Mm::MIN.checked_neg(), None);
        assert_eq!(Mm::MIN.checked_div(-1), None);
        assert_eq!(Mm(7).checked_div(0), None);
        assert_eq!(Mm(-7).checked_div(2), Some(Mm(-3)));
        assert_eq!(Mm(1 << 20).checked_mul(1 << 12), None);
        assert_eq!(Mm(1 << 20).saturating_mul(-(1 << 12)), Mm::MIN);
        assert_eq!(Mm::MAX.saturating_add(Mm(1)), Mm::MAX);
        assert_eq!(Mm::MIN.saturating_sub(Mm(1)), Mm::MIN);
        assert_eq!(Mm::MAX.wrapping_add(Mm(1)), Mm::MIN);
        assert_eq!(Mm::MIN.wrapping_sub(Mm(1)), Mm::MAX);
        assert_eq!(Mm::MIN.abs_diff(Mm::MAX), u32::MAX);
        assert_eq!(Mm::MIN.mul_wide(Mm::MIN), Mm2(1 << 62));
        assert_eq!(Mm2(i64::MAX).checked_add(Mm2(1)), None);
        assert_eq!(Mm2(i64::MIN).saturating_sub(Mm2(1)), Mm2(i64::MIN));
        assert_eq!(Mm2(-1).isqrt(), None);
        assert_eq!(Mm2(1 << 62).isqrt(), None);
        assert_eq!(Mm2((1 << 62) - 1).isqrt(), Some(Mm(i32::MAX)));
        assert_eq!(Mm2(24).isqrt(), Some(Mm(4)));
        assert_eq!(Mm2(25).isqrt(), Some(Mm(5)));

        // dot saturates only at [MIN, MIN] . [MIN, MIN]; cross and len2 are always exact.
        let m = i32::MIN;
        assert_eq!(dot([m, m], [m, m]), i64::MAX);
        assert_eq!(
            dot([m, m], [m, m + 1]),
            (1i64 << 62) + ((1i64 << 62) - (1i64 << 31))
        );
        let ext = [i32::MIN, i32::MAX, -1, 0, 1, 12_345, -98_765];
        for &a0 in &ext {
            for &a1 in &ext {
                for &b0 in &ext {
                    for &b1 in &ext {
                        let c = i128::from(a0) * i128::from(b1) - i128::from(a1) * i128::from(b0);
                        assert_eq!(i128::from(cross([a0, a1], [b0, b1])), c);
                        let d = i128::from(a0) * i128::from(b0) + i128::from(a1) * i128::from(b1);
                        let want = d.min(i128::from(i64::MAX));
                        assert_eq!(i128::from(dot([a0, a1], [b0, b1])), want);
                    }
                }
                let l = i128::from(a0) * i128::from(a0) + i128::from(a1) * i128::from(a1);
                assert_eq!(i128::from(len2([a0, a1])), l);
            }
        }
    }

    #[test]
    fn isqrt_is_the_floor_root() {
        let mut ns: Vec<u64> = vec![0, 1, 2, 3, 4, 15, 16, 17, u64::MAX, u64::MAX - 1, 1 << 63];
        for r in [1u64, 2, 3, 1 << 16, (1 << 32) - 1, 3_037_000_499, 4_294_967_295] {
            let sq = r * r;
            ns.extend([sq - 1, sq, sq + 1]);
        }
        let mut x = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..100_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            ns.push(x >> (x % 64));
        }
        for n in ns {
            let r = u128::from(isqrt_u64(n));
            assert!(r * r <= u128::from(n) && (r + 1) * (r + 1) > u128::from(n), "{n}");
        }
    }
}
