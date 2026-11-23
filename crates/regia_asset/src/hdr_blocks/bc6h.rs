// SPDX-License-Identifier: MPL-2.0
//! BC6H unsigned-float blocks, the one-region modes (Khronos Data Format Specification, "BC6H").
//!
//! A one-region block is a 5-bit mode, 60 endpoint bits and 63 index bits (the anchor texel 0
//! has an implied zero top bit). Endpoint 0 (`w`) is stored at the mode's precision; endpoint 1
//! (`x`) is stored at the same precision (mode 11) or as a signed delta from `w` (modes 12 to 14,
//! "transformed"), wrapped to the precision. Unquantized endpoints are 16-bit values that
//! interpolate with the 4-bit weights; `(c · 31) >> 6` is the half float.

use super::{f16_bits_to_f32, f32_to_f16_bits, fit_line, least_squares};

/// Interpolation weights of 4-bit indices (out of 64).
const WEIGHTS: [i32; 16] = [0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64];

/// The one-region modes this encoder writes (numbered as in the spec's mode table).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Bc6hMode {
    /// Mode 11: both endpoints at 10 bits.
    Mode11,
    /// Mode 12: an 11-bit base and 9-bit deltas.
    Mode12,
    /// Mode 13: a 12-bit base and 8-bit deltas.
    Mode13,
    /// Mode 14: a 16-bit base and 4-bit deltas.
    Mode14,
}

/// Endpoint fields in stream order: `(field, first bit stored, last bit stored)`; field
/// `0..3` = `w.rgb`, `3..6` = `x.rgb`. A field whose first bit is above its last is stored with its
/// bits reversed (the spec's `rw[10:15]` notation).
type Layout = [(usize, u8, u8)];

const LAYOUT_11: &Layout = &[(0, 0, 9), (1, 0, 9), (2, 0, 9), (3, 0, 9), (4, 0, 9), (5, 0, 9)];
const LAYOUT_12: &Layout = &[
    (0, 0, 9),
    (1, 0, 9),
    (2, 0, 9),
    (3, 0, 8),
    (0, 10, 10),
    (4, 0, 8),
    (1, 10, 10),
    (5, 0, 8),
    (2, 10, 10),
];
const LAYOUT_13: &Layout = &[
    (0, 0, 9),
    (1, 0, 9),
    (2, 0, 9),
    (3, 0, 7),
    (0, 11, 10),
    (4, 0, 7),
    (1, 11, 10),
    (5, 0, 7),
    (2, 11, 10),
];
const LAYOUT_14: &Layout = &[
    (0, 0, 9),
    (1, 0, 9),
    (2, 0, 9),
    (3, 0, 3),
    (0, 15, 10),
    (4, 0, 3),
    (1, 15, 10),
    (5, 0, 3),
    (2, 15, 10),
];

impl Bc6hMode {
    /// Every mode, in the order the encoder tries them.
    pub const ALL: [Self; 4] = [Self::Mode11, Self::Mode12, Self::Mode13, Self::Mode14];

    /// The 5-bit mode value in the block's low bits.
    fn code(self) -> u32 {
        match self {
            Self::Mode11 => 0b00011,
            Self::Mode12 => 0b00111,
            Self::Mode13 => 0b01011,
            Self::Mode14 => 0b01111,
        }
    }

    fn from_code(code: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.code() == code)
    }

    /// Endpoint precision, bits.
    fn precision(self) -> u32 {
        match self {
            Self::Mode11 => 10,
            Self::Mode12 => 11,
            Self::Mode13 => 12,
            Self::Mode14 => 16,
        }
    }

    /// Bits of the signed delta of endpoint 1 (`None`: stored at full precision).
    fn delta_bits(self) -> Option<u32> {
        match self {
            Self::Mode11 => None,
            Self::Mode12 => Some(9),
            Self::Mode13 => Some(8),
            Self::Mode14 => Some(4),
        }
    }

    fn layout(self) -> &'static Layout {
        match self {
            Self::Mode11 => LAYOUT_11,
            Self::Mode12 => LAYOUT_12,
            Self::Mode13 => LAYOUT_13,
            Self::Mode14 => LAYOUT_14,
        }
    }
}

/// The bits of one field segment in the order they are stored.
fn segment_bits(first: u8, last: u8) -> impl Iterator<Item = u32> {
    let (first, last) = (u32::from(first), u32::from(last));
    let up = first <= last;
    let n = first.abs_diff(last) + 1;
    (0..n).map(move |k| if up { first + k } else { first - k })
}

/// Packs a block: `stored` are the raw endpoint fields (`x` as the masked delta in modes 12-14).
fn pack(mode: Bc6hMode, stored: [[u32; 3]; 2], indices: &[u8; 16]) -> [u8; 16] {
    let mut b = u128::from(mode.code());
    let mut pos = 5u32;
    for &(field, first, last) in mode.layout() {
        let v = stored[field / 3][field % 3];
        for bit in segment_bits(first, last) {
            b |= u128::from((v >> bit) & 1) << pos;
            pos += 1;
        }
    }
    debug_assert_eq!(pos, 65);
    for (k, &i) in indices.iter().enumerate() {
        let bits = if k == 0 { 3 } else { 4 };
        b |= u128::from(i & ((1 << bits) - 1)) << pos;
        pos += bits;
    }
    b.to_le_bytes()
}

fn unpack(block: &[u8; 16]) -> Option<(Bc6hMode, [[u32; 3]; 2], [u8; 16])> {
    let b = u128::from_le_bytes(*block);
    let mode = Bc6hMode::from_code((b & 0x1F) as u32)?;
    let mut stored = [[0u32; 3]; 2];
    let mut pos = 5u32;
    for &(field, first, last) in mode.layout() {
        for bit in segment_bits(first, last) {
            stored[field / 3][field % 3] |= (((b >> pos) & 1) as u32) << bit;
            pos += 1;
        }
    }
    let mut indices = [0u8; 16];
    for (k, i) in indices.iter_mut().enumerate() {
        let bits = if k == 0 { 3 } else { 4 };
        *i = ((b >> pos) & ((1 << bits) - 1)) as u8;
        pos += bits;
    }
    Some((mode, stored, indices))
}

/// The spec's unsigned unquantization of a `prec`-bit endpoint to 16 bits.
fn unquantize(q: i32, prec: u32) -> i32 {
    if prec >= 15 {
        q
    } else if q == 0 {
        0
    } else if q == (1 << prec) - 1 {
        0xFFFF
    } else {
        ((q << 16) + 0x8000) >> prec
    }
}

/// Sign-extends the low `bits` bits of `v`.
fn sign_extend(v: u32, bits: u32) -> i32 {
    let shift = 32 - bits;
    ((v << shift) as i32) >> shift
}

/// The two endpoints at full precision from the stored fields.
fn endpoints(mode: Bc6hMode, stored: [[u32; 3]; 2]) -> [[i32; 3]; 2] {
    let prec = mode.precision();
    let mask = (1i32 << prec) - 1;
    let w = stored[0].map(|v| v as i32);
    let x = match mode.delta_bits() {
        None => stored[1].map(|v| v as i32),
        Some(bits) => [0, 1, 2].map(|c| (w[c] + sign_extend(stored[1][c], bits)) & mask),
    };
    [w, x]
}

/// The 16 half-float bit patterns of a palette between two unquantized endpoints.
fn palette(u: [[i32; 3]; 2]) -> [[i32; 3]; 16] {
    WEIGHTS.map(|wt| [0, 1, 2].map(|c| (((u[0][c] * (64 - wt) + u[1][c] * wt + 32) >> 6) * 31) >> 6))
}

/// Decodes a block written by [`encode_bc6h_block`] (any of its four modes) to linear RGB;
/// `None` for any other mode.
#[must_use]
pub fn decode_bc6h_block(block: &[u8; 16]) -> Option<[[f32; 3]; 16]> {
    let (mode, stored, indices) = unpack(block)?;
    let prec = mode.precision();
    let u = endpoints(mode, stored).map(|e| e.map(|q| unquantize(q, prec)));
    let pal = palette(u);
    Some(indices.map(|i| pal[usize::from(i)].map(|h| f16_bits_to_f32(h as u16))))
}

/// The nearest `prec`-bit endpoint for a 16-bit target value.
fn quantize(t: f32, prec: u32) -> i32 {
    let max = (1i32 << prec) - 1;
    if prec >= 15 {
        return (t.round() as i32).clamp(0, max);
    }
    let guess = (t / (1u32 << (16 - prec)) as f32 - 0.5).round() as i32;
    (guess - 1..=guess + 1)
        .map(|q| q.clamp(0, max))
        .min_by(|&a, &b| {
            let da = (unquantize(a, prec) as f32 - t).abs();
            let db = (unquantize(b, prec) as f32 - t).abs();
            da.total_cmp(&db)
        })
        .unwrap_or(0)
}

/// Encodes `e0, e1` (16-bit domain) in `mode`, choosing the indices; returns the block and its
/// squared error over the masked texels (half-float units).
fn encode_endpoints(
    mode: Bc6hMode,
    e: [[f32; 3]; 2],
    target: &[[f32; 3]; 16],
    mask: &[bool; 16],
) -> ([u8; 16], f64, [u8; 16]) {
    let prec = mode.precision();
    let mut q = e.map(|p| p.map(|v| quantize(v, prec)));
    let stored_for = |q: [[i32; 3]; 2]| -> [[u32; 3]; 2] {
        match mode.delta_bits() {
            None => q.map(|p| p.map(|v| v as u32)),
            Some(bits) => {
                let (lo, hi) = (-(1i32 << (bits - 1)), (1i32 << (bits - 1)) - 1);
                let d = [0, 1, 2].map(|c| ((q[1][c] - q[0][c]).clamp(lo, hi) as u32) & ((1 << bits) - 1));
                [q[0].map(|v| v as u32), d]
            }
        }
    };
    let choose = |stored: [[u32; 3]; 2]| -> ([u8; 16], f64) {
        let u = endpoints(mode, stored).map(|p| p.map(|v| unquantize(v, prec)));
        let pal = palette(u);
        let mut idx = [0u8; 16];
        let mut err = 0.0f64;
        for (k, (t, m)) in target.iter().zip(mask).enumerate() {
            let range = if k == 0 { 0..8 } else { 0..16 };
            let (best, e) = range
                .map(|i| {
                    let d: f64 = (0..3).map(|c| f64::from(pal[i][c] as f32 - t[c]).powi(2)).sum();
                    (i, d)
                })
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .unwrap_or((0, 0.0));
            idx[k] = best as u8;
            if *m {
                err += e;
            }
        }
        (idx, err)
    };
    let stored = stored_for(q);
    let (idx, err) = choose(stored);
    // The anchor texel only reaches the first half of the palette: try the endpoints swapped.
    q.swap(0, 1);
    let swapped = stored_for(q);
    let (idx2, err2) = choose(swapped);
    if err2 < err {
        (pack(mode, swapped, &idx2), err2, idx2)
    } else {
        (pack(mode, stored, &idx), err, idx)
    }
}

/// Encodes 16 texels (row-major) in one mode (for tests that hold a decoder to each mode).
#[must_use]
pub fn encode_bc6h_block_mode(texels: &[[f32; 3]; 16], mask: &[bool; 16], mode: Bc6hMode) -> [u8; 16] {
    encode_in(texels, mask, &[mode]).0
}

/// Encodes 16 texels (row-major, linear RGB, 0..=65504) as one BC6H block: every one-region mode,
/// keeping the lowest error.
#[must_use]
pub fn encode_bc6h_block(texels: &[[f32; 3]; 16], mask: &[bool; 16]) -> [u8; 16] {
    encode_in(texels, mask, &Bc6hMode::ALL).0
}

fn encode_in(texels: &[[f32; 3]; 16], mask: &[bool; 16], modes: &[Bc6hMode]) -> ([u8; 16], f64) {
    let mask = if mask.iter().any(|m| *m) {
        *mask
    } else {
        [true; 16]
    };
    // Targets in half-float units, and the 16-bit interpolation domain the endpoints live in.
    let target = texels.map(|p| p.map(|v| f32::from(f32_to_f16_bits(v))));
    let points = target.map(|p| p.map(|h| (h + 0.5) * 64.0 / 31.0));
    let (e0, e1) = fit_line(&points, &mask);
    let clamp = |e: [f32; 3]| e.map(|v| v.clamp(0.0, 65535.0));
    let mut best: Option<([u8; 16], f64)> = None;
    for &mode in modes {
        let mut e = [clamp(e0), clamp(e1)];
        for _ in 0..3 {
            let (block, err, idx) = encode_endpoints(mode, e, &target, &mask);
            if best.is_none_or(|(_, b)| err < b) {
                best = Some((block, err));
            }
            if err == 0.0 {
                break;
            }
            // Refit the endpoints for the chosen indices, in the block's endpoint order (the next
            // round tries both orders again).
            let alpha = idx.map(|i| WEIGHTS[usize::from(i)] as f32 / 64.0);
            let Some((a, b)) = least_squares(&points, &mask, &alpha) else {
                break;
            };
            e = [clamp(a), clamp(b)];
        }
    }
    best.unwrap_or(([0; 16], 0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(scale: f32) -> [[f32; 3]; 16] {
        std::array::from_fn(|k| {
            let l = scale * (1.0 + 0.05 * k as f32);
            [l, l * 0.9, l * 0.7]
        })
    }

    #[test]
    fn every_mode_round_trips_its_fields() {
        for mode in Bc6hMode::ALL {
            // Mode 14's 4-bit delta spans only a nearly flat block.
            let px = if mode == Bc6hMode::Mode14 {
                gradient(1000.0).map(|p| p.map(|v| 1000.0 + (v - 1000.0) * 1.0e-4))
            } else {
                gradient(100.0)
            };
            let block = encode_bc6h_block_mode(&px, &[true; 16], mode);
            let (m, stored, idx) = unpack(&block).expect("one of ours");
            assert_eq!(m, mode);
            assert!(idx[0] < 8, "anchor");
            assert_eq!(pack(m, stored, &idx), block, "{mode:?}");
            let back = decode_bc6h_block(&block).expect("decode");
            for (p, q) in px.iter().zip(&back) {
                for c in 0..3 {
                    assert!((p[c] - q[c]).abs() <= p[c] * 0.08, "{mode:?}: {p:?} -> {q:?}");
                }
            }
        }
    }

    #[test]
    fn the_unquantization_hits_both_ends() {
        assert_eq!(unquantize(0, 10), 0);
        assert_eq!(unquantize(1023, 10), 0xFFFF);
        assert_eq!(unquantize(1, 10), 96);
        assert_eq!(unquantize(1234, 16), 1234);
        // The top of the palette is the largest finite half.
        assert_eq!((0xFFFF * 31) >> 6, 0x7BFF);
        assert_eq!(sign_extend(0b1111, 4), -1);
        assert_eq!(sign_extend(0b0111, 4), 7);
    }

    #[test]
    fn the_encoder_picks_the_finest_mode_for_a_flat_block() {
        // A nearly flat block: a delta mode beats the 10-bit endpoints.
        let px = gradient(1000.0).map(|p| p.map(|v| 1000.0 + (v - 1000.0) * 0.01));
        let all = encode_bc6h_block(&px, &[true; 16]);
        let (mode, ..) = unpack(&all).expect("ours");
        assert_ne!(mode, Bc6hMode::Mode11);
        // Squared error: every texel has the same half-float exponent here, so it orders the modes
        // as the encoder's own (half-float) error does.
        let err = |b: &[u8; 16]| -> f32 {
            let d = decode_bc6h_block(b).expect("decode");
            px.iter()
                .zip(&d)
                .map(|(p, q)| (0..3).map(|c| (p[c] - q[c]).powi(2)).sum::<f32>())
                .sum()
        };
        assert!(err(&all) <= err(&encode_bc6h_block_mode(&px, &[true; 16], Bc6hMode::Mode11)));
    }
}
