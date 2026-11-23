// SPDX-License-Identifier: MPL-2.0
//! CPU mip chains for RGBA8 material textures.

/// Linear value of an sRGB-encoded byte.
fn decode(c: u8) -> f32 {
    let c = f32::from(c) / 255.0;
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB byte of a linear value.
fn encode(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0 + 0.5) as u8
}

/// Byte size of a `width`×`height` RGBA8 image, or `None` when it does not fit `usize`
/// (the product is computed in `u64` with checked math, so a huge edge cannot wrap to a
/// small size).
#[must_use]
pub(crate) fn rgba8_bytes(width: u32, height: u32) -> Option<usize> {
    let bytes = u64::from(width).checked_mul(u64::from(height))?.checked_mul(4)?;
    usize::try_from(bytes).ok()
}

/// Levels 1.. of the mip chain of a `width`×`height` RGBA8 image, each `(w, h, bytes)`,
/// down to 1×1. Every level is a 2×2 box filter of the one above (the last output texel of an odd edge
/// also averages the orphan texel). `srgb`: colour channels are filtered in linear light (alpha linearly).
#[must_use]
pub fn mip_chain_rgba8(width: u32, height: u32, rgba: &[u8], srgb: bool) -> Vec<(u32, u32, Vec<u8>)> {
    // A short or oversized source has no chain (the callers fall back to a single level).
    if rgba8_bytes(width.max(1), height.max(1)).is_none_or(|n| rgba.len() < n) {
        return Vec::new();
    }
    let lut: Vec<f32> = (0..=255u8).map(decode).collect();
    let mut out: Vec<(u32, u32, Vec<u8>)> = Vec::new();
    let (mut w, mut h) = (width.max(1), height.max(1));
    while w > 1 || h > 1 {
        let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
        // Size the level like `rgba8_bytes` (a `u64` product): the first mip of a 65536²
        // source is 32768², 2³² bytes, which a `u32` product wraps to 0 (the empty level
        // then panicked in `downsample_rows`). A level that does not fit `usize` ends the
        // chain (the callers fall back to a single level, as for an oversized source).
        let Some(mip_bytes) = rgba8_bytes(nw, nh) else {
            return Vec::new();
        };
        let src: &[u8] = out.last().map_or(rgba, |(_, _, level)| level.as_slice());
        let mut dst = vec![0u8; mip_bytes];
        // Large levels split their rows over a few threads (each output texel depends only
        // on its own 2×2 source block, so the result is the same as one thread's).
        let threads = if u64::from(nw).saturating_mul(u64::from(nh)) >= 1 << 18 {
            std::thread::available_parallelism()
                .map_or(1, std::num::NonZero::get)
                .min(8)
        } else {
            1
        };
        let rows_per = (nh as usize).div_ceil(threads);
        // A row wider than `u32` bytes (a source wider than 2^31 texels) must not wrap the
        // stride. A product that does not fit `usize` ends the chain, as an oversized level does.
        let Some(row_bytes) = u64::from(nw).checked_mul(4).and_then(|n| usize::try_from(n).ok()) else {
            return Vec::new();
        };
        let Some(chunk_bytes) = rows_per.checked_mul(row_bytes) else {
            return Vec::new();
        };
        std::thread::scope(|scope| {
            for (chunk, rows) in dst.chunks_mut(chunk_bytes).enumerate() {
                let lut = &lut;
                let y0 = (chunk * rows_per) as u32;
                let mut run = move || downsample_rows(src, (w, h), rows, (nw, nh), y0, lut, srgb);
                if threads == 1 {
                    run();
                } else {
                    scope.spawn(run);
                }
            }
        });
        out.push((nw, nh, dst));
        w = nw;
        h = nh;
    }
    out
}

/// Output rows `y0..` (as many as `dst` holds) of the 2×2 box filter of a `w`×`h` level.
fn downsample_rows(
    src: &[u8],
    (w, h): (u32, u32),
    dst: &mut [u8],
    (nw, nh): (u32, u32),
    y0: u32,
    lut: &[f32],
    srgb: bool,
) {
    let Some(row_bytes) = u64::from(nw).checked_mul(4).and_then(|n| usize::try_from(n).ok()) else {
        return;
    };
    if row_bytes == 0 {
        return;
    }
    let rows_usize = dst.len() / row_bytes;
    let rows = u32::try_from(rows_usize).unwrap_or(u32::MAX);
    for dy in 0..rows {
        let y = y0 + dy;
        for x in 0..nw {
            // The last output texel of an odd edge also takes the orphan texel (`w - 1`).
            let (x0, x1) = (
                (2 * x).min(w - 1),
                if x + 1 == nw {
                    w - 1
                } else {
                    (2 * x + 1).min(w - 1)
                },
            );
            let (y0s, y1) = (
                (2 * y).min(h - 1),
                if y + 1 == nh {
                    h - 1
                } else {
                    (2 * y + 1).min(h - 1)
                },
            );
            let count = ((x1 - x0 + 1) * (y1 - y0s + 1)) as f32;
            for c in 0..4usize {
                let mut acc = 0.0f32;
                for sy in y0s..=y1 {
                    for sx in x0..=x1 {
                        // `u64` offsets: a 65536-wide source index exceeds `u32`.
                        let v = src[((u64::from(sy) * u64::from(w) + u64::from(sx)) * 4) as usize + c];
                        acc += if srgb && c < 3 {
                            lut[v as usize]
                        } else {
                            f32::from(v)
                        };
                    }
                }
                let mean = acc / count;
                dst[((u64::from(dy) * u64::from(nw) + u64::from(x)) * 4) as usize + c] = if srgb && c < 3 {
                    encode(mean)
                } else {
                    (mean + 0.5).min(255.0) as u8
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba8_bytes_does_not_wrap_and_short_sources_have_no_chain() {
        // 65536² × 4 wrapped to 0 in u32 arithmetic.
        #[cfg(target_pointer_width = "64")]
        assert_eq!(rgba8_bytes(65_536, 65_536), Some(17_179_869_184));
        // The first mip of that source is 32768²: 2³² bytes. The chain must size it in u64
        // (a u32 product is 0, and the empty level panicked in `downsample_rows`).
        #[cfg(target_pointer_width = "64")]
        assert_eq!(rgba8_bytes(32_768, 32_768), Some(4_294_967_296));
        assert_eq!(rgba8_bytes(2, 3), Some(24));
        // A level whose byte size overflows u64 has no size at all (checked math, no wrap).
        assert_eq!(rgba8_bytes(u32::MAX, u32::MAX), None);
        assert!(mip_chain_rgba8(65_536, 65_536, &[], true).is_empty());
        assert!(mip_chain_rgba8(4, 4, &[0; 8], false).is_empty());
    }

    #[test]
    fn the_chain_ends_at_one_texel_and_keeps_the_mean() {
        // 4×2 checker of black and white: every level averages to mid grey (in linear light
        // for colour: sRGB 188, not 128).
        let mut rgba = Vec::new();
        for y in 0..2u32 {
            for x in 0..4u32 {
                let v = if (x + y) % 2 == 0 { 255 } else { 0 };
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let chain = mip_chain_rgba8(4, 2, &rgba, true);
        assert_eq!(chain.len(), 2);
        assert_eq!((chain[0].0, chain[0].1), (2, 1));
        assert_eq!((chain[1].0, chain[1].1), (1, 1));
        let last = &chain[1].2;
        assert!(
            (186..=190).contains(&last[0]),
            "linear-light mean of black and white: {}",
            last[0]
        );
        assert_eq!(last[3], 255);
        let linear = mip_chain_rgba8(4, 2, &rgba, false);
        assert!((127..=128).contains(&linear[1].2[0]), "{}", linear[1].2[0]);
    }

    /// A mip row whose byte stride does not fit in `u32` (the first mip of a source wider than
    /// 2³¹ texels). The row loop used to multiply that stride in `u32` and panic in debug, or
    /// wrap it to 0 and panic in `chunks_mut` in release.
    #[test]
    fn a_mip_row_wider_than_a_u32_does_not_panic() {
        let lut: Vec<f32> = (0..=255u8).map(decode).collect();
        let src = [255u8, 0, 0, 255];
        let mut dst = [7u8; 4];
        downsample_rows(&src, (1 << 31, 1), &mut dst, (1 << 30, 1), 0, &lut, false);
        assert_eq!(dst, [7, 7, 7, 7], "a short destination holds no output row");
    }

    #[test]
    fn an_odd_edge_texel_reaches_the_next_level() {
        // 3×1 row [black, black, white]: the white orphan must show in the 1×1 mip.
        let rgba = [0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255, 255];
        let chain = mip_chain_rgba8(3, 1, &rgba, false);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].2, vec![85, 85, 85, 255]);
    }
}
