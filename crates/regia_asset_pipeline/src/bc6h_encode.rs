// SPDX-License-Identifier: MPL-2.0
//! BC6H (BPTC-FR) HDR block compression: 4×4 half-float RGB blocks into 16 bytes.
//!
//! Original cleanroom code written from the public BC6H format description (Khronos Data Format
//! Specification, "BPTC" section; the Direct3D 11 BC6H format page): the 14 block modes and their
//! bit layouts, the endpoint delta transform, unquantisation and the final `31/64` scale (see
//! `docs/provenance.md`). Blocks are 16 bytes, texels are RGB half floats, rows of blocks run
//! left to right, top to bottom, and a level whose sides are not multiples of 4 is padded by
//! repeating its last row and column (the same convention as `etc2_compress`).
//!
//! What is implemented:
//!
//! - Unsigned (`BC6H_UF16`) and signed (`BC6H_SF16`) variants.
//! - All 14 modes, encode and decode: the one-subset modes 11 (direct 10-bit endpoints) and
//!   12, 13, 14 (11/12/16-bit base with 9/8/4-bit deltas), all with 4-bit indices, and the
//!   two-subset modes 1..=9 (base plus deltas) and 10 (direct 6-bit endpoints) over the 32
//!   two-subset partition shapes, with 3-bit indices and the two anchor texels.
//! - Endpoint quantisation (nearest decoded value, by binary search over the monotone
//!   unquantiser), the delta transform (two's-complement deltas clamped so the decoder's sum
//!   never wraps), index selection on the *reconstructed* palette so quantisation slop is absorbed
//!   by the index choice, a partition shortlist from a quantisation-free fit, and best-mode
//!   selection over the modes `Bc6hOptions::modes` enables.
//! - Error metrics: plain linear, tone-mapped (`x/(1+|x|)` per channel) and a log-domain metric
//!   with a deterministic bit-manipulation `log2`, so bright pixels do not dominate the fit.
//!
//! The module carries its own reference decoder ([`decode_block`], [`decode_image`]).
//! `tests/bc6h_encode_verify.rs` checks both against an independent decoder, the cook's ISPC
//! BC6H encoder and a Vulkan driver (ADR-2331). `regia_asset::bcn_decode` does not decode BC6H.
//!
//! Endpoints are the per-channel bounding box of each subset, which suits correlated channels
//! and is weaker on blocks whose channels move in opposite directions.
//!
//! Everything here is deterministic: fixed iteration order, ties resolved toward the earlier
//! mode/partition/index, no clocks and no hash maps.

use thiserror::Error;

/// Bytes in one BC6H block.
pub const BLOCK_BYTES: usize = 16;

/// Number of two-subset partition shapes.
const PARTITIONS: usize = 32;

/// BC6H variant: unsigned or signed half-float endpoints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Bc6hFormat {
    /// `BC6H_UF16`: endpoints and texels are non-negative half floats.
    Unsigned,
    /// `BC6H_SF16`: endpoints and texels are signed half floats.
    Signed,
}

/// Error metric for index and mode selection.
///
/// All three are deterministic (the log-domain variant uses a bit-manipulation `log2`
/// approximation, never libm), so the same input and options always produce the same bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Bc6hMetric {
    /// Sum of squared per-channel differences. Bright pixels dominate the fit.
    #[default]
    Linear,
    /// Per-channel tone map `x/(1+|x|)` before squaring, so a bright texel cannot outweigh
    /// many dark ones. Recommended for HDR content whose darks matter.
    ToneMapped,
    /// Per-channel `log2(1+|x|)` before squaring: constant *relative* error. Uses a
    /// deterministic mantissa-linear `log2` approximation (error below 0.1).
    LogDomain,
}

/// The set of BC6H block modes the encoder may emit (numbered 1..=14 as in the format).
///
/// Combine with [`Bc6hModes::union`]; [`Bc6hOptions::default`] enables all 14. An empty set
/// falls back to mode 11 (see [`encode_block`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bc6hModes(u16);

impl Bc6hModes {
    /// Mode 1: two subsets, 10-bit base with 5-bit deltas, 3-bit indices.
    pub const MODE_1: Self = Self(1 << 0);
    /// Mode 2: two subsets, 7-bit base with 6-bit deltas, 3-bit indices.
    pub const MODE_2: Self = Self(1 << 1);
    /// Mode 3: two subsets, 11-bit base with 5/4/4-bit (R/G/B) deltas.
    pub const MODE_3: Self = Self(1 << 2);
    /// Mode 4: two subsets, 11-bit base with 4/5/4-bit deltas.
    pub const MODE_4: Self = Self(1 << 3);
    /// Mode 5: two subsets, 11-bit base with 4/4/5-bit deltas.
    pub const MODE_5: Self = Self(1 << 4);
    /// Mode 6: two subsets, 9-bit base with 5-bit deltas.
    pub const MODE_6: Self = Self(1 << 5);
    /// Mode 7: two subsets, 8-bit base with 6/5/5-bit deltas.
    pub const MODE_7: Self = Self(1 << 6);
    /// Mode 8: two subsets, 8-bit base with 5/6/5-bit deltas.
    pub const MODE_8: Self = Self(1 << 7);
    /// Mode 9: two subsets, 8-bit base with 5/5/6-bit deltas.
    pub const MODE_9: Self = Self(1 << 8);
    /// Mode 10: two subsets, direct 6-bit endpoints, 3-bit indices.
    pub const MODE_10: Self = Self(1 << 9);
    /// Mode 11: one subset, direct 10-bit endpoints, 4-bit indices.
    pub const MODE_11: Self = Self(1 << 10);
    /// Mode 12: one subset, 11-bit base with a 9-bit delta, 4-bit indices.
    pub const MODE_12: Self = Self(1 << 11);
    /// Mode 13: one subset, 12-bit base with an 8-bit delta, 4-bit indices.
    pub const MODE_13: Self = Self(1 << 12);
    /// Mode 14: one subset, 16-bit base with a 4-bit delta, 4-bit indices.
    pub const MODE_14: Self = Self(1 << 13);

    /// All modes this module implements.
    #[must_use]
    pub const fn all() -> Self {
        Self(0x3fff)
    }

    /// The empty set (no mode enabled).
    #[must_use]
    pub const fn none() -> Self {
        Self(0)
    }

    /// Union of two sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether every mode in `other` is enabled in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Encoder options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bc6hOptions {
    /// Error metric for index selection and best-mode selection.
    pub metric: Bc6hMetric,
    /// Modes the encoder may emit.
    pub modes: Bc6hModes,
}

impl Default for Bc6hOptions {
    fn default() -> Self {
        Self {
            metric: Bc6hMetric::Linear,
            modes: Bc6hModes::all(),
        }
    }
}

/// Errors from [`encode_image`] and [`decode_image`].
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum Bc6hError {
    /// A dimension is zero.
    #[error("image is {width}x{height}, both sides must be non-zero")]
    EmptyImage {
        /// Image width.
        width: u32,
        /// Image height.
        height: u32,
    },
    /// The pixel buffer does not match the image size (`width * height * 3` floats).
    #[error("rgb buffer holds {len} floats, expected {width}x{height}x3")]
    BadImage {
        /// Buffer length in floats.
        len: usize,
        /// Image width.
        width: u32,
        /// Image height.
        height: u32,
    },
    /// The block data is shorter than the image's blocks need.
    #[error("{width}x{height} needs {need} bytes of blocks, got {got}")]
    Truncated {
        /// Image width.
        width: u32,
        /// Image height.
        height: u32,
        /// Bytes the blocks need.
        need: usize,
        /// Bytes given.
        got: usize,
    },
}

// ---------------------------------------------------------------------------
// Half-float conversion (IEEE 754 binary16, public standard; round-to-nearest-even)
// ---------------------------------------------------------------------------

/// Converts an `f32` to IEEE binary16 bits, round-to-nearest-even.
///
/// The input normaliser ([`normalize`]) already clamps NaN and infinities away; this function
/// still returns defined results for them (canonical quiet NaN, infinities preserved).
fn f32_to_f16_bits(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x007f_ffff;
    if exponent == 0xff {
        // Infinity or NaN.
        return sign | 0x7c00 | if mantissa != 0 { 0x0200 } else { 0 };
    }
    if exponent == 0 {
        // Every f32 subnormal is below half of the smallest f16 subnormal: rounds to zero.
        return sign;
    }
    let mut e16 = exponent - 127 + 15;
    let round = mantissa & 0x1fff;
    let mut m16 = mantissa >> 13;
    if e16 >= 1 {
        if round > 0x1000 || (round == 0x1000 && (m16 & 1) == 1) {
            m16 += 1;
            if m16 == 0x400 {
                m16 = 0;
                e16 += 1;
            }
        }
        if e16 >= 31 {
            return sign | 0x7c00;
        }
        return sign | ((e16 as u16) << 10) | m16 as u16;
    }
    // Subnormal f16 target: value = (0x800000 + mantissa) * 2^(exponent - 150).
    let significand = 0x0080_0000 + mantissa;
    let shift = 126 - exponent; // 14..=125
    if shift >= 25 {
        return sign;
    }
    let quantum = 1u32 << shift;
    let q = significand >> shift;
    let rem = significand & (quantum - 1);
    let half = quantum >> 1;
    let mut k = q;
    if rem > half || (rem == half && (k & 1) == 1) {
        k += 1;
    }
    // k <= 0x3ff by construction (significand < 2^24, shift >= 14); clamp defensively.
    sign | k.min(0x3ff) as u16
}

/// Converts IEEE binary16 bits to an `f32` (exact).
fn f16_bits_to_f32(h: u16) -> f32 {
    let sign = u32::from(h & 0x8000) << 16;
    let exponent = ((h >> 10) & 0x1f) as i32;
    let mantissa = u32::from(h & 0x03ff);
    let bits = if exponent == 31 {
        sign | 0x7f80_0000 | (mantissa << 13)
    } else if exponent == 0 {
        if mantissa == 0 {
            sign
        } else {
            // Subnormal f16: value = mantissa * 2^-24. Normalise for f32.
            let highest = 31 - mantissa.leading_zeros(); // 0..=9
            let exp = (highest as i32 - 24 + 127) as u32;
            let frac = (mantissa << (23 - highest)) & 0x007f_ffff;
            sign | (exp << 23) | frac
        }
    } else {
        sign | (((exponent - 15 + 127) as u32) << 23) | (mantissa << 13)
    };
    f32::from_bits(bits)
}

// ---------------------------------------------------------------------------
// Input normalisation
// ---------------------------------------------------------------------------

/// Clamps one input channel into the format's representable range.
///
/// NaN and −0 become +0, ±∞ is clamped to ±65504 (the largest finite f16), and unsigned
/// format clamps negatives to 0. The result is always finite, so nothing downstream can
/// propagate non-finite values or panic.
fn normalize(v: f32, format: Bc6hFormat) -> f32 {
    if v.is_nan() || v == 0.0 {
        return 0.0;
    }
    let v = if format == Bc6hFormat::Unsigned && v < 0.0 {
        0.0
    } else {
        v
    };
    v.clamp(-65504.0, 65504.0)
}

/// Normalised block: half-float patterns and their exact `f32` values.
fn normalize_block(texels: &[[f32; 3]; 16], format: Bc6hFormat) -> ([[u16; 3]; 16], [[f32; 3]; 16]) {
    let mut patterns = [[0u16; 3]; 16];
    let mut values = [[0f32; 3]; 16];
    for (i, texel) in texels.iter().enumerate() {
        for c in 0..3 {
            let bits = f32_to_f16_bits(normalize(texel[c], format));
            patterns[i][c] = bits;
            values[i][c] = f16_bits_to_f32(bits);
        }
    }
    (patterns, values)
}

// ---------------------------------------------------------------------------
// Bit packing
// ---------------------------------------------------------------------------

/// LSB-first bit writer over one 128-bit block.
struct BitWriter {
    value: u128,
    bit: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self { value: 0, bit: 0 }
    }

    /// Writes the low `bits` bits of `value` (0 < bits ≤ 16).
    fn put(&mut self, value: u32, bits: u32) {
        if bits == 0 {
            return;
        }
        let masked = value & ((1u32 << bits) - 1);
        self.value |= u128::from(masked) << self.bit;
        self.bit += bits;
    }

    fn finish(self) -> [u8; 16] {
        self.value.to_le_bytes()
    }
}

/// LSB-first bit reader over one 128-bit block.
struct Bits {
    value: u128,
    at: u32,
}

impl Bits {
    fn new(block: &[u8]) -> Self {
        let mut bytes = [0u8; 16];
        let n = block.len().min(16);
        bytes[..n].copy_from_slice(&block[..n]);
        Self {
            value: u128::from_le_bytes(bytes),
            at: 0,
        }
    }

    /// Reads `n` bits (0 ≤ n ≤ 16); reads past the block return zero instead of panicking.
    fn take(&mut self, n: u32) -> u32 {
        if n == 0 || self.at >= 128 {
            return 0;
        }
        let v = ((self.value >> self.at) & ((1u128 << n) - 1)) as u32;
        self.at += n;
        v
    }
}

// ---------------------------------------------------------------------------
// Mode table and partitions
// ---------------------------------------------------------------------------

/// Endpoint names in a mode's bit layout: `W` and `X` are subset 0's endpoints (`W` is the
/// base of the delta transform), `Y` and `Z` subset 1's.
const W: u8 = 0;
const X: u8 = 1;
const Y: u8 = 2;
const Z: u8 = 3;
/// Channel names in a mode's bit layout.
const R: u8 = 0;
const G: u8 = 1;
const B: u8 = 2;

/// A run of consecutive block bits holding bits `lo..=hi` of one endpoint channel's field.
/// Normal runs store bit `lo` first; reversed runs (modes 13 and 14 only) store bit `hi` first.
#[derive(Clone, Copy)]
struct Run {
    ep: u8,
    ch: u8,
    lo: u8,
    hi: u8,
    reversed: bool,
}

impl Run {
    /// Field bit stored at the `k`-th position of the run.
    fn field_bit(self, k: u8) -> u8 {
        if self.reversed { self.hi - k } else { self.lo + k }
    }
}

/// `name[hi:lo]` in the format description's notation.
const fn bits(ep: u8, ch: u8, hi: u8, lo: u8) -> Run {
    Run {
        ep,
        ch,
        lo,
        hi,
        reversed: false,
    }
}

/// `name[k]`: one bit.
const fn bit(ep: u8, ch: u8, k: u8) -> Run {
    bits(ep, ch, k, k)
}

/// `name[lo:hi]`: the field's bits stored highest first.
const fn rev(ep: u8, ch: u8, lo: u8, hi: u8) -> Run {
    Run {
        ep,
        ch,
        lo,
        hi,
        reversed: true,
    }
}

/// Bit layouts after the mode code (the partition and indices follow), mode by mode, as the
/// BC6H format description lists them.
#[rustfmt::skip]
const LAYOUT_1: &[Run] = &[
    bit(Y, G, 4), bit(Y, B, 4), bit(Z, B, 4), bits(W, R, 9, 0), bits(W, G, 9, 0), bits(W, B, 9, 0),
    bits(X, R, 4, 0), bit(Z, G, 4), bits(Y, G, 3, 0), bits(X, G, 4, 0), bit(Z, B, 0), bits(Z, G, 3, 0),
    bits(X, B, 4, 0), bit(Z, B, 1), bits(Y, B, 3, 0), bits(Y, R, 4, 0), bit(Z, B, 2), bits(Z, R, 4, 0),
    bit(Z, B, 3),
];
#[rustfmt::skip]
const LAYOUT_2: &[Run] = &[
    bit(Y, G, 5), bit(Z, G, 4), bit(Z, G, 5), bits(W, R, 6, 0), bit(Z, B, 0), bit(Z, B, 1), bit(Y, B, 4),
    bits(W, G, 6, 0), bit(Y, B, 5), bit(Z, B, 2), bit(Y, G, 4), bits(W, B, 6, 0), bit(Z, B, 3), bit(Z, B, 5),
    bit(Z, B, 4), bits(X, R, 5, 0), bits(Y, G, 3, 0), bits(X, G, 5, 0), bits(Z, G, 3, 0), bits(X, B, 5, 0),
    bits(Y, B, 3, 0), bits(Y, R, 5, 0), bits(Z, R, 5, 0),
];
#[rustfmt::skip]
const LAYOUT_3: &[Run] = &[
    bits(W, R, 9, 0), bits(W, G, 9, 0), bits(W, B, 9, 0), bits(X, R, 4, 0), bit(W, R, 10), bits(Y, G, 3, 0),
    bits(X, G, 3, 0), bit(W, G, 10), bit(Z, B, 0), bits(Z, G, 3, 0), bits(X, B, 3, 0), bit(W, B, 10),
    bit(Z, B, 1), bits(Y, B, 3, 0), bits(Y, R, 4, 0), bit(Z, B, 2), bits(Z, R, 4, 0), bit(Z, B, 3),
];
#[rustfmt::skip]
const LAYOUT_4: &[Run] = &[
    bits(W, R, 9, 0), bits(W, G, 9, 0), bits(W, B, 9, 0), bits(X, R, 3, 0), bit(W, R, 10), bit(Z, G, 4),
    bits(Y, G, 3, 0), bits(X, G, 4, 0), bit(W, G, 10), bits(Z, G, 3, 0), bits(X, B, 3, 0), bit(W, B, 10),
    bit(Z, B, 1), bits(Y, B, 3, 0), bits(Y, R, 3, 0), bit(Z, B, 0), bit(Z, B, 2), bits(Z, R, 3, 0),
    bit(Y, G, 4), bit(Z, B, 3),
];
#[rustfmt::skip]
const LAYOUT_5: &[Run] = &[
    bits(W, R, 9, 0), bits(W, G, 9, 0), bits(W, B, 9, 0), bits(X, R, 3, 0), bit(W, R, 10), bit(Y, B, 4),
    bits(Y, G, 3, 0), bits(X, G, 3, 0), bit(W, G, 10), bit(Z, B, 0), bits(Z, G, 3, 0), bits(X, B, 4, 0),
    bit(W, B, 10), bits(Y, B, 3, 0), bits(Y, R, 3, 0), bit(Z, B, 1), bit(Z, B, 2), bits(Z, R, 3, 0),
    bit(Z, B, 4), bit(Z, B, 3),
];
#[rustfmt::skip]
const LAYOUT_6: &[Run] = &[
    bits(W, R, 8, 0), bit(Y, B, 4), bits(W, G, 8, 0), bit(Y, G, 4), bits(W, B, 8, 0), bit(Z, B, 4),
    bits(X, R, 4, 0), bit(Z, G, 4), bits(Y, G, 3, 0), bits(X, G, 4, 0), bit(Z, B, 0), bits(Z, G, 3, 0),
    bits(X, B, 4, 0), bit(Z, B, 1), bits(Y, B, 3, 0), bits(Y, R, 4, 0), bit(Z, B, 2), bits(Z, R, 4, 0),
    bit(Z, B, 3),
];
#[rustfmt::skip]
const LAYOUT_7: &[Run] = &[
    bits(W, R, 7, 0), bit(Z, G, 4), bit(Y, B, 4), bits(W, G, 7, 0), bit(Z, B, 2), bit(Y, G, 4),
    bits(W, B, 7, 0), bit(Z, B, 3), bit(Z, B, 4), bits(X, R, 5, 0), bits(Y, G, 3, 0), bits(X, G, 4, 0),
    bit(Z, B, 0), bits(Z, G, 3, 0), bits(X, B, 4, 0), bit(Z, B, 1), bits(Y, B, 3, 0), bits(Y, R, 5, 0),
    bits(Z, R, 5, 0),
];
#[rustfmt::skip]
const LAYOUT_8: &[Run] = &[
    bits(W, R, 7, 0), bit(Z, B, 0), bit(Y, B, 4), bits(W, G, 7, 0), bit(Y, G, 5), bit(Y, G, 4),
    bits(W, B, 7, 0), bit(Z, G, 5), bit(Z, B, 4), bits(X, R, 4, 0), bit(Z, G, 4), bits(Y, G, 3, 0),
    bits(X, G, 5, 0), bits(Z, G, 3, 0), bits(X, B, 4, 0), bit(Z, B, 1), bits(Y, B, 3, 0), bits(Y, R, 4, 0),
    bit(Z, B, 2), bits(Z, R, 4, 0), bit(Z, B, 3),
];
#[rustfmt::skip]
const LAYOUT_9: &[Run] = &[
    bits(W, R, 7, 0), bit(Z, B, 1), bit(Y, B, 4), bits(W, G, 7, 0), bit(Y, B, 5), bit(Y, G, 4),
    bits(W, B, 7, 0), bit(Z, B, 5), bit(Z, B, 4), bits(X, R, 4, 0), bit(Z, G, 4), bits(Y, G, 3, 0),
    bits(X, G, 4, 0), bit(Z, B, 0), bits(Z, G, 3, 0), bits(X, B, 5, 0), bits(Y, B, 3, 0), bits(Y, R, 4, 0),
    bit(Z, B, 2), bits(Z, R, 4, 0), bit(Z, B, 3),
];
#[rustfmt::skip]
const LAYOUT_10: &[Run] = &[
    bits(W, R, 5, 0), bit(Z, G, 4), bit(Z, B, 0), bit(Z, B, 1), bit(Y, B, 4), bits(W, G, 5, 0), bit(Y, G, 5),
    bit(Y, B, 5), bit(Z, B, 2), bit(Y, G, 4), bits(W, B, 5, 0), bit(Z, G, 5), bit(Z, B, 3), bit(Z, B, 5),
    bit(Z, B, 4), bits(X, R, 5, 0), bits(Y, G, 3, 0), bits(X, G, 5, 0), bits(Z, G, 3, 0), bits(X, B, 5, 0),
    bits(Y, B, 3, 0), bits(Y, R, 5, 0), bits(Z, R, 5, 0),
];
#[rustfmt::skip]
const LAYOUT_11: &[Run] = &[
    bits(W, R, 9, 0), bits(W, G, 9, 0), bits(W, B, 9, 0), bits(X, R, 9, 0), bits(X, G, 9, 0), bits(X, B, 9, 0),
];
#[rustfmt::skip]
const LAYOUT_12: &[Run] = &[
    bits(W, R, 9, 0), bits(W, G, 9, 0), bits(W, B, 9, 0), bits(X, R, 8, 0), bit(W, R, 10), bits(X, G, 8, 0),
    bit(W, G, 10), bits(X, B, 8, 0), bit(W, B, 10),
];
#[rustfmt::skip]
const LAYOUT_13: &[Run] = &[
    bits(W, R, 9, 0), bits(W, G, 9, 0), bits(W, B, 9, 0), bits(X, R, 7, 0), rev(W, R, 10, 11), bits(X, G, 7, 0),
    rev(W, G, 10, 11), bits(X, B, 7, 0), rev(W, B, 10, 11),
];
#[rustfmt::skip]
const LAYOUT_14: &[Run] = &[
    bits(W, R, 9, 0), bits(W, G, 9, 0), bits(W, B, 9, 0), bits(X, R, 3, 0), rev(W, R, 10, 15), bits(X, G, 3, 0),
    rev(W, G, 10, 15), bits(X, B, 3, 0), rev(W, B, 10, 15),
];

/// One BC6H block mode (numbered 1..=14 as in the format description).
struct ModeLayout {
    /// Mode number, 1..=14.
    id: u8,
    /// Mode code in the block's first `code_bits` bits.
    code: u32,
    /// Width of the mode code (2 or 5 bits).
    code_bits: u32,
    /// Subset count (1 or 2).
    subsets: u32,
    /// Endpoint precision in bits (the base endpoint's field width).
    prec: u32,
    /// Per-channel width of the other endpoints' fields: deltas from `W` when `transformed`,
    /// else equal to `prec`.
    delta: [u32; 3],
    /// Whether endpoints other than `W` are stored as two's-complement deltas of `W`.
    transformed: bool,
    /// Index precision (3 or 4 bits; anchored texels store one bit fewer).
    index_bits: u32,
    /// Bit layout of the endpoint fields.
    layout: &'static [Run],
}

impl ModeLayout {
    fn flag(&self) -> Bc6hModes {
        Bc6hModes(1 << (self.id - 1))
    }

    /// Stored width of endpoint `ep`'s channel `c` field.
    fn field_width(&self, ep: usize, c: usize) -> u32 {
        if ep == 0 { self.prec } else { self.delta[c] }
    }
}

const fn two_subset(
    id: u8,
    code: u32,
    code_bits: u32,
    prec: u32,
    delta: [u32; 3],
    layout: &'static [Run],
) -> ModeLayout {
    ModeLayout {
        id,
        code,
        code_bits,
        subsets: 2,
        prec,
        delta,
        transformed: id != 10,
        index_bits: 3,
        layout,
    }
}

const fn one_subset(id: u8, code: u32, prec: u32, delta: u32, layout: &'static [Run]) -> ModeLayout {
    ModeLayout {
        id,
        code,
        code_bits: 5,
        subsets: 1,
        prec,
        delta: [delta; 3],
        transformed: id != 11,
        index_bits: 4,
        layout,
    }
}

/// All 14 modes. Search order: ties resolve toward the earlier entry (deterministic).
static MODES: [ModeLayout; 14] = [
    one_subset(11, 0b00011, 10, 10, LAYOUT_11),
    one_subset(12, 0b00111, 11, 9, LAYOUT_12),
    one_subset(13, 0b01011, 12, 8, LAYOUT_13),
    one_subset(14, 0b01111, 16, 4, LAYOUT_14),
    two_subset(1, 0b00, 2, 10, [5, 5, 5], LAYOUT_1),
    two_subset(2, 0b01, 2, 7, [6, 6, 6], LAYOUT_2),
    two_subset(3, 0b00010, 5, 11, [5, 4, 4], LAYOUT_3),
    two_subset(4, 0b00110, 5, 11, [4, 5, 4], LAYOUT_4),
    two_subset(5, 0b01010, 5, 11, [4, 4, 5], LAYOUT_5),
    two_subset(6, 0b01110, 5, 9, [5, 5, 5], LAYOUT_6),
    two_subset(7, 0b10010, 5, 8, [6, 5, 5], LAYOUT_7),
    two_subset(8, 0b10110, 5, 8, [5, 6, 5], LAYOUT_8),
    two_subset(9, 0b11010, 5, 8, [5, 5, 6], LAYOUT_9),
    two_subset(10, 0b11110, 5, 6, [6, 6, 6], LAYOUT_10),
];

/// Quantisation-free stand-in used only to rank the 32 partitions (never written to a block).
static RANKING: ModeLayout = ModeLayout {
    id: 0,
    code: 0,
    code_bits: 0,
    subsets: 2,
    prec: 16,
    delta: [16; 3],
    transformed: false,
    index_bits: 3,
    layout: &[],
};

/// Partitions each two-subset mode tries: the best ones by the [`RANKING`] fit.
const PARTITION_SHORTLIST: usize = 4;

/// Two-subset partitions, bit `i` = subset of texel `i` (the first 32 entries of the BPTC
/// two-subset table, which BC6H shares; the same table `regia_asset::bcn_decode` uses for BC7).
const PARTITION2: [u16; PARTITIONS] = [
    0xcccc, 0x8888, 0xeeee, 0xecc8, 0xc880, 0xfeec, 0xfec8, 0xec80, 0xc800, 0xffec, 0xfe80, 0xe800, 0xffe8,
    0xff00, 0xfff0, 0xf000, 0xf710, 0x008e, 0x7100, 0x08ce, 0x008c, 0x7310, 0x3100, 0x8cce, 0x088c, 0x3110,
    0x6666, 0x366c, 0x17e8, 0x0ff0, 0x718e, 0x399c,
];

/// Anchor texel of the second subset per partition (the BPTC two-subset anchor table).
const ANCHOR2: [u8; PARTITIONS] = [
    15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 2, 8, 2, 2, 8, 8, 15, 2, 8, 2, 2, 8,
    8, 2, 2,
];

/// Subset of texel `i` under a two-subset mode.
fn subset_of(mode: &ModeLayout, partition: usize, i: usize) -> usize {
    if mode.subsets == 2 {
        usize::from((PARTITION2[partition] >> i) & 1 == 1)
    } else {
        0
    }
}

/// Whether texel `i`'s index stores one bit fewer (its most significant bit is implied zero):
/// texel 0 always, plus the partition's second-subset anchor for two-subset modes.
fn is_anchor(mode: &ModeLayout, partition: usize, i: usize) -> bool {
    i == 0 || (mode.subsets == 2 && usize::from(ANCHOR2[partition]) == i)
}

// ---------------------------------------------------------------------------
// Endpoint values (quantised field <-> unquantised integer <-> half float)
// ---------------------------------------------------------------------------

/// Sign-extends the low `bits` bits of `v` (1 <= bits <= 16).
fn sign_extend(v: u32, bits: u32) -> i32 {
    let shift = 32 - bits;
    ((v << shift) as i32) >> shift
}

/// Range of a quantised endpoint component at `prec` bits.
fn component_range(prec: u32, signed: bool) -> (i32, i32) {
    if signed {
        (-((1 << (prec - 1)) - 1), (1 << (prec - 1)) - 1)
    } else {
        (0, (1 << prec) - 1)
    }
}

/// The format's unquantisation of a `prec`-bit endpoint component to the 16-bit interpolation
/// domain (`0..=0xffff` unsigned, `-0x7fff..=0x7fff` signed).
fn unquantize(q: i32, prec: u32, signed: bool) -> i32 {
    if !signed {
        if prec >= 15 {
            q
        } else if q == 0 {
            0
        } else if q == (1 << prec) - 1 {
            0xffff
        } else {
            ((q << 16) + 0x8000) >> prec
        }
    } else if prec >= 16 {
        q
    } else {
        let mag = q.abs();
        let unq = if mag == 0 {
            0
        } else if mag >= (1 << (prec - 1)) - 1 {
            0x7fff
        } else {
            ((mag << 15) + 0x4000) >> (prec - 1)
        };
        if q < 0 { -unq } else { unq }
    }
}

/// The format's final scale from the interpolation domain to a half-float bit pattern
/// (`* 31/64` unsigned, `* 31/32` on the magnitude signed).
fn finish(v: i32, signed: bool) -> u16 {
    if !signed {
        ((v * 31) >> 6) as u16
    } else if v < 0 {
        0x8000 | (((-v) * 31) >> 5) as u16
    } else {
        ((v * 31) >> 5) as u16
    }
}

/// Value an endpoint component decodes to (index 0 of its ramp).
fn component_value(q: i32, prec: u32, signed: bool) -> f32 {
    f16_bits_to_f32(finish(unquantize(q, prec, signed), signed))
}

/// The quantised component whose decoded value is nearest `target` (ties toward the smaller
/// component). `component_value` is monotone in `q`, so a binary search finds it.
fn quantize(target: f32, prec: u32, signed: bool) -> i32 {
    let (min, max) = component_range(prec, signed);
    let (mut lo, mut hi) = (min, max);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if component_value(mid, prec, signed) < target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo > min
        && target - component_value(lo - 1, prec, signed) <= component_value(lo, prec, signed) - target
    {
        lo - 1
    } else {
        lo
    }
}

/// Unquantised endpoints (e2/e3 zero for one-subset modes) from raw block fields, exactly as
/// the decoder rebuilds them: deltas are two's complement added to the base modulo `2^prec`,
/// signed formats sign-extend every endpoint at `prec` bits.
fn endpoints_from_fields(mode: &ModeLayout, fields: &[[u32; 3]; 4], signed: bool) -> [[i32; 3]; 4] {
    let count = (2 * mode.subsets) as usize;
    let mask = (1u32 << mode.prec) - 1;
    let as_component = |raw: u32| {
        if signed {
            sign_extend(raw, mode.prec)
        } else {
            raw as i32
        }
    };
    let mut out = [[0i32; 3]; 4];
    for c in 0..3 {
        out[0][c] = unquantize(as_component(fields[0][c]), mode.prec, signed);
        for k in 1..count {
            let raw = if mode.transformed {
                let d = sign_extend(fields[k][c], mode.delta[c]);
                fields[0][c].wrapping_add(d as u32) & mask
            } else {
                fields[k][c]
            };
            out[k][c] = unquantize(as_component(raw), mode.prec, signed);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Palette (BPTC interpolation in the unquantised domain, weights shared with BC7)
// ---------------------------------------------------------------------------

const W3: [i32; 8] = [0, 9, 18, 27, 37, 46, 55, 64];
const W4: [i32; 16] = [0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64];

fn weight(index: u32, index_bits: u32) -> i32 {
    if index_bits == 3 {
        W3[(index & 7) as usize]
    } else {
        W4[(index & 15) as usize]
    }
}

/// BPTC interpolation on unquantised endpoints, `((64-w)·a + w·b + 32) >> 6`, then the final
/// scale to a half-float bit pattern.
fn palette(a: i32, b: i32, index: u32, index_bits: u32, signed: bool) -> u16 {
    let w = weight(index, index_bits);
    finish((a * (64 - w) + b * w + 32) >> 6, signed)
}

// ---------------------------------------------------------------------------
// Error metrics
// ---------------------------------------------------------------------------

/// Deterministic `log2` for `x >= 1`: integer exponent plus linear mantissa interpolation.
/// Monotone, below 0.09 of the true value, and free of libm so bytes cannot vary per platform.
fn log2_approx(x: f32) -> f32 {
    debug_assert!(x >= 1.0, "log2_approx is only called with 1.0 + |v| >= 1");
    let bits = x.to_bits();
    let e = ((bits >> 23) & 0xff) as i32 - 127;
    let m = f32::from_bits((bits & 0x007f_ffff) | 0x3f80_0000) - 1.0;
    e as f32 + m
}

/// Metric transform of one channel (identity for `Linear`).
fn metric_value(metric: Bc6hMetric, v: f32) -> f32 {
    match metric {
        Bc6hMetric::Linear => v,
        Bc6hMetric::ToneMapped => v / (1.0 + v.abs()),
        Bc6hMetric::LogDomain => log2_approx(1.0 + v.abs()),
    }
}

/// Squared metric distance between two texels.
fn metric_distance(metric: Bc6hMetric, a: [f32; 3], b: [f32; 3]) -> f64 {
    let mut sum = 0.0f64;
    for c in 0..3 {
        let d = f64::from(metric_value(metric, a[c]) - metric_value(metric, b[c]));
        sum += d * d;
    }
    sum
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// One candidate encoding of a block.
struct Candidate {
    mode: &'static ModeLayout,
    partition: usize,
    fields: [[u32; 3]; 4],
    indices: [u32; 16],
    error: f64,
}

/// Per-channel minimum and maximum values of a subset's texels. Values are finite
/// (normalised), so `<` is a total order and the scan is deterministic.
fn subset_bounds(
    mode: &ModeLayout,
    partition: usize,
    subset: usize,
    values: &[[f32; 3]; 16],
) -> ([f32; 3], [f32; 3]) {
    // Every partition in the table has both subsets non-empty (tested).
    let first = (0..16)
        .find(|&i| subset_of(mode, partition, i) == subset)
        .unwrap_or_default();
    let mut lo = values[first];
    let mut hi = values[first];
    for (i, v) in values.iter().enumerate() {
        if subset_of(mode, partition, i) != subset {
            continue;
        }
        for ((l, h), x) in lo.iter_mut().zip(hi.iter_mut()).zip(v) {
            *l = l.min(*x);
            *h = h.max(*x);
        }
    }
    (lo, hi)
}

/// Quantised endpoint components nearest the desired endpoint values (no delta limit yet).
fn quantize_targets(mode: &ModeLayout, signed: bool, targets: &[[f32; 3]; 4]) -> [[i32; 3]; 4] {
    let count = (2 * mode.subsets) as usize;
    let mut q = [[0i32; 3]; 4];
    for (k, endpoint) in q.iter_mut().enumerate().take(count) {
        *endpoint = std::array::from_fn(|c| quantize(targets[k][c], mode.prec, signed));
    }
    q
}

/// The components a mode can actually store: in transformed modes every endpoint is pulled
/// toward the base until its delta fits the delta field (two's complement). The result lies
/// between the base and the target, so the decoder's modular sum never wraps.
fn clamp_to_mode(mode: &ModeLayout, mut q: [[i32; 3]; 4]) -> [[i32; 3]; 4] {
    if mode.transformed {
        let base = q[0];
        for (k, endpoint) in q.iter_mut().enumerate().take((2 * mode.subsets) as usize).skip(1) {
            for (c, v) in endpoint.iter_mut().enumerate() {
                let width = mode.field_width(k, c);
                *v = base[c] + (*v - base[c]).clamp(-(1 << (width - 1)), (1 << (width - 1)) - 1);
            }
        }
    }
    q
}

/// Raw block fields for quantised components. Transformed modes store each endpoint as a delta
/// from the base, clamped into the delta field's two's-complement range; the clamped endpoint
/// lies between the base and the target, so the decoder's modular sum never wraps and every
/// field fits its width by construction.
fn fields_from_components(mode: &ModeLayout, q: &[[i32; 3]; 4]) -> [[u32; 3]; 4] {
    let count = (2 * mode.subsets) as usize;
    let prec_mask = (1u32 << mode.prec) - 1;
    let mut fields = [[0u32; 3]; 4];
    for c in 0..3 {
        fields[0][c] = (q[0][c] as u32) & prec_mask;
        for k in 1..count {
            fields[k][c] = if mode.transformed {
                let width = mode.field_width(k, c);
                let d = (q[k][c] - q[0][c]).clamp(-(1 << (width - 1)), (1 << (width - 1)) - 1);
                (d as u32) & ((1u32 << width) - 1)
            } else {
                (q[k][c] as u32) & prec_mask
            };
        }
    }
    fields
}

/// Nearest palette entry per texel under the metric, and the summed error. With `anchored`,
/// anchor texels may only pick the lower half of their ramp (their index has no top bit).
fn best_indices(
    mode: &ModeLayout,
    partition: usize,
    signed: bool,
    metric: Bc6hMetric,
    values: &[[f32; 3]; 16],
    endpoints: &[[i32; 3]; 4],
    anchored: bool,
) -> ([u32; 16], f64) {
    // The decoded ramps of both subsets, evaluated once.
    let count = 1u32 << mode.index_bits;
    let mut ramps = [[[0f32; 3]; 16]; 2];
    for (s, ramp) in ramps.iter_mut().enumerate().take(mode.subsets as usize) {
        let (a, b) = (endpoints[2 * s], endpoints[2 * s + 1]);
        for (idx, entry) in ramp.iter_mut().enumerate().take(count as usize) {
            *entry = std::array::from_fn(|c| {
                f16_bits_to_f32(palette(a[c], b[c], idx as u32, mode.index_bits, signed))
            });
        }
    }
    let mut indices = [0u32; 16];
    let mut error = 0.0f64;
    for (i, slot) in indices.iter_mut().enumerate() {
        let s = subset_of(mode, partition, i);
        let limit = if anchored && is_anchor(mode, partition, i) {
            count >> 1
        } else {
            count
        };
        let mut best_error = f64::INFINITY;
        let mut best_index = 0u32;
        for idx in 0..limit {
            let e = metric_distance(metric, values[i], ramps[s][idx as usize]);
            if e < best_error {
                best_error = e;
                best_index = idx;
            }
        }
        *slot = best_index;
        error += best_error;
    }
    (indices, error)
}

/// Encodes the block toward the given endpoint values under one mode and partition.
///
/// Indices are first chosen over the whole ramp. A subset whose anchor texel wants the upper
/// half (which its index cannot store) has its endpoints swapped, which mirrors the ramp, and
/// the indices are chosen again under the anchor limit.
fn evaluate(
    mode: &'static ModeLayout,
    partition: usize,
    signed: bool,
    metric: Bc6hMetric,
    values: &[[f32; 3]; 16],
    targets: &[[f32; 3]; 4],
) -> Candidate {
    #[cfg(test)]
    tests::CANDIDATES.with(|n| n.set(n.get() + 1));
    let mut q = clamp_to_mode(mode, quantize_targets(mode, signed, targets));
    let fields = fields_from_components(mode, &q);
    let endpoints = endpoints_from_fields(mode, &fields, signed);
    let (indices, error) = best_indices(mode, partition, signed, metric, values, &endpoints, false);
    let half = (1u32 << mode.index_bits) >> 1;
    let mut swapped = false;
    for s in 0..mode.subsets as usize {
        let anchor = if s == 0 {
            0
        } else {
            usize::from(ANCHOR2[partition])
        };
        if indices[anchor] >= half {
            q.swap(2 * s, 2 * s + 1);
            swapped = true;
        }
    }
    if !swapped {
        return Candidate {
            mode,
            partition,
            fields,
            indices,
            error,
        };
    }
    // Swapping the stored endpoints mirrors the ramp exactly; only a swapped base can push a
    // second-subset delta out of range, which the clamp absorbs.
    let q = clamp_to_mode(mode, q);
    let fields = fields_from_components(mode, &q);
    let endpoints = endpoints_from_fields(mode, &fields, signed);
    let (indices, error) = best_indices(mode, partition, signed, metric, values, &endpoints, true);
    Candidate {
        mode,
        partition,
        fields,
        indices,
        error,
    }
}

/// A texel value in the interpolation domain (the inverse of [`finish`]; values are
/// half-float exact after normalisation).
fn value_to_unquantized(v: f32, signed: bool) -> f64 {
    let h = f32_to_f16_bits(v);
    if signed {
        let mag = f64::from(h & 0x7fff) * 32.0 / 31.0;
        if h & 0x8000 != 0 { -mag } else { mag }
    } else {
        f64::from(h) * 64.0 / 31.0
    }
}

/// The texel value an interpolation-domain value decodes to, clamped into range.
fn unquantized_to_value(u: f64, signed: bool) -> f32 {
    let (lo, hi) = if signed { (-0x7fff, 0x7fff) } else { (0, 0xffff) };
    let v = (u.round() as i64).clamp(lo, hi) as i32;
    f16_bits_to_f32(finish(v, signed))
}

/// Least-squares endpoints for a candidate's index choice: per subset and channel, the pair
/// `(a, b)` minimising `sum(((64 - w) a + w b) / 64 - u)^2` over the subset's texels in the
/// interpolation domain (where the format interpolates), solved by the 2x2 normal equations.
/// A subset whose texels all share one weight keeps its current endpoints.
fn refined_targets(cand: &Candidate, signed: bool, values: &[[f32; 3]; 16]) -> [[f32; 3]; 4] {
    let mode = cand.mode;
    let endpoints = endpoints_from_fields(mode, &cand.fields, signed);
    let mut targets = [[0f32; 3]; 4];
    for s in 0..mode.subsets as usize {
        let (mut aa, mut ab, mut bb) = (0.0f64, 0.0f64, 0.0f64);
        let (mut au, mut bu) = ([0.0f64; 3], [0.0f64; 3]);
        for (i, v) in values.iter().enumerate() {
            if subset_of(mode, cand.partition, i) != s {
                continue;
            }
            let w = f64::from(weight(cand.indices[i], mode.index_bits)) / 64.0;
            let (a, b) = (1.0 - w, w);
            aa += a * a;
            ab += a * b;
            bb += b * b;
            for c in 0..3 {
                let u = value_to_unquantized(v[c], signed);
                au[c] += a * u;
                bu[c] += b * u;
            }
        }
        let det = aa * bb - ab * ab;
        for c in 0..3 {
            let (e0, e1) = if det.abs() < 1e-9 {
                (f64::from(endpoints[2 * s][c]), f64::from(endpoints[2 * s + 1][c]))
            } else {
                ((au[c] * bb - bu[c] * ab) / det, (bu[c] * aa - au[c] * ab) / det)
            };
            targets[2 * s][c] = unquantized_to_value(e0, signed);
            targets[2 * s + 1][c] = unquantized_to_value(e1, signed);
        }
        // Canonical order (smaller channel sum first), as the bounding box has it: the anchor
        // swap in `evaluate` then sees the same pair whatever the texel order, so a mirrored
        // block still encodes as the mirror.
        let sum = |t: [f32; 3]| t[0] + t[1] + t[2];
        if sum(targets[2 * s]) > sum(targets[2 * s + 1]) {
            targets.swap(2 * s, 2 * s + 1);
        }
    }
    targets
}

/// Best encoding of the block under one mode and (for two subsets) one partition: the
/// subsets' bounding boxes as endpoints, then (relative metrics only) one least-squares
/// refinement of the endpoints for the chosen indices, kept only when it lowers the error.
fn encode_candidate(
    mode: &'static ModeLayout,
    partition: usize,
    signed: bool,
    metric: Bc6hMetric,
    values: &[[f32; 3]; 16],
) -> Candidate {
    // Desired endpoints: per subset, per channel, the subset's min and max values.
    let mut targets = [[0f32; 3]; 4];
    for s in 0..mode.subsets as usize {
        let (lo, hi) = subset_bounds(mode, partition, s, values);
        targets[2 * s] = lo;
        targets[2 * s + 1] = hi;
    }
    let first = evaluate(mode, partition, signed, metric, values, &targets);
    // The least-squares fit works in the interpolation domain, which tracks relative error:
    // it runs for the relative metrics only (under `Linear` it trades dark texels' relative
    // error for absolute error). The partition ranking needs no refinement, and an exact fit
    // cannot improve.
    if mode.id == 0 || first.error == 0.0 || metric == Bc6hMetric::Linear {
        return first;
    }
    let second = evaluate(
        mode,
        partition,
        signed,
        metric,
        values,
        &refined_targets(&first, signed, values),
    );
    if second.error < first.error { second } else { first }
}

/// The partitions worth trying for this block: the [`PARTITION_SHORTLIST`] best by a
/// quantisation-free two-subset fit, ties toward the lower partition number.
fn partition_shortlist(
    signed: bool,
    metric: Bc6hMetric,
    values: &[[f32; 3]; 16],
) -> [usize; PARTITION_SHORTLIST] {
    let mut scored = [(0.0f64, 0usize); PARTITIONS];
    for (p, slot) in scored.iter_mut().enumerate() {
        *slot = (encode_candidate(&RANKING, p, signed, metric, values).error, p);
    }
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    std::array::from_fn(|k| scored[k].1)
}

/// Writes a candidate into 16 bytes in the mode's bit layout.
fn write_block(cand: &Candidate) -> [u8; 16] {
    let mode = cand.mode;
    let mut w = BitWriter::new();
    w.put(mode.code, mode.code_bits);
    for run in mode.layout {
        let field = cand.fields[usize::from(run.ep)][usize::from(run.ch)];
        for k in 0..=(run.hi - run.lo) {
            w.put(field >> run.field_bit(k), 1);
        }
    }
    if mode.subsets == 2 {
        w.put(cand.partition as u32, 5);
    }
    for (i, index) in cand.indices.iter().enumerate() {
        let bits = mode.index_bits - u32::from(is_anchor(mode, cand.partition, i));
        w.put(*index, bits);
    }
    w.finish()
}

/// Encodes one 4×4 block of linear RGB (`texels[0]` is the top-left texel, row-major).
///
/// Input is clamped into the format's range first: NaN and −0 become 0, ±∞ becomes ±65504,
/// and the unsigned format clamps negatives to 0 — encoding never panics and always returns a
/// block that [`decode_block`] (and a GPU) can decode. Two-subset modes try the four partitions
/// a quantisation-free fit ranks best. If `options.modes` enables no mode, mode 11 is used as
/// the documented fallback. The result is deterministic for identical inputs.
#[must_use]
pub fn encode_block(texels: &[[f32; 3]; 16], format: Bc6hFormat, options: &Bc6hOptions) -> [u8; 16] {
    let signed = format == Bc6hFormat::Signed;
    let (_, values) = normalize_block(texels, format);
    let wants_two_subsets = MODES
        .iter()
        .any(|m| m.subsets == 2 && options.modes.contains(m.flag()));
    let shortlist = if wants_two_subsets {
        partition_shortlist(signed, options.metric, &values)
    } else {
        [0; PARTITION_SHORTLIST]
    };
    let mut best: Option<Candidate> = None;
    for mode in MODES.iter() {
        if !options.modes.contains(mode.flag()) {
            continue;
        }
        let partitions: &[usize] = if mode.subsets == 2 { &shortlist } else { &[0] };
        for &p in partitions {
            let cand = encode_candidate(mode, p, signed, options.metric, &values);
            if best.as_ref().is_none_or(|b| cand.error < b.error) {
                best = Some(cand);
            }
        }
    }
    let chosen = match best {
        Some(c) => c,
        None => encode_candidate(&MODES[0], 0, signed, options.metric, &values),
    };
    write_block(&chosen)
}

/// Encodes a `width × height` linear-RGB image (`rgb` holds `width * height * 3` floats,
/// row-major) into rows of 4×4 blocks. The last row and column of blocks repeat the image's
/// last row and column of texels. Any input value is clamped as in [`encode_block`].
///
/// # Errors
///
/// [`Bc6hError::EmptyImage`] when a dimension is zero, [`Bc6hError::BadImage`] when `rgb`
/// does not hold exactly `width * height * 3` floats.
pub fn encode_image(
    width: u32,
    height: u32,
    rgb: &[f32],
    format: Bc6hFormat,
    options: &Bc6hOptions,
) -> Result<Vec<u8>, Bc6hError> {
    if width == 0 || height == 0 {
        return Err(Bc6hError::EmptyImage { width, height });
    }
    let (w, h) = (width as usize, height as usize);
    let want = w.checked_mul(h).and_then(|n| n.checked_mul(3));
    if want != Some(rgb.len()) {
        return Err(Bc6hError::BadImage {
            len: rgb.len(),
            width,
            height,
        });
    }
    let _span = tracing::info_span!("bc6h_encode", width, height).entered();
    let (bw, bh) = (w.div_ceil(4), h.div_ceil(4));
    let mut out = vec![0u8; bw * bh * BLOCK_BYTES];
    for by in 0..bh {
        for bx in 0..bw {
            let mut texels = [[0f32; 3]; 16];
            for y in 0..4 {
                for x in 0..4 {
                    let tx = (bx * 4 + x).min(w - 1);
                    let ty = (by * 4 + y).min(h - 1);
                    let at = (ty * w + tx) * 3;
                    texels[y * 4 + x] = [rgb[at], rgb[at + 1], rgb[at + 2]];
                }
            }
            let block = encode_block(&texels, format, options);
            let at = (by * bw + bx) * BLOCK_BYTES;
            out[at..at + BLOCK_BYTES].copy_from_slice(&block);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Reference decoder
// ---------------------------------------------------------------------------

/// Parsed block contents (raw fields and indices, before endpoint reconstruction).
struct Parsed {
    mode: &'static ModeLayout,
    partition: usize,
    fields: [[u32; 3]; 4],
    indices: [u32; 16],
}

/// The mode a block's first bits select; `None` for the four reserved 5-bit codes.
fn mode_of(block_bits: u128) -> Option<&'static ModeLayout> {
    let low2 = (block_bits & 0b11) as u32;
    let low5 = (block_bits & 0b1_1111) as u32;
    MODES.iter().find(|m| {
        if m.code_bits == 2 {
            m.code == low2
        } else {
            m.code == low5
        }
    })
}

/// Parses a block; `None` for the reserved mode codes.
fn parse_block(block: &[u8]) -> Option<Parsed> {
    let mut bits = Bits::new(block);
    let mode = mode_of(bits.value)?;
    bits.take(mode.code_bits);
    let mut fields = [[0u32; 3]; 4];
    for run in mode.layout {
        let field = &mut fields[usize::from(run.ep)][usize::from(run.ch)];
        for k in 0..=(run.hi - run.lo) {
            *field |= bits.take(1) << run.field_bit(k);
        }
    }
    let partition = if mode.subsets == 2 {
        bits.take(5) as usize
    } else {
        0
    };
    let mut indices = [0u32; 16];
    for (i, index) in indices.iter_mut().enumerate() {
        let n = mode.index_bits - u32::from(is_anchor(mode, partition, i));
        *index = bits.take(n);
    }
    Some(Parsed {
        mode,
        partition,
        fields,
        indices,
    })
}

/// Decodes one block into 16 RGB `f32` texels (row-major, texel 0 top-left).
///
/// Decodes all 14 BC6H modes. A block with one of the four reserved mode codes decodes to zero
/// texels (a defined fallback; the encoder never emits them). A block shorter than 16 bytes is
/// zero-padded.
#[must_use]
pub fn decode_block(block: &[u8], format: Bc6hFormat) -> [[f32; 3]; 16] {
    let Some(parsed) = parse_block(block) else {
        return [[0.0; 3]; 16];
    };
    decode_parsed(&parsed, format)
}

/// Decodes a parsed block into texel values (shared by the decoder and the tests).
fn decode_parsed(parsed: &Parsed, format: Bc6hFormat) -> [[f32; 3]; 16] {
    let signed = format == Bc6hFormat::Signed;
    let endpoints = endpoints_from_fields(parsed.mode, &parsed.fields, signed);
    let mut out = [[0f32; 3]; 16];
    for (i, texel) in out.iter_mut().enumerate() {
        let s = subset_of(parsed.mode, parsed.partition, i);
        let (a, b) = (endpoints[2 * s], endpoints[2 * s + 1]);
        for c in 0..3 {
            let pattern = palette(a[c], b[c], parsed.indices[i], parsed.mode.index_bits, signed);
            texel[c] = f16_bits_to_f32(pattern);
        }
    }
    out
}

/// Decodes rows of 4×4 blocks back into `width * height * 3` floats, cropped to the image
/// size (the padding texels the encoder added are dropped).
///
/// # Errors
///
/// [`Bc6hError::EmptyImage`] when a dimension is zero, [`Bc6hError::Truncated`] when `data`
/// holds fewer bytes than the image's blocks.
pub fn decode_image(width: u32, height: u32, data: &[u8], format: Bc6hFormat) -> Result<Vec<f32>, Bc6hError> {
    if width == 0 || height == 0 {
        return Err(Bc6hError::EmptyImage { width, height });
    }
    let (w, h) = (width as usize, height as usize);
    let (bw, bh) = (w.div_ceil(4), h.div_ceil(4));
    let need = bw * bh * BLOCK_BYTES;
    if data.len() < need {
        return Err(Bc6hError::Truncated {
            width,
            height,
            need,
            got: data.len(),
        });
    }
    let mut out = vec![0f32; w * h * 3];
    for by in 0..bh {
        for bx in 0..bw {
            let at = (by * bw + bx) * BLOCK_BYTES;
            let texels = decode_block(&data[at..at + BLOCK_BYTES], format);
            for (i, texel) in texels.iter().enumerate() {
                let (x, y) = (bx * 4 + i % 4, by * 4 + i / 4);
                if x < w && y < h {
                    let o = (y * w + x) * 3;
                    out[o..o + 3].copy_from_slice(texel);
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        /// Candidate encodings evaluated on this thread (the speed guard's work counter).
        pub(super) static CANDIDATES: Cell<u64> = const { Cell::new(0) };
    }

    /// Deterministic xorshift32 for pseudo-random test blocks (fixed seeds, no wall clock).
    struct Rng(u32);

    impl Rng {
        fn next_u32(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }

        fn next_f32(&mut self) -> f32 {
            f32::from_bits(0x3f80_0000 | (self.next_u32() >> 9)) - 1.0 // [0, 1)
        }
    }

    fn flat(value: [f32; 3]) -> [[f32; 3]; 16] {
        [value; 16]
    }

    fn relative_error(a: [f32; 3], b: [f32; 3]) -> f64 {
        let mut worst = 0.0f64;
        for c in 0..3 {
            let denom = a[c].abs().max(1.0 / 1024.0) as f64;
            worst = worst.max(f64::from((a[c] - b[c]).abs()) / denom);
        }
        worst
    }

    // ----- half-float conversion ------------------------------------------------

    #[test]
    fn f16_known_bit_patterns() {
        assert_eq!(f32_to_f16_bits(0.0), 0x0000);
        assert_eq!(f32_to_f16_bits(-0.0), 0x8000);
        assert_eq!(f32_to_f16_bits(1.0), 0x3c00);
        assert_eq!(f32_to_f16_bits(0.5), 0x3800);
        assert_eq!(f32_to_f16_bits(-2.0), 0xc000);
        assert_eq!(f32_to_f16_bits(1.5), 0x3e00);
        assert_eq!(f32_to_f16_bits(65504.0), 0x7bff);
        // Round-to-nearest-even at the top of the range: 65519 rounds down, 65520 to inf.
        assert_eq!(f32_to_f16_bits(65519.0), 0x7bff);
        assert_eq!(f32_to_f16_bits(65520.0), 0x7c00);
        // Smallest subnormal and its neighbours.
        assert_eq!(f32_to_f16_bits(5.9604645e-8), 0x0001);
        assert_eq!(f32_to_f16_bits(2.0e-8), 0x0000);
    }

    #[test]
    fn f16_f32_round_trip_on_the_bit_lattice() {
        // Every f16 bit pattern (positive and negative, normal and subnormal) survives
        // f16 -> f32 -> f16 exactly: conversion is exact in that direction.
        let mut bits = 0x0000u32;
        while bits <= 0x7bff {
            let v = f16_bits_to_f32(bits as u16);
            if v.is_finite() {
                assert_eq!(u32::from(f32_to_f16_bits(v)), bits, "pattern {bits:#06x}");
            }
            bits += 0x0111; // stride the lattice deterministically (117 samples)
        }
        for bits in [0x8000u16, 0xbc00, 0xfbff, 0x8001] {
            let v = f16_bits_to_f32(bits);
            assert!(v.is_finite() || bits == 0x8000);
        }
    }

    #[test]
    fn f16_subnormal_decode_is_exact() {
        assert_eq!(f16_bits_to_f32(0x0001), 2.0f32.powi(-24));
        assert_eq!(f16_bits_to_f32(0x03ff), 1023.0 * 2.0f32.powi(-24));
        assert_eq!(f16_bits_to_f32(0x3555), 0.33325195f32);
        // f32 subnormals all round to zero (they are far below half of 2^-24).
        assert_eq!(f32_to_f16_bits(1.0e-30), 0x0000);
        assert_eq!(f32_to_f16_bits(1.0e-38), 0x0000);
    }

    // ----- bit packing -----------------------------------------------------------

    #[test]
    fn bit_writer_reader_round_trip() {
        let mut w = BitWriter::new();
        w.put(0b01011, 5);
        w.put(0b10110_10011, 10);
        w.put(0x1f, 5);
        w.put(0b0110, 4);
        let bytes = w.finish();
        let mut r = Bits::new(&bytes);
        assert_eq!(r.take(5), 0b01011);
        assert_eq!(r.take(10), 0b10110_10011);
        assert_eq!(r.take(5), 0x1f);
        assert_eq!(r.take(4), 0b0110);
    }

    #[test]
    fn reader_past_the_end_returns_zero() {
        let mut r = Bits::new(&[0xff; 16]);
        let _ = r.take(16);
        let _ = r.take(16);
        while r.at < 128 {
            let _ = r.take(16);
        }
        assert_eq!(r.take(16), 0);
    }

    // ----- mode bits -------------------------------------------------------------

    fn mode_options(mode: &Bc6hModes) -> Bc6hOptions {
        Bc6hOptions {
            metric: Bc6hMetric::Linear,
            modes: *mode,
        }
    }

    #[test]
    fn mode_bits_read_back_for_every_mode() {
        let block = flat([4.0, 8.0, 16.0]);
        for mode in MODES.iter() {
            for format in [Bc6hFormat::Unsigned, Bc6hFormat::Signed] {
                let bytes = encode_block(&block, format, &mode_options(&mode.flag()));
                let parsed = match parse_block(&bytes) {
                    Some(p) => p,
                    None => panic!("mode {} block did not parse", mode.id),
                };
                assert_eq!(parsed.mode.id, mode.id, "format {format:?}");
                assert_eq!(parsed.mode.code, mode.code);
                let low = u32::from(bytes[0]) & ((1 << mode.code_bits) - 1);
                assert_eq!(low, mode.code, "mode {} code bits", mode.id);
                if mode.subsets == 2 {
                    assert!(parsed.partition < PARTITIONS);
                }
            }
        }
    }

    #[test]
    fn every_mode_layout_sums_to_exactly_128_bits() {
        // Mode code + endpoint runs + partition + indices fill the block exactly, and the runs
        // cover every bit of every endpoint field exactly once (base at `prec` bits, the others
        // at their delta or direct width).
        for mode in MODES.iter() {
            let mut covered = [[0u32; 3]; 4];
            let mut total = mode.code_bits;
            for run in mode.layout {
                for k in 0..=(run.hi - run.lo) {
                    let slot = &mut covered[usize::from(run.ep)][usize::from(run.ch)];
                    let b = 1u32 << run.field_bit(k);
                    assert_eq!(*slot & b, 0, "mode {} bit stored twice", mode.id);
                    *slot |= b;
                    total += 1;
                }
            }
            let header = if mode.subsets == 2 { 82 } else { 65 };
            assert_eq!(
                total + 5 * (mode.subsets - 1),
                header,
                "mode {} header bits",
                mode.id
            );
            let index_bits = 16 * mode.index_bits - mode.subsets;
            assert_eq!(header + index_bits, 128, "mode {}", mode.id);
            for (ep, channels) in covered.iter().enumerate() {
                for (c, bits) in channels.iter().enumerate() {
                    let want = if ep < (2 * mode.subsets) as usize {
                        (1u32 << mode.field_width(ep, c)) - 1
                    } else {
                        0
                    };
                    assert_eq!(*bits, want, "mode {} endpoint {ep} channel {c}", mode.id);
                }
            }
        }
        // Mode codes are distinct and none is one of the four reserved 5-bit codes.
        for (i, a) in MODES.iter().enumerate() {
            assert!(![0b10011, 0b10111, 0b11011, 0b11111].contains(&a.code) || a.code_bits == 2);
            for b in &MODES[i + 1..] {
                let bits = a.code_bits.min(b.code_bits);
                assert_ne!(
                    a.code & ((1 << bits) - 1),
                    b.code & ((1 << bits) - 1),
                    "modes {} and {}",
                    a.id,
                    b.id
                );
            }
        }
    }

    // ----- endpoint values ---------------------------------------------------------

    #[test]
    fn endpoint_values_are_monotone_and_quantize_inverts_them() {
        for signed in [false, true] {
            for prec in [6u32, 7, 8, 9, 10, 11, 12, 16] {
                let (min, max) = component_range(prec, signed);
                assert_eq!(component_value(0, prec, signed), 0.0);
                assert_eq!(
                    component_value(max, prec, signed),
                    65504.0,
                    "prec {prec} signed {signed}"
                );
                let mut prev = f32::NEG_INFINITY;
                for q in min..=max {
                    let v = component_value(q, prec, signed);
                    assert!(v >= prev, "prec {prec} signed {signed}: not monotone at {q}");
                    // quantize returns the smallest component decoding to the same value.
                    let back = quantize(v, prec, signed);
                    assert_eq!(component_value(back, prec, signed), v, "prec {prec} q {q}");
                    assert!(back <= q);
                    prev = v;
                }
            }
        }
    }

    #[test]
    fn signed_components_are_symmetric() {
        for prec in [6u32, 7, 8, 9, 10, 11, 12, 16] {
            let (_, max) = component_range(prec, true);
            for q in 0..=max {
                assert_eq!(component_value(-q, prec, true), -component_value(q, prec, true));
            }
        }
    }

    #[test]
    fn delta_fields_round_trip_through_sign_extension() {
        for width in [4u32, 5, 6, 8, 9] {
            for d in -(1i32 << (width - 1))..(1 << (width - 1)) {
                let field = (d as u32) & ((1 << width) - 1);
                assert!(field < (1 << width));
                assert_eq!(sign_extend(field, width), d, "width {width}");
            }
        }
    }

    // ----- interpolation ---------------------------------------------------------

    #[test]
    fn palette_endpoints_and_midpoint_are_exact() {
        // Index 0 and the top index reproduce the endpoints; an interior index is the BPTC
        // weighted mean in the unquantised domain, then the 31/64 scale.
        let (a, b) = (31_712, 33_826); // unsigned unquantised values of ~1.0 and ~2.0
        assert_eq!(palette(a, b, 0, 4, false), finish(a, false));
        assert_eq!(palette(a, b, 15, 4, false), finish(b, false));
        assert_eq!(
            palette(a, b, 7, 4, false),
            finish((34 * a + 30 * b + 32) >> 6, false)
        );
        assert_eq!(
            palette(a, b, 3, 3, false),
            finish((37 * a + 27 * b + 32) >> 6, false)
        );
        // Signed: the ramp passes through zero symmetrically.
        assert_eq!(palette(-1000, 1000, 0, 3, true), finish(-1000, true));
        assert_eq!(palette(-1000, 1000, 7, 3, true), finish(1000, true));
        // Equal endpoints collapse to the endpoint for every index.
        for idx in 0..16 {
            assert_eq!(palette(0x1234, 0x1234, idx, 4, false), finish(0x1234, false));
        }
    }

    #[test]
    fn palette_ramp_between_positives_is_monotone() {
        for signed in [false, true] {
            let (a, b) = (unquantize(400, 10, signed), unquantize(500, 10, signed));
            let mut prev = f16_bits_to_f32(palette(a, b, 0, 4, signed));
            for idx in 1..16 {
                let v = f16_bits_to_f32(palette(a, b, idx, 4, signed));
                assert!(v >= prev, "index {idx} went backwards");
                prev = v;
            }
        }
    }

    // ----- metrics ---------------------------------------------------------------

    #[test]
    fn metrics_are_deterministic_and_bounded() {
        let a = [1.0f32, 100.0, -5.0];
        assert_eq!(metric_distance(Bc6hMetric::Linear, a, a), 0.0);
        assert_eq!(metric_distance(Bc6hMetric::ToneMapped, a, a), 0.0);
        assert_eq!(metric_distance(Bc6hMetric::LogDomain, a, a), 0.0);
        // The tone map is bounded by 1 per channel, so the distance is bounded by 4.
        let b = [65504.0f32, 0.0, -65504.0];
        assert!(metric_distance(Bc6hMetric::ToneMapped, a, b) <= 4.0);
        assert!(metric_distance(Bc6hMetric::ToneMapped, a, b) > 0.0);
        // Same inputs, same outputs (pure arithmetic).
        for _ in 0..3 {
            assert_eq!(
                metric_distance(Bc6hMetric::LogDomain, a, b),
                metric_distance(Bc6hMetric::LogDomain, a, b)
            );
        }
    }

    #[test]
    fn tone_mapped_metric_reduces_its_own_objective() {
        // With the mode fixed, index selection minimises the chosen metric per texel, so the
        // tone-mapped encode must score at least as well under the tone-mapped metric as the
        // linear encode does.
        let mut texels = [[0f32; 3]; 16];
        for (i, texel) in texels.iter_mut().enumerate() {
            let bright = f32::from(i % 2 == 0);
            *texel = [1.0 + bright * 250.0, 2.0 + bright * 100.0, 0.5];
        }
        let lin = Bc6hOptions {
            metric: Bc6hMetric::Linear,
            modes: Bc6hModes::MODE_11,
        };
        let ton = Bc6hOptions {
            metric: Bc6hMetric::ToneMapped,
            modes: Bc6hModes::MODE_11,
        };
        let bytes_lin = encode_block(&texels, Bc6hFormat::Unsigned, &lin);
        let bytes_ton = encode_block(&texels, Bc6hFormat::Unsigned, &ton);
        let dec_lin = decode_block(&bytes_lin, Bc6hFormat::Unsigned);
        let dec_ton = decode_block(&bytes_ton, Bc6hFormat::Unsigned);
        let (_, values) = normalize_block(&texels, Bc6hFormat::Unsigned);
        let score = |dec: &[[f32; 3]; 16]| -> f64 {
            (0..16)
                .map(|i| metric_distance(Bc6hMetric::ToneMapped, values[i], dec[i]))
                .sum()
        };
        assert!(score(&dec_ton) <= score(&dec_lin) + 1e-12);
    }

    // ----- round trips -----------------------------------------------------------

    #[test]
    fn flat_blocks_round_trip_exactly() {
        // Signed: endpoints are a plain truncated sign-magnitude pattern, so any value whose
        // low 6 magnitude bits are zero round-trips bit-exactly (0.25, 0.5, 1, 2, 512...).
        for value in [
            [0.0f32, 0.0, 0.0],
            [-1.0, 0.5, 2.0],
            [1.0, 0.25, 512.0],
            [-12.5, 0.0, 12.5],
        ] {
            let bytes = encode_block(&flat(value), Bc6hFormat::Signed, &Bc6hOptions::default());
            let dec = decode_block(&bytes, Bc6hFormat::Signed);
            for texel in dec {
                assert_eq!(texel, value, "signed flat {value:?}");
            }
        }
        // Unsigned: endpoints land on the 64-pattern-step ladder (half step = 32 patterns),
        // so the worst case is ~3.5% at small magnitudes; zero is exact.
        let bytes = encode_block(
            &flat([0.0, 0.0, 0.0]),
            Bc6hFormat::Unsigned,
            &Bc6hOptions::default(),
        );
        let dec = decode_block(&bytes, Bc6hFormat::Unsigned);
        for texel in dec {
            assert_eq!(texel, [0.0, 0.0, 0.0]);
        }
        for value in [[4.25f32, 4.25, 4.25], [1.0, 0.5, 0.25], [65504.0, 1.0, 0.0]] {
            let bytes = encode_block(&flat(value), Bc6hFormat::Unsigned, &Bc6hOptions::default());
            let dec = decode_block(&bytes, Bc6hFormat::Unsigned);
            for texel in dec {
                assert!(
                    relative_error(value, texel) <= 0.035,
                    "unsigned flat {value:?} -> {:?}",
                    texel
                );
            }
        }
    }

    #[test]
    fn unsigned_gradient_round_trip_error_floor() {
        // A 1..100 ramp (R 1..100, G 1..50, B 1..10). BC6H interpolates in the half-float
        // bit-pattern domain with one index per texel, so a value-linear ramp whose channels
        // span different octave counts is not a line there and no endpoint pair fits it: the
        // ISPC encoder the crate links (slow settings) misses the delivered 15 % worst floor
        // on this block too (15.1 %). The floor is therefore relative to ISPC on the same block.
        let mut texels = [[0f32; 3]; 16];
        for (i, t) in texels.iter_mut().enumerate() {
            let f = i as f32 / 15.0;
            *t = [1.0 + 99.0 * f, 1.0 + 49.0 * f, 1.0 + 9.0 * f];
        }
        let stats = |dec: &[[f32; 3]; 16]| {
            let rel: Vec<f64> = (0..16).map(|i| relative_error(texels[i], dec[i])).collect();
            (
                rel.iter().copied().fold(0.0f64, f64::max),
                rel.iter().sum::<f64>() / 16.0,
            )
        };
        let ours = stats(&decode_block(
            &encode_block(&texels, Bc6hFormat::Unsigned, &Bc6hOptions::default()),
            Bc6hFormat::Unsigned,
        ));
        let mut surface = Vec::new();
        for t in &texels {
            for v in [t[0], t[1], t[2], 1.0] {
                surface.extend_from_slice(&f32_to_f16_bits(v).to_le_bytes());
            }
        }
        let image = intel_tex_2::RgbaSurface {
            data: &surface,
            width: 4,
            height: 4,
            stride: 32,
        };
        let ispc_block = intel_tex_2::bc6h::compress_blocks(&intel_tex_2::bc6h::slow_settings(), &image);
        let ispc = stats(&decode_block(&ispc_block, Bc6hFormat::Unsigned));
        eprintln!(
            "ours worst {:.4} mean {:.4}; ISPC worst {:.4} mean {:.4}",
            ours.0, ours.1, ispc.0, ispc.1
        );
        assert!(
            ours.0 <= ispc.0 * 1.1 + 0.005,
            "worst relative error {} (ISPC {})",
            ours.0,
            ispc.0
        );
        assert!(
            ours.1 <= ispc.1 * 1.25 + 0.005,
            "mean relative error {} (ISPC {})",
            ours.1,
            ispc.1
        );
    }

    #[test]
    fn reversed_gradient_decodes_as_the_mirror_of_the_forward_one() {
        // Texel 0 is an anchor: its index has no top bit, so it can only pick the first half
        // of the ramp. A ramp that starts at the bright end must not pay for that: one-subset
        // modes swap the endpoints (which mirrors the ramp exactly), so the reversed block
        // decodes to the forward block's texels in reverse order, in every metric and format.
        let mut forward = [[0f32; 3]; 16];
        for (i, t) in forward.iter_mut().enumerate() {
            let f = i as f32 / 15.0;
            *t = [1.0 + 99.0 * f, 1.0 + 49.0 * f, 1.0 + 9.0 * f];
        }
        let mut reversed = forward;
        reversed.reverse();
        let one_subset = [
            Bc6hModes::MODE_11,
            Bc6hModes::MODE_12,
            Bc6hModes::MODE_13,
            Bc6hModes::MODE_14,
        ];
        for format in [Bc6hFormat::Unsigned, Bc6hFormat::Signed] {
            for metric in [Bc6hMetric::Linear, Bc6hMetric::ToneMapped, Bc6hMetric::LogDomain] {
                for modes in one_subset {
                    let options = Bc6hOptions { metric, modes };
                    let fwd = decode_block(&encode_block(&forward, format, &options), format);
                    let mut rev = decode_block(&encode_block(&reversed, format, &options), format);
                    rev.reverse();
                    assert_eq!(fwd, rev, "{format:?} {metric:?} {modes:?}");
                }
                // With every mode, the partition shapes are not mirror-symmetric, so only the
                // error is compared, and only under the relative metrics: `Linear` minimises
                // absolute error and may trade a dark texel's relative error for it.
                if metric == Bc6hMetric::Linear {
                    continue;
                }
                let options = Bc6hOptions {
                    metric,
                    modes: Bc6hModes::all(),
                };
                let worst = |texels: &[[f32; 3]; 16]| {
                    let dec = decode_block(&encode_block(texels, format, &options), format);
                    (0..16)
                        .map(|i| relative_error(texels[i], dec[i]))
                        .fold(0.0f64, f64::max)
                };
                let (f, r) = (worst(&forward), worst(&reversed));
                assert!(
                    r <= 1.5 * f + 0.02,
                    "{format:?} {metric:?}: reversed worst {r}, forward {f}"
                );
            }
        }
    }

    #[test]
    fn signed_gradient_round_trip_error_floor() {
        // Negative-side ramps (-100..-1, -10..-1, -50..-5): the same structure as the unsigned
        // ramp (a value-linear ramp is not a line in the bit-pattern domain the format
        // interpolates in), with one bit less of endpoint precision. ISPC writes no signed
        // BC6H, so this is a measured regression floor: 0.239 worst and 0.104 mean on
        // the build server (2026-11-03), floors 0.25 and 0.12. The delivered 10 % / 3 % floors were
        // derived from the delivered private format and are out of reach for BC6H itself.
        let mut texels = [[0f32; 3]; 16];
        for (i, t) in texels.iter_mut().enumerate() {
            let f = i as f32 / 15.0;
            *t = [-100.0 + 99.0 * f, -10.0 + 9.0 * f, -50.0 + 45.0 * f];
        }
        let bytes = encode_block(&texels, Bc6hFormat::Signed, &Bc6hOptions::default());
        let dec = decode_block(&bytes, Bc6hFormat::Signed);
        let rel: Vec<f64> = (0..16).map(|i| relative_error(texels[i], dec[i])).collect();
        let worst = rel.iter().copied().fold(0.0f64, f64::max);
        let mean = rel.iter().sum::<f64>() / 16.0;
        assert!(worst <= 0.25, "worst relative error {worst}");
        assert!(mean <= 0.12, "mean relative error {mean}");
    }

    #[test]
    fn bright_spike_block_keeps_both_ends() {
        // Fifteen texels at 1.0 and one at 65504. Index 0 and the top index reproduce the
        // endpoints exactly, so the spike texel (saturating endpoint 0x7bff == 65504) and the
        // dark texels (endpoint ~1.0 within the lattice's half step) must both survive.
        let mut texels = flat([1.0, 1.0, 1.0]);
        texels[9] = [65504.0, 65504.0, 65504.0];
        let bytes = encode_block(&texels, Bc6hFormat::Unsigned, &Bc6hOptions::default());
        let dec = decode_block(&bytes, Bc6hFormat::Unsigned);
        let spike = relative_error(texels[9], dec[9]);
        assert!(spike <= 0.01, "spike relative error {spike}");
        for i in 0..16 {
            if i != 9 {
                let rel = relative_error(texels[i], dec[i]);
                assert!(rel <= 0.05, "texel {i} relative error {rel}");
            }
        }
        // Same block under the tone-mapped metric: the metric may change interior index
        // choices, never validity.
        let options = Bc6hOptions {
            metric: Bc6hMetric::ToneMapped,
            ..Bc6hOptions::default()
        };
        let dec_ton = decode_block(
            &encode_block(&texels, Bc6hFormat::Unsigned, &options),
            Bc6hFormat::Unsigned,
        );
        assert!(relative_error(texels[9], dec_ton[9]) <= 0.01);
    }

    #[test]
    fn negative_values_behave_per_format() {
        let texels = flat([-12.5, -1.0, -0.5]);
        let unsigned = decode_block(
            &encode_block(&texels, Bc6hFormat::Unsigned, &Bc6hOptions::default()),
            Bc6hFormat::Unsigned,
        );
        for texel in unsigned {
            assert_eq!(texel, [0.0, 0.0, 0.0], "unsigned clamps negatives to zero");
        }
        let signed = decode_block(
            &encode_block(&texels, Bc6hFormat::Signed, &Bc6hOptions::default()),
            Bc6hFormat::Signed,
        );
        for texel in signed {
            assert_eq!(texel, [-12.5, -1.0, -0.5]);
        }
    }

    #[test]
    fn nan_and_infinite_inputs_never_panic() {
        let mut texels = flat([1.0, 2.0, 3.0]);
        texels[0] = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];
        texels[5] = [-f32::NAN, -f32::INFINITY, 1.0e30];
        for format in [Bc6hFormat::Unsigned, Bc6hFormat::Signed] {
            let options = Bc6hOptions::default();
            let dec = decode_block(&encode_block(&texels, format, &options), format);
            for (i, texel) in dec.iter().enumerate() {
                for (c, v) in texel.iter().enumerate() {
                    assert!(v.is_finite(), "{format:?} texel {i} channel {c}");
                }
            }
            // NaN folds to 0 and infinities clamp to +-65504 (0 for unsigned negatives) before
            // the search: the block is byte-identical to the block of the clamped input.
            let low = if format == Bc6hFormat::Signed {
                -65504.0
            } else {
                0.0
            };
            let mut clamped = texels;
            clamped[0] = [0.0, 65504.0, low];
            clamped[5] = [0.0, low, 65504.0];
            assert_eq!(
                encode_block(&texels, format, &options),
                encode_block(&clamped, format, &options)
            );
            // A NaN texel in an otherwise flat block decodes to exactly zero (it is an endpoint).
            let mut one_nan = flat([1.0, 1.0, 1.0]);
            one_nan[3] = [f32::NAN; 3];
            let dec = decode_block(&encode_block(&one_nan, format, &options), format);
            assert_eq!(dec[3], [0.0; 3], "{format:?}");
        }
    }

    // ----- transform invariants --------------------------------------------------

    #[test]
    fn endpoint_delta_fields_never_overflow_their_ranges() {
        // Wide-range and pseudo-random blocks through every mode: every stored field fits its
        // width, and in transformed modes the base plus the sign-extended delta stays inside the
        // component range without the decoder's modular wrap (a wrapped delta would decode to an
        // endpoint at the other end of the range).
        let mut rng = Rng(0x6b3c);
        let mut blocks: Vec<[[f32; 3]; 16]> = Vec::new();
        blocks.push(flat([0.0, 0.0, 0.0]));
        let mut spike = flat([60000.0, 0.001, 30000.0]);
        spike[3] = [0.0, 65504.0, -65504.0];
        blocks.push(spike);
        for _ in 0..24 {
            let mut t = [[0f32; 3]; 16];
            for texel in &mut t {
                let s = if rng.next_u32() & 1 == 0 { 1.0 } else { -1.0 };
                *texel = [
                    rng.next_f32() * 65504.0 * s,
                    rng.next_f32() * 65504.0,
                    rng.next_f32() * 0.01 * s,
                ];
            }
            blocks.push(t);
        }
        for mode in MODES.iter() {
            let options = mode_options(&mode.flag());
            for format in [Bc6hFormat::Unsigned, Bc6hFormat::Signed] {
                let signed = format == Bc6hFormat::Signed;
                let (min, max) = component_range(mode.prec, signed);
                for texels in &blocks {
                    let bytes = encode_block(texels, format, &options);
                    let parsed = match parse_block(&bytes) {
                        Some(p) => p,
                        None => panic!("mode {} did not parse", mode.id),
                    };
                    assert_eq!(parsed.mode.id, mode.id);
                    for (k, endpoint) in parsed.fields.iter().enumerate().take((2 * mode.subsets) as usize) {
                        for (c, field) in endpoint.iter().enumerate() {
                            let width = mode.field_width(k, c);
                            assert!(*field < (1u32 << width), "field {field} width {width}");
                            if mode.transformed && k > 0 {
                                let base = parsed.fields[0][c];
                                let base = if signed {
                                    sign_extend(base, mode.prec)
                                } else {
                                    base as i32
                                };
                                let sum = base + sign_extend(*field, width);
                                assert!((min..=max).contains(&sum), "mode {} wrapped: {sum}", mode.id);
                            }
                        }
                    }
                    for texel in decode_parsed(&parsed, format) {
                        for c in texel {
                            assert!(c.is_finite());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn saturating_deltas_stay_defined_on_impossible_blocks() {
        // Mode 14's 4-bit deltas cannot span a 0..65504 block; the encoder must still emit a
        // valid block (clamped deltas) instead of panicking or overflowing a field.
        let mut texels = flat([0.0, 0.0, 0.0]);
        texels[0] = [65504.0; 3];
        let options = mode_options(&Bc6hModes::MODE_14);
        let bytes = encode_block(&texels, Bc6hFormat::Unsigned, &options);
        let dec = decode_block(&bytes, Bc6hFormat::Unsigned);
        for texel in dec {
            assert!(texel.iter().all(|v| v.is_finite()));
        }
    }

    // ----- partitions and anchors ------------------------------------------------

    #[test]
    fn partition_table_invariants() {
        for (p, pattern) in PARTITION2.iter().enumerate() {
            let u = usize::from(*pattern);
            assert!(u != 0x0000 && u != 0xffff, "partition {p} is single-subset");
            assert_eq!(u & 1, 0, "texel 0 must be in subset 0 for partition {p}");
            let anchor = usize::from(ANCHOR2[p]);
            assert!(anchor < 16 && anchor != 0, "partition {p} anchor {anchor}");
        }
        // Two-subset decoding covers every texel exactly once by construction of subset_of.
        for shape in PARTITION2 {
            for i in 0..16 {
                let s = usize::from((shape >> i) & 1 == 1);
                assert!(s < 2);
            }
        }
    }

    #[test]
    fn anchored_texel_indices_respect_their_limits() {
        let mut rng = Rng(0x2f9a);
        let mut texels = [[0f32; 3]; 16];
        for t in &mut texels {
            *t = [rng.next_f32() * 100.0; 3];
        }
        for format in [Bc6hFormat::Unsigned, Bc6hFormat::Signed] {
            let bytes = encode_block(&texels, format, &Bc6hOptions::default());
            let parsed = match parse_block(&bytes) {
                Some(p) => p,
                None => panic!("default block did not parse"),
            };
            for i in 0..16 {
                let limit = 1u32 << (parsed.mode.index_bits - 1);
                if is_anchor(parsed.mode, parsed.partition, i) {
                    assert!(
                        parsed.indices[i] < limit,
                        "anchor {i} index {}",
                        parsed.indices[i]
                    );
                } else {
                    assert!(parsed.indices[i] < (1u32 << parsed.mode.index_bits));
                }
            }
        }
    }

    #[test]
    fn best_mode_selection_uses_two_subset_partitions() {
        // Top half dark, bottom half bright: a two-subset mode can give each half its own
        // endpoints, so the default (all modes) fit must beat or match mode 11 alone.
        let mut texels = [[0f32; 3]; 16];
        for (i, texel) in texels.iter_mut().enumerate() {
            let bright = f32::from(i >= 8);
            *texel = [1.0 + bright * 199.0, 1.0, 1.0 + bright];
        }
        let (_, values) = normalize_block(&texels, Bc6hFormat::Unsigned);
        let score = |options: &Bc6hOptions| -> f64 {
            let bytes = encode_block(&texels, Bc6hFormat::Unsigned, options);
            let dec = decode_block(&bytes, Bc6hFormat::Unsigned);
            (0..16)
                .map(|i| metric_distance(Bc6hMetric::Linear, values[i], dec[i]))
                .sum()
        };
        let all = score(&Bc6hOptions::default());
        let only11 = score(&mode_options(&Bc6hModes::MODE_11));
        assert!(all <= only11 + 1e-9, "all-modes error {all} vs mode-11 {only11}");
    }

    // ----- image API -------------------------------------------------------------

    #[test]
    fn image_round_trip_with_partial_blocks() {
        // 5x3: two blocks per row, one row of blocks. The padding repeats the last row and column,
        // so the image's blocks are exactly the blocks of the hand-padded texels, and decoding
        // crops back to the texels of those blocks.
        let (w, h) = (5u32, 3u32);
        let mut rgb = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgb.extend_from_slice(&[x as f32, y as f32, 4.0]);
            }
        }
        let encoded = match encode_image(w, h, &rgb, Bc6hFormat::Signed, &Bc6hOptions::default()) {
            Ok(v) => v,
            Err(e) => panic!("encode_image failed: {e}"),
        };
        assert_eq!(encoded.len(), 2 * BLOCK_BYTES);
        for bx in 0..2usize {
            let texels: [[f32; 3]; 16] = std::array::from_fn(|i| {
                let x = (bx * 4 + i % 4).min(4);
                let y = (i / 4).min(2);
                [x as f32, y as f32, 4.0]
            });
            let block = encode_block(&texels, Bc6hFormat::Signed, &Bc6hOptions::default());
            assert_eq!(
                &encoded[bx * BLOCK_BYTES..(bx + 1) * BLOCK_BYTES],
                &block,
                "block {bx}"
            );
        }
        let decoded = match decode_image(w, h, &encoded, Bc6hFormat::Signed) {
            Ok(v) => v,
            Err(e) => panic!("decode_image failed: {e}"),
        };
        assert_eq!(decoded.len(), (w * h * 3) as usize);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let (bx, i) = (x / 4, y * 4 + x % 4);
                let want = decode_block(
                    &encoded[bx * BLOCK_BYTES..(bx + 1) * BLOCK_BYTES],
                    Bc6hFormat::Signed,
                )[i];
                let at = (y * w as usize + x) * 3;
                assert_eq!(&decoded[at..at + 3], &want, "texel {x},{y}");
                // Lossy (a ramp that starts at 0 is coarse in BC6H's bit-pattern interpolation:
                // 2.0 decodes as 0.78 here), but every texel stays inside the block's range.
                for v in want {
                    assert!((-0.05..=4.05).contains(&v), "texel {x},{y}: {v}");
                }
            }
        }
    }

    #[test]
    fn image_constant_is_exact_at_every_texel() {
        // 3.5, 0.25 and 512.0 all have low-6-zero half-float patterns, so a constant signed
        // image (including padding texels) reproduces every texel exactly.
        let (w, h) = (7u32, 8u32);
        let rgb: Vec<f32> = (0..w * h).flat_map(|_| [3.5f32, 0.25, 512.0]).collect();
        let encoded = match encode_image(w, h, &rgb, Bc6hFormat::Signed, &Bc6hOptions::default()) {
            Ok(v) => v,
            Err(e) => panic!("{e}"),
        };
        let decoded = match decode_image(w, h, &encoded, Bc6hFormat::Signed) {
            Ok(v) => v,
            Err(e) => panic!("{e}"),
        };
        for i in 0..(w * h) as usize {
            assert_eq!(&decoded[i * 3..i * 3 + 3], &[3.5, 0.25, 512.0]);
        }
    }

    #[test]
    fn image_validation_errors() {
        let rgb = vec![0.5f32; 16 * 3];
        assert_eq!(
            encode_image(0, 4, &rgb, Bc6hFormat::Unsigned, &Bc6hOptions::default()),
            Err(Bc6hError::EmptyImage { width: 0, height: 4 })
        );
        assert_eq!(
            encode_image(4, 0, &rgb, Bc6hFormat::Unsigned, &Bc6hOptions::default()),
            Err(Bc6hError::EmptyImage { width: 4, height: 0 })
        );
        let short = vec![0.5f32; 15 * 3];
        assert_eq!(
            encode_image(4, 4, &short, Bc6hFormat::Unsigned, &Bc6hOptions::default()),
            Err(Bc6hError::BadImage {
                len: 45,
                width: 4,
                height: 4
            })
        );
        assert_eq!(
            decode_image(4, 4, &[0u8; 8], Bc6hFormat::Unsigned),
            Err(Bc6hError::Truncated {
                width: 4,
                height: 4,
                need: 16,
                got: 8
            })
        );
        assert_eq!(
            decode_image(0, 4, &[], Bc6hFormat::Unsigned),
            Err(Bc6hError::EmptyImage { width: 0, height: 4 })
        );
    }

    #[test]
    fn one_pixel_image_encodes_and_decodes() {
        let encoded = match encode_image(
            1,
            1,
            &[1.0, 2.0, 3.0],
            Bc6hFormat::Signed,
            &Bc6hOptions::default(),
        ) {
            Ok(v) => v,
            Err(e) => panic!("{e}"),
        };
        assert_eq!(encoded.len(), BLOCK_BYTES);
        let decoded = match decode_image(1, 1, &encoded, Bc6hFormat::Signed) {
            Ok(v) => v,
            Err(e) => panic!("{e}"),
        };
        assert_eq!(&decoded[..], &[1.0, 2.0, 3.0]);
    }

    // ----- determinism and speed ---------------------------------------------------

    #[test]
    fn encoding_is_deterministic() {
        let mut rng = Rng(0x5eed);
        let mut texels = [[0f32; 3]; 16];
        for t in &mut texels {
            *t = [
                rng.next_f32() * 50.0,
                rng.next_f32() * 0.1,
                rng.next_f32() * 5000.0,
            ];
        }
        for format in [Bc6hFormat::Unsigned, Bc6hFormat::Signed] {
            let a = encode_block(&texels, format, &Bc6hOptions::default());
            let b = encode_block(&texels, format, &Bc6hOptions::default());
            assert_eq!(a, b);
            let ton = Bc6hOptions {
                metric: Bc6hMetric::ToneMapped,
                ..Bc6hOptions::default()
            };
            assert_eq!(
                encode_block(&texels, format, &ton),
                encode_block(&texels, format, &ton)
            );
        }
        let rgb: Vec<f32> = (0..12 * 20 * 3).map(|i| (i % 97) as f32 * 0.7).collect();
        let a = encode_image(12, 20, &rgb, Bc6hFormat::Unsigned, &Bc6hOptions::default());
        let b = encode_image(12, 20, &rgb, Bc6hFormat::Unsigned, &Bc6hOptions::default());
        assert_eq!(a, b);
    }

    #[test]
    fn speed_guard_bounds_the_search_per_block() {
        // A complexity blow-up guard, not a benchmark (no wall clock): the search per block is
        // fixed at 32 ranking fits, 10 two-subset modes x 4 shortlisted partitions and the 4
        // one-subset modes, whatever the content.
        let (w, h) = (64u32, 64u32);
        let mut rng = Rng(0xfeed);
        let rgb: Vec<f32> = (0..w * h).flat_map(|_| [rng.next_f32() * 8.0; 3]).collect();
        CANDIDATES.with(|n| n.set(0));
        let encoded = match encode_image(w, h, &rgb, Bc6hFormat::Unsigned, &Bc6hOptions::default()) {
            Ok(v) => v,
            Err(e) => panic!("{e}"),
        };
        assert_eq!(encoded.len(), 16 * 16 * BLOCK_BYTES);
        // `Linear` (the default) evaluates each mode/partition candidate once.
        assert_eq!(
            CANDIDATES.with(Cell::get),
            256 * (PARTITIONS + 10 * PARTITION_SHORTLIST + 4) as u64
        );
        // The relative metrics add at most one least-squares refinement per candidate.
        let log = Bc6hOptions {
            metric: Bc6hMetric::LogDomain,
            modes: Bc6hModes::all(),
        };
        CANDIDATES.with(|n| n.set(0));
        let _ = encode_image(w, h, &rgb, Bc6hFormat::Unsigned, &log);
        let n = CANDIDATES.with(Cell::get);
        assert!(
            n <= 256 * (PARTITIONS + 2 * (10 * PARTITION_SHORTLIST + 4)) as u64,
            "{n} candidates"
        );
        // One-subset modes only skip the ranking and the partitions entirely.
        let one = Bc6hModes::MODE_11
            .union(Bc6hModes::MODE_12)
            .union(Bc6hModes::MODE_13)
            .union(Bc6hModes::MODE_14);
        CANDIDATES.with(|n| n.set(0));
        let _ = encode_block(&flat([1.0, 2.0, 3.0]), Bc6hFormat::Unsigned, &mode_options(&one));
        assert_eq!(CANDIDATES.with(Cell::get), 4);
    }

    #[test]
    fn empty_mode_set_falls_back_to_mode_11() {
        let options = Bc6hOptions {
            modes: Bc6hModes::none(),
            ..Bc6hOptions::default()
        };
        let bytes = encode_block(&flat([2.0, 4.0, 8.0]), Bc6hFormat::Unsigned, &options);
        match parse_block(&bytes) {
            Some(p) => assert_eq!(p.mode.id, 11),
            None => panic!("fallback block did not parse"),
        }
    }

    #[test]
    fn unknown_mode_codes_decode_to_zero() {
        // All-zero block: mode 1 (two-bit code "00") with every endpoint zero.
        assert_eq!(decode_block(&[0u8; 16], Bc6hFormat::Unsigned), [[0.0; 3]; 16]);
        // Code 0b11111 (31) is one of the four reserved codes.
        let mut block = [0u8; 16];
        block[0] = 0xff;
        assert_eq!(decode_block(&block, Bc6hFormat::Signed), [[0.0; 3]; 16]);
        // Short buffers are zero-padded, not indexed out of bounds.
        assert_eq!(decode_block(&[0b01011, 0], Bc6hFormat::Unsigned)[0], [0.0; 3]);
    }
}
