// SPDX-License-Identifier: MPL-2.0
//! Far-field splat reconstruction (ADR-1456). The CPU mirror of `splat_packs` in `terrain.wgsl`, so the
//! math can be tested without a GPU.
//!
//! Bilinear interpolation of the splat packs bends at every texel edge. The height blend raises the
//! contrast of the layer weights, so the patch outline it draws is the 0.5 contour of that bent field:
//! where a pack texel spans only a few pixels, the bends read as a notched, stair-stepped outline around
//! every painted patch (look round 2, finding 8: 10–20 m steps from 3 km). There the shader reads the packs
//! with a cubic B-spline instead, which is smooth across texel edges (C², positive weights, so the weights
//! stay a partition of unity), as four bilinear taps (Sigg & Hadwiger 2005, "Fast Third-Order Texture
//! Filtering", GPU Gems 2 ch. 20). The B-spline is mixed in by [`far_splat_fade`] as the pixel footprint's
//! short axis grows from [`FAR_SPLAT_START`] to [`FAR_SPLAT_FULL`] pack texels, and the packs are read
//! [`FAR_SPLAT_LOD_BIAS`] levels coarser by the same share ([`far_splat_lod`]); below the start (a texel over
//! ten pixels across) the shader keeps its two bilinear taps at the footprint's level exactly.

/// Short axis of the pixel footprint, in pack texels, at which the cubic B-spline starts to mix in.
pub const FAR_SPLAT_START: f32 = 0.1;

/// Short axis of the pixel footprint, in pack texels, from which the packs are read with the cubic
/// B-spline alone.
pub const FAR_SPLAT_FULL: f32 = 0.25;

/// Mip levels added to the splat packs' footprint level at a full B-spline share: the far field reads them
/// half a level coarser, which also smooths the data's own stair of a few texels along a slanted edge (the
/// patches keep their place and size; only fragments smaller than about two texels fade).
pub const FAR_SPLAT_LOD_BIAS: f32 = 0.5;

/// The level the far field reads the packs at: the footprint's level `lod` plus [`FAR_SPLAT_LOD_BIAS`]
/// times the B-spline share `k` ([`far_splat_fade`]), at most 12. `k` = 0 returns `lod` unchanged.
#[must_use]
pub fn far_splat_lod(lod: f32, k: f32) -> f32 {
    (lod + FAR_SPLAT_LOD_BIAS * k).min(12.0)
}

/// Share (0..1) of the cubic B-spline in the splat reconstruction for a pixel whose footprint's short axis
/// is `short_footprint_m` metres over pack texels of `texel_m` metres. Exactly 0 at or below
/// [`FAR_SPLAT_START`] texels (and where no footprint was measured: 0 m), 1 from [`FAR_SPLAT_FULL`],
/// smoothstep between.
#[must_use]
pub fn far_splat_fade(short_footprint_m: f32, texel_m: f32) -> f32 {
    let r = short_footprint_m / texel_m.max(1e-6);
    let t = ((r - FAR_SPLAT_START) / (FAR_SPLAT_FULL - FAR_SPLAT_START)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The four uniform cubic B-spline weights of the texels at −1, 0, +1 and +2 from the texel at or before
/// a sample `f` (0..1) texels past its centre.
#[must_use]
pub fn bspline_weights(f: f32) -> [f32; 4] {
    let f2 = f * f;
    let f3 = f2 * f;
    let r = 1.0 - f;
    [
        r * r * r / 6.0,
        (3.0 * f3 - 6.0 * f2 + 4.0) / 6.0,
        (-3.0 * f3 + 3.0 * f2 + 3.0 * f + 1.0) / 6.0,
        f3 / 6.0,
    ]
}

/// One axis of the 4-texel filter as two bilinear taps: weights `g0`, `g1` and offsets `h0`, `h1` in
/// texels from the centre of the texel at or before the sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AxisTaps {
    /// Weight of the tap between texels −1 and 0.
    pub g0: f32,
    /// Weight of the tap between texels +1 and +2.
    pub g1: f32,
    /// Offset of the first tap, texels (−1..0).
    pub h0: f32,
    /// Offset of the second tap, texels (+1..+2).
    pub h1: f32,
}

/// The two taps of one axis for the cubic B-spline at fraction `f` mixed by `k` (0..1) with linear
/// interpolation. At `k` = 0 the taps sit on the two nearest texel centres with the bilinear weights
/// (`1 − f` at 0, `f` at +1), so the result is the plain bilinear sample.
#[must_use]
pub fn far_splat_axis(f: f32, k: f32) -> AxisTaps {
    let b = bspline_weights(f);
    let a0 = b[0] * k;
    let a1 = (1.0 - f) + (b[1] - (1.0 - f)) * k;
    let a2 = f + (b[2] - f) * k;
    let a3 = b[3] * k;
    let g0 = a0 + a1;
    let g1 = a2 + a3;
    AxisTaps {
        g0,
        g1,
        h0: a1 / g0.max(1e-6) - 1.0,
        h1: a3 / g1.max(1e-6) + 1.0,
    }
}

/// A single-channel splat grid (one layer's weights), row-major, `width × height` texels; texel `(i, j)`
/// has its centre at `(i + 0.5, j + 0.5)` in texel units. Reads past the edge clamp to the edge texel
/// (the shader clamps its UV half a texel inside the map).
#[derive(Clone, Copy, Debug)]
pub struct SplatGrid<'a> {
    /// Texels per row.
    pub width: usize,
    /// Rows.
    pub height: usize,
    /// `width × height` weights; missing texels read as 0.
    pub texels: &'a [f32],
}

impl SplatGrid<'_> {
    fn texel(&self, i: i64, j: i64) -> f32 {
        if self.width == 0 || self.height == 0 {
            return 0.0;
        }
        let i = i.clamp(0, self.width as i64 - 1) as usize;
        let j = j.clamp(0, self.height as i64 - 1) as usize;
        self.texels.get(j * self.width + i).copied().unwrap_or(0.0)
    }

    /// The bilinear (hardware-filtered) sample at `(x, z)` in texel units.
    #[must_use]
    pub fn bilinear(&self, x: f32, z: f32) -> f32 {
        let xs = x - 0.5;
        let zs = z - 0.5;
        let bx = xs.floor();
        let bz = zs.floor();
        let fx = xs - bx;
        let fz = zs - bz;
        let (i, j) = (bx as i64, bz as i64);
        let top = self.texel(i, j) * (1.0 - fx) + self.texel(i + 1, j) * fx;
        let bottom = self.texel(i, j + 1) * (1.0 - fx) + self.texel(i + 1, j + 1) * fx;
        top * (1.0 - fz) + bottom * fz
    }

    /// The shader's far-field sample at `(x, z)` in texel units: four bilinear taps placed by
    /// [`far_splat_axis`] with the B-spline share `k` ([`far_splat_fade`]); `k` = 0 is [`Self::bilinear`].
    #[must_use]
    pub fn far_sample(&self, x: f32, z: f32, k: f32) -> f32 {
        let xs = x - 0.5;
        let zs = z - 0.5;
        let base_x = xs.floor() + 0.5;
        let base_z = zs.floor() + 0.5;
        let ax = far_splat_axis(xs - xs.floor(), k);
        let az = far_splat_axis(zs - zs.floor(), k);
        let (x0, x1) = (base_x + ax.h0, base_x + ax.h1);
        let (z0, z1) = (base_z + az.h0, base_z + az.h1);
        self.bilinear(x0, z0) * ax.g0 * az.g0
            + self.bilinear(x1, z0) * ax.g1 * az.g0
            + self.bilinear(x0, z1) * ax.g0 * az.g1
            + self.bilinear(x1, z1) * ax.g1 * az.g1
    }

    /// The cubic B-spline sample at `(x, z)` summed over its 4 × 4 texels directly (the reference the
    /// four-tap form must equal).
    #[must_use]
    pub fn bspline(&self, x: f32, z: f32) -> f32 {
        let xs = x - 0.5;
        let zs = z - 0.5;
        let (i, j) = (xs.floor() as i64, zs.floor() as i64);
        let wx = bspline_weights(xs - xs.floor());
        let wz = bspline_weights(zs - zs.floor());
        let mut sum = 0.0;
        for (b, wzb) in wz.iter().enumerate() {
            for (a, wxa) in wx.iter().enumerate() {
                sum += wxa * wzb * self.texel(i - 1 + a as i64, j - 1 + b as i64);
            }
        }
        sum
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fade_is_exactly_off_near_and_full_far() {
        assert_eq!(far_splat_fade(0.0, 2.0), 0.0, "no footprint measured");
        assert_eq!(far_splat_fade(0.05, 2.0), 0.0);
        assert_eq!(far_splat_fade(FAR_SPLAT_START * 2.0, 2.0), 0.0);
        assert_eq!(far_splat_fade(FAR_SPLAT_FULL * 2.0, 2.0), 1.0);
        assert_eq!(far_splat_fade(30.0, 2.0), 1.0);
        let mut prev = 0.0;
        for step in 0..=100 {
            let k = far_splat_fade(step as f32 * 0.01, 2.0);
            assert!((0.0..=1.0).contains(&k) && k >= prev, "{step}: {k} after {prev}");
            prev = k;
        }
    }

    #[test]
    fn the_level_bias_follows_the_share() {
        assert_eq!(far_splat_lod(1.3, 0.0), 1.3, "no share, no bias");
        assert_eq!(far_splat_lod(1.0, 1.0), 1.0 + FAR_SPLAT_LOD_BIAS);
        assert_eq!(far_splat_lod(11.9, 1.0), 12.0);
    }

    #[test]
    fn the_taps_keep_a_partition_of_unity_with_positive_weights() {
        for fi in 0..=20 {
            let f = fi as f32 / 20.0;
            let b = bspline_weights(f);
            assert!((b.iter().sum::<f32>() - 1.0).abs() < 1e-6, "{b:?}");
            assert!(b.iter().all(|&w| w >= 0.0), "{b:?}");
            for ki in 0..=10 {
                let k = ki as f32 / 10.0;
                let t = far_splat_axis(f, k);
                assert!((t.g0 + t.g1 - 1.0).abs() < 1e-6, "f {f} k {k}: {t:?}");
                assert!(t.g0 >= 0.0 && t.g1 >= 0.0, "f {f} k {k}: {t:?}");
                assert!((-1.0..=0.0).contains(&t.h0), "f {f} k {k}: {t:?}");
                assert!((1.0..=2.0).contains(&t.h1), "f {f} k {k}: {t:?}");
            }
        }
    }

    /// A binary patch edge on a 64² grid of 2 m texels: 1 where the texel centre lies past the line
    /// z = slope · x + offset (texel units), like a painted pack seen texel by texel.
    fn staircase(slope: f32, offset: f32) -> Vec<f32> {
        let n = 64;
        let mut texels = Vec::with_capacity(n * n);
        for j in 0..n {
            for i in 0..n {
                let (x, z) = (i as f32 + 0.5, j as f32 + 0.5);
                texels.push(if z > slope * x + offset { 1.0 } else { 0.0 });
            }
        }
        texels
    }

    fn grid(texels: &[f32]) -> SplatGrid<'_> {
        SplatGrid {
            width: 64,
            height: 64,
            texels,
        }
    }

    #[test]
    fn zero_share_is_the_bilinear_sample_and_full_share_the_b_spline() {
        let texels = staircase(0.37, 20.3);
        let g = grid(&texels);
        let mut worst_lin = 0.0_f32;
        let mut worst_cubic = 0.0_f32;
        for s in 0..400 {
            let x = 10.0 + s as f32 * 0.113;
            let z = 25.0 + (s % 37) as f32 * 0.071;
            worst_lin = worst_lin.max((g.far_sample(x, z, 0.0) - g.bilinear(x, z)).abs());
            worst_cubic = worst_cubic.max((g.far_sample(x, z, 1.0) - g.bspline(x, z)).abs());
        }
        assert!(worst_lin < 1e-5, "k = 0 differs from bilinear by {worst_lin}");
        assert!(
            worst_cubic < 1e-5,
            "k = 1 differs from the 4×4 B-spline by {worst_cubic}"
        );
    }

    /// The height blend's share of the patch layer between two covering layers of equal height texture
    /// (keys = weights; `terrain.wgsl` `surface_layers`), at blend depth `depth`.
    fn blend_share(w: f32, depth: f32) -> f32 {
        let (ks, kg) = (w, 1.0 - w);
        let floor = ks.max(kg) - depth;
        let (s, g) = ((ks - floor).max(0.0), (kg - floor).max(0.0));
        s / (s + g).max(1e-6)
    }

    /// Where the height-blended outline (share 0.5) crosses the column at `x`, texel units (the
    /// field is monotonic along z for these edges).
    fn crossing(g: &SplatGrid<'_>, x: f32, k: f32, depth: f32) -> f32 {
        let (mut lo, mut hi) = (2.0_f32, 62.0_f32);
        for _ in 0..40 {
            let mid = 0.5 * (lo + hi);
            if blend_share(g.far_sample(x, mid, k), depth) >= 0.5 {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        0.5 * (lo + hi)
    }

    /// Outline of a staircase edge across 32 texels, sampled every 0.05 texel: (mean position, the
    /// peak-to-peak of its steps in metres). The steps are what is left after a one-texel running mean
    /// (the outline's own course at the texel scale) is taken away.
    fn outline_steps(slope: f32, k: f32) -> (f32, f32) {
        const TEXEL_M: f32 = 2.0;
        let depth = 0.5 + (0.12 - 0.5) * 0.6; // height_blend 0.6, the showcase layers
        // The line crosses z = 30.13 at x = 32: the sampled stretch stays inside the grid.
        let texels = staircase(slope, 30.13 - 32.0 * slope);
        let g = grid(&texels);
        let zs: Vec<f32> = (0..=640)
            .map(|s| crossing(&g, 16.0 + s as f32 * 0.05, k, depth))
            .collect();
        let window = 20;
        let mut lo = f32::MAX;
        let mut hi = f32::MIN;
        for (i, w) in zs.windows(window).enumerate() {
            let mean = w.iter().sum::<f32>() / window as f32;
            let r = zs[i + window / 2] - mean;
            lo = lo.min(r);
            hi = hi.max(r);
        }
        let mean = zs.iter().sum::<f32>() / zs.len() as f32;
        (mean, (hi - lo) * TEXEL_M)
    }

    #[test]
    fn the_b_spline_halves_the_outline_steps_of_a_painted_edge() {
        for slope in [0.2_f32, 0.33, 0.5, 0.71, 0.9] {
            let (mean_lin, steps_lin) = outline_steps(slope, 0.0);
            let (mean_cubic, steps_cubic) = outline_steps(slope, 1.0);
            eprintln!("slope {slope}: steps bilinear {steps_lin:.3} m, B-spline {steps_cubic:.3} m");
            assert!(
                steps_lin > 0.3,
                "slope {slope}: the bilinear outline steps {steps_lin} m"
            );
            assert!(
                steps_cubic < 0.6 * steps_lin && steps_cubic < 0.25,
                "slope {slope}: B-spline steps {steps_cubic} m vs bilinear {steps_lin} m"
            );
            // The patch keeps its place: the smoothing must not move or shrink it.
            assert!(
                (mean_cubic - mean_lin).abs() < 0.05,
                "slope {slope}: outline moved {} texels",
                mean_cubic - mean_lin
            );
        }
    }
}
