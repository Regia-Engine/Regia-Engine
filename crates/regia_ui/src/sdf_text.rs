// SPDX-License-Identifier: MPL-2.0
//! Signed-distance-field text (ADR-273 L07, ADR-2045): opt-in per node with [`SdfText`].
//!
//! A node marked [`SdfText`] draws its monochrome glyphs from distance fields instead of
//! coverage bitmaps: each glyph is rasterized once at [`SDF_REF_PX`], turned into a signed
//! distance field with a [`SDF_SPREAD_PX`] margin ([`distance_field`]), stored in the normal
//! monochrome atlas under its own key, and drawn at any size by overlay kind 9, which turns the
//! sampled distance into coverage at the screen scale. One field serves every size, so text whose
//! size animates or that is drawn large stays sharp without re-rasterizing per size. Color emoji
//! keep the bitmap path. Nodes without the marker draw exactly as before.
//!
//! The field is the exact Euclidean distance transform of the glyph's inside and outside sets
//! (Felzenszwalb and Huttenlocher's lower-envelope algorithm), refined at the anti-aliased edge by
//! the coverage, encoded in 8 bits as `0.5 - d / (2 · spread)` (inside above 0.5).
//! [`sdf_coverage`] is the CPU model of the shader.

use cosmic_text as ct;
use regia_app::prelude::*;

use crate::draw::{TextureRef, UiQuad};

/// Size the distance fields are built at, in px.
pub const SDF_REF_PX: f32 = 48.0;
/// Distance range encoded around the edge, in reference px (also the field's padding).
pub const SDF_SPREAD_PX: u32 = 6;
/// The atlas key bit that keeps distance fields apart from coverage bitmaps of the same glyph.
const SDF_KEY_BIT: u32 = 1 << 30;

/// Marker: draw this text node's monochrome glyphs from signed distance fields (ADR-2045).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SdfText;

/// The atlas key of the distance field of the glyph shaped as `key`: same face, glyph and
/// weight at [`SDF_REF_PX`], no sub-pixel bin, plus a private flag bit.
pub fn sdf_key(key: ct::CacheKey) -> ct::CacheKey {
    ct::CacheKey {
        font_size_bits: SDF_REF_PX.to_bits(),
        x_bin: ct::SubpixelBin::Zero,
        y_bin: ct::SubpixelBin::Zero,
        flags: key.flags | ct::CacheKeyFlags::from_bits_retain(SDF_KEY_BIT),
        ..key
    }
}

/// The rasterizer key behind an [`sdf_key`] (the flag bit removed).
pub fn raster_key(key: ct::CacheKey) -> ct::CacheKey {
    ct::CacheKey {
        flags: key
            .flags
            .difference(ct::CacheKeyFlags::from_bits_retain(SDF_KEY_BIT)),
        ..sdf_key(key)
    }
}

/// One-dimensional squared Euclidean distance transform of `f` (0 on the set, a large value
/// elsewhere) into `d`: the lower envelope of parabolas rooted at each sample (Felzenszwalb and
/// Huttenlocher, "Distance Transforms of Sampled Functions", 2012). `v` and `z` are scratch.
fn edt_1d(f: &[f32], d: &mut [f32], v: &mut [usize], z: &mut [f32]) {
    let n = f.len();
    if n == 0 {
        return;
    }
    let mut k = 0usize;
    v[0] = 0;
    z[0] = f32::NEG_INFINITY;
    z[1] = f32::INFINITY;
    for (q, &f_q) in f.iter().enumerate().skip(1) {
        let fq = f_q + (q * q) as f32;
        let mut s;
        loop {
            let p = v[k];
            s = (fq - (f[p] + (p * p) as f32)) / (2.0 * (q as f32 - p as f32));
            if s <= z[k] && k > 0 {
                k -= 1;
            } else {
                break;
            }
        }
        if s <= z[k] {
            // k == 0 and the new parabola wins everywhere.
            v[0] = q;
            z[0] = f32::NEG_INFINITY;
            z[1] = f32::INFINITY;
            k = 0;
            continue;
        }
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f32::INFINITY;
    }
    k = 0;
    for (q, out) in d.iter_mut().enumerate() {
        while z[k + 1] < q as f32 {
            k += 1;
        }
        let p = v[k];
        let dq = q as f32 - p as f32;
        *out = dq * dq + f[p];
    }
}

/// Squared distance from every pixel to the nearest pixel where `set` is true (0 on the set).
fn edt_2d(set: &[bool], w: usize, h: usize) -> Vec<f32> {
    const FAR: f32 = 1.0e12;
    let mut grid: Vec<f32> = set.iter().map(|s| if *s { 0.0 } else { FAR }).collect();
    let n = w.max(h);
    let (mut f, mut d) = (vec![0.0f32; n], vec![0.0f32; n]);
    let (mut v, mut z) = (vec![0usize; n], vec![0.0f32; n + 1]);
    for x in 0..w {
        for (y, fy) in f.iter_mut().enumerate().take(h) {
            *fy = grid[y * w + x];
        }
        edt_1d(&f[..h], &mut d[..h], &mut v, &mut z);
        for (y, dy) in d.iter().enumerate().take(h) {
            grid[y * w + x] = *dy;
        }
    }
    for y in 0..h {
        f[..w].copy_from_slice(&grid[y * w..(y + 1) * w]);
        edt_1d(&f[..w], &mut d[..w], &mut v, &mut z);
        grid[y * w..(y + 1) * w].copy_from_slice(&d[..w]);
    }
    grid
}

/// The 8-bit signed distance field of a `w × h` coverage mask, padded by `spread` px on every
/// side: `(field, width, height)`. Inside the glyph the value is above 128, at the 50 % edge
/// about 128, and it falls to 0 `spread` px outside.
pub fn distance_field(mask: &[u8], w: u32, h: u32, spread: u32) -> Option<(Vec<u8>, u32, u32)> {
    let (w, h, s) = (w as usize, h as usize, spread as usize);
    if mask.len() < w.checked_mul(h)? {
        return None;
    }
    let (pw, ph) = (w + 2 * s, h + 2 * s);
    let mut cov = vec![0u8; pw.checked_mul(ph)?];
    for y in 0..h {
        cov[(y + s) * pw + s..(y + s) * pw + s + w].copy_from_slice(&mask[y * w..(y + 1) * w]);
    }
    let inside: Vec<bool> = cov.iter().map(|a| *a >= 128).collect();
    let outside: Vec<bool> = inside.iter().map(|i| !i).collect();
    let to_inside = edt_2d(&inside, pw, ph);
    let to_outside = edt_2d(&outside, pw, ph);
    let range = 2.0 * spread.max(1) as f32;
    let field = (0..pw * ph)
        .map(|i| {
            let a = f32::from(cov[i]) / 255.0;
            // Signed distance in px to the 50 % contour, negative inside.
            let mut sd = if inside[i] {
                0.5 - to_outside[i].sqrt()
            } else {
                to_inside[i].sqrt() - 0.5
            };
            if a > 0.0 && a < 1.0 && sd.abs() <= 1.0 {
                sd = 0.5 - a;
            }
            ((0.5 - sd / range).clamp(0.0, 1.0) * 255.0).round() as u8
        })
        .collect();
    Some((field, u32::try_from(pw).ok()?, u32::try_from(ph).ok()?))
}

/// The distance-field image of the glyph behind `key` (an [`sdf_key`] or any shaped key): the
/// glyph rasterized at [`SDF_REF_PX`] and transformed, with the placement grown by the spread.
/// `None` for color glyphs and empty bitmaps.
pub fn sdf_image(fonts: &mut crate::text::TextSystem, key: ct::CacheKey) -> Option<ct::SwashImage> {
    let raster = fonts.glyph_image(raster_key(key))?;
    if !matches!(raster.content, ct::SwashContent::Mask) {
        return None;
    }
    let (w, h) = (raster.placement.width, raster.placement.height);
    if w == 0 || h == 0 {
        return None;
    }
    let (field, fw, fh) = distance_field(&raster.data, w, h, SDF_SPREAD_PX)?;
    let mut image = ct::SwashImage::new();
    image.content = ct::SwashContent::Mask;
    image.placement.left = raster.placement.left - SDF_SPREAD_PX as i32;
    image.placement.top = raster.placement.top + SDF_SPREAD_PX as i32;
    image.placement.width = fw;
    image.placement.height = fh;
    image.data = field;
    Some(image)
}

/// CPU model of overlay kind 9: the coverage of a pixel that samples field value `v` when one
/// reference px spans `px_per_ref` screen px.
pub fn sdf_coverage(v: u8, px_per_ref: f32) -> f32 {
    let d = (f32::from(v) / 255.0 - 0.5) * 2.0 * SDF_SPREAD_PX as f32 * px_per_ref;
    (d + 0.5).clamp(0.0, 1.0)
}

/// The draw quad of one distance-field glyph: the field's atlas `rect` on `page` (offset
/// `offset` at the reference size) scaled to `font_px`, its pen at `pen` (physical px).
pub fn sdf_quad(
    pen: [f32; 2],
    offset: [i32; 2],
    page: u32,
    rect: [u32; 4],
    page_px: f32,
    font_px: f32,
) -> UiQuad {
    let s = (font_px / SDF_REF_PX).max(1e-3);
    let pos = [pen[0] + offset[0] as f32 * s, pen[1] + offset[1] as f32 * s];
    let size = [rect[2] as f32 * s, rect[3] as f32 * s];
    UiQuad {
        uv: [
            rect[0] as f32 / page_px,
            rect[1] as f32 / page_px,
            (rect[0] + rect[2]) as f32 / page_px,
            (rect[1] + rect[3]) as f32 / page_px,
        ],
        border_width: [s, SDF_SPREAD_PX as f32, 0.0, 0.0],
        texture: TextureRef::SdfGlyph(page),
        ..UiQuad::fill(pos, size, crate::color::UiColor::WHITE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square_mask(n: usize, lo: usize, hi: usize) -> Vec<u8> {
        let mut m = vec![0u8; n * n];
        for y in lo..hi {
            for x in lo..hi {
                m[y * n + x] = 255;
            }
        }
        m
    }

    #[test]
    fn edt_matches_brute_force() {
        let (w, h) = (9usize, 7usize);
        let set: Vec<bool> = (0..w * h).map(|i| i == 10 || i == 40 || i == 62).collect();
        let fast = edt_2d(&set, w, h);
        for y in 0..h {
            for x in 0..w {
                let brute = (0..w * h)
                    .filter(|i| set[*i])
                    .map(|i| {
                        let (sx, sy) = ((i % w) as f32, (i / w) as f32);
                        (sx - x as f32).powi(2) + (sy - y as f32).powi(2)
                    })
                    .fold(f32::INFINITY, f32::min);
                assert!(
                    (fast[y * w + x] - brute).abs() < 1e-3,
                    "({x},{y}) {} vs {brute}",
                    fast[y * w + x]
                );
            }
        }
    }

    #[test]
    fn field_is_high_inside_mid_at_the_edge_low_outside() {
        let mask = square_mask(20, 5, 15);
        let (f, w, h) = distance_field(&mask, 20, 20, 6).unwrap();
        assert_eq!((w, h), (32, 32));
        let at = |x: usize, y: usize| f[y * 32 + x];
        // Centre of the square (10, 10) + padding 6.
        assert!(at(16, 16) > 200, "{}", at(16, 16));
        // Pixel just inside the left edge (x = 5 → 11) and just outside (x = 4 → 10).
        assert!(at(11, 16) > 128 && at(11, 16) < 160, "{}", at(11, 16));
        assert!(at(10, 16) < 128 && at(10, 16) > 96, "{}", at(10, 16));
        assert_eq!(at(0, 0), 0, "far outside");
        // Monotonic away from the edge along the row.
        for x in 1..11 {
            assert!(at(x, 16) >= at(x - 1, 16));
        }
        assert!(distance_field(&mask, 21, 20, 6).is_none(), "short mask");
    }

    #[test]
    fn coverage_model_is_sharp_at_any_scale() {
        // At the edge value the pixel is half covered at every scale.
        assert!((sdf_coverage(128, 1.0) - 0.5).abs() < 0.05);
        assert!((sdf_coverage(128, 8.0) - 0.5).abs() < 0.3);
        // Deep inside and far outside saturate.
        assert_eq!(sdf_coverage(255, 1.0), 1.0);
        assert_eq!(sdf_coverage(0, 1.0), 0.0);
        // The ramp narrows as the text grows: a value just inside the edge is fully in at 8x
        // and only partly at half size.
        assert_eq!(sdf_coverage(140, 8.0), 1.0);
        assert!(sdf_coverage(140, 0.5) < 1.0);
    }

    #[test]
    fn keys_are_distinct_and_round_trip() {
        let key = ct::CacheKey {
            font_id: ct::fontdb::ID::dummy(),
            glyph_id: 7,
            font_size_bits: 18.0f32.to_bits(),
            x_bin: ct::SubpixelBin::Two,
            y_bin: ct::SubpixelBin::Zero,
            font_weight: ct::fontdb::Weight::NORMAL,
            flags: ct::CacheKeyFlags::empty(),
        };
        let s = sdf_key(key);
        assert_ne!(s, key);
        assert_eq!(s.font_size_bits, SDF_REF_PX.to_bits());
        let r = raster_key(s);
        assert_eq!(r.flags, ct::CacheKeyFlags::empty());
        assert_ne!(r, s);
        assert_eq!(sdf_key(r), s);
    }

    #[test]
    fn real_glyph_builds_a_padded_field() {
        let mut fonts = crate::text::TextSystem::embedded_only();
        let loc = crate::localization::Localization::default();
        let shaped = fonts.shape(&crate::text::Text::new("O"), 1.0, None, &loc);
        let glyph = shaped.glyphs.first().expect("one glyph");
        let raster = fonts.glyph_image(raster_key(glyph.cache_key)).expect("raster");
        let image = sdf_image(&mut fonts, glyph.cache_key).expect("field");
        assert_eq!(image.placement.width, raster.placement.width + 2 * SDF_SPREAD_PX);
        assert_eq!(image.placement.left, raster.placement.left - SDF_SPREAD_PX as i32);
        // The hole of the "O" is outside (below 128), its ring inside somewhere.
        assert!(image.data.iter().any(|v| *v > 160));
        let (w, h) = (image.placement.width as usize, image.placement.height as usize);
        assert!(
            image.data[(h / 2) * w + w / 2] < 128,
            "the counter of the O is outside"
        );
        let q = sdf_quad([10.0, 20.0], [-6, -40], 0, [0, 0, 32, 48], 512.0, 24.0);
        assert_eq!(q.size, [16.0, 24.0]);
        assert_eq!(q.pos, [7.0, 0.0]);
        assert_eq!(q.texture, TextureRef::SdfGlyph(0));
    }
}
