// SPDX-License-Identifier: MPL-2.0
//! Normal-variance roughness at mip generation (Toksvig 2005, "Mipmapping normal maps").
//!
//! A cooked normal map is two-channel (BC5, z rebuilt in the shader), so the runtime cannot read
//! the spread of the normals a mip texel averages from the sample's length, as it does for RGBA8
//! maps (`regia_render::specular_aa`). The cook bakes that spread into the packed ORM instead:
//! [`apply_normal_variance_roughness`] raises the roughness channel (G) of each ORM mip by the
//! Toksvig variance `σ² = (1 − |N̄|)/|N̄|` of the normal map over the texel's footprint,
//! `α'² = α² + 2σ²` ([`regia_render::specular_aa::widen_roughness`]). Level 0 at the normal
//! map's own resolution is unchanged (one unit normal per texel has no spread), so close-ups do
//! not move; distant, minified normal detail turns into the roughness it really has instead of
//! sparkling.

use regia_render::specular_aa::{toksvig_variance, widen_roughness};

/// Decoded tangent-space unit normal of an RGBA8 texel, X/Y scaled by `scale` (glTF
/// `normalTexture.scale`) before normalising. A degenerate texel reads as +Z.
fn decode_normal(texel: &[u8; 4], scale: f32) -> [f32; 3] {
    let x = (f32::from(texel[0]) / 255.0 * 2.0 - 1.0) * scale;
    let y = (f32::from(texel[1]) / 255.0 * 2.0 - 1.0) * scale;
    let z = f32::from(texel[2]) / 255.0 * 2.0 - 1.0;
    let len = (x * x + y * y + z * z).sqrt();
    if len > 1e-6 {
        [x / len, y / len, z / len]
    } else {
        [0.0, 0.0, 1.0]
    }
}

/// One level of mean normal vectors (unnormalised averages of unit normals).
struct MeanLevel {
    width: u32,
    height: u32,
    mean: Vec<[f32; 3]>,
}

impl MeanLevel {
    /// 2×2 box average of `self` (the last sample of an odd edge also takes the remainder texel, as the mip chains do).
    fn half(&self) -> Self {
        let (w, h) = (self.width, self.height);
        let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
        let mut mean = vec![[0.0f32; 3]; (nw * nh) as usize];
        for y in 0..nh {
            let (y0, y1) = crate::texture_settings::box_window(y, nh, h);
            for x in 0..nw {
                let (x0, x1) = crate::texture_settings::box_window(x, nw, w);
                let inv = 1.0 / ((x1 - x0) * (y1 - y0)) as f32;
                let mut acc = [0.0f32; 3];
                for sy in y0..y1 {
                    for sx in x0..x1 {
                        let v = self.mean[(sy * w + sx) as usize];
                        acc[0] += v[0];
                        acc[1] += v[1];
                        acc[2] += v[2];
                    }
                }
                mean[(y * nw + x) as usize] = [acc[0] * inv, acc[1] * inv, acc[2] * inv];
            }
        }
        Self {
            width: nw,
            height: nh,
            mean,
        }
    }

    /// Mean-vector length at the texel covering normalised position `(u, v)` (nearest).
    fn length_at(&self, u: f32, v: f32) -> f32 {
        let x = ((u * self.width as f32) as u32).min(self.width - 1);
        let y = ((v * self.height as f32) as u32).min(self.height - 1);
        let m = self.mean[(y * self.width + x) as usize];
        (m[0] * m[0] + m[1] * m[1] + m[2] * m[2]).sqrt()
    }
}

/// The normal-map level whose resolution matches a `width`×`height` mip: `log2` of the
/// larger of the two size ratios, rounded (0 when the mip is as large as the normal map or
/// larger).
fn matching_level(normal_w: u32, normal_h: u32, width: u32, height: u32) -> u32 {
    let rx = normal_w as f32 / width.max(1) as f32;
    let ry = normal_h as f32 / height.max(1) as f32;
    // The chain of mean normals ends at 1x1, `floor(log2(larger edge))` halvings down: a mip
    // smaller than that (a 1x1 ORM level under a 12x4 normal map) reads the last level.
    let deepest = halvings_to_unit(normal_w, normal_h);
    (rx.max(ry).max(1.0).log2().round() as u32).min(deepest)
}

/// Halvings from `width`×`height` to 1×1 (each halving floors an edge, never below 1).
fn halvings_to_unit(width: u32, height: u32) -> u32 {
    width.max(height).max(1).ilog2()
}

/// Raise the roughness channel (G) of a packed ORM mip chain by the normal-map variance over
/// each texel's footprint (Toksvig; see the module docs).
///
/// - `orm_levels`: the full chain, level 0 first, each `(width, height, RGBA8)`; G is the glTF
///   roughness channel that scales `roughness_factor` in the shader.
/// - `normal`: the tangent-space normal map (`width`, `height`, RGBA8, linear), at any
///   resolution. Its mean normals are box-averaged level by level from its own base.
/// - `normal_scale`: glTF `normalTexture.scale` (X/Y scale). `<= 0` leaves the chain unchanged.
/// - `roughness_factor`: the material's roughness factor. The shader shades
///   `factor · G`, so the baked G is `widen(factor · G) / factor`, clamped to 1. A zero
///   factor shades mirror-smooth regardless of G and is left unchanged.
///
/// Returns the number of texels whose roughness rose. A malformed normal map (bytes shorter
/// than `width · height · 4`) leaves the chain unchanged.
pub(crate) fn apply_normal_variance_roughness(
    orm_levels: &mut [(u32, u32, Vec<u8>)],
    normal: (u32, u32, &[u8]),
    normal_scale: f32,
    roughness_factor: f32,
) -> usize {
    let (nw, nh, bytes) = normal;
    if normal_scale <= 0.0
        || roughness_factor <= 0.0
        || nw == 0
        || nh == 0
        || bytes.len() < (nw * nh * 4) as usize
    {
        return 0;
    }
    let _span = tracing::info_span!("normal_variance_roughness", nw, nh).entered();
    // The deepest normal level any ORM mip needs; nothing finer than level 1 is ever stored
    // as floats (level 0 is one unit normal per texel: no variance).
    let deepest = orm_levels
        .iter()
        .map(|(w, h, _)| matching_level(nw, nh, *w, *h))
        .max()
        .unwrap_or(0);
    if deepest == 0 {
        return 0;
    }
    let base = MeanLevel {
        width: nw,
        height: nh,
        mean: bytes
            .as_chunks::<4>()
            .0
            .iter()
            .take((nw * nh) as usize)
            .map(|t| decode_normal(t, normal_scale))
            .collect(),
    };
    let mut level = base.half();
    drop(base);
    let mut index = 1u32;
    let mut raised = 0usize;
    loop {
        for (w, h, rgba) in orm_levels.iter_mut() {
            if matching_level(nw, nh, *w, *h) != index {
                continue;
            }
            let (w, h) = (*w, *h);
            for y in 0..h {
                let v = (y as f32 + 0.5) / h as f32;
                for x in 0..w {
                    let u = (x as f32 + 0.5) / w as f32;
                    let sigma2 = toksvig_variance(level.length_at(u, v));
                    if sigma2 <= 0.0 {
                        continue;
                    }
                    let i = ((y * w + x) * 4 + 1) as usize;
                    let Some(g) = rgba.get_mut(i) else {
                        continue;
                    };
                    let shaded = roughness_factor * f32::from(*g) / 255.0;
                    let widened = widen_roughness(shaded, sigma2);
                    let baked = ((widened / roughness_factor).min(1.0) * 255.0 + 0.5) as u8;
                    if baked > *g {
                        *g = baked;
                        raised += 1;
                    }
                }
            }
        }
        if index >= deepest || (level.width == 1 && level.height == 1) {
            break;
        }
        level = level.half();
        index += 1;
    }
    raised
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_of_an_odd_edge_averages_the_remainder_texel() {
        let level = MeanLevel {
            width: 3,
            height: 1,
            mean: vec![[0.0, 0.0, 1.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]],
        };
        let half = level.half();
        assert_eq!((half.width, half.height), (1, 1));
        let m = half.mean[0];
        assert!(
            (m[0] - 1.0 / 3.0).abs() < 1e-6 && (m[2] - 2.0 / 3.0).abs() < 1e-6,
            "{m:?}"
        );
    }

    fn flat_normal(w: u32, h: u32) -> Vec<u8> {
        [128u8, 128, 255, 255].repeat((w * h) as usize)
    }

    /// A checker of normals tilted ±`tilt` radians about Y.
    fn checker_normal(w: u32, h: u32, tilt: f32) -> Vec<u8> {
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let s = if (x + y) % 2 == 0 { 1.0 } else { -1.0 };
                let n = [s * tilt.sin(), 0.0, tilt.cos()];
                out.extend(n.iter().map(|c| ((c * 0.5 + 0.5) * 255.0 + 0.5) as u8));
                out.push(255);
            }
        }
        out
    }

    fn chain(w: u32, h: u32, g: u8) -> Vec<(u32, u32, Vec<u8>)> {
        let base = [255u8, g, 0, 255].repeat((w * h) as usize);
        crate::texture_compress::generate_mips_rgba8(w, h, &base)
    }

    #[test]
    fn a_flat_normal_map_leaves_every_level_unchanged() {
        let mut levels = chain(16, 16, 100);
        let before = levels.clone();
        let raised = apply_normal_variance_roughness(&mut levels, (16, 16, &flat_normal(16, 16)), 1.0, 1.0);
        assert_eq!(raised, 0);
        assert_eq!(levels, before);
    }

    #[test]
    fn a_bumpy_normal_map_roughens_the_mips_but_not_level_zero() {
        let mut levels = chain(16, 16, 60);
        let normal = checker_normal(16, 16, 0.5);
        let raised = apply_normal_variance_roughness(&mut levels, (16, 16, &normal), 1.0, 1.0);
        assert!(raised > 0);
        // Level 0: same resolution as the normal map, one normal per texel.
        assert!(levels[0].2.chunks(4).all(|t| t[1] == 60));
        // Level 1: each texel averages ±0.5 rad: |N̄| = cos 0.5 (quantised).
        let len = 0.5f32.cos();
        let expect = widen_roughness(60.0 / 255.0, toksvig_variance(len));
        let g = f32::from(levels[1].2[1]) / 255.0;
        assert!((g - expect).abs() < 0.02, "{g} vs {expect}");
        assert!(
            levels[1..]
                .iter()
                .all(|(_, _, px)| px.chunks(4).all(|t| t[1] > 60))
        );
        // Only G moves.
        assert!(
            levels
                .iter()
                .all(|(_, _, px)| px.chunks(4).all(|t| t[0] == 255 && t[2] == 0))
        );
    }

    #[test]
    fn the_factor_is_folded_into_the_baked_channel() {
        let normal = checker_normal(8, 8, 0.15);
        // factor 1 · G 80 and factor 0.5 · G 160 shade the same roughness before and after.
        let mut full = chain(8, 8, 80);
        let mut half = chain(8, 8, 160);
        apply_normal_variance_roughness(&mut full, (8, 8, &normal), 1.0, 1.0);
        apply_normal_variance_roughness(&mut half, (8, 8, &normal), 1.0, 0.5);
        let shaded_full = f32::from(full[1].2[1]) / 255.0;
        let shaded_half = 0.5 * f32::from(half[1].2[1]) / 255.0;
        assert!(full[1].2[1] > 80 && half[1].2[1] > 160 && half[1].2[1] < 255);
        assert!(
            (shaded_full - shaded_half).abs() < 0.01,
            "{shaded_full} vs {shaded_half}"
        );
    }

    #[test]
    fn a_larger_normal_map_roughens_level_zero_and_a_zero_scale_does_nothing() {
        // A 16² normal map on an 8² ORM: ORM level 0 covers 2×2 normals.
        let normal = checker_normal(16, 16, 0.5);
        let mut levels = chain(8, 8, 60);
        apply_normal_variance_roughness(&mut levels, (16, 16, &normal), 1.0, 1.0);
        assert!(levels[0].2.chunks(4).all(|t| t[1] > 60));
        let mut untouched = chain(8, 8, 60);
        let before = untouched.clone();
        assert_eq!(
            apply_normal_variance_roughness(&mut untouched, (16, 16, &normal), 0.0, 1.0),
            0
        );
        assert_eq!(untouched, before);
        // Short normal bytes are rejected, not indexed.
        assert_eq!(
            apply_normal_variance_roughness(&mut untouched, (16, 16, &normal[..10]), 1.0, 1.0),
            0
        );
    }

    #[test]
    fn non_finite_and_degenerate_inputs_never_panic_or_poison_the_chain() {
        let normal = checker_normal(8, 8, 0.5);
        for (scale, factor) in [
            (f32::NAN, 1.0),
            (f32::INFINITY, 1.0),
            (1.0, f32::NAN),
            (1.0, f32::INFINITY),
            (-1.0, 1.0),
            (1.0, -1.0),
            (1.0, 1e-30),
        ] {
            let mut levels = chain(8, 8, 90);
            let raised = apply_normal_variance_roughness(&mut levels, (8, 8, &normal), scale, factor);
            let _ = raised;
            // Whatever the inputs, a texel only ever rises, and only G changes.
            for (_, _, px) in &levels {
                assert!(px.chunks(4).all(|t| t[1] >= 90 && t[0] == 255 && t[2] == 0));
            }
        }
        // An empty chain, a zero-sized normal map, and a one-texel normal map.
        assert_eq!(
            apply_normal_variance_roughness(&mut [], (8, 8, &normal), 1.0, 1.0),
            0
        );
        let mut levels = chain(8, 8, 90);
        assert_eq!(
            apply_normal_variance_roughness(&mut levels, (0, 8, &[]), 1.0, 1.0),
            0
        );
        assert_eq!(
            apply_normal_variance_roughness(&mut levels, (1, 1, &[128, 128, 255, 255]), 1.0, 1.0),
            0
        );
    }

    #[test]
    fn non_square_and_odd_normal_maps_cover_every_mip() {
        let normal = checker_normal(12, 4, 0.5);
        let mut levels = chain(12, 4, 40);
        let raised = apply_normal_variance_roughness(&mut levels, (12, 4, &normal), 1.0, 1.0);
        assert!(raised > 0);
        assert!(levels[0].2.chunks(4).all(|t| t[1] == 40), "level 0 unchanged");
        for (w, h, px) in &levels[1..] {
            assert!(px.chunks(4).all(|t| t[1] > 40), "{w}x{h} was not roughened");
        }
    }

    #[test]
    fn repeated_runs_give_identical_bytes() {
        let normal = checker_normal(16, 16, 0.3);
        let run = || {
            let mut levels = chain(16, 16, 77);
            let raised = apply_normal_variance_roughness(&mut levels, (16, 16, &normal), 1.5, 0.8);
            (raised, levels)
        };
        assert_eq!(run(), run());
    }
}
