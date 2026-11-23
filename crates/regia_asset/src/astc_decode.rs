// SPDX-License-Identifier: MPL-2.0
//! CPU decoder for ASTC LDR blocks (2D footprints 4×4 to 12×12), for adapters that cannot sample
//! ASTC (`docs/specs/12-assets.md` § 7.1, ADR-2063). The renderer decodes an Android-cooked map to
//! RGBA8 only when the device lacks `TEXTURE_COMPRESSION_ASTC`; every other phone uploads the
//! blocks as they are.
//!
//! Original code written from the Khronos Data Format Specification 1.3, chapter "ASTC
//! Compressed Texture Image Formats": block mode layout, integer sequence encoding (trit and quint
//! packing), colour and weight unquantization, the LDR colour endpoint modes, weight infill, the
//! partition hash and the void-extent block. Decoded texels follow the spec's "unorm8" decode:
//! endpoints expanded to 16 bits (`(e << 8) | 0x80` for sRGB, `(e << 8) | e` for linear),
//! interpolated with 6-bit weights, the top 8 bits kept. They are the texels before any sRGB
//! decode, like the other decoders in this crate.
//!
//! HDR endpoint modes and HDR void-extent blocks are not LDR content: like an LDR-profile decoder
//! they come out as the error colour (opaque magenta; for an HDR endpoint mode, the texels of its
//! partition), as do reserved or invalid blocks.

use thiserror::Error;

/// The error colour of an invalid or HDR block (opaque magenta).
pub const ERROR_COLOR: [u8; 4] = [255, 0, 255, 255];

/// The 2D block footprints ASTC defines (width, height).
pub const FOOTPRINTS: [(u32, u32); 14] = [
    (4, 4),
    (5, 4),
    (5, 5),
    (6, 5),
    (6, 6),
    (8, 5),
    (8, 6),
    (8, 8),
    (10, 5),
    (10, 6),
    (10, 8),
    (10, 10),
    (12, 10),
    (12, 12),
];

/// Errors from [`decode_astc`].
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum AstcDecodeError {
    /// The block footprint is not one of [`FOOTPRINTS`].
    #[error("{0}x{1} is not an ASTC 2D block footprint")]
    Footprint(u32, u32),
    /// The level holds fewer bytes than its blocks need.
    #[error("ASTC {block_w}x{block_h} level {width}x{height} needs {need} bytes, got {got}")]
    Truncated {
        /// Block width.
        block_w: u32,
        /// Block height.
        block_h: u32,
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

/// Decodes one `width`×`height` level of ASTC blocks (`block_w`×`block_h` footprint, rows of
/// `ceil(width / block_w)` blocks, 16 bytes each) into row-major RGBA8, cropped to the level.
/// `srgb` selects the sRGB endpoint expansion (the texels are still sRGB-encoded).
///
/// # Errors
///
/// The footprint is not an ASTC 2D footprint, or `data` is shorter than the level's blocks.
pub fn decode_astc(
    width: u32,
    height: u32,
    (block_w, block_h): (u32, u32),
    srgb: bool,
    data: &[u8],
) -> Result<Vec<u8>, AstcDecodeError> {
    if !FOOTPRINTS.contains(&(block_w, block_h)) {
        return Err(AstcDecodeError::Footprint(block_w, block_h));
    }
    let (w, h) = (width.max(1) as usize, height.max(1) as usize);
    let (bw, bh) = (block_w as usize, block_h as usize);
    let (cols, rows) = (w.div_ceil(bw), h.div_ceil(bh));
    let need = cols.checked_mul(rows).and_then(|n| n.checked_mul(16));
    if need.is_none_or(|n| data.len() < n) {
        return Err(AstcDecodeError::Truncated {
            block_w,
            block_h,
            width,
            height,
            need: need.unwrap_or(usize::MAX),
            got: data.len(),
        });
    }
    let mut out = vec![0u8; w * h * 4];
    let mut texels = [[0u8; 4]; 144];
    for by in 0..rows {
        for bx in 0..cols {
            let at = (by * cols + bx) * 16;
            let mut block = [0u8; 16];
            block.copy_from_slice(&data[at..at + 16]);
            decode_block(&block, (block_w, block_h), srgb, &mut texels);
            for ty in 0..bh {
                let y = by * bh + ty;
                if y >= h {
                    break;
                }
                for tx in 0..bw {
                    let x = bx * bw + tx;
                    if x >= w {
                        break;
                    }
                    let o = (y * w + x) * 4;
                    out[o..o + 4].copy_from_slice(&texels[ty * bw + tx]);
                }
            }
        }
    }
    Ok(out)
}

/// Decodes one 16-byte block of footprint `block_w`×`block_h` into `out[..block_w * block_h]`
/// (row-major RGBA8). Invalid, reserved and HDR blocks fill it with [`ERROR_COLOR`]. A footprint
/// that is not in [`FOOTPRINTS`] (or a too-short `out`) leaves `out` untouched.
pub fn decode_block(block: &[u8; 16], (block_w, block_h): (u32, u32), srgb: bool, out: &mut [[u8; 4]]) {
    let texel_count = (block_w * block_h) as usize;
    if !FOOTPRINTS.contains(&(block_w, block_h)) || out.len() < texel_count {
        return;
    }
    let out = &mut out[..texel_count];
    if decode_block_inner(u128::from_le_bytes(*block), block_w, block_h, srgb, out).is_none() {
        out.fill(ERROR_COLOR);
    }
}

/// Bits `start..start + count` of `bits` (`count` ≤ 32).
fn field(bits: u128, start: u32, count: u32) -> u32 {
    if count == 0 || start >= 128 {
        return 0;
    }
    ((bits >> start) & ((1u128 << count) - 1)) as u32
}

/// An integer-sequence-encoding range: `trits` or `quints` (at most one) and `bits` low bits.
#[derive(Clone, Copy)]
struct Quant {
    trits: bool,
    quints: bool,
    bits: u32,
}

/// The quantization ranges in increasing number of levels (2, 3, 4, 5, 6, 8, 10, 12, 16, 20, 24,
/// 32, 40, 48, 64, 80, 96, 128, 160, 192, 256).
const QUANTS: [Quant; 21] = {
    const fn q(trits: bool, quints: bool, bits: u32) -> Quant {
        Quant { trits, quints, bits }
    }
    [
        q(false, false, 1),
        q(true, false, 0),
        q(false, false, 2),
        q(false, true, 0),
        q(true, false, 1),
        q(false, false, 3),
        q(false, true, 1),
        q(true, false, 2),
        q(false, false, 4),
        q(false, true, 2),
        q(true, false, 3),
        q(false, false, 5),
        q(false, true, 3),
        q(true, false, 4),
        q(false, false, 6),
        q(false, true, 4),
        q(true, false, 5),
        q(false, false, 7),
        q(false, true, 5),
        q(true, false, 6),
        q(false, false, 8),
    ]
};

/// Index of the 6-level range in [`QUANTS`]: the least colour precision a block may use.
const MIN_COLOR_QUANT: usize = 4;

/// Bits an integer sequence of `count` values in `q` takes.
fn ise_bits(q: Quant, count: u32) -> u32 {
    let mut bits = count * q.bits;
    if q.trits {
        bits += (8 * count).div_ceil(5);
    }
    if q.quints {
        bits += (7 * count).div_ceil(3);
    }
    bits
}

/// Reads `count` values of range `q` packed from bit `start` of `bits` into `out` (each value
/// `(trit or quint) << q.bits | low bits`). Bits past the sequence's own length read as zero.
fn decode_ise(bits: u128, start: u32, count: usize, q: Quant, out: &mut [u32]) {
    let end = start + ise_bits(q, count as u32);
    let read = |pos: &mut u32, n: u32| -> u32 {
        let avail = end.saturating_sub(*pos).min(n);
        let v = field(bits, *pos, avail);
        *pos += n;
        v
    };
    let mut pos = start;
    let b = q.bits;
    if q.trits {
        for group in out[..count].chunks_mut(5) {
            let mut m = [0u32; 5];
            let mut t = 0u32;
            m[0] = read(&mut pos, b);
            t |= read(&mut pos, 2);
            m[1] = read(&mut pos, b);
            t |= read(&mut pos, 2) << 2;
            m[2] = read(&mut pos, b);
            t |= read(&mut pos, 1) << 4;
            m[3] = read(&mut pos, b);
            t |= read(&mut pos, 2) << 5;
            m[4] = read(&mut pos, b);
            t |= read(&mut pos, 1) << 7;
            let trits = unpack_trits(t);
            for (i, v) in group.iter_mut().enumerate() {
                *v = (trits[i] << b) | m[i];
            }
        }
    } else if q.quints {
        for group in out[..count].chunks_mut(3) {
            let mut m = [0u32; 3];
            let mut qv = 0u32;
            m[0] = read(&mut pos, b);
            qv |= read(&mut pos, 3);
            m[1] = read(&mut pos, b);
            qv |= read(&mut pos, 2) << 3;
            m[2] = read(&mut pos, b);
            qv |= read(&mut pos, 2) << 5;
            let quints = unpack_quints(qv);
            for (i, v) in group.iter_mut().enumerate() {
                *v = (quints[i] << b) | m[i];
            }
        }
    } else {
        for v in &mut out[..count] {
            *v = read(&mut pos, b);
        }
    }
}

/// Bit `i` of `v`.
fn bit(v: u32, i: u32) -> u32 {
    (v >> i) & 1
}

/// The five trits an 8-bit packed trit block holds (spec "Integer Sequence Encoding").
fn unpack_trits(t: u32) -> [u32; 5] {
    let (c, t3, t4);
    if (t >> 2) & 0b111 == 0b111 {
        c = ((t >> 5) & 0b111) << 2 | (t & 0b11);
        t4 = 2;
        t3 = 2;
    } else {
        c = t & 0x1f;
        if (t >> 5) & 0b11 == 0b11 {
            t4 = 2;
            t3 = bit(t, 7);
        } else {
            t4 = bit(t, 7);
            t3 = (t >> 5) & 0b11;
        }
    }
    let (t0, t1, t2);
    if c & 0b11 == 0b11 {
        t2 = 2;
        t1 = bit(c, 4);
        t0 = (bit(c, 3) << 1) | (bit(c, 2) & !bit(c, 3) & 1);
    } else if (c >> 2) & 0b11 == 0b11 {
        t2 = 2;
        t1 = 2;
        t0 = c & 0b11;
    } else {
        t2 = bit(c, 4);
        t1 = (c >> 2) & 0b11;
        t0 = (bit(c, 1) << 1) | (bit(c, 0) & !bit(c, 1) & 1);
    }
    [t0, t1, t2, t3, t4]
}

/// The three quints a 7-bit packed quint block holds.
fn unpack_quints(q: u32) -> [u32; 3] {
    let (q0, q1, q2);
    if (q >> 1) & 0b11 == 0b11 && (q >> 5) & 0b11 == 0 {
        let n0 = !bit(q, 0) & 1;
        q2 = (bit(q, 0) << 2) | ((bit(q, 4) & n0) << 1) | (bit(q, 3) & n0);
        q1 = 4;
        q0 = 4;
    } else {
        let c;
        if (q >> 1) & 0b11 == 0b11 {
            q2 = 4;
            c = (((q >> 3) & 0b11) << 3) | ((!(q >> 5) & 0b11) << 1) | bit(q, 0);
        } else {
            q2 = (q >> 5) & 0b11;
            c = q & 0x1f;
        }
        if c & 0b111 == 0b101 {
            q1 = 4;
            q0 = (c >> 3) & 0b11;
        } else {
            q1 = (c >> 3) & 0b11;
            q0 = c & 0b111;
        }
    }
    [q0, q1, q2]
}

/// Bit-replicates the low `bits` bits of `v` to `to` bits.
fn replicate(v: u32, bits: u32, to: u32) -> u32 {
    if bits == 0 {
        return 0;
    }
    let mut out = 0u32;
    let mut have = 0u32;
    while have < to {
        out = (out << bits) | v;
        have += bits;
    }
    out >> (have - to)
}

/// A quantized colour endpoint value back to 0..=255.
fn unquantize_color(q: Quant, v: u32) -> u32 {
    if !q.trits && !q.quints {
        return replicate(v, q.bits, 8);
    }
    let m = v & ((1 << q.bits) - 1);
    let d = v >> q.bits;
    let a = if m & 1 == 1 { 0x1ff } else { 0 };
    let (b1, c1, d1, e1, f1) = (bit(m, 1), bit(m, 2), bit(m, 3), bit(m, 4), bit(m, 5));
    let (bb, cc) = match (q.trits, q.bits) {
        (true, 1) => (0, 204),
        (false, 1) => (0, 113),
        (true, 2) => (b1 * 0x116, 93),
        (false, 2) => (b1 * 0x10c, 54),
        (true, 3) => (c1 * 0x10a + b1 * 0x085, 44),
        (false, 3) => (c1 * 0x105 + b1 * 0x082, 26),
        (true, 4) => (d1 * 0x104 + c1 * 0x082 + b1 * 0x041, 22),
        (false, 4) => (d1 * 0x102 + c1 * 0x081 + b1 * 0x040, 13),
        (true, 5) => (e1 * 0x102 + d1 * 0x081 + c1 * 0x040 + b1 * 0x020, 11),
        (false, 5) => (e1 * 0x101 + d1 * 0x080 + c1 * 0x040 + b1 * 0x020, 6),
        (true, _) => (f1 * 0x101 + e1 * 0x080 + d1 * 0x040 + c1 * 0x020 + b1 * 0x010, 5),
        (false, _) => (0, 0),
    };
    let t = (d * cc + bb) ^ a;
    (a & 0x80) | (t >> 2)
}

/// A quantized weight back to 0..=64.
fn unquantize_weight(q: Quant, v: u32) -> u32 {
    let raw = if !q.trits && !q.quints {
        replicate(v, q.bits, 6)
    } else if q.bits == 0 {
        match (q.trits, v) {
            (true, 0) => 0,
            (true, 1) => 32,
            (true, _) => 63,
            (false, 0) => 0,
            (false, 1) => 16,
            (false, 2) => 32,
            (false, 3) => 47,
            (false, _) => 63,
        }
    } else {
        let m = v & ((1 << q.bits) - 1);
        let d = v >> q.bits;
        let a = if m & 1 == 1 { 0x7f } else { 0 };
        let (b1, c1) = (bit(m, 1), bit(m, 2));
        let (bb, cc) = match (q.trits, q.bits) {
            (true, 1) => (0, 50),
            (false, 1) => (0, 28),
            (true, 2) => (b1 * 0x45, 23),
            (false, 2) => (b1 * 0x42, 13),
            (true, _) => (c1 * 0x42 + b1 * 0x21, 11),
            (false, _) => (0, 0),
        };
        let t = (d * cc + bb) ^ a;
        (a & 0x20) | (t >> 2)
    };
    if raw > 32 { raw + 1 } else { raw }
}

/// The weight grid a block mode describes.
struct BlockMode {
    grid_w: u32,
    grid_h: u32,
    dual_plane: bool,
    weight_quant: Quant,
}

/// Decodes the 11-bit block mode (`None` for reserved encodings and the void-extent pattern).
fn block_mode(mode: u32) -> Option<BlockMode> {
    let (r, high, dual, w, h);
    if mode & 0b11 != 0 {
        r = bit(mode, 4) | ((mode & 0b11) << 1);
        high = bit(mode, 9);
        dual = bit(mode, 10);
        let a = (mode >> 5) & 0b11;
        let b = (mode >> 7) & 0b11;
        match (mode >> 2) & 0b11 {
            0 => (w, h) = (b + 4, a + 2),
            1 => (w, h) = (b + 8, a + 2),
            2 => (w, h) = (a + 2, b + 8),
            _ if bit(mode, 8) == 0 => (w, h) = (a + 2, bit(mode, 7) + 6),
            _ => (w, h) = (bit(mode, 7) + 2, a + 2),
        }
    } else {
        r = bit(mode, 4) | (((mode >> 2) & 0b11) << 1);
        if (mode >> 2) & 0b11 == 0 {
            return None;
        }
        let a = (mode >> 5) & 0b11;
        match (mode >> 7) & 0b11 {
            0 => {
                (w, h) = (12, a + 2);
                (high, dual) = (bit(mode, 9), bit(mode, 10));
            }
            1 => {
                (w, h) = (a + 2, 12);
                (high, dual) = (bit(mode, 9), bit(mode, 10));
            }
            2 => {
                (w, h) = (a + 6, ((mode >> 9) & 0b11) + 6);
                (high, dual) = (0, 0);
            }
            _ => {
                match a {
                    0 => (w, h) = (6, 10),
                    1 => (w, h) = (10, 6),
                    _ => return None,
                }
                (high, dual) = (bit(mode, 9), bit(mode, 10));
            }
        }
    }
    let index = (r - 2 + 6 * high) as usize;
    Some(BlockMode {
        grid_w: w,
        grid_h: h,
        dual_plane: dual == 1,
        weight_quant: QUANTS[index],
    })
}

/// Decodes a block into `out`; `None` when it is invalid, reserved or HDR (the caller fills the
/// error colour).
fn decode_block_inner(bits: u128, block_w: u32, block_h: u32, srgb: bool, out: &mut [[u8; 4]]) -> Option<()> {
    if field(bits, 0, 9) == 0x1fc {
        return void_extent(bits, out);
    }
    let mode = block_mode(field(bits, 0, 11))?;
    let partitions = field(bits, 11, 2) + 1;
    let weight_count = mode.grid_w * mode.grid_h * (1 + u32::from(mode.dual_plane));
    if mode.grid_w > block_w || mode.grid_h > block_h || weight_count > 64 {
        return None;
    }
    if mode.dual_plane && partitions == 4 {
        return None;
    }
    let weight_bits = ise_bits(mode.weight_quant, weight_count);
    if !(24..=96).contains(&weight_bits) {
        return None;
    }

    // Colour endpoint modes: one for the block, or one per partition with the extra bits just
    // below the weights.
    let mut below_weights = 128 - weight_bits;
    let mut cems = [0u32; 4];
    let color_start;
    if partitions == 1 {
        cems[0] = field(bits, 13, 4);
        color_start = 17;
    } else {
        color_start = 29;
        let low = field(bits, 23, 6);
        if low & 0b11 == 0 {
            let cem = low >> 2;
            cems[..partitions as usize].fill(cem);
        } else {
            let extra = 3 * partitions - 4;
            below_weights = below_weights.checked_sub(extra)?;
            let encoded = low | (field(bits, below_weights, extra) << 6);
            let base = (encoded & 0b11) - 1;
            for p in 0..partitions {
                let class = bit(encoded, 2 + p);
                let m = (encoded >> (2 + partitions + 2 * p)) & 0b11;
                cems[p as usize] = ((base + class) << 2) | m;
            }
        }
    }
    let mut plane2_component = None;
    if mode.dual_plane {
        below_weights = below_weights.checked_sub(2)?;
        plane2_component = Some(field(bits, below_weights, 2) as usize);
    }
    let cems = &cems[..partitions as usize];
    // HDR endpoint modes are an error in an LDR decode: the texels of such a partition take the
    // error colour, the other partitions decode (as the reference decoder does).
    let mut hdr = [false; 4];
    for (h, &c) in hdr.iter_mut().zip(cems) {
        *h = matches!(c, 2 | 3 | 7 | 11 | 14 | 15);
    }
    let value_count: u32 = cems.iter().map(|&c| ((c >> 2) + 1) * 2).sum();
    if value_count > 18 {
        return None;
    }
    let color_bits = below_weights.checked_sub(color_start)?;
    let color_quant = (0..QUANTS.len())
        .rev()
        .find(|&i| ise_bits(QUANTS[i], value_count) <= color_bits)?;
    if color_quant < MIN_COLOR_QUANT {
        return None;
    }
    let cq = QUANTS[color_quant];
    let mut raw = [0u32; 18];
    decode_ise(bits, color_start, value_count as usize, cq, &mut raw);
    let mut endpoints = [[[0u32; 4]; 2]; 4];
    let mut at = 0usize;
    for (p, &cem) in cems.iter().enumerate() {
        let n = (((cem >> 2) + 1) * 2) as usize;
        let mut v = [0i32; 8];
        for (dst, &src) in v.iter_mut().zip(&raw[at..at + n]) {
            *dst = unquantize_color(cq, src) as i32;
        }
        at += n;
        endpoints[p] = decode_endpoints(cem, v);
    }

    // Weights: stored bit-reversed from the top of the block.
    let reversed = bits.reverse_bits();
    let mut weights_q = [0u32; 64];
    decode_ise(
        reversed,
        0,
        weight_count as usize,
        mode.weight_quant,
        &mut weights_q,
    );
    let mut weights = [0u32; 64];
    for (w, &q) in weights.iter_mut().zip(&weights_q[..weight_count as usize]) {
        *w = unquantize_weight(mode.weight_quant, q);
    }

    let seed = field(bits, 13, 10);
    let small = block_w * block_h < 31;
    for y in 0..block_h {
        for x in 0..block_w {
            let p = if partitions > 1 {
                select_partition(seed, x, y, partitions, small)
            } else {
                0
            };
            if hdr[p] {
                out[(y * block_w + x) as usize] = ERROR_COLOR;
                continue;
            }
            let [e0, e1] = endpoints[p];
            let w0 = infill(&weights, &mode, 0, (block_w, block_h), (x, y));
            let w1 = if mode.dual_plane {
                infill(&weights, &mode, 1, (block_w, block_h), (x, y))
            } else {
                w0
            };
            let texel = &mut out[(y * block_w + x) as usize];
            for c in 0..4 {
                let w = if plane2_component == Some(c) { w1 } else { w0 };
                let expand = |e: u32| if srgb { (e << 8) | 0x80 } else { (e << 8) | e };
                let (c0, c1) = (expand(e0[c]), expand(e1[c]));
                let v = (c0 * (64 - w) + c1 * w + 32) >> 6;
                texel[c] = (v >> 8) as u8;
            }
        }
    }
    Some(())
}

/// A void-extent (constant colour) block: four 16-bit UNORM channels in the top 64 bits. An HDR
/// one (bit 9) is an error in an LDR decode, as are clear reserved bits (10, 11) and an extent
/// (unless it is all ones, "no extent") whose minimum is not below its maximum.
fn void_extent(bits: u128, out: &mut [[u8; 4]]) -> Option<()> {
    if field(bits, 9, 1) == 1 || field(bits, 10, 2) != 0b11 {
        return None;
    }
    let (s_min, s_max, t_min, t_max) = (
        field(bits, 12, 13),
        field(bits, 25, 13),
        field(bits, 38, 13),
        field(bits, 51, 13),
    );
    let none = 0x1fff;
    let no_extent = s_min == none && s_max == none && t_min == none && t_max == none;
    if !no_extent && (s_min >= s_max || t_min >= t_max) {
        return None;
    }
    let mut texel = [0u8; 4];
    for (c, t) in texel.iter_mut().enumerate() {
        *t = (field(bits, 64 + 16 * c as u32, 16) >> 8) as u8;
    }
    out.fill(texel);
    Some(())
}

/// The 6-bit `value`-bit transfer of the base+offset modes: returns `(offset, base)`.
fn bit_transfer_signed(a: i32, b: i32) -> (i32, i32) {
    let b = (b >> 1) | (a & 0x80);
    let mut a = (a >> 1) & 0x3f;
    if a & 0x20 != 0 {
        a -= 0x40;
    }
    (a, b)
}

/// `(r, g, b, a)` with red and green pulled towards blue (the "blue contraction").
fn blue_contract(c: [i32; 4]) -> [i32; 4] {
    [(c[0] + c[2]) >> 1, (c[1] + c[2]) >> 1, c[2], c[3]]
}

/// The two RGBA8 endpoints an LDR colour endpoint mode decodes `v` into.
fn decode_endpoints(cem: u32, v: [i32; 8]) -> [[u32; 4]; 2] {
    let (e0, e1): ([i32; 4], [i32; 4]) = match cem {
        0 => ([v[0], v[0], v[0], 255], [v[1], v[1], v[1], 255]),
        1 => {
            let l0 = (v[0] >> 2) | (v[1] & 0xc0);
            let l1 = (l0 + (v[1] & 0x3f)).min(255);
            ([l0, l0, l0, 255], [l1, l1, l1, 255])
        }
        4 => ([v[0], v[0], v[0], v[2]], [v[1], v[1], v[1], v[3]]),
        5 => {
            let (o0, b0) = bit_transfer_signed(v[1], v[0]);
            let (o1, b1) = bit_transfer_signed(v[3], v[2]);
            ([b0, b0, b0, b1], [b0 + o0, b0 + o0, b0 + o0, b1 + o1])
        }
        6 => (
            [(v[0] * v[3]) >> 8, (v[1] * v[3]) >> 8, (v[2] * v[3]) >> 8, 255],
            [v[0], v[1], v[2], 255],
        ),
        8 | 12 => {
            let (a0, a1) = if cem == 12 { (v[6], v[7]) } else { (255, 255) };
            if v[1] + v[3] + v[5] >= v[0] + v[2] + v[4] {
                ([v[0], v[2], v[4], a0], [v[1], v[3], v[5], a1])
            } else {
                (
                    blue_contract([v[1], v[3], v[5], a1]),
                    blue_contract([v[0], v[2], v[4], a0]),
                )
            }
        }
        9 | 13 => {
            let (or, br) = bit_transfer_signed(v[1], v[0]);
            let (og, bg) = bit_transfer_signed(v[3], v[2]);
            let (ob, bb) = bit_transfer_signed(v[5], v[4]);
            let (oa, ba) = if cem == 13 {
                bit_transfer_signed(v[7], v[6])
            } else {
                (0, 255)
            };
            if or + og + ob >= 0 {
                ([br, bg, bb, ba], [br + or, bg + og, bb + ob, ba + oa])
            } else {
                (
                    blue_contract([br + or, bg + og, bb + ob, ba + oa]),
                    blue_contract([br, bg, bb, ba]),
                )
            }
        }
        10 => (
            [(v[0] * v[3]) >> 8, (v[1] * v[3]) >> 8, (v[2] * v[3]) >> 8, v[4]],
            [v[0], v[1], v[2], v[5]],
        ),
        // HDR modes are rejected before endpoint decoding.
        _ => ([255, 0, 255, 255], [255, 0, 255, 255]),
    };
    let clamp = |c: [i32; 4]| c.map(|x| x.clamp(0, 255) as u32);
    [clamp(e0), clamp(e1)]
}

/// The weight (0..=64) of plane `plane` at texel `(x, y)`: the grid bilinearly infilled to the
/// block footprint (spec "Weight Infill").
fn infill(
    weights: &[u32; 64],
    mode: &BlockMode,
    plane: usize,
    (block_w, block_h): (u32, u32),
    (x, y): (u32, u32),
) -> u32 {
    let (gw, gh) = (mode.grid_w, mode.grid_h);
    let planes = 1 + usize::from(mode.dual_plane);
    let at = |gx: u32, gy: u32| -> u32 {
        let i = (gy * gw + gx) as usize * planes + plane;
        weights.get(i).copied().unwrap_or(0)
    };
    let ds = (1024 + block_w / 2) / (block_w - 1);
    let dt = (1024 + block_h / 2) / (block_h - 1);
    let gs = (ds * x * (gw - 1) + 32) >> 6;
    let gt = (dt * y * (gh - 1) + 32) >> 6;
    let (js, fs, jt, ft) = (gs >> 4, gs & 0xf, gt >> 4, gt & 0xf);
    let w11 = (fs * ft + 8) >> 4;
    let w10 = ft - w11;
    let w01 = fs - w11;
    let w00 = 16 + w11 - fs - ft;
    let p00 = at(js, jt);
    let p01 = if w01 > 0 || w11 > 0 { at(js + 1, jt) } else { 0 };
    let p10 = if w10 > 0 || w11 > 0 { at(js, jt + 1) } else { 0 };
    let p11 = if w11 > 0 { at(js + 1, jt + 1) } else { 0 };
    (p00 * w00 + p01 * w01 + p10 * w10 + p11 * w11 + 8) >> 4
}

/// The spec's 32-bit hash of the partition seed.
fn hash52(mut p: u32) -> u32 {
    p ^= p >> 15;
    p = p.wrapping_sub(p << 17);
    p = p.wrapping_add(p << 7);
    p = p.wrapping_add(p << 4);
    p ^= p >> 5;
    p = p.wrapping_add(p << 16);
    p ^= p >> 7;
    p ^= p >> 3;
    p ^= p << 6;
    p ^= p >> 17;
    p
}

/// The partition (0..partitions) texel `(x, y)` belongs to, for the 10-bit partition `seed`.
fn select_partition(seed: u32, x: u32, y: u32, partitions: u32, small: bool) -> usize {
    let (x, y, z) = if small {
        (x << 1, y << 1, 0u32)
    } else {
        (x, y, 0u32)
    };
    let seed = seed + (partitions - 1) * 1024;
    let rnum = hash52(seed);
    let mut s = [
        rnum & 0xf,
        (rnum >> 4) & 0xf,
        (rnum >> 8) & 0xf,
        (rnum >> 12) & 0xf,
        (rnum >> 16) & 0xf,
        (rnum >> 20) & 0xf,
        (rnum >> 24) & 0xf,
        (rnum >> 28) & 0xf,
        (rnum >> 18) & 0xf,
        (rnum >> 22) & 0xf,
        (rnum >> 26) & 0xf,
        rnum.rotate_left(2) & 0xf,
    ];
    for v in &mut s {
        *v *= *v;
    }
    let (sh1, sh2) = if seed & 1 == 1 {
        (
            if seed & 2 != 0 { 4 } else { 5 },
            if partitions == 3 { 6 } else { 5 },
        )
    } else {
        (
            if partitions == 3 { 6 } else { 5 },
            if seed & 2 != 0 { 4 } else { 5 },
        )
    };
    let sh3 = if seed & 0x10 != 0 { sh1 } else { sh2 };
    for (i, v) in s.iter_mut().enumerate().take(8) {
        *v >>= if i % 2 == 0 { sh1 } else { sh2 };
    }
    for v in &mut s[8..] {
        *v >>= sh3;
    }
    let a = (s[0] * x + s[1] * y + s[10] * z + (rnum >> 14)) & 0x3f;
    let b = (s[2] * x + s[3] * y + s[11] * z + (rnum >> 10)) & 0x3f;
    let mut c = (s[4] * x + s[5] * y + s[8] * z + (rnum >> 6)) & 0x3f;
    let mut d = (s[6] * x + s[7] * y + s[9] * z + (rnum >> 2)) & 0x3f;
    if partitions < 4 {
        d = 0;
    }
    if partitions < 3 {
        c = 0;
    }
    if a >= b && a >= c && a >= d {
        0
    } else if b >= c && b >= d {
        1
    } else if c >= d {
        2
    } else {
        3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A void-extent block of one 16-bit RGBA colour.
    fn void_block(rgba16: [u16; 4]) -> [u8; 16] {
        let mut bits: u128 = 0x1fc | (0b11 << 10);
        // All-ones extent coordinates: "no extent".
        bits |= ((1u128 << 52) - 1) << 12;
        for (c, v) in rgba16.iter().enumerate() {
            bits |= u128::from(*v) << (64 + 16 * c);
        }
        bits.to_le_bytes()
    }

    #[test]
    fn a_void_extent_block_is_its_colour_everywhere() {
        let block = void_block([200 * 257, 10 * 257, 0, 65535]);
        let out = decode_astc(6, 6, (6, 6), false, &block).expect("decode");
        assert!(out.as_chunks::<4>().0.iter().all(|p| *p == [200, 10, 0, 255]));
        // An HDR void extent is an error in an LDR decode.
        let mut hdr = u128::from_le_bytes(block);
        hdr |= 1 << 9;
        let out = decode_astc(4, 4, (4, 4), false, &hdr.to_le_bytes()).expect("decode");
        assert!(out.as_chunks::<4>().0.iter().all(|p| *p == ERROR_COLOR));
    }

    #[test]
    fn reserved_blocks_decode_as_the_error_colour() {
        // Block mode bits all zero: reserved.
        let out = decode_astc(4, 4, (4, 4), true, &[0u8; 16]).expect("decode");
        assert!(out.as_chunks::<4>().0.iter().all(|p| *p == ERROR_COLOR));
    }

    #[test]
    fn bad_footprints_and_short_data_are_errors() {
        assert_eq!(
            decode_astc(8, 8, (7, 7), false, &[0; 16]),
            Err(AstcDecodeError::Footprint(7, 7))
        );
        assert!(matches!(
            decode_astc(8, 8, (4, 4), false, &[0; 63]),
            Err(AstcDecodeError::Truncated {
                need: 64,
                got: 63,
                ..
            })
        ));
        // A partial edge block still needs its whole block.
        assert!(decode_astc(5, 4, (4, 4), false, &[0; 32]).is_ok());
    }

    #[test]
    fn trits_and_quints_unpack_every_combination() {
        // Every packed value maps into range, and every combination is reachable.
        let mut seen = std::collections::BTreeSet::new();
        for t in 0..256 {
            let trits = unpack_trits(t);
            assert!(trits.iter().all(|&v| v < 3), "{t}: {trits:?}");
            seen.insert(trits);
        }
        assert_eq!(seen.len(), 243);
        let mut seen = std::collections::BTreeSet::new();
        for q in 0..128 {
            let quints = unpack_quints(q);
            assert!(quints.iter().all(|&v| v < 5), "{q}: {quints:?}");
            seen.insert(quints);
        }
        assert_eq!(seen.len(), 125);
    }

    #[test]
    fn unquantized_ranges_are_monotonic_and_span_the_full_scale() {
        let levels = [
            2u32, 3, 4, 5, 6, 8, 10, 12, 16, 20, 24, 32, 40, 48, 64, 80, 96, 128, 160, 192, 256,
        ];
        for (i, q) in QUANTS.iter().enumerate() {
            let n = levels[i];
            // ISE values are (trit|quint) << bits | low bits; sort the decoded values.
            // Every ISE value of the range: (trit or quint) << bits | low bits, or just the bits.
            let values: Vec<u32> = (0..n)
                .map(|k| {
                    if q.trits || q.quints {
                        let step = 1 << q.bits;
                        ((k / step) << q.bits) | (k % step)
                    } else {
                        k
                    }
                })
                .collect();
            if i >= MIN_COLOR_QUANT {
                let mut c: Vec<u32> = values.iter().map(|&v| unquantize_color(*q, v)).collect();
                c.sort_unstable();
                c.dedup();
                assert_eq!(c.len() as u32, n, "colour range {n}");
                assert_eq!((c[0], c[c.len() - 1]), (0, 255), "colour range {n}");
            }
            if i <= 11 {
                let mut w: Vec<u32> = values.iter().map(|&v| unquantize_weight(*q, v)).collect();
                w.sort_unstable();
                w.dedup();
                assert_eq!(w.len() as u32, n, "weight range {n}");
                assert_eq!((w[0], w[w.len() - 1]), (0, 64), "weight range {n}");
            }
        }
    }

    #[test]
    fn a_single_partition_two_level_block_decodes() {
        // Block mode: bits[1:0] = 10 and R0 = 0 give R = 4 (4 levels, 2 bits a weight); layout
        // bits[3:2] = 00 with B = 0, A = 2 is a 4×4 grid; no dual plane, low precision.
        let mode = 0b10 | (2 << 5);
        let bm = block_mode(mode).expect("valid mode");
        assert_eq!((bm.grid_w, bm.grid_h, bm.dual_plane), (4, 4, false));
        // CEM 0 (luminance direct), endpoints 0 and 255 at the colour range the bits allow.
        // One partition (bits 11..13 = 0), CEM 0 (bits 13..17 = 0). Colour bits 17..96 hold two
        // values at 8 bits each (256 levels): 0, then 255 at bit 25.
        let mut bits: u128 = u128::from(mode);
        bits |= 255u128 << 25;
        // All weights 3 (top level → 64): bit-reversed field at the top, all ones for 32 bits.
        bits |= 0xffff_ffffu128 << 96;
        let out = decode_astc(4, 4, (4, 4), false, &bits.to_le_bytes()).expect("decode");
        assert!(
            out.as_chunks::<4>().0.iter().all(|p| *p == [255, 255, 255, 255]),
            "{out:?}"
        );
        // Weights 0: the first endpoint.
        let bits0 = bits & !(0xffff_ffffu128 << 96);
        let out = decode_astc(4, 4, (4, 4), false, &bits0.to_le_bytes()).expect("decode");
        assert!(
            out.as_chunks::<4>().0.iter().all(|p| *p == [0, 0, 0, 255]),
            "{out:?}"
        );
    }
}
