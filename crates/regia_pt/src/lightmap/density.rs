// SPDX-License-Identifier: MPL-2.0
//! Automatic texel density for small curved receivers (ADR-2404, `docs/specs/lightmaps.md` § 3).
//!
//! On a smooth curved receiver the chart seams show when a texel spans too much of the curve: the
//! two sides of a seam read different texels, and the light changes with the normal across each
//! of them. The v0.3.0 evidence (ADR-2315) measured it on a 0.35 m sphere: the seam `p95`
//! (`regia_asset::lightmap::seam_report`) halves with every doubling of the density, and is close
//! to a fixed multiple of the angle the normal turns across one texel (`κ / d`, `κ` the surface's
//! curvature, `d` its texels per metre).
//!
//! **Prediction.** [`receiver_curvature`] is the mean curvature of the curved part of the mesh by
//! the discrete Steiner formula `∫ H dA ≈ ½ Σ_e φ_e L_e` (Meyer, Desbrun, Schröder and Barr, 2003):
//! `κ = Σ φ_e L_e / (2 A)` over the smooth bent edges (two faces welded by position that meet at
//! more than 0.06° and less than the seam metric's crease limit; `φ_e` the bend, `L_e` the edge's
//! length), `A` the area of the faces that touch such an edge. It is `1/r` on a sphere of radius
//! `r` at any tessellation. The predicted seam `p95` is `K · κ / d`, `K` = [`SEAM_ERROR_PER_RADIAN`] without the seam stitch and
//! [`STITCHED_SEAM_ERROR_PER_RADIAN`] with it (`LightmapBakeSettings::stitch_seams`), both measured
//! on the evidence sphere (ADR-2404) and rounded up. A flat receiver (walls, floors, boxes) has no
//! bent edge, so its prediction is 0.
//!
//! **Rule** ([`auto_density_scale`]): a receiver whose predicted `p95` at the requested density is
//! over the budget (the shipping default `DEFAULT_SEAM_BUDGET`, 0.25, or `[lightmaps]
//! seam_budget`) gets the smallest power-of-two `density_scale` that brings the prediction under it,
//! at most [`AUTO_DENSITY_MAX_SCALE`] (the top of `LightmapScale`). A receiver that still predicts
//! over the budget at the cap is reported with [`DENSITY_LOW_CODE`]. The rule only replaces the
//! default scale: a scale the user set (`LightmapSurface::density_scale` in a level) is kept, and
//! reported with the same code when it predicts over the budget. The atlas generator's own limits
//! (rectangle size, atlas fit) still apply after it.

use std::collections::HashMap;

use regia_asset::lightmap::DEFAULT_HARD_EDGE_DEG;

/// The warning code of a receiver whose predicted seams pass the budget at the density it gets.
pub const DENSITY_LOW_CODE: &str = "lightmap_density_low";
/// Largest `density_scale` the rule picks (the top of `LightmapScale`, `docs/specs/lightmaps.md` § 3).
pub const AUTO_DENSITY_MAX_SCALE: f32 = 8.0;
/// Predicted seam `p95` per radian of normal turn across one texel, without the seam stitch
/// (the evidence sphere: 3.25 at 16 texels/m to 3.54 at 128; rounded up).
pub const SEAM_ERROR_PER_RADIAN: f32 = 3.6;
/// The same with the seam stitch on (the evidence sphere in ADR-2404: 0.07 at 8 and at 16
/// texels/m; rounded up).
pub const STITCHED_SEAM_ERROR_PER_RADIAN: f32 = 0.1;
/// Smallest face bend that counts as curvature, radians (0.06°): below it a surface is flat up to
/// float rounding.
const MIN_BEND_RAD: f64 = 1e-3;

/// What [`auto_density_scale`] decided for one receiver.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct AutoDensity {
    /// Mean curvature of the receiver's curved part, 1/m (0 for a flat receiver).
    pub curvature: f32,
    /// Predicted seam `p95` at the requested density (scale 1).
    pub predicted_p95: f32,
    /// The `density_scale` picked (a power of two, 1 to [`AUTO_DENSITY_MAX_SCALE`]).
    pub density_scale: f32,
    /// Predicted seam `p95` at the picked scale.
    pub predicted_p95_scaled: f32,
    /// The prediction at the picked scale is still over the budget (the cap was reached).
    pub over_budget: bool,
}

/// Mean curvature of the curved part of a triangle mesh, 1/m (module docs); 0 for a mesh with no
/// smooth bent edge (flat, or only creases).
#[must_use]
pub fn receiver_curvature(positions: &[[f32; 3]], indices: &[u32]) -> f32 {
    let tris = indices.as_chunks::<3>().0;
    if tris.iter().flatten().any(|&i| i as usize >= positions.len()) {
        return 0.0;
    }
    let p = |i: u32| positions[i as usize].map(f64::from);
    let mut welds: HashMap<[u32; 3], u32> = HashMap::new();
    let weld: Vec<u32> = positions
        .iter()
        .map(|q| {
            let next = welds.len() as u32;
            *welds.entry(q.map(|c| (c + 0.0).to_bits())).or_insert(next)
        })
        .collect();
    // Unit normal and area of a face.
    let face = |t: &[u32; 3]| -> Option<([f64; 3], f64)> {
        let (a, b, c) = (p(t[0]), p(t[1]), p(t[2]));
        let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let n = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        (len > 1e-20).then(|| (n.map(|x| x / len), 0.5 * len))
    };
    let mut edges: Vec<((u32, u32), u32, f64)> = Vec::with_capacity(tris.len() * 3);
    for (k, t) in tris.iter().enumerate() {
        for c in 0..3 {
            let (a, b) = (t[c], t[(c + 1) % 3]);
            let (wa, wb) = (weld[a as usize], weld[b as usize]);
            if wa != wb {
                let (pa, pb) = (p(a), p(b));
                let len =
                    ((pb[0] - pa[0]).powi(2) + (pb[1] - pa[1]).powi(2) + (pb[2] - pa[2]).powi(2)).sqrt();
                edges.push(((wa.min(wb), wa.max(wb)), k as u32, len));
            }
        }
    }
    edges.sort_by_key(|e| (e.0, e.1));
    let crease_cos = f64::from(DEFAULT_HARD_EDGE_DEG.to_radians().cos());
    let mut bent = vec![false; tris.len()];
    let mut weighted = 0.0f64;
    for pair in edges.windows(2) {
        let ((key, t0, edge_len), (key1, t1, _)) = (pair[0], pair[1]);
        if key != key1 {
            continue;
        }
        let (Some((n0, _)), Some((n1, _))) = (face(&tris[t0 as usize]), face(&tris[t1 as usize])) else {
            continue;
        };
        let cos = (n0[0] * n1[0] + n0[1] * n1[1] + n0[2] * n1[2]).clamp(-1.0, 1.0);
        let bend = cos.acos();
        if cos < crease_cos || bend < MIN_BEND_RAD {
            continue;
        }
        weighted += bend * edge_len;
        bent[t0 as usize] = true;
        bent[t1 as usize] = true;
    }
    let area: f64 = tris
        .iter()
        .zip(&bent)
        .filter(|(_, b)| **b)
        .filter_map(|(t, _)| face(t).map(|f| f.1))
        .sum();
    if area <= 0.0 {
        0.0
    } else {
        (weighted / (2.0 * area)) as f32
    }
}

/// The predicted seam `p95` of a receiver of curvature `curvature` (1/m) at `texels_per_metre`
/// (module docs); 0 for a flat receiver or a non-positive density.
#[must_use]
pub fn predicted_seam_p95(curvature: f32, texels_per_metre: f32, stitched: bool) -> f32 {
    if !(curvature > 0.0 && texels_per_metre > 0.0) {
        return 0.0;
    }
    let k = if stitched {
        STITCHED_SEAM_ERROR_PER_RADIAN
    } else {
        SEAM_ERROR_PER_RADIAN
    };
    k * curvature / texels_per_metre
}

/// The density rule for one receiver at `texels_per_metre` (the bake's, before its scale) and seam
/// budget `budget` (module docs).
#[must_use]
pub fn auto_density_scale(
    positions: &[[f32; 3]],
    indices: &[u32],
    texels_per_metre: f32,
    budget: f32,
    stitched: bool,
) -> AutoDensity {
    let curvature = receiver_curvature(positions, indices);
    let at = |scale: f32| predicted_seam_p95(curvature, texels_per_metre * scale, stitched);
    let mut scale = 1.0f32;
    while at(scale) > budget && scale < AUTO_DENSITY_MAX_SCALE {
        scale *= 2.0;
    }
    AutoDensity {
        curvature,
        predicted_p95: at(1.0),
        density_scale: scale,
        predicted_p95_scaled: at(scale),
        over_budget: at(scale) > budget,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A UV sphere of radius `r` (`seg` around, `rings` from pole to pole), welded at the seam.
    fn sphere(r: f32, seg: u32, rings: u32) -> (Vec<[f32; 3]>, Vec<u32>) {
        let mut p = Vec::new();
        for j in 0..=rings {
            let th = std::f32::consts::PI * j as f32 / rings as f32;
            for i in 0..=seg {
                let ph = std::f32::consts::TAU * (i % seg) as f32 / seg as f32;
                p.push([r * th.sin() * ph.cos(), r * th.cos(), r * th.sin() * ph.sin()]);
            }
        }
        let mut idx = Vec::new();
        for j in 0..rings {
            for i in 0..seg {
                let a = j * (seg + 1) + i;
                let b = a + seg + 1;
                idx.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
            }
        }
        (p, idx)
    }

    #[test]
    fn a_flat_quad_and_a_box_have_no_curvature() {
        let quad = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 0.0, 1.0], [0.0, 0.0, 1.0]];
        assert_eq!(receiver_curvature(&quad, &[0, 1, 2, 0, 2, 3]), 0.0);
        let d = auto_density_scale(&quad, &[0, 1, 2, 0, 2, 3], 8.0, 0.25, false);
        assert_eq!(
            (d.density_scale, d.predicted_p95, d.over_budget),
            (1.0, 0.0, false)
        );
    }

    #[test]
    fn a_sphere_curvature_is_about_one_over_its_radius() {
        for r in [0.35f32, 1.0, 4.0] {
            let (p, i) = sphere(r, 32, 16);
            let k = receiver_curvature(&p, &i);
            assert!((k * r - 1.0).abs() < 0.15, "r {r}: curvature {k}");
        }
    }

    #[test]
    fn the_rule_raises_a_small_sphere_by_powers_of_two_and_leaves_a_large_one() {
        let (p, i) = sphere(0.35, 32, 16);
        let d = auto_density_scale(&p, &i, 16.0, 0.25, false);
        // Unstitched: about 3.6 · 2.86 / 16 = 0.64 at scale 1, so scale 4 (0.16).
        assert!(d.predicted_p95 > 0.5, "{d:?}");
        assert_eq!(d.density_scale, 4.0, "{d:?}");
        assert!(d.predicted_p95_scaled <= 0.25 && !d.over_budget, "{d:?}");
        let (p, i) = sphere(20.0, 64, 32);
        let big = auto_density_scale(&p, &i, 8.0, 0.25, false);
        assert_eq!(big.density_scale, 1.0, "{big:?}");
    }

    #[test]
    fn the_cap_is_reported_as_over_budget() {
        // A 5 mm sphere at 4 texels/m: even stitched (0.1 · 200 / 32 = 0.63 at the cap).
        let (p, i) = sphere(0.005, 32, 16);
        let d = auto_density_scale(&p, &i, 4.0, 0.25, true);
        assert_eq!(d.density_scale, AUTO_DENSITY_MAX_SCALE);
        assert!(d.over_budget, "{d:?}");
        // Bad input never panics.
        assert_eq!(receiver_curvature(&p, &[0, 1, 99_999]), 0.0);
        assert_eq!(predicted_seam_p95(f32::NAN, 8.0, true), 0.0);
    }
}
