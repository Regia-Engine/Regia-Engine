// SPDX-License-Identifier: MPL-2.0
//! CPU decoders for the block-compressed formats the cook writes on desktop (BC1, BC3, BC4, BC5,
//! BC7), for adapters that cannot sample them (`docs/specs/12-assets.md` § 7.1; ADR "cooked
//! runtime"). The renderer decodes a cooked map to RGBA8 only when the device lacks
//! `TEXTURE_COMPRESSION_BC`; every other device uploads the blocks as they are.
//!
//! Decoded texels equal what a GPU returns when it samples the block format (before any sRGB
//! decode): BC4 gives `(r, 0, 0, 255)` and BC5 `(r, g, 0, 255)`, like an `R`/`RG` texture.
//!
//! Sources (public): Khronos Data Format Specification 1.3, § "S3TC Compressed Texture Image
//! Formats" (BC1/BC3), § "RGTC" (BC4/BC5) and § "BPTC" (BC7: mode table, bit layout, partition
//! tables 114–115, anchor tables 116–118, interpolation weights table 119). The partition and
//! anchor tables below are transcribed from that specification.

use thiserror::Error;

/// A block-compressed format this module decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BcnKind {
    /// BC1 (DXT1), 8 bytes a block, 1-bit alpha in its 3-colour mode.
    Bc1,
    /// BC3 (DXT5), 16 bytes: an interpolated alpha block and a 4-colour BC1 block.
    Bc3,
    /// BC4 unsigned, 8 bytes, one channel.
    Bc4,
    /// BC5 unsigned, 16 bytes, two channels.
    Bc5,
    /// BC7 (BPTC unorm), 16 bytes, eight modes.
    Bc7,
}

impl BcnKind {
    /// Bytes of one 4×4 block.
    #[must_use]
    pub const fn block_bytes(self) -> usize {
        match self {
            Self::Bc1 | Self::Bc4 => 8,
            Self::Bc3 | Self::Bc5 | Self::Bc7 => 16,
        }
    }
}

/// Errors from [`decode_bcn`].
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum BcnDecodeError {
    /// The level holds fewer bytes than its blocks need.
    #[error("{kind:?} level {width}x{height} needs {need} bytes, got {got}")]
    Truncated {
        /// Format.
        kind: BcnKind,
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

/// Decodes one `width`×`height` level of tightly packed blocks (rows of `ceil(width/4)` blocks)
/// into row-major RGBA8, cropped to the level size.
///
/// # Errors
///
/// [`BcnDecodeError::Truncated`] when `data` is shorter than the level's blocks.
pub fn decode_bcn(kind: BcnKind, width: u32, height: u32, data: &[u8]) -> Result<Vec<u8>, BcnDecodeError> {
    let (bw, bh) = (
        width.max(1).div_ceil(4) as usize,
        height.max(1).div_ceil(4) as usize,
    );
    let bb = kind.block_bytes();
    let need = bw.checked_mul(bh).and_then(|n| n.checked_mul(bb));
    if need.is_none_or(|n| data.len() < n) {
        return Err(BcnDecodeError::Truncated {
            kind,
            width,
            height,
            need: need.unwrap_or(usize::MAX),
            got: data.len(),
        });
    }
    let (w, h) = (width.max(1) as usize, height.max(1) as usize);
    let mut out = vec![0u8; w * h * 4];
    for by in 0..bh {
        for bx in 0..bw {
            let at = (by * bw + bx) * bb;
            let block = &data[at..at + bb];
            let texels = decode_block(kind, block);
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

/// Decodes one block (normally `block.len()` ≥ [`BcnKind::block_bytes`]; a shorter slice is
/// zero-padded) into 16 RGBA8 texels, row-major.
#[must_use]
pub fn decode_block(kind: BcnKind, block: &[u8]) -> [[u8; 4]; 16] {
    // A short block is zero-padded instead of indexing past its end.
    let mut padded = [0u8; 16];
    let block = if block.len() < kind.block_bytes() {
        let n = block.len().min(16);
        padded[..n].copy_from_slice(&block[..n]);
        &padded[..]
    } else {
        block
    };
    match kind {
        BcnKind::Bc1 => bc1_block(block, true),
        BcnKind::Bc3 => {
            let mut px = bc1_block(&block[8..16], false);
            let a = bc4_channel(&block[0..8]);
            for (p, a) in px.iter_mut().zip(a) {
                p[3] = a;
            }
            px
        }
        BcnKind::Bc4 => bc4_channel(&block[0..8]).map(|r| [r, 0, 0, 255]),
        BcnKind::Bc5 => {
            let r = bc4_channel(&block[0..8]);
            let g = bc4_channel(&block[8..16]);
            std::array::from_fn(|i| [r[i], g[i], 0, 255])
        }
        BcnKind::Bc7 => bc7_block(block),
    }
}

fn rgb565(c: u16) -> [u32; 3] {
    let (r, g, b) = (u32::from(c >> 11), u32::from((c >> 5) & 63), u32::from(c & 31));
    [(r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2)]
}

/// BC1 colour block. `three_colour_mode` allows the `c0 <= c1` palette (BC1 only; BC3's colour
/// block always uses four colours).
fn bc1_block(b: &[u8], three_colour_mode: bool) -> [[u8; 4]; 16] {
    let c0 = u16::from_le_bytes([b[0], b[1]]);
    let c1 = u16::from_le_bytes([b[2], b[3]]);
    let (e0, e1) = (rgb565(c0), rgb565(c1));
    let mix = |w0: u32, w1: u32, d: u32| -> [u8; 4] {
        let ch = |i: usize| ((w0 * e0[i] + w1 * e1[i]) / d) as u8;
        [ch(0), ch(1), ch(2), 255]
    };
    let palette = if c0 > c1 || !three_colour_mode {
        [mix(1, 0, 1), mix(0, 1, 1), mix(2, 1, 3), mix(1, 2, 3)]
    } else {
        [mix(1, 0, 1), mix(0, 1, 1), mix(1, 1, 2), [0, 0, 0, 0]]
    };
    let bits = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    std::array::from_fn(|i| palette[((bits >> (2 * i)) & 3) as usize])
}

/// One BC4 channel (also BC3 alpha and each BC5 channel).
fn bc4_channel(b: &[u8]) -> [u8; 16] {
    let (r0, r1) = (u32::from(b[0]), u32::from(b[1]));
    let mut palette = [0u32; 8];
    palette[0] = r0;
    palette[1] = r1;
    if r0 > r1 {
        for (k, p) in palette.iter_mut().enumerate().skip(2) {
            let k = k as u32;
            *p = ((8 - k) * r0 + (k - 1) * r1) / 7;
        }
    } else {
        for (k, p) in palette.iter_mut().enumerate().take(6).skip(2) {
            let k = k as u32;
            *p = ((6 - k) * r0 + (k - 1) * r1) / 5;
        }
        palette[6] = 0;
        palette[7] = 255;
    }
    let mut bits = 0u64;
    for (i, byte) in b[2..8].iter().enumerate() {
        bits |= u64::from(*byte) << (8 * i);
    }
    std::array::from_fn(|i| palette[((bits >> (3 * i)) & 7) as usize] as u8)
}

/// BC7 two-subset partitions: bit `i` is the subset of texel `i` (Khronos DF 1.3 table 114).
const P2: [u16; 64] = [
    0xcccc, 0x8888, 0xeeee, 0xecc8, 0xc880, 0xfeec, 0xfec8, 0xec80, 0xc800, 0xffec, 0xfe80, 0xe800, 0xffe8,
    0xff00, 0xfff0, 0xf000, 0xf710, 0x008e, 0x7100, 0x08ce, 0x008c, 0x7310, 0x3100, 0x8cce, 0x088c, 0x3110,
    0x6666, 0x366c, 0x17e8, 0x0ff0, 0x718e, 0x399c, 0xaaaa, 0xf0f0, 0x5a5a, 0x33cc, 0x3c3c, 0x55aa, 0x9696,
    0xa55a, 0x73ce, 0x13c8, 0x324c, 0x3bdc, 0x6996, 0xc33c, 0x9966, 0x0660, 0x0272, 0x04e4, 0x4e40, 0x2720,
    0xc936, 0x936c, 0x39c6, 0x639c, 0x9336, 0x9cc6, 0x817e, 0xe718, 0xccf0, 0x0fcc, 0x7744, 0xee22,
];

/// BC7 three-subset partitions: bits `2i..2i+2` are the subset of texel `i` (table 115).
const P3: [u32; 64] = [
    0xaa685050, 0x6a5a5040, 0x5a5a4200, 0x5450a0a8, 0xa5a50000, 0xa0a05050, 0x5555a0a0, 0x5a5a5050,
    0xaa550000, 0xaa555500, 0xaaaa5500, 0x90909090, 0x94949494, 0xa4a4a4a4, 0xa9a59450, 0x2a0a4250,
    0xa5945040, 0x0a425054, 0xa5a5a500, 0x55a0a0a0, 0xa8a85454, 0x6a6a4040, 0xa4a45000, 0x1a1a0500,
    0x0050a4a4, 0xaaa59090, 0x14696914, 0x69691400, 0xa08585a0, 0xaa821414, 0x50a4a450, 0x6a5a0200,
    0xa9a58000, 0x5090a0a8, 0xa8a09050, 0x24242424, 0x00aa5500, 0x24924924, 0x24499224, 0x50a50a50,
    0x500aa550, 0xaaaa4444, 0x66660000, 0xa5a0a5a0, 0x50a050a0, 0x69286928, 0x44aaaa44, 0x66666600,
    0xaa444444, 0x54a854a8, 0x95809580, 0x96969600, 0xa85454a8, 0x80959580, 0xaa141414, 0x96960000,
    0xaaaa1414, 0xa05050a0, 0xa0a5a5a0, 0x96000000, 0x40804080, 0xa9a8a9a8, 0xaaaaaa44, 0x2a4a5254,
];

/// Anchor texel of the second subset of a two-subset partition (table 118).
const A2: [u8; 64] = [
    15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 2, 8, 2, 2, 8, 8, 15, 2, 8, 2, 2, 8,
    8, 2, 2, 15, 15, 6, 8, 2, 8, 15, 15, 2, 8, 2, 2, 2, 15, 15, 6, 6, 2, 6, 8, 15, 15, 2, 2, 15, 15, 15, 15,
    15, 2, 2, 15,
];

/// Anchor texel of the second subset of a three-subset partition (table 116).
const A32: [u8; 64] = [
    3, 3, 15, 15, 8, 3, 15, 15, 8, 8, 6, 6, 6, 5, 3, 3, 3, 3, 8, 15, 3, 3, 6, 10, 5, 8, 8, 6, 8, 5, 15, 15,
    8, 15, 3, 5, 6, 10, 8, 15, 15, 3, 15, 5, 15, 15, 15, 15, 3, 15, 5, 5, 5, 8, 5, 10, 5, 10, 8, 13, 15, 12,
    3, 3,
];

/// Anchor texel of the third subset of a three-subset partition (table 117).
const A33: [u8; 64] = [
    15, 8, 8, 3, 15, 15, 3, 8, 15, 15, 15, 15, 15, 15, 15, 8, 15, 8, 15, 3, 15, 8, 15, 8, 3, 15, 6, 10, 15,
    15, 10, 8, 15, 3, 15, 10, 10, 8, 9, 10, 6, 15, 8, 15, 3, 6, 6, 8, 15, 3, 15, 15, 15, 15, 15, 15, 15, 15,
    15, 15, 3, 15, 15, 8,
];

const W2: [u32; 4] = [0, 21, 43, 64];
const W3: [u32; 8] = [0, 9, 18, 27, 37, 46, 55, 64];
const W4: [u32; 16] = [0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64];

/// Per-mode layout: subsets, partition bits, rotation bits, index-selection bits, colour bits,
/// alpha bits, per-endpoint P-bits, shared P-bits, index bits, secondary index bits.
#[derive(Clone, Copy)]
struct Bc7Mode {
    subsets: usize,
    partition_bits: u32,
    rotation_bits: u32,
    index_selection_bits: u32,
    color_bits: u32,
    alpha_bits: u32,
    endpoint_pbits: bool,
    shared_pbits: bool,
    index_bits: u32,
    index2_bits: u32,
}

const MODES: [Bc7Mode; 8] = [
    Bc7Mode {
        subsets: 3,
        partition_bits: 4,
        rotation_bits: 0,
        index_selection_bits: 0,
        color_bits: 4,
        alpha_bits: 0,
        endpoint_pbits: true,
        shared_pbits: false,
        index_bits: 3,
        index2_bits: 0,
    },
    Bc7Mode {
        subsets: 2,
        partition_bits: 6,
        rotation_bits: 0,
        index_selection_bits: 0,
        color_bits: 6,
        alpha_bits: 0,
        endpoint_pbits: false,
        shared_pbits: true,
        index_bits: 3,
        index2_bits: 0,
    },
    Bc7Mode {
        subsets: 3,
        partition_bits: 6,
        rotation_bits: 0,
        index_selection_bits: 0,
        color_bits: 5,
        alpha_bits: 0,
        endpoint_pbits: false,
        shared_pbits: false,
        index_bits: 2,
        index2_bits: 0,
    },
    Bc7Mode {
        subsets: 2,
        partition_bits: 6,
        rotation_bits: 0,
        index_selection_bits: 0,
        color_bits: 7,
        alpha_bits: 0,
        endpoint_pbits: true,
        shared_pbits: false,
        index_bits: 2,
        index2_bits: 0,
    },
    Bc7Mode {
        subsets: 1,
        partition_bits: 0,
        rotation_bits: 2,
        index_selection_bits: 1,
        color_bits: 5,
        alpha_bits: 6,
        endpoint_pbits: false,
        shared_pbits: false,
        index_bits: 2,
        index2_bits: 3,
    },
    Bc7Mode {
        subsets: 1,
        partition_bits: 0,
        rotation_bits: 2,
        index_selection_bits: 0,
        color_bits: 7,
        alpha_bits: 8,
        endpoint_pbits: false,
        shared_pbits: false,
        index_bits: 2,
        index2_bits: 2,
    },
    Bc7Mode {
        subsets: 1,
        partition_bits: 0,
        rotation_bits: 0,
        index_selection_bits: 0,
        color_bits: 7,
        alpha_bits: 7,
        endpoint_pbits: true,
        shared_pbits: false,
        index_bits: 4,
        index2_bits: 0,
    },
    Bc7Mode {
        subsets: 2,
        partition_bits: 6,
        rotation_bits: 0,
        index_selection_bits: 0,
        color_bits: 5,
        alpha_bits: 5,
        endpoint_pbits: true,
        shared_pbits: false,
        index_bits: 2,
        index2_bits: 0,
    },
];

/// LSB-first bit reader over one 128-bit block.
struct Bits {
    value: u128,
    at: u32,
}

impl Bits {
    fn take(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = ((self.value >> self.at) & ((1u128 << n) - 1)) as u32;
        self.at += n;
        v
    }
}

/// Expands a `bits`-bit endpoint (4..=8 bits, every BC7 mode) to 8 bits by replicating its
/// high bits.
fn expand(v: u32, bits: u32) -> u32 {
    if bits >= 8 {
        return v & 255;
    }
    (v << (8 - bits)) | (v >> (2 * bits).saturating_sub(8))
}

fn subset_of(subsets: usize, partition: usize, texel: usize) -> usize {
    match subsets {
        2 => usize::from((P2[partition] >> texel) & 1 == 1),
        3 => ((P3[partition] >> (2 * texel)) & 3) as usize,
        _ => 0,
    }
}

fn is_anchor(subsets: usize, partition: usize, texel: usize) -> bool {
    texel == 0
        || match subsets {
            2 => usize::from(A2[partition]) == texel,
            3 => usize::from(A32[partition]) == texel || usize::from(A33[partition]) == texel,
            _ => false,
        }
}

fn weights(bits: u32) -> &'static [u32] {
    match bits {
        2 => &W2,
        3 => &W3,
        _ => &W4,
    }
}

fn bc7_block(b: &[u8]) -> [[u8; 4]; 16] {
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&b[..16]);
    let value = u128::from_le_bytes(bytes);
    let Some(mode_index) = (0..8).find(|m| (value >> m) & 1 == 1) else {
        // Reserved mode: transparent black (Khronos DF 1.3 BPTC).
        return [[0; 4]; 16];
    };
    let m = MODES[mode_index];
    let mut bits = Bits {
        value,
        at: mode_index as u32 + 1,
    };
    let partition = bits.take(m.partition_bits) as usize;
    let rotation = bits.take(m.rotation_bits);
    let index_selection = bits.take(m.index_selection_bits);
    let endpoints = m.subsets * 2;
    // [endpoint][channel] at stored precision.
    let mut ep = [[0u32; 4]; 6];
    for ch in 0..3 {
        for e in ep.iter_mut().take(endpoints) {
            e[ch] = bits.take(m.color_bits);
        }
    }
    for e in ep.iter_mut().take(endpoints) {
        e[3] = bits.take(m.alpha_bits);
    }
    let (mut color_bits, mut alpha_bits) = (m.color_bits, m.alpha_bits);
    if m.endpoint_pbits || m.shared_pbits {
        let mut pbits = [0u32; 6];
        if m.endpoint_pbits {
            for p in pbits.iter_mut().take(endpoints) {
                *p = bits.take(1);
            }
        } else {
            for s in 0..m.subsets {
                let p = bits.take(1);
                pbits[2 * s] = p;
                pbits[2 * s + 1] = p;
            }
        }
        for (e, p) in ep.iter_mut().zip(&pbits).take(endpoints) {
            for (ch, v) in e.iter_mut().enumerate() {
                if ch < 3 || m.alpha_bits > 0 {
                    *v = (*v << 1) | p;
                }
            }
        }
        color_bits += 1;
        if m.alpha_bits > 0 {
            alpha_bits += 1;
        }
    }
    for e in ep.iter_mut().take(endpoints) {
        for v in e.iter_mut().take(3) {
            *v = expand(*v, color_bits);
        }
        e[3] = if m.alpha_bits > 0 {
            expand(e[3], alpha_bits)
        } else {
            255
        };
    }
    let mut index = [0u32; 16];
    for (t, ix) in index.iter_mut().enumerate() {
        let n = m.index_bits - u32::from(is_anchor(m.subsets, partition, t));
        *ix = bits.take(n);
    }
    let mut index2 = [0u32; 16];
    if m.index2_bits > 0 {
        for (t, ix) in index2.iter_mut().enumerate() {
            *ix = bits.take(m.index2_bits - u32::from(t == 0));
        }
    }
    std::array::from_fn(|t| {
        let s = subset_of(m.subsets, partition, t);
        let (e0, e1) = (ep[2 * s], ep[2 * s + 1]);
        let lerp = |a: u32, b: u32, w: u32| (((64 - w) * a + w * b + 32) >> 6) as u8;
        let (cw, aw) = if m.index2_bits == 0 {
            let w = weights(m.index_bits)[index[t] as usize];
            (w, w)
        } else if index_selection == 0 {
            (
                weights(m.index_bits)[index[t] as usize],
                weights(m.index2_bits)[index2[t] as usize],
            )
        } else {
            (
                weights(m.index2_bits)[index2[t] as usize],
                weights(m.index_bits)[index[t] as usize],
            )
        };
        let mut px = [
            lerp(e0[0], e1[0], cw),
            lerp(e0[1], e1[1], cw),
            lerp(e0[2], e1[2], cw),
            lerp(e0[3], e1[3], aw),
        ];
        match rotation {
            1 => px.swap(0, 3),
            2 => px.swap(1, 3),
            3 => px.swap(2, 3),
            _ => {}
        }
        px
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflowing_level_size_is_truncated_not_a_panic() {
        for kind in [BcnKind::Bc3, BcnKind::Bc5, BcnKind::Bc7] {
            let e = decode_bcn(kind, u32::MAX, u32::MAX, &[]);
            assert!(matches!(e, Err(BcnDecodeError::Truncated { .. })));
        }
    }

    #[test]
    fn short_block_does_not_panic() {
        for kind in [
            BcnKind::Bc1,
            BcnKind::Bc3,
            BcnKind::Bc4,
            BcnKind::Bc5,
            BcnKind::Bc7,
        ] {
            let _ = decode_block(kind, &[1, 2, 3, 4]);
            let _ = decode_block(kind, &[]);
        }
    }

    #[test]
    fn partition_tables_put_anchors_in_their_subsets() {
        for p in 0..64 {
            assert_eq!(subset_of(2, p, 0), 0);
            assert_eq!(subset_of(2, p, usize::from(A2[p])), 1, "2-subset {p}");
            assert_eq!(subset_of(3, p, 0), 0);
            assert_eq!(subset_of(3, p, usize::from(A32[p])), 1, "3-subset {p} second");
            assert_eq!(subset_of(3, p, usize::from(A33[p])), 2, "3-subset {p} third");
        }
    }

    #[test]
    fn bc1_endpoints_and_thirds() {
        // c0 = pure red (0xF800), c1 = black; indices 0,1,2,3 on the first row.
        let block = [0x00, 0xF8, 0x00, 0x00, 0b1110_0100, 0, 0, 0];
        let px = decode_block(BcnKind::Bc1, &block);
        assert_eq!(px[0], [255, 0, 0, 255]);
        assert_eq!(px[1], [0, 0, 0, 255]);
        assert_eq!(px[2], [170, 0, 0, 255]);
        assert_eq!(px[3], [85, 0, 0, 255]);
    }

    #[test]
    fn bc1_three_colour_mode_has_transparent_black() {
        // c0 <= c1 selects the 3-colour palette; index 3 is transparent.
        let block = [0x00, 0x00, 0x00, 0xF8, 0b1110_0100, 0, 0, 0];
        let px = decode_block(BcnKind::Bc1, &block);
        assert_eq!(px[2], [127, 0, 0, 255]);
        assert_eq!(px[3], [0, 0, 0, 0]);
    }

    #[test]
    fn bc4_six_and_four_value_palettes() {
        // r0 > r1: indices 2, 7, 3 on the first three texels (bits 0x08FA).
        let six = decode_block(BcnKind::Bc4, &[210, 0, 0xFA, 0x08, 0, 0, 0, 0]);
        assert_eq!([six[0][0], six[1][0], six[2][0]], [180, 30, 150]);
        assert_eq!(six[0][1..], [0, 0, 255]);
        // r0 <= r1: index 6 is 0, index 7 is 255, index 0 is r0 (bits 0x3E).
        let four = decode_block(BcnKind::Bc4, &[10, 200, 0x3E, 0, 0, 0, 0, 0]);
        assert_eq!([four[0][0], four[1][0], four[2][0]], [0, 255, 10]);
    }

    #[test]
    fn bc7_mode6_solid_block() {
        // Mode 6: bit 6 set; endpoints 7 bits + p-bit. All endpoints 127 with p-bit 1 -> 255.
        let mut v: u128 = 1 << 6;
        let mut at = 7u32;
        let mut put = |value: u128, n: u32| {
            v |= value << at;
            at += n;
        };
        for _ in 0..8 {
            put(127, 7); // R0 R1 G0 G1 B0 B1 A0 A1
        }
        put(1, 1);
        put(1, 1);
        let px = decode_block(BcnKind::Bc7, &v.to_le_bytes());
        assert!(px.iter().all(|p| *p == [255, 255, 255, 255]));
    }

    #[test]
    fn reserved_bc7_mode_is_transparent_black() {
        let px = decode_block(BcnKind::Bc7, &[0u8; 16]);
        assert!(px.iter().all(|p| *p == [0; 4]));
    }

    #[test]
    fn crops_partial_blocks_and_rejects_short_levels() {
        let solid = [0x00, 0xF8, 0x00, 0xF8, 0, 0, 0, 0];
        let rgba = decode_bcn(BcnKind::Bc1, 2, 3, &solid).expect("decode");
        assert_eq!(rgba.len(), 2 * 3 * 4);
        assert!(rgba.chunks(4).all(|p| p == [255, 0, 0, 255]));
        assert!(matches!(
            decode_bcn(BcnKind::Bc7, 8, 8, &[0; 16]),
            Err(BcnDecodeError::Truncated { need: 64, .. })
        ));
    }
}
