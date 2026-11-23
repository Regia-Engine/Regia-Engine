// SPDX-License-Identifier: MPL-2.0
//! Runtime lookups of a lightmap on the CPU, the mirrors of `regia_render`'s
//! `shaders/lightmap.wgsl` (ADR-1556):
//!
//! - **Bicubic** ([`Lightmap::sample_bicubic`]): a uniform cubic B-spline over the 4×4 texels
//!   around the lookup, clamp to edge. The GPU evaluates the same filter with four bilinear taps
//!   (Sigg & Hadwiger 2005, *GPU Gems 2* ch. 20). The B-spline smooths rather than interpolates:
//!   it hides the texel grid of a low-density lightmap and reproduces linear gradients exactly.
//!   Its footprint reaches two texels past the lookup, which the bake's `padding + 1` dilation
//!   rounds cover.
//! - **Directions** ([`encode_direction`], [`decode_direction`], [`shade_directional`]): per
//!   texel, the luminance-weighted mean direction `d` of the baked light (`|d| <= 1`, 1 when all
//!   of it comes from one direction) and `w = n0 · d`, its agreement with the normal `n0` the
//!   texel was baked for. Irradiance for a shading normal `n` is rebalanced with a half-cosine
//!   lobe around `d`: `E(n) = E · (1 + n · d) / (1 + w)`. At `n = n0` this is `E` exactly; with
//!   `d = 0` (light from everywhere) the normal changes nothing; the factor stays in `0..=2`.
//!   Ramamoorthi & Hanrahan (2001) show irradiance is dominated by its constant and linear terms
//!   in the normal; the half-cosine lobe is the cheapest bounded linear term.

use super::Lightmap;

/// Uniform cubic B-spline weights of the four taps around a lookup at fraction `t` (0..1) past
/// the second tap.
#[must_use]
pub(crate) fn bspline_weights(t: f32) -> [f32; 4] {
    let (t2, t3) = (t * t, t * t * t);
    let s = 1.0 - t;
    [
        s * s * s / 6.0,
        (3.0 * t3 - 6.0 * t2 + 4.0) / 6.0,
        (-3.0 * t3 + 3.0 * t2 + 3.0 * t + 1.0) / 6.0,
        t3 / 6.0,
    ]
}

/// Packs a direction `[x, y, z, w]` (`xyz` in `-1..=1`, `w` in `0..=1`) as RGBA8 unorm, red in
/// the low byte: `xyz · 0.5 + 0.5` and `w`, each rounded to 1/255. Out-of-range values clamp; a
/// non-finite component encodes as 0.
#[must_use]
pub fn encode_direction(d: [f32; 4]) -> u32 {
    let unorm = |v: f32| -> u32 { (v.clamp(0.0, 1.0) * 255.0).round() as u32 };
    let finite = |v: f32| if v.is_finite() { v } else { 0.0 };
    let [x, y, z, w] = d.map(finite);
    unorm(x * 0.5 + 0.5) | (unorm(y * 0.5 + 0.5) << 8) | (unorm(z * 0.5 + 0.5) << 16) | (unorm(w) << 24)
}

/// Unpacks [`encode_direction`] the way the GPU reads an `Rgba8Unorm` texel: `xyz` back to
/// `-1..=1`, `w` in `0..=1`.
#[must_use]
pub fn decode_direction(texel: u32) -> [f32; 4] {
    let byte = |k: u32| ((texel >> (8 * k)) & 0xff) as f32 / 255.0;
    [
        byte(0) * 2.0 - 1.0,
        byte(1) * 2.0 - 1.0,
        byte(2) * 2.0 - 1.0,
        byte(3),
    ]
}

/// Irradiance from a directional lightmap texel for the unit shading normal `n`:
/// `E · max(1 + n · d, 0) / max(1 + w, 1)` with `[dx, dy, dz, w] = direction`
/// (`lightmap_shade_directional` in WGSL).
#[must_use]
pub fn shade_directional(irradiance: [f32; 3], direction: [f32; 4], n: [f32; 3]) -> [f32; 3] {
    let nd = n[0] * direction[0] + n[1] * direction[1] + n[2] * direction[2];
    let factor = (1.0 + nd).max(0.0) / (1.0 + direction[3]).max(1.0);
    irradiance.map(|e| e * factor)
}

impl Lightmap {
    /// Bicubic B-spline irradiance (lux) at atlas UV (`0..1`, texel centres at
    /// `(i + 0.5) / width`), clamp to edge: the lookup `shaders/lightmap.wgsl` does when its
    /// bicubic flag is on. Unlike [`Self::sample`], empty texels are not skipped, as on the GPU:
    /// the gutters stand in for them.
    #[must_use]
    pub fn sample_bicubic(&self, uv: [f32; 2]) -> [f32; 3] {
        self.sample_bicubic_page(0, uv)
    }

    /// [`Self::sample_bicubic`] on page `page` (clamped to that page's edges, ADR-3300); black past
    /// the last page.
    #[must_use]
    pub fn sample_bicubic_page(&self, page: u32, uv: [f32; 2]) -> [f32; 3] {
        if !self.holds_page(page) {
            return [0.0; 3];
        }
        let row0 = page * self.height;
        let fx = uv[0] * self.width as f32 - 0.5;
        let fy = uv[1] * self.height as f32 - 0.5;
        let (x0, y0) = (fx.floor(), fy.floor());
        let (wx, wy) = (bspline_weights(fx - x0), bspline_weights(fy - y0));
        let clamp = |v: f32, n: u32| v.clamp(0.0, (n - 1) as f32) as u32;
        let mut sum = [0.0f32; 3];
        for (j, wj) in wy.iter().enumerate() {
            let y = row0 + clamp(y0 - 1.0 + j as f32, self.height);
            for (i, wi) in wx.iter().enumerate() {
                let x = clamp(x0 - 1.0 + i as f32, self.width);
                let c = self.irradiance_at(x, y);
                for (s, c) in sum.iter_mut().zip(c) {
                    *s += wi * wj * c;
                }
            }
        }
        sum
    }

    /// Plain bilinear direction (decoded, [`decode_direction`]) at atlas UV, clamp to edge, as
    /// the GPU's linear sampler reads the direction texture; `None` without directions.
    #[must_use]
    pub fn sample_direction(&self, uv: [f32; 2]) -> Option<[f32; 4]> {
        self.sample_direction_page(0, uv)
    }

    /// [`Self::sample_direction`] on page `page` (ADR-3300); `None` without directions or past the
    /// last page.
    #[must_use]
    pub fn sample_direction_page(&self, page: u32, uv: [f32; 2]) -> Option<[f32; 4]> {
        if !self.is_directional() || !self.holds_page(page) {
            return None;
        }
        let row0 = page * self.height;
        let fx = uv[0] * self.width as f32 - 0.5;
        let fy = uv[1] * self.height as f32 - 0.5;
        let (x0, y0) = (fx.floor(), fy.floor());
        let (tx, ty) = (fx - x0, fy - y0);
        let clamp = |v: f32, n: u32| v.clamp(0.0, (n - 1) as f32) as u32;
        let mut sum = [0.0f32; 4];
        for (dx, dy, w) in [
            (0.0, 0.0, (1.0 - tx) * (1.0 - ty)),
            (1.0, 0.0, tx * (1.0 - ty)),
            (0.0, 1.0, (1.0 - tx) * ty),
            (1.0, 1.0, tx * ty),
        ] {
            let d = self
                .direction_at(clamp(x0 + dx, self.width), row0 + clamp(y0 + dy, self.height))
                .unwrap_or([0.0; 4]);
            for (s, v) in sum.iter_mut().zip(d) {
                *s += w * v;
            }
        }
        Some(sum)
    }

    /// Irradiance at an entry's local UV2 for the unit shading normal `n`, the full runtime
    /// lookup: bicubic when `bicubic`, else bilinear ([`Self::sample`]), then
    /// [`shade_directional`] when the lightmap has directions.
    #[must_use]
    pub fn shade_entry(&self, entry: usize, local: [f32; 2], n: [f32; 3], bicubic: bool) -> [f32; 3] {
        let Some(e) = self.entries.get(entry) else {
            return [0.0; 3];
        };
        let s = e.scale_offset;
        let uv = [local[0] * s[0] + s[2], local[1] * s[1] + s[3]];
        let irradiance = if bicubic {
            self.sample_bicubic_page(e.page, uv)
        } else {
            self.sample_page(e.page, uv)
        };
        match self.sample_direction_page(e.page, uv) {
            Some(d) => shade_directional(irradiance, d, n),
            None => irradiance,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Coverage, LightmapBakeInfo, LightmapEntry, LightmapLayout, MeshUnwrap};
    use super::*;

    /// A `w × h` atlas, one entry over all of it, irradiance from `f(x, y)`.
    fn atlas(w: u32, h: u32, f: impl Fn(u32, u32) -> [f32; 3]) -> Lightmap {
        let irr: Vec<[f32; 3]> = (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .map(|(x, y)| f(x, y))
            .collect();
        Lightmap::from_irradiance(
            LightmapLayout {
                width: w,
                height: h,
                density: 1.0,
                padding: 1,
                entries: vec![LightmapEntry {
                    name: "e".into(),
                    source_vertices: 3,
                    rect: [0, 0, w, h],
                    scale_offset: [1.0, 1.0, 0.0, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 1,
                    area_m2: 1.0,
                    texels_per_metre: 1.0,
                    key: None,
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![MeshUnwrap {
                    xref: vec![0, 1, 2],
                    uv: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                    indices: vec![0, 1, 2],
                }],
            },
            &irr,
            vec![Coverage::BAKED; (w * h) as usize],
            LightmapBakeInfo::default(),
        )
        .expect("valid atlas")
    }

    fn close(a: [f32; 3], b: [f32; 3], rel: f32) -> bool {
        a.iter()
            .zip(b)
            .all(|(x, y)| (x - y).abs() <= rel * y.abs().max(1.0))
    }

    #[test]
    fn bspline_weights_partition_unity_and_match_the_textbook_ends() {
        for k in 0..=16 {
            let w = bspline_weights(k as f32 / 16.0);
            assert!((w.iter().sum::<f32>() - 1.0).abs() < 1e-6, "{w:?}");
            assert!(w.iter().all(|&v| v >= 0.0));
        }
        let w = bspline_weights(0.0);
        for (got, want) in w.iter().zip([1.0 / 6.0, 4.0 / 6.0, 1.0 / 6.0, 0.0]) {
            assert!((got - want).abs() < 1e-6, "{w:?}");
        }
    }

    #[test]
    fn bicubic_keeps_a_constant_and_reproduces_a_linear_ramp() {
        let flat = atlas(8, 8, |_, _| [40.0, 20.0, 10.0]);
        for uv in [[0.0, 0.0], [0.31, 0.77], [0.5, 0.5], [1.0, 1.0]] {
            assert!(
                close(flat.sample_bicubic(uv), flat.irradiance_at(0, 0), 1e-5),
                "constant at {uv:?}"
            );
        }
        // Texel x holds 100 + 10·x (lux); the B-spline reproduces a linear function exactly
        // wherever its 4×4 footprint stays inside the atlas (x from 1.5 to 5.5 texels here).
        let ramp = atlas(8, 8, |x, _| [100.0 + 10.0 * x as f32, 50.0, 50.0]);
        for px in [2.0f32, 2.5, 3.25, 4.0, 4.9] {
            let uv = [(px + 0.5) / 8.0, 0.5];
            let got = ramp.sample_bicubic(uv)[0];
            // RGB9E5 keeps 9 mantissa bits: the stored ramp is within 1/512 of the exact one.
            let want = 100.0 + 10.0 * px;
            assert!((got - want).abs() <= want / 256.0, "x {px}: {got} vs {want}");
        }
    }

    #[test]
    fn bicubic_is_smoother_than_bilinear_on_a_single_bright_texel() {
        // One bright texel: bilinear returns it at its centre, the B-spline spreads it
        // (4/6 · 4/6 of the peak at the centre, the rest over the neighbours).
        let spot = atlas(9, 9, |x, y| if (x, y) == (4, 4) { [900.0; 3] } else { [0.0; 3] });
        let centre = [4.5 / 9.0, 4.5 / 9.0];
        let peak = spot.irradiance_at(4, 4)[0];
        assert!((spot.sample(centre)[0] - peak).abs() < 1e-3);
        let b = spot.sample_bicubic(centre)[0];
        assert!(
            (b - peak * 16.0 / 36.0).abs() <= peak * 1e-4,
            "{b} vs {}",
            peak * 16.0 / 36.0
        );
        // One texel away the B-spline still sees it (bilinear does not).
        let next = [5.5 / 9.0, 4.5 / 9.0];
        assert!(spot.sample(next)[0].abs() < 1e-3);
        assert!(spot.sample_bicubic(next)[0] > peak * 0.1);
    }

    #[test]
    fn directions_round_trip_within_one_step_and_sanitize() {
        for d in [
            [0.0, 1.0, 0.0, 1.0],
            [0.6, -0.8, 0.0, 0.25],
            [-1.0, -1.0, 1.0, 0.0],
        ] {
            let back = decode_direction(encode_direction(d));
            for (a, b) in back.iter().zip(d) {
                assert!((a - b).abs() <= 1.0 / 255.0 + 1e-6, "{d:?} -> {back:?}");
            }
        }
        assert_eq!(
            decode_direction(encode_direction([5.0, -5.0, f32::NAN, 2.0])),
            decode_direction(encode_direction([1.0, -1.0, 0.0, 1.0]))
        );
        // Red is the low byte (the byte order an `Rgba8Unorm` upload expects).
        assert_eq!(encode_direction([1.0, -1.0, -1.0, 0.0]), 0xff);
    }

    #[test]
    fn shade_directional_is_identity_at_the_baked_normal_and_bounded() {
        let e = [300.0, 200.0, 100.0];
        let up = [0.0, 1.0, 0.0];
        // All light from straight above, baked for an up normal: w = n0 · d = 1.
        let d = [0.0, 1.0, 0.0, 1.0];
        assert!(close(shade_directional(e, d, up), e, 1e-6));
        // Facing away from the light: nothing.
        assert_eq!(shade_directional(e, d, [0.0, -1.0, 0.0]), [0.0; 3]);
        // Tilted 90°: half.
        assert!(close(
            shade_directional(e, d, [1.0, 0.0, 0.0]),
            e.map(|v| v * 0.5),
            1e-6
        ));
        // No dominant direction: the normal changes nothing.
        let diffuse = [0.0, 0.0, 0.0, 0.0];
        for n in [up, [1.0, 0.0, 0.0], [0.0, -1.0, 0.0]] {
            assert!(close(shade_directional(e, diffuse, n), e, 1e-6));
        }
        // Light from 60° off the baked normal: identity at n0, brighter toward the light.
        let s = 3.0f32.sqrt() / 2.0;
        let d = [s * 0.8, 0.5 * 0.8, 0.0, 0.5 * 0.8];
        assert!(close(shade_directional(e, d, up), e, 1e-6));
        assert!(shade_directional(e, d, [s, 0.5, 0.0])[0] > e[0]);
        assert!(shade_directional(e, d, [s, 0.5, 0.0])[0] <= 2.0 * e[0]);
    }

    #[test]
    fn shade_entry_uses_the_direction_section_when_present() {
        let mut map = atlas(4, 4, |_, _| [100.0; 3]);
        let n_side = [1.0, 0.0, 0.0];
        assert!(close(
            map.shade_entry(0, [0.5, 0.5], n_side, true),
            [100.0; 3],
            1e-3
        ));
        assert_eq!(map.sample_direction([0.5, 0.5]), None);
        map.set_directions(&[[0.0, 1.0, 0.0, 1.0]; 16])
            .expect("one per texel");
        assert!(map.is_directional());
        let side = map.shade_entry(0, [0.5, 0.5], n_side, true);
        assert!((side[0] - 50.0).abs() < 1.0, "{side:?}");
        let up = map.shade_entry(0, [0.5, 0.5], [0.0, 1.0, 0.0], false);
        assert!((up[0] - 100.0).abs() < 1.0, "{up:?}");
        assert_eq!(map.shade_entry(9, [0.5, 0.5], n_side, true), [0.0; 3]);
        assert!(map.set_directions(&[[0.0; 4]; 3]).is_err());
    }
}
