// SPDX-License-Identifier: MPL-2.0
//! ASTC 4×4 HDR blocks (Khronos Data Format Specification, chapter "ASTC Compressed Texture Image
//! Formats"), the one layout this encoder writes:
//!
//! - **Block mode** `0x053`: a 4×4 weight grid, one plane, weight range 8 (3 bits a weight, 48
//!   bits). With one partition (bits 11-12 zero) the endpoint mode sits in bits 13-16, and the 63
//!   bits left for colour hold the six endpoint values at the full range of 256 (8 bits each, bits
//!   17-64).
//! - **Colour endpoint mode 11** (HDR RGB, direct): six 8-bit values unpack to two RGB endpoints
//!   of 12 bits (a major component, a mode value 0-7 choosing how the variable bits are spent, and
//!   the differences `b`, `c`, `d` from the major component), or with major component 3 to two
//!   plain 8/8/7-bit endpoints.
//! - **Weights** are read from the top of the block down (bit 127 is the first weight's lowest
//!   bit). Interpolation is `(C0 · (64 − w) + C1 · w + 32) >> 6` on the 16-bit endpoint values, which
//!   are LNS (a 5-bit exponent and an 11-bit piecewise-linear mantissa); the result converts to a
//!   half float with the spec's mantissa transform.
//!
//! The existing LDR decoder (`crate::astc_decode`) reads the same block mode and weight layout
//! with an LDR endpoint mode, which the tests use to hold the packing to it.

use super::{f16_bits_to_f32, f32_to_f16_bits, fit_line, least_squares};

/// Block mode: 4×4 weights, one plane, 8 weight levels.
const BLOCK_MODE: u128 = 0x053;
/// Colour endpoint mode 11: HDR RGB, direct.
const CEM_HDR_RGB: u128 = 11;
/// The unquantized weights of the 8-level range.
const WEIGHTS: [i32; 8] = [0, 9, 18, 27, 37, 46, 55, 64];

/// Packs a block: endpoint mode `cem`, six 8-bit endpoint values and 16 weights (0..8).
fn pack(cem: u128, v: [u8; 6], weights: &[u8; 16]) -> [u8; 16] {
    let mut b = BLOCK_MODE | (cem << 13);
    for (k, &val) in v.iter().enumerate() {
        b |= u128::from(val) << (17 + 8 * k);
    }
    for (k, &w) in weights.iter().enumerate() {
        for bit in 0..3 {
            if (w >> bit) & 1 == 1 {
                b |= 1u128 << (127 - (3 * k + bit));
            }
        }
    }
    b.to_le_bytes()
}

fn unpack(block: &[u8; 16]) -> Option<([u8; 6], [u8; 16])> {
    let b = u128::from_le_bytes(*block);
    if b & 0x7FF != BLOCK_MODE || (b >> 11) & 3 != 0 || (b >> 13) & 0xF != CEM_HDR_RGB {
        return None;
    }
    let v = std::array::from_fn(|k| ((b >> (17 + 8 * k)) & 0xFF) as u8);
    let weights = std::array::from_fn(|k| {
        (0..3).fold(0u8, |w, bit| {
            w | ((((b >> (127 - (3 * k + bit))) & 1) as u8) << bit)
        })
    });
    Some((v, weights))
}

/// A field of mode 11's major-component frame.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    A,
    B0,
    B1,
    C,
    D0,
    D1,
}

/// For each mode value, the field and bit the six variable bits fill. The variable bits are, in
/// order: `v2` bit 6, `v3` bit 6, `v4` bit 6, `v5` bit 6, `v4` bit 5, `v5` bit 5.
const VARIABLE: [[(Field, u8); 6]; 8] = {
    use Field::{A, B0, B1, C, D0, D1};
    [
        [(B0, 6), (B1, 6), (D0, 6), (D1, 6), (D0, 5), (D1, 5)],
        [(B0, 6), (B1, 6), (B0, 7), (B1, 7), (D0, 5), (D1, 5)],
        [(A, 9), (C, 6), (D0, 6), (D1, 6), (D0, 5), (D1, 5)],
        [(B0, 6), (B1, 6), (A, 9), (C, 6), (D0, 5), (D1, 5)],
        [(B0, 6), (B1, 6), (B0, 7), (B1, 7), (A, 9), (A, 10)],
        [(A, 9), (A, 10), (C, 7), (C, 6), (D0, 5), (D1, 5)],
        [(B0, 6), (B1, 6), (A, 11), (C, 6), (A, 9), (A, 10)],
        [(A, 9), (A, 10), (A, 11), (C, 6), (D0, 5), (D1, 5)],
    ]
};

/// Bits of `d0`/`d1` per mode value (they are signed).
const D_BITS: [u32; 8] = [7, 6, 7, 6, 5, 6, 5, 6];

/// Bits of each field in a mode value (fixed bits plus its variable ones).
fn field_bits(mode: usize, field: Field) -> u32 {
    let fixed = match field {
        Field::A => 9,
        Field::B0 | Field::B1 | Field::C => 6,
        Field::D0 | Field::D1 => return D_BITS[mode],
    };
    fixed + VARIABLE[mode].iter().filter(|(f, _)| *f == field).count() as u32
}

/// Left shift that brings a mode value's fields to 12 bits.
fn shift(mode: usize) -> u32 {
    ((mode as u32) >> 1) ^ 3
}

/// Mode 11 endpoints from six values: two RGB endpoints as 16-bit LNS values.
fn unpack_endpoints(v: [u8; 6]) -> [[i32; 3]; 2] {
    let v = v.map(i32::from);
    let mode = (((v[1] >> 7) & 1) | (((v[2] >> 7) & 1) << 1) | (((v[3] >> 7) & 1) << 2)) as usize;
    let major = (((v[4] >> 7) & 1) | (((v[5] >> 7) & 1) << 1)) as usize;
    if major == 3 {
        return [
            [v[0] << 8, v[2] << 8, (v[4] & 0x7F) << 9],
            [v[1] << 8, v[3] << 8, (v[5] & 0x7F) << 9],
        ];
    }
    let mut a = v[0] | ((v[1] & 0x40) << 2);
    let mut b0 = v[2] & 0x3F;
    let mut b1 = v[3] & 0x3F;
    let mut c = v[1] & 0x3F;
    let mut d0 = v[4] & 0x1F;
    let mut d1 = v[5] & 0x1F;
    let var = [
        (v[2] >> 6) & 1,
        (v[3] >> 6) & 1,
        (v[4] >> 6) & 1,
        (v[5] >> 6) & 1,
        (v[4] >> 5) & 1,
        (v[5] >> 5) & 1,
    ];
    for (&(field, bit), &value) in VARIABLE[mode].iter().zip(&var) {
        let slot = match field {
            Field::A => &mut a,
            Field::B0 => &mut b0,
            Field::B1 => &mut b1,
            Field::C => &mut c,
            Field::D0 => &mut d0,
            Field::D1 => &mut d1,
        };
        *slot |= value << bit;
    }
    let dbits = D_BITS[mode];
    let sext = |d: i32| (d << (32 - dbits)) >> (32 - dbits);
    let (d0, d1) = (sext(d0), sext(d1));
    let s = shift(mode);
    let (a, b0, b1, c, d0, d1) = (a << s, b0 << s, b1 << s, c << s, d0 << s, d1 << s);
    let clamp = |x: i32| x.clamp(0, 4095);
    let one = [a, a - b0, a - b1].map(clamp);
    let zero = [a - c, a - b0 - c - d0, a - b1 - c - d1].map(clamp);
    // Back from the major-component frame to RGB.
    let unswap = |e: [i32; 3]| match major {
        1 => [e[1], e[0], e[2]],
        2 => [e[2], e[1], e[0]],
        _ => e,
    };
    [unswap(zero).map(|x| x << 4), unswap(one).map(|x| x << 4)]
}

/// The six values that encode `e` (two RGB endpoints, 16-bit LNS domain) best, with the decoded
/// endpoints.
fn encode_endpoints(e: [[f32; 3]; 2]) -> ([u8; 6], [[i32; 3]; 2]) {
    let err = |d: &[[i32; 3]; 2], t: &[[f32; 3]; 2]| -> f32 {
        (0..2)
            .flat_map(|k| (0..3).map(move |c| (k, c)))
            .map(|(k, c)| (d[k][c] as f32 - t[k][c]).powi(2))
            .sum()
    };
    // Major component 3: plain endpoints, 8/8/7 bits.
    let q = |x: f32, step: f32, max: i32| ((x / step).round() as i32).clamp(0, max) as u8;
    let plain = [
        q(e[0][0], 256.0, 255),
        q(e[1][0], 256.0, 255),
        q(e[0][1], 256.0, 255),
        q(e[1][1], 256.0, 255),
        q(e[0][2], 512.0, 127) | 0x80,
        q(e[1][2], 512.0, 127) | 0x80,
    ];
    let decoded = unpack_endpoints(plain);
    let mut best = (plain, decoded, err(&decoded, &e));
    // The other major components, in the 12-bit domain of the major-component frame.
    let e12 = e.map(|p| p.map(|x| x / 16.0));
    for major in 0..3usize {
        let frame = |p: [f32; 3]| match major {
            1 => [p[1], p[0], p[2]],
            2 => [p[2], p[1], p[0]],
            _ => p,
        };
        let (f0, f1) = (frame(e12[0]), frame(e12[1]));
        // The decoded endpoint 1 carries the larger major component (c >= 0).
        let (lo, hi, swapped) = if f1[0] >= f0[0] {
            (f0, f1, false)
        } else {
            (f1, f0, true)
        };
        for (mode, &dbits) in D_BITS.iter().enumerate() {
            let s = shift(mode);
            let step = (1u32 << s) as f32;
            let unsigned = |x: f32, field: Field| -> i32 {
                ((x / step).round() as i32).clamp(0, (1 << field_bits(mode, field)) - 1)
            };
            let a = unsigned(hi[0], Field::A);
            let a12 = (a << s) as f32;
            let b0 = unsigned(a12 - hi[1], Field::B0);
            let b1 = unsigned(a12 - hi[2], Field::B1);
            let c = unsigned(a12 - lo[0], Field::C);
            let half = 1i32 << (dbits - 1);
            let signed = |x: f32| ((x / step).round() as i32).clamp(-half, half - 1);
            let d0 = signed(a12 - ((b0 + c) << s) as f32 - lo[1]);
            let d1 = signed(a12 - ((b1 + c) << s) as f32 - lo[2]);
            let v = pack_mode11(mode, major, [a, b0, b1, c, d0, d1]);
            let d = unpack_endpoints(v);
            // `d` is in the block's order (lo, hi); the target in the caller's.
            let er = if swapped {
                err(&[d[1], d[0]], &e)
            } else {
                err(&d, &e)
            };
            if er < best.2 {
                best = (v, d, er);
            }
        }
    }
    (best.0, best.1)
}

/// Packs mode 11 fields (`a, b0, b1, c, d0, d1` already in range for `mode`) into six values.
fn pack_mode11(mode: usize, major: usize, f: [i32; 6]) -> [u8; 6] {
    let [a, b0, b1, c, d0, d1] = f;
    let get = |field: Field, bit: u8| -> i32 {
        let x = match field {
            Field::A => a,
            Field::B0 => b0,
            Field::B1 => b1,
            Field::C => c,
            Field::D0 => d0,
            Field::D1 => d1,
        };
        (x >> bit) & 1
    };
    let var = VARIABLE[mode].map(|(field, bit)| get(field, bit));
    let (m, mj) = (mode as i32, major as i32);
    [
        (a & 0xFF) as u8,
        ((c & 0x3F) | (((a >> 8) & 1) << 6) | ((m & 1) << 7)) as u8,
        ((b0 & 0x3F) | (var[0] << 6) | (((m >> 1) & 1) << 7)) as u8,
        ((b1 & 0x3F) | (var[1] << 6) | (((m >> 2) & 1) << 7)) as u8,
        ((d0 & 0x1F) | (var[4] << 5) | (var[2] << 6) | ((mj & 1) << 7)) as u8,
        ((d1 & 0x1F) | (var[5] << 5) | (var[3] << 6) | (((mj >> 1) & 1) << 7)) as u8,
    ]
}

/// LNS (16-bit) of a half float: the exponent and the inverse of the mantissa transform.
fn half_to_lns(h: u16) -> f32 {
    if h == 0 {
        return 0.0;
    }
    let e = f32::from(h >> 10);
    let mt = f32::from(h & 0x3FF) * 8.0 + 4.0;
    let m = if mt < 1536.0 {
        mt / 3.0
    } else if mt < 5632.0 {
        (mt + 512.0) / 4.0
    } else {
        (mt + 2048.0) / 5.0
    };
    e * 2048.0 + m
}

/// The half float of an interpolated 16-bit LNS value (the spec's mantissa transform), at most
/// the largest finite half.
fn lns_to_half(c: i32) -> u16 {
    let e = c >> 11;
    let m = c & 0x7FF;
    let mt = if m < 512 {
        3 * m
    } else if m < 1536 {
        4 * m - 512
    } else {
        5 * m - 2048
    };
    (((e << 10) | (mt >> 3)) as u16).min(0x7BFF)
}

fn interpolate(e: &[[i32; 3]; 2], w: i32) -> [i32; 3] {
    [0, 1, 2].map(|c| (e[0][c] * (64 - w) + e[1][c] * w + 32) >> 6)
}

/// Decodes a block written by [`encode_astc_hdr_block`] to linear RGB; `None` for any other
/// layout.
#[must_use]
pub fn decode_astc_hdr_block(block: &[u8; 16]) -> Option<[[f32; 3]; 16]> {
    let (v, weights) = unpack(block)?;
    let e = unpack_endpoints(v);
    Some(weights.map(|w| interpolate(&e, WEIGHTS[usize::from(w)]).map(|c| f16_bits_to_f32(lns_to_half(c)))))
}

/// The weights for decoded endpoints `e` and the squared LNS error over the masked texels.
fn choose_weights(e: &[[i32; 3]; 2], target: &[[f32; 3]; 16], mask: &[bool; 16]) -> ([u8; 16], f64) {
    let pal = WEIGHTS.map(|w| interpolate(e, w));
    let mut out = [0u8; 16];
    let mut total = 0.0f64;
    for (k, (t, m)) in target.iter().zip(mask).enumerate() {
        let (best, err) = pal
            .iter()
            .enumerate()
            .map(|(i, p)| {
                (
                    i,
                    (0..3).map(|c| f64::from(p[c] as f32 - t[c]).powi(2)).sum::<f64>(),
                )
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap_or((0, 0.0));
        out[k] = best as u8;
        if *m {
            total += err;
        }
    }
    (out, total)
}

/// Encodes 16 texels (row-major, linear RGB, 0..=65504) as one ASTC 4×4 HDR block.
#[must_use]
pub fn encode_astc_hdr_block(texels: &[[f32; 3]; 16], mask: &[bool; 16]) -> [u8; 16] {
    let mask = if mask.iter().any(|m| *m) {
        *mask
    } else {
        [true; 16]
    };
    let target = texels.map(|p| p.map(|v| half_to_lns(f32_to_f16_bits(v))));
    let (e0, e1) = fit_line(&target, &mask);
    let clamp = |e: [f32; 3]| e.map(|x| x.clamp(0.0, 65535.0));
    let mut e = [clamp(e0), clamp(e1)];
    let mut best: Option<([u8; 16], f64)> = None;
    for _ in 0..3 {
        let (v, decoded) = encode_endpoints(e);
        let (weights, err) = choose_weights(&decoded, &target, &mask);
        if best.is_none_or(|(_, b)| err < b) {
            best = Some((pack(CEM_HDR_RGB, v, &weights), err));
        }
        if err == 0.0 {
            break;
        }
        let alpha = weights.map(|w| WEIGHTS[usize::from(w)] as f32 / 64.0);
        let Some((a, b)) = least_squares(&target, &mask, &alpha) else {
            break;
        };
        e = [clamp(a), clamp(b)];
    }
    best.map_or([0; 16], |(b, _)| b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spec's procedure for mode 11 written out with one-hot mode masks, held against the
    /// table form [`unpack_endpoints`] uses, over many value sets.
    fn unpack_reference(v: [u8; 6]) -> [[i32; 3]; 2] {
        let v = v.map(i32::from);
        let modeval = ((v[1] & 0x80) >> 7) | (((v[2] & 0x80) >> 7) << 1) | (((v[3] & 0x80) >> 7) << 2);
        let majcomp = ((v[4] & 0x80) >> 7) | (((v[5] & 0x80) >> 7) << 1);
        if majcomp == 3 {
            return [
                [v[0] << 8, v[2] << 8, (v[4] & 0x7F) << 9],
                [v[1] << 8, v[3] << 8, (v[5] & 0x7F) << 9],
            ];
        }
        let mut a = v[0] | ((v[1] & 0x40) << 2);
        let mut b0 = v[2] & 0x3F;
        let mut b1 = v[3] & 0x3F;
        let mut c = v[1] & 0x3F;
        let mut d0 = v[4] & 0x7F;
        let mut d1 = v[5] & 0x7F;
        let dbits = [7, 6, 7, 6, 5, 6, 5, 6][modeval as usize];
        let bit = |x: i32, n: i32| (x >> n) & 1;
        let (x0, x1, x2, x3, x4, x5) = (
            bit(v[2], 6),
            bit(v[3], 6),
            bit(v[4], 6),
            bit(v[5], 6),
            bit(v[4], 5),
            bit(v[5], 5),
        );
        let oh = 1 << modeval;
        if oh & 0xA4 != 0 {
            a |= x0 << 9;
        }
        if oh & 0x8 != 0 {
            a |= x2 << 9;
        }
        if oh & 0x50 != 0 {
            a |= x4 << 9;
            a |= x5 << 10;
        }
        if oh & 0xA0 != 0 {
            a |= x1 << 10;
        }
        if oh & 0xC0 != 0 {
            a |= x2 << 11;
        }
        if oh & 0x4 != 0 {
            c |= x1 << 6;
        }
        if oh & 0xE8 != 0 {
            c |= x3 << 6;
        }
        if oh & 0x20 != 0 {
            c |= x2 << 7;
        }
        if oh & 0x5B != 0 {
            b0 |= x0 << 6;
            b1 |= x1 << 6;
        }
        if oh & 0x12 != 0 {
            b0 |= x2 << 7;
            b1 |= x3 << 7;
        }
        if oh & 0xAF != 0 {
            d0 |= x4 << 5;
            d1 |= x5 << 5;
        }
        if oh & 0x5 != 0 {
            d0 |= x2 << 6;
            d1 |= x3 << 6;
        }
        let sx = 32 - dbits;
        let (d0, d1) = ((d0 << sx) >> sx, (d1 << sx) >> sx);
        let s = (modeval >> 1) ^ 3;
        let (a, b0, b1, c, d0, d1) = (a << s, b0 << s, b1 << s, c << s, d0 << s, d1 << s);
        let cl = |x: i32| x.clamp(0, 4095);
        let (mut r1, mut g1, mut bl1) = (cl(a), cl(a - b0), cl(a - b1));
        let (mut r0, mut g0, mut bl0) = (cl(a - c), cl(a - b0 - c - d0), cl(a - b1 - c - d1));
        match majcomp {
            1 => {
                std::mem::swap(&mut r0, &mut g0);
                std::mem::swap(&mut r1, &mut g1);
            }
            2 => {
                std::mem::swap(&mut r0, &mut bl0);
                std::mem::swap(&mut r1, &mut bl1);
            }
            _ => {}
        }
        [[r0 << 4, g0 << 4, bl0 << 4], [r1 << 4, g1 << 4, bl1 << 4]]
    }

    #[test]
    fn the_mode_table_matches_the_spec_procedure() {
        let mut s = 0x1234_5678u32;
        for _ in 0..20_000 {
            let v: [u8; 6] = std::array::from_fn(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                s as u8
            });
            assert_eq!(unpack_endpoints(v), unpack_reference(v), "{v:?}");
        }
        // Each mode spends its six variable bits once, and its fields fill 12 bits.
        for mode in 0..8 {
            assert_eq!(field_bits(mode, Field::A) + shift(mode), 12, "mode {mode}");
        }
    }

    #[test]
    fn the_fields_survive_pack_and_unpack() {
        for mode in 0..8usize {
            for major in 0..3usize {
                let s = shift(mode);
                let a = (1 << field_bits(mode, Field::A)) - 1;
                let f = [a, 3, 2, 1, -1, 1];
                let v = pack_mode11(mode, major, f);
                let e = unpack_endpoints(v);
                // Endpoint 1's major component is `a` shifted to 12 bits.
                let maj = [0, 1, 2][major];
                assert_eq!(e[1][maj], (a << s).min(4095) << 4, "mode {mode} major {major}");
            }
        }
    }

    /// The block mode and weight layout through the crate's LDR decoder: the same packing with
    /// LDR endpoint mode 8 (RGB direct) must decode to the LDR interpolation of its endpoints.
    #[test]
    fn the_packing_matches_the_ldr_decoder() {
        let v = [10u8, 240, 20, 200, 30, 220];
        let weights: [u8; 16] = std::array::from_fn(|k| (k % 8) as u8);
        let block = pack(8, v, &weights);
        let mut out = [[0u8; 4]; 16];
        crate::astc_decode::decode_block(&block, (4, 4), false, &mut out);
        for (k, px) in out.iter().enumerate() {
            let w = WEIGHTS[usize::from(weights[k])];
            for (c, &(lo, hi)) in [(v[0], v[1]), (v[2], v[3]), (v[4], v[5])].iter().enumerate() {
                let (lo, hi) = (i32::from(lo) * 257, i32::from(hi) * 257);
                let want = ((lo * (64 - w) + hi * w + 32) >> 6) >> 8;
                assert!(
                    (i32::from(px[c]) - want).abs() <= 1,
                    "texel {k} channel {c}: {px:?} vs {want}"
                );
            }
            assert_eq!(px[3], 255);
        }
        // And our unpacker refuses the LDR block.
        assert!(decode_astc_hdr_block(&block).is_none());
    }

    #[test]
    fn lns_and_half_convert_back_and_forth() {
        for h in [0x3C00u16, 0x0400, 0x7800, 0x3555, 0x7BFF, 0x0001, 0x2E66] {
            let back = lns_to_half(half_to_lns(h).round() as i32);
            assert!(back.abs_diff(h) <= 1, "{h:#06x} -> {back:#06x}");
        }
        assert_eq!(lns_to_half(0), 0);
        assert_eq!(lns_to_half(0xFFFF), 0x7BFF);
    }
}
