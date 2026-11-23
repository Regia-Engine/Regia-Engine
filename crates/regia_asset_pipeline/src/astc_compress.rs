// SPDX-License-Identifier: MPL-2.0
//! ASTC block compression via vendored ARM astcenc (`ctt-astcenc`, Apache-2.0).
//!
//! `intel_tex_2` leaves ASTC encode unimplemented; Android cook uses this module instead.
//! [`decompress_astc_rgba8`] is the same library's reference decoder: the cook's quality tests
//! and the `astc_survey` example measure the encoder with it.

use ctt_astcenc::bindings::{astcenc_image, astcenc_type_ASTCENC_TYPE_U8};
use ctt_astcenc::{Context, Flags, Preset, Profile, Swizzle, SwizzleChannel, config_init};
use std::ffi::c_void;
use thiserror::Error;

/// ASTC compression failure.
#[derive(Debug, Error)]
pub enum AstcCompressError {
    /// astcenc library error.
    #[error("astcenc: {0}")]
    Codec(#[from] ctt_astcenc::Error),

    /// Input buffer size does not match width × height × 4.
    #[error("rgba buffer length {len} does not match {width}x{height}")]
    BadBuffer {
        /// Pixel buffer length.
        len: usize,
        /// Image width.
        width: u32,
        /// Image height.
        height: u32,
    },

    /// A strip encoder thread panicked.
    #[error("an ASTC strip encoder panicked")]
    WorkerPanicked,
}

/// What the texels of an ASTC texture hold: picks the encoder's channel swizzle and error
/// weighting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AstcContent {
    /// Four channels (colour with alpha, packed ORM with a fourth term).
    Rgba,
    /// Three channels; alpha is 1 everywhere, so the encoder spends no bits on it.
    Rgb,
    /// Tangent-space normal: X in R, Y in G at encode (`RRRG` swizzle, astcenc's normal mode);
    /// the decoded texel holds X in R, G and B and Y in A.
    Normal,
    /// One channel in R (height, AO, a mask): encoded as `RRR1`, decoded with R = G = B.
    Single,
    /// Two channels in R and G, decoded like BC5 (`R G 0 1`): a normal map for a shader that
    /// reads `.rg` (RGB endpoint modes, so more error than [`Self::Normal`] at the same block).
    Rg,
}

impl AstcContent {
    fn swizzle(self) -> Swizzle {
        match self {
            Self::Rgba => Swizzle::IDENTITY,
            Self::Rgb => Swizzle::RGB1,
            Self::Normal => Swizzle::RRRG,
            Self::Single => Swizzle::RRR1,
            Self::Rg => Swizzle {
                r: SwizzleChannel::R,
                g: SwizzleChannel::G,
                b: SwizzleChannel::Zero,
                a: SwizzleChannel::One,
            },
        }
    }
}

/// Compress RGBA8 into ASTC blocks (16 bytes per block) at the cook's preset (astcenc
/// `medium`). Any size is accepted: edge blocks of a level that is not a whole number of blocks
/// are padded by the encoder (the tail mips of a chain).
pub fn compress_astc_rgba8(
    width: u32,
    height: u32,
    rgba: &[u8],
    (block_width, block_height): (u32, u32),
    srgb: bool,
    content: AstcContent,
) -> Result<Vec<u8>, AstcCompressError> {
    compress_astc_rgba8_preset(
        width,
        height,
        rgba,
        (block_width, block_height),
        srgb,
        content,
        Preset::Medium,
    )
}

/// [`compress_astc_rgba8`] at an explicit astcenc search preset (the survey compares them).
pub fn compress_astc_rgba8_preset(
    width: u32,
    height: u32,
    rgba: &[u8],
    (block_width, block_height): (u32, u32),
    srgb: bool,
    content: AstcContent,
    preset: Preset,
) -> Result<Vec<u8>, AstcCompressError> {
    let expected = (width as usize) * (height as usize) * 4;
    if rgba.len() != expected {
        return Err(AstcCompressError::BadBuffer {
            len: rgba.len(),
            width,
            height,
        });
    }

    // Blocks are independent: a large level is cut into strips of whole block rows, encoded on
    // scoped threads and concatenated (row-major blocks, so the bytes are the same as one call).
    let block_rows = height.div_ceil(block_height);
    let threads = strip_threads(width, height, block_rows);
    if threads <= 1 {
        return compress_strip(
            width,
            height,
            rgba,
            (block_width, block_height),
            srgb,
            content,
            preset,
        );
    }
    let strip_rows = block_rows.div_ceil(threads as u32) * block_height;
    let row_bytes = width as usize * 4;
    let strips: Vec<&[u8]> = rgba.chunks(strip_rows as usize * row_bytes).collect();
    let encoded: Vec<Result<Vec<u8>, AstcCompressError>> = std::thread::scope(|scope| {
        let handles: Vec<_> = strips
            .iter()
            .map(|strip| {
                let h = (strip.len() / row_bytes) as u32;
                scope.spawn(move || {
                    compress_strip(
                        width,
                        h,
                        strip,
                        (block_width, block_height),
                        srgb,
                        content,
                        preset,
                    )
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or(Err(AstcCompressError::WorkerPanicked)))
            .collect()
    });
    let mut output = Vec::with_capacity((width.div_ceil(block_width) * block_rows * 16) as usize);
    for strip in encoded {
        output.extend_from_slice(&strip?);
    }
    Ok(output)
}

/// Threads (strips of whole block rows) to encode a `width`×`height` level with: one below
/// 512², else the machine's parallelism (at most 16, at most one per block row, at most the
/// calling thread's band-thread cap, `texture_compress::with_band_thread_cap`).
pub(crate) fn strip_threads(width: u32, height: u32, block_rows: u32) -> usize {
    if u64::from(width) * u64::from(height) < 512 * 512 {
        return 1;
    }
    std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(16)
        .min(block_rows as usize)
        .min(crate::texture_compress::band_thread_cap())
}

/// One astcenc call over a whole image (or a strip of whole block rows).
fn compress_strip(
    width: u32,
    height: u32,
    rgba: &[u8],
    (block_width, block_height): (u32, u32),
    srgb: bool,
    content: AstcContent,
    preset: Preset,
) -> Result<Vec<u8>, AstcCompressError> {
    let profile = if srgb { Profile::LdrSrgb } else { Profile::Ldr };
    let mut flags = Flags::USE_DECODE_UNORM8;
    if content == AstcContent::Normal {
        flags |= Flags::MAP_NORMAL;
    }
    let config = config_init(profile, block_width, block_height, 1, preset, flags)?;
    let mut ctx = Context::new(&config)?;

    let blocks_x = width.div_ceil(block_width);
    let blocks_y = height.div_ceil(block_height);
    let out_len = (blocks_x * blocks_y * 16) as usize;
    let mut output = vec![0u8; out_len];

    let mut pixels = rgba.to_vec();
    let mut slice_ptrs: [*mut c_void; 1] = [pixels.as_mut_ptr() as *mut c_void];
    let mut image = astcenc_image {
        dim_x: width,
        dim_y: height,
        dim_z: 1,
        data_type: astcenc_type_ASTCENC_TYPE_U8,
        data: slice_ptrs.as_mut_ptr(),
    };

    ctx.compress(&mut image, content.swizzle(), &mut output)?;
    ctx.compress_reset()?;
    Ok(output)
}

/// Decodes ASTC blocks to RGBA8 with astcenc's reference decoder (the texels a GPU returns
/// before any sRGB decode; a [`AstcContent::Normal`] texture keeps its encoded `X X X Y` layout).
pub fn decompress_astc_rgba8(
    width: u32,
    height: u32,
    blocks: &[u8],
    (block_width, block_height): (u32, u32),
    srgb: bool,
) -> Result<Vec<u8>, AstcCompressError> {
    let profile = if srgb { Profile::LdrSrgb } else { Profile::Ldr };
    let config = config_init(
        profile,
        block_width,
        block_height,
        1,
        Preset::Fastest,
        Flags::USE_DECODE_UNORM8 | Flags::DECOMPRESS_ONLY,
    )?;
    let mut ctx = Context::new(&config)?;
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    let mut slice_ptrs: [*mut c_void; 1] = [pixels.as_mut_ptr() as *mut c_void];
    let mut image = astcenc_image {
        dim_x: width,
        dim_y: height,
        dim_z: 1,
        data_type: astcenc_type_ASTCENC_TYPE_U8,
        data: slice_ptrs.as_mut_ptr(),
    };
    ctx.decompress(blocks, &mut image, Swizzle::IDENTITY)?;
    ctx.decompress_reset()?;
    Ok(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A smooth gradient survives a 6×6 round trip closely, a level that is not a whole number
    /// of blocks (10×7) encodes and decodes, and a single-channel map decodes with R = G = B.
    #[test]
    fn round_trips_through_the_reference_decoder() {
        let (w, h) = (24u32, 18u32);
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                [(x * 10) as u8, (y * 12) as u8, 128, 255]
            })
            .collect();
        let blocks = compress_astc_rgba8(w, h, &rgba, (6, 6), false, AstcContent::Rgb).expect("encode");
        assert_eq!(blocks.len(), (4 * 3 * 16) as usize);
        let back = decompress_astc_rgba8(w, h, &blocks, (6, 6), false).expect("decode");
        let worst = rgba
            .iter()
            .zip(&back)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0);
        assert!(worst <= 8, "gradient error {worst}");

        let odd: Vec<u8> = [200u8, 90, 40, 255].repeat(10 * 7);
        let blocks = compress_astc_rgba8(10, 7, &odd, (6, 6), true, AstcContent::Rgb).expect("odd");
        assert_eq!(blocks.len(), 2 * 2 * 16);
        let back = decompress_astc_rgba8(10, 7, &blocks, (6, 6), true).expect("odd decode");
        assert!(back.chunks(4).all(|p| p[0].abs_diff(200) <= 2 && p[3] == 255));

        // A level big enough to be cut into strips encodes to the same bytes as one call.
        let (bw, bh) = (512u32, 520u32);
        let big: Vec<u8> = (0..bw * bh)
            .flat_map(|i| [(i % 251) as u8, (i / 7 % 253) as u8, (i % 13 * 19) as u8, 255])
            .collect();
        let whole =
            compress_strip(bw, bh, &big, (4, 4), false, AstcContent::Rgb, Preset::Fastest).expect("whole");
        let strips =
            compress_astc_rgba8_preset(bw, bh, &big, (4, 4), false, AstcContent::Rgb, Preset::Fastest)
                .expect("strips");
        assert_eq!(whole, strips);

        let mask: Vec<u8> = (0..16 * 16).flat_map(|i| [(i % 256) as u8, 0, 0, 255]).collect();
        let blocks = compress_astc_rgba8(16, 16, &mask, (8, 8), false, AstcContent::Single).expect("mask");
        let back = decompress_astc_rgba8(16, 16, &blocks, (8, 8), false).expect("mask decode");
        assert!(
            back.chunks(4)
                .all(|p| p[0] == p[1] && p[1] == p[2] && p[3] == 255)
        );
    }
}
