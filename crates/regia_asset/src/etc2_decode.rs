// SPDX-License-Identifier: MPL-2.0
//! CPU decoder for ETC2 and EAC blocks, for adapters that cannot sample them
//! (`docs/specs/12-assets.md` § 7.1, ADR-2063): ETC2 RGB8, RGB8 with punch-through alpha, RGBA8
//! (EAC alpha + ETC2 colour), EAC R11 and RG11 (unsigned).
//!
//! Original code written from the Khronos Data Format Specification 1.3, chapter "ETC2
//! Compressed Texture Image Formats": the individual, differential, T, H and planar modes, the
//! punch-through alpha rules, the intensity modifier and distance tables and the EAC modifier
//! tables (the tables are the specification's). Decoded texels are the values a GPU samples
//! before any sRGB decode; channels a format does not store come back as an `R` / `RG` texture
//! returns them (`R 0 0 255`, `R G 0 255`; alpha 255 for RGB8).

use thiserror::Error;

use Etc2Format as Format;

/// The ETC2 / EAC formats this module decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Etc2Format {
    /// ETC2 RGB8, 8 bytes a 4×4 block.
    Rgb8,
    /// ETC2 RGB8 with punch-through (1-bit) alpha, 8 bytes.
    Rgb8A1,
    /// ETC2 RGBA8: an EAC alpha block, then an ETC2 RGB8 block (16 bytes).
    Rgba8,
    /// EAC R11 unsigned, 8 bytes.
    R11,
    /// EAC RG11 unsigned, 16 bytes.
    Rg11,
}

impl Etc2Format {
    /// Bytes of one 4×4 block.
    #[must_use]
    pub const fn block_bytes(self) -> usize {
        match self {
            Self::Rgb8 | Self::Rgb8A1 | Self::R11 => 8,
            Self::Rgba8 | Self::Rg11 => 16,
        }
    }
}

/// Errors from [`decode_etc2`].
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum Etc2DecodeError {
    /// The level holds fewer bytes than its blocks need.
    #[error("{format:?} level {width}x{height} needs {need} bytes, got {got}")]
    Truncated {
        /// Format.
        format: Etc2Format,
        /// Texel width of the level.
        width: u32,
        /// Texel height of the level.
        height: u32,
        /// Bytes the blocks need.
        need: usize,
        /// Bytes given.
        got: usize,
    },
}

/// The ETC1/ETC2 intensity modifier tables, `[small, large]` per table codeword.
const INTENSITY: [[i32; 2]; 8] = [
    [2, 8],
    [5, 17],
    [9, 29],
    [13, 42],
    [18, 60],
    [24, 80],
    [33, 106],
    [47, 183],
];

/// The T and H mode distance table.
const DISTANCE: [i32; 8] = [3, 6, 11, 16, 23, 32, 41, 64];

/// The EAC modifier tables, by table index then 3-bit pixel index.
const EAC_MODIFIERS: [[i32; 8]; 16] = [
    [-3, -6, -9, -15, 2, 5, 8, 14],
    [-3, -7, -10, -13, 2, 6, 9, 12],
    [-2, -5, -8, -13, 1, 4, 7, 12],
    [-2, -4, -6, -13, 1, 3, 5, 12],
    [-3, -6, -8, -12, 2, 5, 7, 11],
    [-3, -7, -9, -11, 2, 6, 8, 10],
    [-4, -7, -8, -11, 3, 6, 7, 10],
    [-3, -5, -8, -11, 2, 4, 7, 10],
    [-2, -6, -8, -10, 1, 5, 7, 9],
    [-2, -5, -8, -10, 1, 4, 7, 9],
    [-2, -4, -8, -10, 1, 3, 7, 9],
    [-2, -5, -7, -10, 1, 4, 6, 9],
    [-3, -4, -7, -10, 2, 3, 6, 9],
    [-1, -2, -3, -10, 0, 1, 2, 9],
    [-4, -6, -8, -9, 3, 5, 7, 8],
    [-3, -5, -7, -9, 2, 4, 6, 8],
];

/// Decodes one `width`×`height` level of tightly packed blocks (rows of `ceil(width / 4)` blocks)
/// into row-major RGBA8, cropped to the level size.
///
/// # Errors
///
/// [`Etc2DecodeError::Truncated`] when `data` is shorter than the level's blocks.
pub fn decode_etc2(
    format: Etc2Format,
    width: u32,
    height: u32,
    data: &[u8],
) -> Result<Vec<u8>, Etc2DecodeError> {
    let (w, h) = (width.max(1) as usize, height.max(1) as usize);
    let (cols, rows) = (w.div_ceil(4), h.div_ceil(4));
    let bb = format.block_bytes();
    let need = cols.checked_mul(rows).and_then(|n| n.checked_mul(bb));
    if need.is_none_or(|n| data.len() < n) {
        return Err(Etc2DecodeError::Truncated {
            format,
            width,
            height,
            need: need.unwrap_or(usize::MAX),
            got: data.len(),
        });
    }
    let mut out = vec![0u8; w * h * 4];
    for by in 0..rows {
        for bx in 0..cols {
            let at = (by * cols + bx) * bb;
            let texels = decode_block(format, &data[at..at + bb]);
            for (i, texel) in texels.iter().enumerate() {
                let (x, y) = (bx * 4 + i % 4, by * 4 + i / 4);
                if x < w && y < h {
                    let o = (y * w + x) * 4;
                    out[o..o + 4].copy_from_slice(texel);
                }
            }
        }
    }
    Ok(out)
}

/// Decodes one block (a slice shorter than [`Etc2Format::block_bytes`] is zero-padded) into 16
/// RGBA8 texels, row-major.
#[must_use]
pub fn decode_block(format: Etc2Format, block: &[u8]) -> [[u8; 4]; 16] {
    let word = |at: usize| {
        let mut b = [0u8; 8];
        for (i, dst) in b.iter_mut().enumerate() {
            *dst = block.get(at + i).copied().unwrap_or(0);
        }
        u64::from_be_bytes(b)
    };
    let mut out = [[0u8, 0, 0, 255]; 16];
    match format {
        Format::Rgb8 => out = decode_color(word(0), false),
        Format::Rgb8A1 => out = decode_color(word(0), true),
        Format::Rgba8 => {
            out = decode_color(word(8), false);
            let alpha = decode_eac(word(0), false);
            for (i, t) in out.iter_mut().enumerate() {
                t[3] = alpha[i] as u8;
            }
        }
        Format::R11 | Format::Rg11 => {
            let channels = if format == Format::R11 { 1 } else { 2 };
            for c in 0..channels {
                let values = decode_eac(word(c * 8), true);
                for (t, v) in out.iter_mut().zip(values) {
                    t[c] = eleven_to_unorm8(v);
                }
            }
        }
    }
    out
}

/// An 11-bit EAC value as the nearest 8-bit UNORM.
fn eleven_to_unorm8(v: i32) -> u8 {
    ((v * 255 + 1023) / 2047).clamp(0, 255) as u8
}

/// Texel `(x, y)` of a block is pixel number `x * 4 + y` in every ETC and EAC index field.
fn pixel_number(x: usize, y: usize) -> usize {
    x * 4 + y
}

/// Bits `shift..shift + count` of `bits`.
fn take(bits: u64, shift: u32, count: u32) -> i32 {
    ((bits >> shift) & ((1u64 << count) - 1)) as i32
}

fn expand4(c: i32) -> i32 {
    (c << 4) | c
}

fn expand5(c: i32) -> i32 {
    (c << 3) | (c >> 2)
}

fn expand6(c: i32) -> i32 {
    (c << 2) | (c >> 4)
}

fn expand7(c: i32) -> i32 {
    (c << 1) | (c >> 6)
}

/// The 2-bit pixel index (`msb lsb`) of pixel number `p`.
fn pixel_index(bits: u64, p: usize) -> usize {
    ((((bits >> (16 + p)) & 1) << 1) | ((bits >> p) & 1)) as usize
}

/// `rgb + delta` clamped to 0..=255 per channel, opaque.
fn offset(rgb: [i32; 3], delta: i32) -> [u8; 4] {
    let c = rgb.map(|v| (v + delta).clamp(0, 255) as u8);
    [c[0], c[1], c[2], 255]
}

/// Decodes the 64-bit ETC2 colour block `bits` (individual, differential, T, H or planar) into
/// 16 RGBA texels, row-major. With `punch_through` (RGB8A1) the differential bit is the
/// "opaque" bit: when clear there is no individual mode, the modifier of pixel index 0 is 0 and
/// index 2 is transparent black (in the differential, T and H modes; planar stays opaque).
fn decode_color(bits: u64, punch_through: bool) -> [[u8; 4]; 16] {
    let diff_bit = (bits >> 33) & 1 == 1;
    let (differential, opaque) = if punch_through {
        (true, diff_bit)
    } else {
        (diff_bit, true)
    };
    if !differential {
        // Individual mode: two 4-bit base colours, R1 R2 G1 G2 B1 B2 from bit 63 down.
        let base = |shift: u32| [0, 1, 2].map(|c| expand4(take(bits, shift - 8 * c, 4)));
        return etc1_texels(bits, [base(60), base(56)], opaque);
    }
    let r = take(bits, 59, 5) + signed3(take(bits, 56, 3));
    let g = take(bits, 51, 5) + signed3(take(bits, 48, 3));
    let b = take(bits, 43, 5) + signed3(take(bits, 40, 3));
    if !(0..=31).contains(&r) {
        return t_mode(bits, opaque);
    }
    if !(0..=31).contains(&g) {
        return h_mode(bits, opaque);
    }
    if !(0..=31).contains(&b) {
        return planar(bits);
    }
    let b1 = [
        expand5(take(bits, 59, 5)),
        expand5(take(bits, 51, 5)),
        expand5(take(bits, 43, 5)),
    ];
    let b2 = [expand5(r), expand5(g), expand5(b)];
    etc1_texels(bits, [b1, b2], opaque)
}

/// A 3-bit two's complement value.
fn signed3(v: i32) -> i32 {
    if v >= 4 { v - 8 } else { v }
}

/// The individual / differential modes' texels: two sub-blocks (side by side, or stacked when the
/// flip bit is set), each a base colour plus its table's modifier.
fn etc1_texels(bits: u64, bases: [[i32; 3]; 2], opaque: bool) -> [[u8; 4]; 16] {
    let flip = (bits >> 32) & 1 == 1;
    let tables = [take(bits, 37, 3) as usize, take(bits, 34, 3) as usize];
    let mut out = [[0u8; 4]; 16];
    for y in 0..4 {
        for x in 0..4 {
            let half = usize::from(if flip { y >= 2 } else { x >= 2 });
            let index = pixel_index(bits, pixel_number(x, y));
            let [small, large] = INTENSITY[tables[half]];
            out[y * 4 + x] = match (opaque, index) {
                (false, 2) => [0, 0, 0, 0],
                (false, 0) => offset(bases[half], 0),
                (_, 0) => offset(bases[half], small),
                (_, 1) => offset(bases[half], large),
                (_, 2) => offset(bases[half], -small),
                _ => offset(bases[half], -large),
            };
        }
    }
    out
}

/// Texels from four paint colours selected by each pixel's 2-bit index (index 2 transparent
/// black when not `opaque`).
fn paint_texels(bits: u64, paint: [[u8; 4]; 4], opaque: bool) -> [[u8; 4]; 16] {
    let mut out = [[0u8; 4]; 16];
    for y in 0..4 {
        for x in 0..4 {
            let index = pixel_index(bits, pixel_number(x, y));
            out[y * 4 + x] = if !opaque && index == 2 {
                [0, 0, 0, 0]
            } else {
                paint[index]
            };
        }
    }
    out
}

/// The T mode: base colour 1 alone, base colour 2 and ± a distance.
fn t_mode(bits: u64, opaque: bool) -> [[u8; 4]; 16] {
    let c1 = [
        expand4((take(bits, 59, 2) << 2) | take(bits, 56, 2)),
        expand4(take(bits, 52, 4)),
        expand4(take(bits, 48, 4)),
    ];
    let c2 = [
        expand4(take(bits, 44, 4)),
        expand4(take(bits, 40, 4)),
        expand4(take(bits, 36, 4)),
    ];
    let d = DISTANCE[((take(bits, 34, 2) << 1) | take(bits, 32, 1)) as usize];
    let paint = [offset(c1, 0), offset(c2, d), offset(c2, 0), offset(c2, -d)];
    paint_texels(bits, paint, opaque)
}

/// The H mode: both base colours ± a distance.
fn h_mode(bits: u64, opaque: bool) -> [[u8; 4]; 16] {
    let r1 = take(bits, 59, 4);
    let g1 = (take(bits, 56, 3) << 1) | take(bits, 52, 1);
    let b1 = (take(bits, 51, 1) << 3) | take(bits, 47, 3);
    let r2 = take(bits, 43, 4);
    let g2 = take(bits, 39, 4);
    let b2 = take(bits, 35, 4);
    let ordering = i32::from(((r1 << 8) | (g1 << 4) | b1) >= ((r2 << 8) | (g2 << 4) | b2));
    let index = (take(bits, 34, 1) << 2) | (take(bits, 32, 1) << 1) | ordering;
    let d = DISTANCE[index as usize];
    let c1 = [expand4(r1), expand4(g1), expand4(b1)];
    let c2 = [expand4(r2), expand4(g2), expand4(b2)];
    let paint = [offset(c1, d), offset(c1, -d), offset(c2, d), offset(c2, -d)];
    paint_texels(bits, paint, opaque)
}

/// The planar mode: a colour at the origin, one step right (H) and one step down (V),
/// extrapolated over the block.
fn planar(bits: u64) -> [[u8; 4]; 16] {
    let ro = expand6(take(bits, 57, 6));
    let go = expand7((take(bits, 56, 1) << 6) | take(bits, 49, 6));
    let bo = expand6((take(bits, 48, 1) << 5) | (take(bits, 43, 2) << 3) | take(bits, 39, 3));
    let rh = expand6((take(bits, 34, 5) << 1) | take(bits, 32, 1));
    let gh = expand7(take(bits, 25, 7));
    let bh = expand6(take(bits, 19, 6));
    let rv = expand6(take(bits, 13, 6));
    let gv = expand7(take(bits, 6, 7));
    let bv = expand6(take(bits, 0, 6));
    let mut out = [[0u8; 4]; 16];
    for y in 0..4i32 {
        for x in 0..4i32 {
            let f =
                |o: i32, h: i32, v: i32| ((x * (h - o) + y * (v - o) + 4 * o + 2) >> 2).clamp(0, 255) as u8;
            out[(y * 4 + x) as usize] = [f(ro, rh, rv), f(go, gh, gv), f(bo, bh, bv), 255];
        }
    }
    out
}

/// Decodes one EAC block into 16 values, row-major: 8-bit alpha, or 11-bit with `eleven`.
fn decode_eac(bits: u64, eleven: bool) -> [i32; 16] {
    let base = take(bits, 56, 8);
    let multiplier = take(bits, 52, 4);
    let table = take(bits, 48, 4) as usize;
    let mut out = [0i32; 16];
    for y in 0..4 {
        for x in 0..4 {
            let p = pixel_number(x, y);
            let index = take(bits, 45 - 3 * p as u32, 3) as usize;
            let m = EAC_MODIFIERS[table][index];
            out[y * 4 + x] = if eleven {
                // A zero multiplier is read as 1/8: the modifier itself in 11-bit units.
                let v = if multiplier == 0 {
                    base * 8 + 4 + m
                } else {
                    base * 8 + 4 + m * multiplier * 8
                };
                v.clamp(0, 2047)
            } else {
                (base + m * multiplier).clamp(0, 255)
            };
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pack a planar block from 6/7/6-bit components (origin, horizontal, vertical).
    fn planar_block(o: [u64; 3], h: [u64; 3], v: [u64; 3]) -> [u8; 8] {
        let mut bits = 0u64;
        bits |= o[0] << 57;
        bits |= (o[1] >> 6) << 56;
        bits |= (o[1] & 0x3f) << 49;
        bits |= (o[2] >> 5) << 48;
        bits |= ((o[2] >> 3) & 0b11) << 43;
        bits |= (o[2] & 0b111) << 39;
        bits |= (h[0] >> 1) << 34;
        bits |= (h[0] & 1) << 32;
        bits |= h[1] << 25;
        bits |= h[2] << 19;
        bits |= v[0] << 13;
        bits |= v[1] << 6;
        bits |= v[2];
        bits |= 1 << 33;
        // The free bits (63, 55, 47..45, 42) are chosen so that red and green stay in range and
        // blue overflows, which is what selects the planar mode.
        let in_range = |b: u64, base: u32, delta: u32| {
            let v = take(b, base, 5) + signed3(take(b, delta, 3));
            (0..=31).contains(&v)
        };
        for free in 0..64u64 {
            let b = bits
                | ((free & 1) << 63)
                | (((free >> 1) & 1) << 55)
                | (((free >> 2) & 0b111) << 45)
                | (((free >> 5) & 1) << 42);
            if in_range(b, 59, 56) && in_range(b, 51, 48) && !in_range(b, 43, 40) {
                return b.to_be_bytes();
            }
        }
        panic!("no planar encoding for {o:?} {h:?} {v:?}");
    }

    #[test]
    fn a_flat_planar_block_is_its_origin_colour() {
        // O = H = V: every texel is the origin colour.
        let block = planar_block([40, 100, 52], [40, 100, 52], [40, 100, 52]);
        let out = decode_etc2(Etc2Format::Rgb8, 4, 4, &block).expect("decode");
        let want = [expand6(40) as u8, expand7(100) as u8, expand6(52) as u8, 255];
        assert!(out.as_chunks::<4>().0.iter().all(|p| *p == want), "{out:?}");
    }

    #[test]
    fn an_individual_block_with_zero_tables_is_its_base_plus_two() {
        // Individual mode, base colours 8 (0x88) and 4 (0x44) per channel, tables 0, all pixel
        // indices 0 (+2): the left half 0x8a, the right half 0x46.
        let mut bits = 0u64;
        for c in 0..3 {
            bits |= 8 << (60 - 8 * c);
            bits |= 4 << (56 - 8 * c);
        }
        let out = decode_etc2(Etc2Format::Rgb8, 4, 4, &bits.to_be_bytes()).expect("decode");
        for (i, p) in out.as_chunks::<4>().0.iter().enumerate() {
            let want = if i % 4 < 2 { 0x8a } else { 0x46 };
            assert_eq!(*p, [want, want, want, 255], "texel {i}");
        }
    }

    #[test]
    fn eac_r11_and_alpha_follow_the_tables() {
        // Base 128, multiplier 1, table 0, every index 7 (+14): alpha 142; R11 128*8+4+14*8 = 1140.
        let mut bits = (128u64 << 56) | (1 << 52);
        for p in 0..16 {
            bits |= 7 << (45 - 3 * p);
        }
        let mut rgba = [0u8; 16];
        rgba[..8].copy_from_slice(&bits.to_be_bytes());
        let out = decode_etc2(Etc2Format::Rgba8, 4, 4, &rgba).expect("decode");
        assert!(out.as_chunks::<4>().0.iter().all(|p| p[3] == 142));
        let out = decode_etc2(Etc2Format::R11, 4, 4, &bits.to_be_bytes()).expect("decode");
        let want = eleven_to_unorm8(1140);
        assert!(out.as_chunks::<4>().0.iter().all(|p| *p == [want, 0, 0, 255]));
    }

    #[test]
    fn punch_through_index_two_is_transparent() {
        // Differential mode with the opaque bit clear: index 2 (msb 1, lsb 0) is transparent.
        let mut bits = 0u64;
        for c in 0..3 {
            bits |= 16 << (59 - 8 * c);
        }
        // Every pixel's msb set, lsb clear.
        bits |= 0xffff << 16;
        let out = decode_etc2(Etc2Format::Rgb8A1, 4, 4, &bits.to_be_bytes()).expect("decode");
        assert!(out.as_chunks::<4>().0.iter().all(|p| *p == [0, 0, 0, 0]));
        // With the opaque bit set the same block is opaque.
        let out = decode_etc2(Etc2Format::Rgb8A1, 4, 4, &(bits | 1 << 33).to_be_bytes()).expect("decode");
        assert!(out.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
    }

    #[test]
    fn short_data_is_an_error_and_edges_crop() {
        assert!(matches!(
            decode_etc2(Etc2Format::Rg11, 8, 4, &[0; 31]),
            Err(Etc2DecodeError::Truncated {
                need: 32,
                got: 31,
                ..
            })
        ));
        let out = decode_etc2(Etc2Format::Rgb8, 5, 3, &[0; 16]).expect("decode");
        assert_eq!(out.len(), 5 * 3 * 4);
    }
}
