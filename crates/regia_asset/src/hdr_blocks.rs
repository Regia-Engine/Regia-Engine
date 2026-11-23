// SPDX-License-Identifier: MPL-2.0
//! HDR block compression for baked light (ADR-2314): BC6H (unsigned float) for desktop GPUs and
//! ASTC 4×4 HDR for phones, encoder and CPU decoder, over linear RGB texels. The `.flmap` texel
//! encodings 2 and 3 store these blocks (`crate::lightmap`, `docs/formats/lightmap.md`); the cook
//! writes them (`regia_asset_pipeline::cook`) and the renderer uploads them as they are when the
//! device samples the format, else it uses the CPU decode.
//!
//! Original code written from the Khronos Data Format Specification (chapter "BC6H", chapter "ASTC
//! Compressed Texture Image Formats": block mode layout, HDR endpoint mode 11, the LNS to FP16
//! conversion). Both encoders write one subset of their format, and the CPU decoders read exactly
//! that subset, so any block they refuse did not come from this encoder:
//!
//! - **BC6H** ([`encode_bc6h_block`]): the four one-region modes (11 to 14 of the spec's table: 10-bit
//!   endpoints, and 11 + 9, 12 + 8 and 16 + 4 bits base + delta), 4-bit indices. Each block tries
//!   all four with a principal-axis endpoint fit and two least-squares refinements and keeps the
//!   lowest error. The two-region modes are not used: baked light is smooth inside a chart.
//! - **ASTC HDR** ([`encode_astc_hdr_block`]): single partition, single plane, a 4×4 weight grid at 8
//!   levels (3 bits), and colour endpoint mode 11 (HDR RGB, direct) at the full 8-bit endpoint
//!   range. Each block tries every major component and mode value of mode 11.
//!
//! Errors are measured in the formats' own near-logarithmic domain (half-float bits for BC6H, the
//! LNS value for ASTC), so a dark texel and a bright one weigh alike. Texels a `mask` marks as
//! unused (a lightmap's empty texels) do not pull the endpoints. Values clamp to 0..=65504 (the
//! largest finite half float); a lightmap's texels are at most 32768 after its exposure.
//!
//! The documented error (tests in this module and in `crate::lightmap`): on smooth content the
//! decoded texel stays within [`BC6H_MAX_REL_ERROR`] / [`ASTC_HDR_MAX_REL_ERROR`] of the input per
//! channel (relative to the texel's brightest channel), and the mean is far below.

use thiserror::Error;

mod astc_hdr;
mod bc6h;

pub use astc_hdr::{decode_astc_hdr_block, encode_astc_hdr_block};
pub use bc6h::{Bc6hMode, decode_bc6h_block, encode_bc6h_block, encode_bc6h_block_mode};

/// Bytes of one 4×4 block (both formats).
pub const HDR_BLOCK_BYTES: usize = 16;
/// The largest per-channel error of a BC6H texel on smooth content (a 4×4 block whose texels lie
/// within a factor of two of each other), relative to the texel's brightest channel. Measured 4.5 %
/// at worst on the test gradient, whose tint changes across a block (one region fits a line in
/// colour space); the mean is about a tenth of it.
pub const BC6H_MAX_REL_ERROR: f32 = 0.05;
/// The same bound for ASTC HDR texels.
pub const ASTC_HDR_MAX_REL_ERROR: f32 = 0.06;
/// The largest value the encoders keep (the largest finite half float).
pub const HDR_BLOCK_MAX: f32 = 65504.0;

/// The two block formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HdrBlockFormat {
    /// BC6H, unsigned float (desktop: `Bc6hRgbUfloat`).
    Bc6h,
    /// ASTC 4×4, HDR profile (phones with ASTC HDR: `Astc { B4x4, Hdr }`).
    AstcHdr4x4,
}

impl HdrBlockFormat {
    /// The lowercase name (`bc6h`, `astc_hdr`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Bc6h => "bc6h",
            Self::AstcHdr4x4 => "astc_hdr",
        }
    }

    fn encode_block(self, texels: &[[f32; 3]; 16], mask: &[bool; 16]) -> [u8; 16] {
        match self {
            Self::Bc6h => encode_bc6h_block(texels, mask),
            Self::AstcHdr4x4 => encode_astc_hdr_block(texels, mask),
        }
    }

    fn decode_block(self, block: &[u8; 16]) -> Option<[[f32; 3]; 16]> {
        match self {
            Self::Bc6h => decode_bc6h_block(block),
            Self::AstcHdr4x4 => decode_astc_hdr_block(block),
        }
    }
}

/// A block that is not in the subset the CPU decoder reads.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum HdrBlockError {
    /// The data is not one block per 4×4 tile of the image.
    #[error("{found} bytes of {format} blocks for a {width}×{height} image ({expected} expected)")]
    Size {
        /// Format name.
        format: &'static str,
        /// Image width.
        width: u32,
        /// Image height.
        height: u32,
        /// Bytes given.
        found: usize,
        /// Bytes expected.
        expected: usize,
    },
    /// A block this decoder does not read (another mode or layout).
    #[error("{format} block {index} uses a mode this decoder does not read")]
    Block {
        /// Format name.
        format: &'static str,
        /// Block index, row-major.
        index: usize,
    },
}

/// Bytes of the blocks of a `width × height` image (partial blocks at the right and bottom
/// edges count whole).
#[must_use]
pub fn hdr_block_bytes(width: u32, height: u32) -> usize {
    width.div_ceil(4) as usize * height.div_ceil(4) as usize * HDR_BLOCK_BYTES
}

/// Encodes `width × height` linear RGB texels (row-major) into 4×4 blocks of `format`, row-major.
/// `mask` (one per texel, when given) marks the texels whose value matters; the others still get
/// a value but do not pull the fit. Edge blocks repeat the last row and column. Deterministic: the
/// same input gives the same bytes for any thread count.
///
/// # Panics
///
/// Never on sizes: texels past the end read as black (the caller passes `width * height`).
#[must_use]
pub fn encode_hdr_blocks(
    format: HdrBlockFormat,
    width: u32,
    height: u32,
    texels: &[[f32; 3]],
    mask: Option<&[bool]>,
) -> Vec<u8> {
    let _span = tracing::info_span!("encode_hdr_blocks", format = format.name(), width, height).entered();
    let (bw, bh) = (width.div_ceil(4) as usize, height.div_ceil(4) as usize);
    let mut out = vec![0u8; bw * bh * HDR_BLOCK_BYTES];
    if bw == 0 || bh == 0 {
        return out;
    }
    let (w, h) = (width as usize, height as usize);
    let encode_row = |by: usize, row: &mut [u8]| {
        for (bx, dst) in row.as_chunks_mut::<HDR_BLOCK_BYTES>().0.iter_mut().enumerate() {
            let mut px = [[0.0f32; 3]; 16];
            let mut m = [true; 16];
            for (k, (p, used)) in px.iter_mut().zip(m.iter_mut()).enumerate() {
                let x = (bx * 4 + k % 4).min(w - 1);
                let y = (by * 4 + k / 4).min(h - 1);
                let i = y * w + x;
                *p = texels.get(i).copied().unwrap_or([0.0; 3]);
                *used = mask.is_none_or(|m| m.get(i).copied().unwrap_or(false));
            }
            dst.copy_from_slice(&format.encode_block(&px, &m));
        }
    };
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, 16);
    let row_bytes = bw * HDR_BLOCK_BYTES;
    if threads == 1 || bh < 4 {
        for (by, row) in out.chunks_exact_mut(row_bytes).enumerate() {
            encode_row(by, row);
        }
        return out;
    }
    let rows_per = bh.div_ceil(threads);
    std::thread::scope(|s| {
        for (t, band) in out.chunks_mut(rows_per * row_bytes).enumerate() {
            let encode_row = &encode_row;
            s.spawn(move || {
                for (k, row) in band.chunks_exact_mut(row_bytes).enumerate() {
                    encode_row(t * rows_per + k, row);
                }
            });
        }
    });
    out
}

/// Decodes blocks written by [`encode_hdr_blocks`] back to `width × height` linear RGB texels.
///
/// # Errors
///
/// [`HdrBlockError::Size`] when `blocks` is not one block per tile, [`HdrBlockError::Block`] for a
/// block outside the subset the encoder writes.
pub fn decode_hdr_blocks(
    format: HdrBlockFormat,
    width: u32,
    height: u32,
    blocks: &[u8],
) -> Result<Vec<[f32; 3]>, HdrBlockError> {
    let mut out = vec![[0.0f32; 3]; width as usize * height as usize];
    visit_hdr_blocks(format, width, height, blocks, |index, px| out[index] = px)?;
    Ok(out)
}

/// Calls `visit` with each in-bounds texel of [`decode_hdr_blocks`], in block order, as
/// `(y * width + x, rgb)`. A caller that wants RGB9E5 (or an error figure) writes it directly
/// and does not keep the float image.
///
/// # Errors
///
/// As [`decode_hdr_blocks`].
pub(crate) fn visit_hdr_blocks(
    format: HdrBlockFormat,
    width: u32,
    height: u32,
    blocks: &[u8],
    mut visit: impl FnMut(usize, [f32; 3]),
) -> Result<(), HdrBlockError> {
    let _span = tracing::info_span!("decode_hdr_blocks", format = format.name(), width, height).entered();
    let expected = hdr_block_bytes(width, height);
    if blocks.len() != expected {
        return Err(HdrBlockError::Size {
            format: format.name(),
            width,
            height,
            found: blocks.len(),
            expected,
        });
    }
    let (w, h) = (width as usize, height as usize);
    let bw = w.div_ceil(4);
    for (index, block) in blocks.as_chunks::<HDR_BLOCK_BYTES>().0.iter().enumerate() {
        let px = format.decode_block(block).ok_or(HdrBlockError::Block {
            format: format.name(),
            index,
        })?;
        let (bx, by) = (index % bw, index / bw);
        for (k, p) in px.iter().enumerate() {
            let (x, y) = (bx * 4 + k % 4, by * 4 + k / 4);
            if x < w && y < h {
                visit(y * w + x, *p);
            }
        }
    }
    Ok(())
}

/// Half-float bits of a non-negative value, rounded to nearest even; negative and NaN give 0,
/// anything at or above 65504 the largest finite half.
#[must_use]
pub fn f32_to_f16_bits(v: f32) -> u16 {
    if v.is_nan() || v <= 0.0 {
        return 0;
    }
    if v >= HDR_BLOCK_MAX {
        return 0x7BFF;
    }
    // Below the smallest normal half (2^-14): a multiple of 2^-24.
    if v < 6.103_515_6e-5 {
        return (v * 16_777_216.0).round() as u16;
    }
    let bits = v.to_bits();
    let exp = ((bits >> 23) & 0xFF) as i32 - 127 + 15;
    let mant = bits & 0x7F_FFFF;
    let mut h = ((exp as u32) << 10) | (mant >> 13);
    let rest = mant & 0x1FFF;
    if rest > 0x1000 || (rest == 0x1000 && h & 1 == 1) {
        h += 1;
    }
    h.min(0x7BFF) as u16
}

/// The value of half-float bits (sign ignored: these formats are unsigned).
#[must_use]
pub fn f16_bits_to_f32(h: u16) -> f32 {
    let exp = u32::from((h >> 10) & 0x1F);
    let mant = u32::from(h & 0x3FF);
    match exp {
        0 => mant as f32 / 16_777_216.0,
        31 => f32::INFINITY,
        _ => f32::from_bits(((exp + 112) << 23) | (mant << 13)),
    }
}

/// The principal axis of `points` (weighted by `mask`) through their mean, by power iteration on
/// the covariance; the unit diagonal when the points are all equal.
fn principal_axis(points: &[[f32; 3]; 16], mask: &[bool; 16]) -> ([f32; 3], [f32; 3]) {
    let mut mean = [0.0f32; 3];
    let mut n = 0.0f32;
    for (p, _) in points.iter().zip(mask).filter(|(_, m)| **m) {
        for c in 0..3 {
            mean[c] += p[c];
        }
        n += 1.0;
    }
    if n == 0.0 {
        return (mean, [0.577_350_26; 3]);
    }
    mean = mean.map(|v| v / n);
    let mut cov = [[0.0f32; 3]; 3];
    for (p, _) in points.iter().zip(mask).filter(|(_, m)| **m) {
        let d = [p[0] - mean[0], p[1] - mean[1], p[2] - mean[2]];
        for (r, row) in cov.iter_mut().enumerate() {
            for (c, v) in row.iter_mut().enumerate() {
                *v += d[r] * d[c];
            }
        }
    }
    let mut axis = [1.0f32, 1.0, 1.0];
    for _ in 0..12 {
        let next = [0, 1, 2].map(|r| cov[r][0] * axis[0] + cov[r][1] * axis[1] + cov[r][2] * axis[2]);
        let len = (next[0] * next[0] + next[1] * next[1] + next[2] * next[2]).sqrt();
        if len.is_nan() || len <= 1e-20 {
            return (mean, [0.577_350_26; 3]);
        }
        axis = next.map(|v| v / len);
    }
    (mean, axis)
}

/// Endpoints along the principal axis spanning the masked points, ordered so the first texel
/// lies nearer the first endpoint.
fn fit_line(points: &[[f32; 3]; 16], mask: &[bool; 16]) -> ([f32; 3], [f32; 3]) {
    let (mean, axis) = principal_axis(points, mask);
    let t =
        |p: &[f32; 3]| (p[0] - mean[0]) * axis[0] + (p[1] - mean[1]) * axis[1] + (p[2] - mean[2]) * axis[2];
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    for (p, _) in points.iter().zip(mask).filter(|(_, m)| **m) {
        let v = t(p);
        lo = lo.min(v);
        hi = hi.max(v);
    }
    if !lo.is_finite() {
        return (mean, mean);
    }
    let at = |s: f32| [0, 1, 2].map(|c| mean[c] + axis[c] * s);
    let (e0, e1) = (at(lo), at(hi));
    let first = mask.iter().position(|m| *m).unwrap_or(0);
    if (t(&points[first]) - lo).abs() > (hi - t(&points[first])).abs() {
        (e1, e0)
    } else {
        (e0, e1)
    }
}

/// Least-squares endpoints for fixed interpolation factors `alpha` (0..=1 per texel): per
/// channel, the `e0, e1` minimising `Σ ((1 − α) e0 + α e1 − p)²` over masked texels. `None` when
/// the system is singular (every texel at one factor).
fn least_squares(
    points: &[[f32; 3]; 16],
    mask: &[bool; 16],
    alpha: &[f32; 16],
) -> Option<([f32; 3], [f32; 3])> {
    let (mut aa, mut ab, mut bb) = (0.0f64, 0.0f64, 0.0f64);
    let mut ap = [0.0f64; 3];
    let mut bp = [0.0f64; 3];
    for ((p, &m), &a) in points.iter().zip(mask).zip(alpha) {
        if !m {
            continue;
        }
        let (a, b) = (f64::from(1.0 - a), f64::from(a));
        aa += a * a;
        ab += a * b;
        bb += b * b;
        for c in 0..3 {
            ap[c] += a * f64::from(p[c]);
            bp[c] += b * f64::from(p[c]);
        }
    }
    let det = aa * bb - ab * ab;
    if det.abs() < 1e-9 {
        return None;
    }
    let e0 = [0, 1, 2].map(|c| ((bb * ap[c] - ab * bp[c]) / det) as f32);
    let e1 = [0, 1, 2].map(|c| ((aa * bp[c] - ab * ap[c]) / det) as f32);
    Some((e0, e1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_floats_round_trip_and_clamp() {
        for v in [0.0f32, 1.0, 0.5, 2.0, 65504.0, 1.0e-6, 3.25, 1000.25] {
            let back = f16_bits_to_f32(f32_to_f16_bits(v));
            assert!((back - v).abs() <= v.abs() * 1.0e-3 + 6.0e-8, "{v} -> {back}");
        }
        assert_eq!(f32_to_f16_bits(1.0), 0x3C00);
        assert_eq!(f32_to_f16_bits(-3.0), 0);
        assert_eq!(f32_to_f16_bits(f32::NAN), 0);
        assert_eq!(f32_to_f16_bits(f32::NEG_INFINITY), 0);
        assert_eq!(f32_to_f16_bits(f32::INFINITY), 0x7BFF);
        assert_eq!(f32_to_f16_bits(1.0e9), 0x7BFF);
        assert_eq!(f16_bits_to_f32(0x7BFF), 65504.0);
        // Round to nearest even at the halfway point between 1.0 and the next half.
        assert_eq!(f32_to_f16_bits(1.0 + 1.0 / 2048.0), 0x3C00);
        assert_eq!(f32_to_f16_bits(1.0 + 3.0 / 2048.0), 0x3C02);
    }

    /// A smooth HDR image: a gradient over one decade with a colour tilt (a 4×4 block stays within
    /// a factor of two, the documented error's condition).
    pub(super) fn smooth(w: u32, h: u32) -> Vec<[f32; 3]> {
        (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f32, (i / w) as f32);
                let l = 4.0 * (x / w as f32 * 2.3).exp() * (1.0 + 0.3 * (y * 0.2).sin());
                [l, l * 0.8, l * (0.5 + 0.02 * y)]
            })
            .collect()
    }

    fn max_rel(a: &[[f32; 3]], b: &[[f32; 3]]) -> (f32, f32) {
        let (mut worst, mut sum, mut n) = (0.0f32, 0.0f64, 0usize);
        for (p, q) in a.iter().zip(b) {
            let top = p.iter().copied().fold(0.0f32, f32::max).max(1e-6);
            for c in 0..3 {
                let r = (p[c] - q[c]).abs() / top;
                worst = worst.max(r);
                sum += f64::from(r);
                n += 1;
            }
        }
        (worst, (sum / n as f64) as f32)
    }

    #[test]
    fn both_formats_decode_smooth_content_within_the_documented_error() {
        // 18 × 10: partial edge blocks on both axes.
        let (w, h) = (18u32, 10u32);
        let img = smooth(w, h);
        for (format, bound) in [
            (HdrBlockFormat::Bc6h, BC6H_MAX_REL_ERROR),
            (HdrBlockFormat::AstcHdr4x4, ASTC_HDR_MAX_REL_ERROR),
        ] {
            let blocks = encode_hdr_blocks(format, w, h, &img, None);
            assert_eq!(blocks.len(), hdr_block_bytes(w, h));
            let back = decode_hdr_blocks(format, w, h, &blocks).expect("decode");
            let (worst, mean) = max_rel(&img, &back);
            assert!(worst <= bound, "{format:?}: worst {worst}");
            assert!(mean <= bound / 4.0, "{format:?}: mean {mean}");
            // Deterministic.
            assert_eq!(blocks, encode_hdr_blocks(format, w, h, &img, None));
        }
    }

    #[test]
    fn masked_texels_do_not_pull_the_fit() {
        // Half the block is black and unused (a chart edge): the used half keeps its precision.
        let (w, h) = (4u32, 4u32);
        let img: Vec<[f32; 3]> = (0..16)
            .map(|i| {
                if i % 4 < 2 {
                    [0.0; 3]
                } else {
                    [900.0 + i as f32, 700.0, 500.0]
                }
            })
            .collect();
        let mask: Vec<bool> = (0..16).map(|i| i % 4 >= 2).collect();
        for format in [HdrBlockFormat::Bc6h, HdrBlockFormat::AstcHdr4x4] {
            let back = decode_hdr_blocks(format, w, h, &encode_hdr_blocks(format, w, h, &img, Some(&mask)))
                .expect("decode");
            for (i, (p, q)) in img.iter().zip(&back).enumerate().filter(|(i, _)| mask[*i]) {
                for c in 0..3 {
                    assert!(
                        (p[c] - q[c]).abs() <= p[0] * 0.03,
                        "{format:?} texel {i}: {p:?} -> {q:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn wrong_sizes_and_foreign_blocks_are_errors_not_panics() {
        for format in [HdrBlockFormat::Bc6h, HdrBlockFormat::AstcHdr4x4] {
            assert!(matches!(
                decode_hdr_blocks(format, 5, 5, &[0u8; 16]),
                Err(HdrBlockError::Size { expected: 64, .. })
            ));
            // An all-ones block is no mode either encoder writes.
            assert!(matches!(
                decode_hdr_blocks(format, 4, 4, &[0xFF; 16]),
                Err(HdrBlockError::Block { index: 0, .. })
            ));
        }
        assert!(
            decode_hdr_blocks(HdrBlockFormat::Bc6h, 0, 0, &[])
                .expect("empty")
                .is_empty()
        );
    }

    /// Sizes with partial edge tiles on one or both axes: 1×1, 1×5, 3×7, 4×4, 5×4, 5×5.
    const EDGE_SIZES: [(u32, u32); 6] = [(1, 1), (1, 5), (3, 7), (4, 4), (5, 4), (5, 5)];

    /// An image whose border texels are analytically distinct: the red channel carries the
    /// position along `vertical`'s axis (brightest, with a 15 % step — detectable under the
    /// documented error bound), the green and blue channels fixed tilts of it, so every texel of
    /// a block lies within a factor of two of the others (the documented error's condition) and
    /// a flip, transpose or misindexed decode moves red by more than twice the bound.
    fn border_image(w: u32, h: u32, vertical: bool) -> Vec<[f32; 3]> {
        (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                let a = if vertical { y } else { x } as f32;
                let r = 30.0 * 1.15f32.powf(a);
                [r, r * 0.8, r * 0.65]
            })
            .collect()
    }

    /// The tile `encode_hdr_blocks` feeds a codec for block `(bx, by)`: texels (and mask) at
    /// clamped coordinates — the documented repeated-last-row/column padding.
    fn padded_tile(
        img: &[[f32; 3]],
        w: usize,
        h: usize,
        mask: Option<&[bool]>,
        bx: usize,
        by: usize,
    ) -> ([[f32; 3]; 16], [bool; 16]) {
        let mut px = [[0.0f32; 3]; 16];
        let mut m = [true; 16];
        for (k, (p, used)) in px.iter_mut().zip(m.iter_mut()).enumerate() {
            let x = (bx * 4 + k % 4).min(w - 1);
            let y = (by * 4 + k / 4).min(h - 1);
            let i = y * w + x;
            *p = img[i];
            *used = mask.is_none_or(|m| m.get(i).copied().unwrap_or(false));
        }
        (px, m)
    }

    /// Every tile of every edge size encodes byte for byte like an explicitly padded 4×4 tile
    /// (the repeated-last-row/column policy), the block count is one per 4×4 tile row-major, and
    /// a mask with holes rides the same padding into its tile.
    #[test]
    fn edge_tiles_match_an_explicitly_padded_tile() {
        for &(w, h) in &EDGE_SIZES {
            let (bw, bh) = (w.div_ceil(4) as usize, h.div_ceil(4) as usize);
            let img = border_image(w, h, false);
            let mask: Vec<bool> = (0..w * h).map(|i| i % 5 != 3).collect();
            for format in [HdrBlockFormat::Bc6h, HdrBlockFormat::AstcHdr4x4] {
                let blocks = encode_hdr_blocks(format, w, h, &img, Some(&mask));
                assert_eq!(blocks.len(), bw * bh * HDR_BLOCK_BYTES, "{format:?} {w}×{h}");
                assert_eq!(blocks.len(), hdr_block_bytes(w, h));
                let bare = encode_hdr_blocks(format, w, h, &img, None);
                for (index, tile) in blocks.as_chunks::<HDR_BLOCK_BYTES>().0.iter().enumerate() {
                    let (bx, by) = (index % bw, index / bw);
                    let (px, m) = padded_tile(&img, w as usize, h as usize, Some(&mask), bx, by);
                    assert_eq!(tile, &format.encode_block(&px, &m), "{format:?} tile {bx},{by}");
                    let (px, m) = padded_tile(&img, w as usize, h as usize, None, bx, by);
                    let bare_tile: [u8; HDR_BLOCK_BYTES] = bare[index * HDR_BLOCK_BYTES..][..HDR_BLOCK_BYTES]
                        .try_into()
                        .expect("one tile");
                    assert_eq!(
                        bare_tile,
                        format.encode_block(&px, &m),
                        "{format:?} tile {bx},{by}"
                    );
                }
            }
        }
    }

    /// Decoding keeps the dimensions and the row-major orientation on every edge size: red
    /// carries the position along one axis (both axes across the two images), and each decoded
    /// channel stays within the documented error of its input relative to the texel's brightest
    /// channel — a margin no flip, transpose or linearization error survives.
    #[test]
    fn decode_keeps_dimensions_and_orientation_on_distinct_borders() {
        for &(w, h) in &EDGE_SIZES {
            for vertical in [false, true] {
                let img = border_image(w, h, vertical);
                for (format, bound) in [
                    (HdrBlockFormat::Bc6h, BC6H_MAX_REL_ERROR),
                    (HdrBlockFormat::AstcHdr4x4, ASTC_HDR_MAX_REL_ERROR),
                ] {
                    let blocks = encode_hdr_blocks(format, w, h, &img, None);
                    let back = decode_hdr_blocks(format, w, h, &blocks).expect("decode");
                    assert_eq!(back.len(), (w * h) as usize, "{format:?} {w}×{h} dimensions");
                    for (i, (p, q)) in img.iter().zip(&back).enumerate() {
                        let top = p[0];
                        for c in 0..3 {
                            assert!(
                                (p[c] - q[c]).abs() <= bound * top,
                                "{format:?} {w}×{h} texel {i} channel {c}: {p:?} -> {q:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// The fit contract for masked blocks: with one valid texel and with four valid corners,
    /// changing only the invalid texels over black, mid and the brightest finite value leaves
    /// the decoded valid texels bit for bit identical and within the documented error of their
    /// input.
    #[test]
    fn masked_out_texels_do_not_change_valid_texels() {
        let invalid_sets: [[f32; 3]; 3] = [[0.0; 3], [120.0, 240.0, 480.0], [65504.0; 3]];
        // One valid texel, and four valid corners, each within a factor of two (smooth).
        let cases: [([bool; 16], [[f32; 3]; 16]); 2] = [
            (
                std::array::from_fn(|k| k == 6),
                std::array::from_fn(|k| if k == 6 { [640.0, 512.0, 400.0] } else { [0.0; 3] }),
            ),
            (
                std::array::from_fn(|k| matches!(k, 0 | 3 | 12 | 15)),
                std::array::from_fn(|k| match k {
                    0 => [500.0, 400.0, 320.0],
                    3 => [560.0, 448.0, 358.0],
                    12 => [620.0, 496.0, 397.0],
                    15 => [680.0, 544.0, 435.0],
                    _ => [0.0; 3],
                }),
            ),
        ];
        for format in [HdrBlockFormat::Bc6h, HdrBlockFormat::AstcHdr4x4] {
            let bound = match format {
                HdrBlockFormat::Bc6h => BC6H_MAX_REL_ERROR,
                HdrBlockFormat::AstcHdr4x4 => ASTC_HDR_MAX_REL_ERROR,
            };
            for (mask, base) in cases {
                let mut reference: Option<Vec<[f32; 3]>> = None;
                for invalid in invalid_sets {
                    let img: Vec<[f32; 3]> = base
                        .iter()
                        .zip(&mask)
                        .map(|(p, m)| if *m { *p } else { invalid })
                        .collect();
                    let blocks = encode_hdr_blocks(format, 4, 4, &img, Some(&mask));
                    let back = decode_hdr_blocks(format, 4, 4, &blocks).expect("decode");
                    for (k, m) in mask.iter().enumerate() {
                        if !m {
                            continue;
                        }
                        // The valid texels are invariant to the invalid values.
                        if let Some(prev) = &reference {
                            assert_eq!(back[k], prev[k], "{format:?} texel {k} moves with {invalid:?}");
                        }
                        // And they hold the documented error relative to the brightest channel.
                        for c in 0..3 {
                            assert!(
                                (base[k][c] - back[k][c]).abs() <= bound * base[k][0],
                                "{format:?} texel {k}: {:?} -> {:?}",
                                base[k],
                                back[k]
                            );
                        }
                    }
                    if reference.is_none() {
                        reference = Some(back);
                    }
                }
            }
        }
    }

    /// An empty mask (no valid texel) is deterministic and both codecs fall back to encoding
    /// every texel, exactly as an absent mask does.
    #[test]
    fn an_empty_mask_is_deterministic_and_encodes_every_texel() {
        let (w, h) = (5u32, 4u32); // two edge tiles, the second partial on x
        let img = smooth(w, h);
        let empty = vec![false; (w * h) as usize];
        for format in [HdrBlockFormat::Bc6h, HdrBlockFormat::AstcHdr4x4] {
            let masked = encode_hdr_blocks(format, w, h, &img, Some(&empty));
            assert_eq!(
                masked,
                encode_hdr_blocks(format, w, h, &img, Some(&empty)),
                "deterministic"
            );
            assert_eq!(
                masked,
                encode_hdr_blocks(format, w, h, &img, None),
                "all-texel fallback"
            );
            let back = decode_hdr_blocks(format, w, h, &masked).expect("decode");
            assert_eq!(back.len(), (w * h) as usize);
        }
    }

    /// NaN, infinities, negatives and values past the largest finite half clamp as documented:
    /// the decoded block is finite, in 0..=65504, and never NaN.
    #[test]
    fn non_finite_and_over_range_texels_stay_finite() {
        let mut img = smooth(4, 4);
        img[0] = [f32::NAN, 1.0, 1.0];
        img[1] = [f32::INFINITY, 1.0, 1.0];
        img[2] = [f32::NEG_INFINITY, 1.0, 1.0];
        img[3] = [1.0e30, 1.0e30, 1.0e30];
        img[4] = [-50.0, 5.0, 5.0];
        for format in [HdrBlockFormat::Bc6h, HdrBlockFormat::AstcHdr4x4] {
            let back = decode_hdr_blocks(format, 4, 4, &encode_hdr_blocks(format, 4, 4, &img, None))
                .expect("decode");
            for p in back {
                for v in p {
                    assert!(
                        v.is_finite() && (0.0..=HDR_BLOCK_MAX).contains(&v),
                        "{format:?} {p:?}"
                    );
                }
            }
        }
    }

    /// Truncation and an extra block are [`HdrBlockError::Size`] with the full byte count, and a
    /// corrupt block is reported at its own row-major index (bounded arrays, no allocation).
    #[test]
    fn truncated_extra_and_foreign_blocks_report_size_and_index() {
        const W: u32 = 5;
        const H: u32 = 5; // 2×2 tiles, 4 blocks, 64 bytes
        const FULL: usize = 64;
        const BLOCKS: usize = FULL / HDR_BLOCK_BYTES;
        let img = smooth(W, H);
        for format in [HdrBlockFormat::Bc6h, HdrBlockFormat::AstcHdr4x4] {
            let encoded = encode_hdr_blocks(format, W, H, &img, None);
            let short_len = FULL - HDR_BLOCK_BYTES;
            let mut short = [0u8; FULL - HDR_BLOCK_BYTES];
            short.copy_from_slice(&encoded[..short_len]);
            assert!(matches!(
                decode_hdr_blocks(format, W, H, &short),
                Err(HdrBlockError::Size { found, expected: FULL, .. }) if found == short.len()
            ));
            let mut extra = [0u8; FULL + HDR_BLOCK_BYTES];
            extra[..FULL].copy_from_slice(&encoded);
            assert!(matches!(
                decode_hdr_blocks(format, W, H, &extra),
                Err(HdrBlockError::Size { found, expected: FULL, .. }) if found == extra.len()
            ));
            for index in 0..BLOCKS {
                let mut bad = [0u8; FULL];
                bad.copy_from_slice(&encoded);
                for b in bad[index * HDR_BLOCK_BYTES..][..HDR_BLOCK_BYTES].iter_mut() {
                    *b = 0xFF;
                }
                assert!(
                    matches!(
                        decode_hdr_blocks(format, W, H, &bad),
                        Err(HdrBlockError::Block { index: i, .. }) if i == index
                    ),
                    "{format:?} corrupt block {index}"
                );
            }
        }
    }

    #[test]
    fn black_and_saturated_texels_survive() {
        let img = vec![[0.0f32; 3]; 16];
        for format in [HdrBlockFormat::Bc6h, HdrBlockFormat::AstcHdr4x4] {
            let back =
                decode_hdr_blocks(format, 4, 4, &encode_hdr_blocks(format, 4, 4, &img, None)).expect("black");
            assert!(back.iter().flatten().all(|v| *v == 0.0), "{format:?} {back:?}");
            let hot = vec![[1.0e9f32, 32768.0, 1.0]; 16];
            let back =
                decode_hdr_blocks(format, 4, 4, &encode_hdr_blocks(format, 4, 4, &hot, None)).expect("hot");
            for p in back {
                assert!(p.iter().all(|v| v.is_finite()), "{format:?} {p:?}");
                assert!(
                    (p[1] - 32768.0).abs() <= 32768.0 * ASTC_HDR_MAX_REL_ERROR,
                    "{format:?} {p:?}"
                );
            }
        }
    }
}
