// SPDX-License-Identifier: MPL-2.0
//! Independent reference for the Cornell room: classical radiosity on a convex room of planar
//! diffuse quads (Goral, Torrance, Greenberg, Battaile 1984; Cohen & Wallace 1993), with
//! point-to-polygon form factors from Lambert's closed-form contour integral and a final
//! gather at the query point. A convex room has no occlusion, so the only approximation is the
//! piecewise-constant radiosity of the patches.
//!
//! Used to check the path tracer's multi-bounce transport (`specs/27` § 1.5 item 5); it shares
//! no code with the tracer.

use regia_core::glam::DVec3;

use crate::scenes::RoomQuad;

/// One patch: a planar quad with its centre, normal, reflectance and emission.
#[derive(Clone, Debug)]
struct Patch {
    corners: [DVec3; 4],
    centre: DVec3,
    normal: DVec3,
    albedo: [f64; 3],
    emission: [f64; 3],
    quad: usize,
}

/// A solved room.
#[derive(Clone, Debug)]
pub struct Radiosity {
    patches: Vec<Patch>,
    radiance: Vec<[f64; 3]>,
    quads: Vec<RoomQuad>,
}

/// Lambert's form factor from a differential area at `x` with unit normal `n` to the polygon
/// `poly` (any winding): `|Σ βᵢ (n · Γᵢ)| / 2π`.
#[must_use]
pub fn point_to_polygon(x: DVec3, n: DVec3, poly: &[DVec3]) -> f64 {
    let mut sum = 0.0;
    for i in 0..poly.len() {
        let a = (poly[i] - x).normalize_or_zero();
        let b = (poly[(i + 1) % poly.len()] - x).normalize_or_zero();
        let c = a.cross(b);
        let len = c.length();
        if len < 1e-15 {
            continue;
        }
        let beta = len.atan2(a.dot(b));
        sum += beta * n.dot(c / len);
    }
    (sum / std::f64::consts::TAU).abs()
}

fn to_d(p: [f32; 3]) -> DVec3 {
    DVec3::new(f64::from(p[0]), f64::from(p[1]), f64::from(p[2]))
}

/// Most patches along one quad edge: the form-factor matrix is dense, so the patch count
/// squared is the memory.
const MAX_EDGE_PATCHES: usize = 48;

/// Patches along an edge of `length` metres for a `patch_size` target, in `1..=MAX_EDGE_PATCHES`.
/// A zero, negative or non-finite size takes the cap rather than asking for `usize::MAX`.
fn edge_patches(length: f64, patch_size: f64) -> usize {
    if !(patch_size > 0.0 && patch_size.is_finite()) {
        return MAX_EDGE_PATCHES;
    }
    ((length / patch_size).ceil().min(MAX_EDGE_PATCHES as f64) as usize).max(1)
}

impl Radiosity {
    /// Subdivides every quad into patches of about `patch_size` metres (at most 48 patches per edge; a non-positive or non-finite size
    /// takes that cap) and solves
    /// `L = Le + ρ Σ F L` by Jacobi iteration.
    #[must_use]
    pub fn solve(quads: &[RoomQuad], patch_size: f64) -> Self {
        let _span = tracing::info_span!("pt_radiosity").entered();
        let mut patches = Vec::new();
        for (qi, q) in quads.iter().enumerate() {
            let c = q.corners.map(to_d);
            let nu = edge_patches((c[1] - c[0]).length(), patch_size);
            let nv = edge_patches((c[3] - c[0]).length(), patch_size);
            let normal = (c[1] - c[0]).cross(c[2] - c[0]).normalize_or_zero();
            if normal == DVec3::ZERO {
                // A collapsed quad has no area to exchange light.
                continue;
            }
            let at = |u: f64, v: f64| {
                let a = c[0].lerp(c[1], u);
                let b = c[3].lerp(c[2], u);
                a.lerp(b, v)
            };
            for i in 0..nu {
                for j in 0..nv {
                    let (u0, u1) = (i as f64 / nu as f64, (i + 1) as f64 / nu as f64);
                    let (v0, v1) = (j as f64 / nv as f64, (j + 1) as f64 / nv as f64);
                    let corners = [at(u0, v0), at(u1, v0), at(u1, v1), at(u0, v1)];
                    patches.push(Patch {
                        centre: at(0.5 * (u0 + u1), 0.5 * (v0 + v1)),
                        corners,
                        normal,
                        albedo: q.albedo.map(f64::from),
                        emission: q.emission.map(f64::from),
                        quad: qi,
                    });
                }
            }
        }
        let n = patches.len();
        // Dense form-factor matrix (row i: from patch i's centre to every patch j).
        let mut ff = vec![0.0f64; n * n];
        for i in 0..n {
            for j in 0..n {
                if i == j || patches[i].normal.dot(patches[j].normal) > 1.0 - 1e-9 {
                    // Coplanar (same wall) patches do not exchange light.
                    if (patches[j].centre - patches[i].centre)
                        .dot(patches[i].normal)
                        .abs()
                        < 1e-9
                    {
                        continue;
                    }
                }
                ff[i * n + j] = point_to_polygon(patches[i].centre, patches[i].normal, &patches[j].corners);
            }
        }
        let mut radiance: Vec<[f64; 3]> = patches.iter().map(|p| p.emission).collect();
        for _ in 0..500 {
            let mut next = vec![[0.0f64; 3]; n];
            let mut change = 0.0f64;
            for i in 0..n {
                let mut gather = [0.0f64; 3];
                for j in 0..n {
                    let f = ff[i * n + j];
                    if f == 0.0 {
                        continue;
                    }
                    for k in 0..3 {
                        gather[k] += f * radiance[j][k];
                    }
                }
                for k in 0..3 {
                    next[i][k] = patches[i].emission[k] + patches[i].albedo[k] * gather[k];
                    change = change.max((next[i][k] - radiance[i][k]).abs());
                }
            }
            radiance = next;
            if change < 1e-9 {
                break;
            }
        }
        Self {
            patches,
            radiance,
            quads: quads.to_vec(),
        }
    }

    /// Number of patches.
    #[must_use]
    pub fn patch_count(&self) -> usize {
        self.patches.len()
    }

    /// Outgoing radiance at point `x` on quad `quad`, by a final gather over every patch.
    #[must_use]
    pub fn radiance_at(&self, quad: usize, x: [f64; 3]) -> [f64; 3] {
        let q = &self.quads[quad];
        let c = q.corners.map(to_d);
        let n = (c[1] - c[0]).cross(c[2] - c[0]).normalize_or_zero();
        let x = DVec3::from_array(x);
        let mut gather = [0.0f64; 3];
        for (p, l) in self.patches.iter().zip(&self.radiance) {
            if p.quad == quad || (p.centre - x).dot(n).abs() < 1e-9 {
                continue;
            }
            let f = point_to_polygon(x, n, &p.corners);
            for k in 0..3 {
                gather[k] += f * l[k];
            }
        }
        let mut out = [0.0; 3];
        for k in 0..3 {
            out[k] = f64::from(q.emission[k]) + f64::from(q.albedo[k]) * gather[k];
        }
        out
    }

    /// First quad hit by the ray `o + t·d` (t > 0) and the hit point, by brute force.
    #[must_use]
    pub fn hit(&self, o: [f64; 3], d: [f64; 3]) -> Option<(usize, [f64; 3])> {
        let o = DVec3::from_array(o);
        let d = DVec3::from_array(d);
        let mut best: Option<(f64, usize)> = None;
        for (qi, q) in self.quads.iter().enumerate() {
            let c = q.corners.map(to_d);
            for tri in [[c[0], c[1], c[2]], [c[0], c[2], c[3]]] {
                let e1 = tri[1] - tri[0];
                let e2 = tri[2] - tri[0];
                let p = d.cross(e2);
                let det = e1.dot(p);
                if det.abs() < 1e-18 {
                    continue;
                }
                let tv = o - tri[0];
                let u = tv.dot(p) / det;
                let qv = tv.cross(e1);
                let v = d.dot(qv) / det;
                let t = e2.dot(qv) / det;
                if u >= 0.0 && v >= 0.0 && u + v <= 1.0 && t > 1e-9 && best.is_none_or(|(bt, _)| t < bt) {
                    best = Some((t, qi));
                }
            }
        }
        best.map(|(t, qi)| (qi, (o + d * t).to_array()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_factor_of_a_hemisphere_covering_plane_is_one() {
        // A huge square right above the point covers (almost) the whole hemisphere.
        let poly = [
            DVec3::new(-1e4, 1.0, -1e4),
            DVec3::new(1e4, 1.0, -1e4),
            DVec3::new(1e4, 1.0, 1e4),
            DVec3::new(-1e4, 1.0, 1e4),
        ];
        let f = point_to_polygon(DVec3::ZERO, DVec3::Y, &poly);
        assert!((f - 1.0).abs() < 1e-3, "{f}");
    }

    #[test]
    fn form_factor_to_a_small_disc_like_square() {
        // Small square of area A at height h straight above: F ≈ A / (π h²).
        let s = 0.01;
        let poly = [
            DVec3::new(-s, 1.0, -s),
            DVec3::new(s, 1.0, -s),
            DVec3::new(s, 1.0, s),
            DVec3::new(-s, 1.0, s),
        ];
        let f = point_to_polygon(DVec3::ZERO, DVec3::Y, &poly);
        let expect = (2.0 * s) * (2.0 * s) / std::f64::consts::PI;
        assert!((f / expect - 1.0).abs() < 1e-3, "{f} vs {expect}");
    }

    #[test]
    fn white_closed_cube_balances_energy() {
        // Closed unit cube, all walls albedo 0.5, the floor emits 1: every non-emitting wall
        // sees the floor, so the room settles to a finite positive radiance.
        let quads = vec![
            RoomQuad {
                corners: [[0.0, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 1.0], [1.0, 0.0, 0.0]],
                albedo: [0.5; 3],
                emission: [1.0; 3],
            },
            RoomQuad {
                corners: [[0.0, 1.0, 0.0], [1.0, 1.0, 0.0], [1.0, 1.0, 1.0], [0.0, 1.0, 1.0]],
                albedo: [0.5; 3],
                emission: [0.0; 3],
            },
        ];
        let r = Radiosity::solve(&quads, 0.25);
        assert_eq!(r.patch_count(), 32);
        let l = r.radiance_at(1, [0.5, 1.0, 0.5]);
        assert!(l[0] > 0.1 && l[0] < 1.0, "{l:?}");
    }

    #[test]
    fn degenerate_patch_sizes_are_capped_not_unbounded() {
        assert_eq!(edge_patches(1.0, 0.0), MAX_EDGE_PATCHES);
        assert_eq!(edge_patches(1.0, -1.0), MAX_EDGE_PATCHES);
        assert_eq!(edge_patches(1.0, f64::NAN), MAX_EDGE_PATCHES);
        assert_eq!(edge_patches(1.0, f64::INFINITY), MAX_EDGE_PATCHES);
        assert_eq!(edge_patches(1.0, 1e-12), MAX_EDGE_PATCHES);
        assert_eq!(edge_patches(1.0, 0.25), 4);
        assert_eq!(edge_patches(0.0, 0.25), 1);
    }

    #[test]
    fn a_collapsed_quad_does_not_poison_the_solve() {
        // The first quad has a zero edge (two coincident corners): no area, no NaN.
        let quads = vec![
            RoomQuad {
                corners: [[0.0; 3], [0.0; 3], [1.0, 0.0, 1.0], [1.0, 0.0, 0.0]],
                albedo: [0.5; 3],
                emission: [1.0; 3],
            },
            RoomQuad {
                corners: [[0.0, 1.0, 0.0], [1.0, 1.0, 0.0], [1.0, 1.0, 1.0], [0.0, 1.0, 1.0]],
                albedo: [0.5; 3],
                emission: [0.0; 3],
            },
        ];
        let r = Radiosity::solve(&quads, 0.25);
        assert!(r.patch_count() > 0);
        let l = r.radiance_at(1, [0.5, 1.0, 0.5]);
        assert!(l.iter().all(|v| v.is_finite()), "{l:?}");
        let c = r.radiance_at(0, [0.5, 0.0, 0.5]);
        assert!(c.iter().all(|v| v.is_finite()), "{c:?}");
    }
}
