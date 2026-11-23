// SPDX-License-Identifier: MPL-2.0
//! Lightmap-space checks of the gallery bake (ADR-4108): the baked irradiance looked up at world
//! points of the gallery's large flat walls, as the runtime's bilinear lookup reads it, and three
//! figures measured on it without a GPU or a reference image.
//!
//! - **Mottle**: the relative RMS of a band-pass of the wall's luminance, a 0.5 m box mean over a
//!   3 m box mean, minus one. A wall lit through openings changes over metres, so the band-pass of
//!   the true light is small; noise the denoiser smeared into patches of 0.5 to 2 m is what it
//!   sees.
//! - **Seam step**: across a straight chart border on a flat wall (the east wall is a grid of
//!   blocks around the adit, so its inner face is five charts), a line fitted to each side over
//!   1 m, extrapolated to the border; the step is their difference over their mean, averaged along
//!   the border. A continuous light field gives the difference of two fits of one line (small);
//!   a step between charts gives itself.
//! - **Mean colour** of a region (reported beside the path tracer's in the screen-space checks).

use regia_asset::CpuMesh;
use regia_asset::lightmap::Lightmap;
use regia_core::glam::{Vec2, Vec3};

/// The grid step of the wall samples, metres (one texel at the HIGH tier's 8 texels/m).
pub const STEP_M: f32 = 0.125;
/// The band-pass's small box, metres.
pub const MOTTLE_SMALL_M: f32 = 0.5;
/// The band-pass's large box, metres.
pub const MOTTLE_LARGE_M: f32 = 3.0;
/// The run of each side's line fit across a border, metres.
pub const SEAM_FIT_M: f32 = 1.0;
/// How far either side of a border [`edge_step`] looks, metres.
pub const EDGE_GAP_M: f32 = 0.02;

/// One entry's unwrapped triangles in world space, for lookups.
pub struct EntryGeometry {
    entry: usize,
    /// World corners, UV2 (entry-local) corners and the face normal of each triangle.
    tris: Vec<([Vec3; 3], [Vec2; 3], Vec3)>,
}

impl EntryGeometry {
    /// The unwrapped triangles of `entry`, whose source mesh is `mesh` (the importer's order).
    pub fn new(map: &Lightmap, entry: usize, mesh: &CpuMesh) -> Self {
        let unwrap = &map.unwraps[entry];
        let tris = unwrap
            .indices
            .as_chunks::<3>()
            .0
            .iter()
            .map(|t| {
                let p = t.map(|i| Vec3::from_array(mesh.positions[unwrap.xref[i as usize] as usize]));
                let uv = t.map(|i| Vec2::from_array(unwrap.uv[i as usize]));
                let n = (p[1] - p[0]).cross(p[2] - p[0]).normalize_or_zero();
                (p, uv, n)
            })
            .collect();
        Self { entry, tris }
    }

    /// The bilinear lookup at world point `p` of a face whose outward normal is `n` (within 1 cm
    /// of the face's plane); `None` when no triangle of such a face holds `p`.
    pub fn at(&self, map: &Lightmap, p: Vec3, n: Vec3) -> Option<[f32; 3]> {
        for (q, uv, fnrm) in &self.tris {
            if fnrm.dot(n) < 0.999 || (p - q[0]).dot(*fnrm).abs() > 0.01 {
                continue;
            }
            // Barycentrics in the face's plane.
            let (e0, e1, ep) = (q[1] - q[0], q[2] - q[0], p - q[0]);
            let (d00, d01, d11) = (e0.dot(e0), e0.dot(e1), e1.dot(e1));
            let (d20, d21) = (ep.dot(e0), ep.dot(e1));
            let den = d00 * d11 - d01 * d01;
            if den.abs() < 1e-12 {
                continue;
            }
            let v = (d11 * d20 - d01 * d21) / den;
            let w = (d00 * d21 - d01 * d20) / den;
            let u = 1.0 - v - w;
            if u < -1e-4 || v < -1e-4 || w < -1e-4 {
                continue;
            }
            let local = uv[0] * u + uv[1] * v + uv[2] * w;
            return Some(map.sample_entry(self.entry, local.to_array()));
        }
        None
    }
}

/// Rec. 709 luminance.
pub fn lum(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// A flat wall region sampled on a grid: `origin + a·u + b·v` for `a` in `0..na`, `b` in `0..nb`
/// steps of [`STEP_M`], the face's outward normal `n`, and which samples are on the region.
pub struct WallGrid {
    /// Region name.
    pub name: &'static str,
    /// The grid's first sample.
    pub origin: Vec3,
    /// First axis (unit).
    pub u: Vec3,
    /// Second axis (unit).
    pub v: Vec3,
    /// Outward normal of the face.
    pub n: Vec3,
    /// Samples along `u`.
    pub na: usize,
    /// Samples along `v`.
    pub nb: usize,
}

impl WallGrid {
    /// World point of sample `(a, b)`.
    pub fn point(&self, a: usize, b: usize) -> Vec3 {
        self.origin + self.u * (a as f32 * STEP_M) + self.v * (b as f32 * STEP_M)
    }

    /// The luminance at every sample (`None` off the face), row-major in `b`.
    pub fn sample(&self, map: &Lightmap, geo: &EntryGeometry) -> Vec<Option<[f32; 3]>> {
        (0..self.nb)
            .flat_map(|b| (0..self.na).map(move |a| (a, b)))
            .map(|(a, b)| geo.at(map, self.point(a, b), self.n))
            .collect()
    }
}

/// Box mean of `values` (row-major `na × nb`, `None` skipped) of half width `r` samples.
fn box_mean(values: &[Option<f32>], na: usize, nb: usize, r: usize) -> Vec<Option<f32>> {
    // Separable masked box: sums and counts along a, then along b.
    let mut sum_a = vec![0.0f64; na * nb];
    let mut cnt_a = vec![0u32; na * nb];
    for b in 0..nb {
        for a in 0..na {
            let (lo, hi) = (a.saturating_sub(r), (a + r).min(na - 1));
            for k in lo..=hi {
                if let Some(v) = values[b * na + k] {
                    sum_a[b * na + a] += f64::from(v);
                    cnt_a[b * na + a] += 1;
                }
            }
        }
    }
    let mut out = vec![None; na * nb];
    for b in 0..nb {
        for a in 0..na {
            if values[b * na + a].is_none() {
                continue;
            }
            let (lo, hi) = (b.saturating_sub(r), (b + r).min(nb - 1));
            let (mut s, mut c) = (0.0f64, 0u32);
            for k in lo..=hi {
                s += sum_a[k * na + a];
                c += cnt_a[k * na + a];
            }
            if c > 0 {
                out[b * na + a] = Some((s / f64::from(c)) as f32);
            }
        }
    }
    out
}

/// The mottle of a wall's samples (module docs): the relative RMS of `box(0.5 m) / box(3 m) - 1`
/// over samples whose whole 3 m box lies on the region (so a border or opening never enters the
/// large box one-sided).
pub fn mottle(samples: &[Option<[f32; 3]>], na: usize, nb: usize) -> Option<f32> {
    let l: Vec<Option<f32>> = samples.iter().map(|s| s.map(lum)).collect();
    let rs = (MOTTLE_SMALL_M / STEP_M / 2.0).round() as usize;
    let rl = (MOTTLE_LARGE_M / STEP_M / 2.0).round() as usize;
    let small = box_mean(&l, na, nb, rs);
    let large = box_mean(&l, na, nb, rl);
    let full = |a: usize, b: usize| {
        a >= rl
            && b >= rl
            && a + rl < na
            && b + rl < nb
            && (b - rl..=b + rl).all(|bb| (a - rl..=a + rl).all(|aa| l[bb * na + aa].is_some()))
    };
    let (mut sq, mut n) = (0.0f64, 0u32);
    for b in 0..nb {
        for a in 0..na {
            let i = b * na + a;
            if let (Some(s), Some(g)) = (small[i], large[i])
                && g > 0.0
                && full(a, b)
            {
                let d = f64::from(s / g - 1.0);
                sq += d * d;
                n += 1;
            }
        }
    }
    (n > 0).then(|| (sq / f64::from(n)).sqrt() as f32)
}

/// A straight border on a wall: the border line `start + t·along` for `t` in `0..length`, and the
/// unit direction `across` it. Samples are taken every [`STEP_M`] along it.
pub struct Border {
    /// Name.
    pub name: &'static str,
    /// Start of the border line.
    pub start: Vec3,
    /// Unit direction along it.
    pub along: Vec3,
    /// Its length, metres.
    pub length: f32,
    /// Unit direction across it (in the wall's plane).
    pub across: Vec3,
    /// The wall's outward normal.
    pub n: Vec3,
}

/// Least-squares line through `(x, y)` points, evaluated at 0.
fn fit_at_zero(points: &[(f32, f32)]) -> Option<f32> {
    let n = points.len() as f64;
    if points.len() < 3 {
        return None;
    }
    let (sx, sy) = points.iter().fold((0.0f64, 0.0f64), |a, p| {
        (a.0 + f64::from(p.0), a.1 + f64::from(p.1))
    });
    let (mx, my) = (sx / n, sy / n);
    let (mut sxx, mut sxy) = (0.0f64, 0.0f64);
    for p in points {
        let dx = f64::from(p.0) - mx;
        sxx += dx * dx;
        sxy += dx * (f64::from(p.1) - my);
    }
    if sxx <= 0.0 {
        return None;
    }
    Some((my - sxy / sxx * mx) as f32)
}

/// The seam step across `border` (module docs): the mean over the border of
/// `|left - right| / ((left + right) / 2)`, each side a line fitted to its luminance over
/// [`SEAM_FIT_M`] (starting half a sample from the border) and extrapolated to it. `None` when no
/// position along the border has both sides on the wall.
pub fn seam_step(lookup: impl Fn(Vec3, Vec3) -> Option<[f32; 3]>, border: &Border) -> Option<f32> {
    let steps = (border.length / STEP_M) as usize;
    let fit_n = (SEAM_FIT_M / STEP_M) as usize;
    let (mut sum, mut n) = (0.0f64, 0u32);
    for k in 0..=steps {
        let base = border.start + border.along * (k as f32 * STEP_M);
        let side = |sign: f32| -> Option<f32> {
            let pts: Vec<(f32, f32)> = (0..fit_n)
                .filter_map(|i| {
                    let d = (i as f32 + 0.5) * STEP_M;
                    lookup(base + border.across * (sign * d), border.n).map(|c| (d, lum(c)))
                })
                .collect();
            (pts.len() == fit_n).then(|| fit_at_zero(&pts)).flatten()
        };
        if let (Some(l), Some(r)) = (side(-1.0), side(1.0)) {
            let m = 0.5 * (l + r);
            if m > 0.0 {
                sum += f64::from((l - r).abs() / m);
                n += 1;
            }
        }
    }
    (n > 0).then(|| (sum / f64::from(n)) as f32)
}

/// The hard step at `border`: the mean over the border of `|left - right| / ((left + right) / 2)`
/// for lookups [`EDGE_GAP_M`] either side of it (what the eye sees as a line; a stitched seam
/// reads close to zero here even where [`seam_step`]'s fits still differ).
pub fn edge_step(lookup: impl Fn(Vec3, Vec3) -> Option<[f32; 3]>, border: &Border) -> Option<f32> {
    let steps = (border.length / STEP_M) as usize;
    let (mut sum, mut n) = (0.0f64, 0u32);
    for k in 0..=steps {
        let base = border.start + border.along * (k as f32 * STEP_M);
        let l = lookup(base - border.across * EDGE_GAP_M, border.n).map(lum);
        let r = lookup(base + border.across * EDGE_GAP_M, border.n).map(lum);
        if let (Some(l), Some(r)) = (l, r) {
            let m = 0.5 * (l + r);
            if m > 0.0 {
                sum += f64::from((l - r).abs() / m);
                n += 1;
            }
        }
    }
    (n > 0).then(|| (sum / f64::from(n)) as f32)
}

/// Mean colour of the samples on the face.
pub fn mean_colour(samples: &[Option<[f32; 3]>]) -> Option<[f32; 3]> {
    let (mut s, mut n) = ([0.0f64; 3], 0u32);
    for c in samples.iter().flatten() {
        for k in 0..3 {
            s[k] += f64::from(c[k]);
        }
        n += 1;
    }
    (n > 0).then(|| s.map(|v| (v / f64::from(n)) as f32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_fit_extrapolates_exactly() {
        let pts: Vec<(f32, f32)> = (0..8)
            .map(|i| (i as f32 * 0.5 + 0.25, 3.0 + 2.0 * i as f32))
            .collect();
        let at0 = fit_at_zero(&pts).expect("fit");
        assert!((at0 - (3.0 - 1.0)).abs() < 1e-4, "{at0}");
    }

    #[test]
    fn a_smooth_ramp_has_no_mottle_and_patches_do() {
        let (na, nb) = (96, 64);
        let ramp: Vec<Option<[f32; 3]>> = (0..na * nb)
            .map(|i| {
                let a = (i % na) as f32;
                Some([100.0 + a; 3])
            })
            .collect();
        let m = mottle(&ramp, na, nb).expect("mottle");
        assert!(m < 1e-3, "{m}");
        // 1 m patches of ±5 %.
        let patches: Vec<Option<[f32; 3]>> = (0..na * nb)
            .map(|i| {
                let (a, b) = (i % na, i / na);
                let s = if ((a / 8) + (b / 8)).is_multiple_of(2) {
                    1.05
                } else {
                    0.95
                };
                Some([100.0 * s; 3])
            })
            .collect();
        let m = mottle(&patches, na, nb).expect("mottle");
        assert!(m > 0.02, "{m}");
    }
}
