// SPDX-License-Identifier: MPL-2.0
//! Where a crack page lies on a glass mesh ([`DamageSpace`]) and how a hit point maps into it.

use regia_asset::glasscrack::DamageSpace;
use regia_core::math::{Mat4, Vec2, Vec3};

use crate::LabVertex;

/// The page mapping of one mesh.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MaskSpace {
    /// Planar or a UV set (UV1 is not in the forward vertex stream: it maps like planar).
    pub mode: DamageSpace,
    /// Planar: the two local axes spanning the page (0 = x, 1 = y, 2 = z).
    pub axes: [usize; 2],
    /// Planar: local coordinates of the page's (0, 0) corner along `axes`.
    pub origin: [f32; 2],
    /// Planar: local length covered by the page side (square page over the bbox's larger side).
    pub extent: f32,
}

impl MaskSpace {
    /// The mapping of `vertices` for `mode`. A degenerate mesh gets a unit planar page.
    #[must_use]
    pub fn from_mesh(vertices: &[LabVertex], mode: DamageSpace) -> Self {
        let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for v in vertices {
            let p = Vec3::from_array(v.position);
            lo = lo.min(p);
            hi = hi.max(p);
        }
        if vertices.is_empty() {
            lo = Vec3::ZERO;
            hi = Vec3::ONE;
        }
        let size = (hi - lo).to_array();
        // The thinnest bbox axis is the projection axis (a pane's normal, a canopy's up axis).
        let thin = (0..3).min_by(|a, b| size[*a].total_cmp(&size[*b])).unwrap_or(2);
        let axes = match thin {
            0 => [2, 1],
            1 => [0, 2],
            _ => [0, 1],
        };
        let extent = size[axes[0]].max(size[axes[1]]).max(1e-4);
        let lo = lo.to_array();
        let hi = hi.to_array();
        // Centre the square page over the rectangle.
        let origin = [
            (lo[axes[0]] + hi[axes[0]]) * 0.5 - extent * 0.5,
            (lo[axes[1]] + hi[axes[1]]) * 0.5 - extent * 0.5,
        ];
        let mode = if mode == DamageSpace::Uv0 {
            DamageSpace::Uv0
        } else {
            DamageSpace::Planar
        };
        Self {
            mode,
            axes,
            origin,
            extent,
        }
    }

    /// Planar page UV of a mesh-local point (outside 0..1 when off the page).
    #[must_use]
    pub fn planar_uv(&self, local: Vec3) -> Vec2 {
        let p = local.to_array();
        Vec2::new(
            (p[self.axes[0]] - self.origin[0]) / self.extent,
            (p[self.axes[1]] - self.origin[1]) / self.extent,
        )
    }

    /// Page UV of a mesh-local hit and the local length one UV unit spans there. UV0 finds the
    /// closest triangle and interpolates its UVs; planar projects.
    #[must_use]
    pub fn hit_uv(&self, vertices: &[LabVertex], indices: &[u32], local: Vec3) -> Option<(Vec2, f32)> {
        if self.mode == DamageSpace::Planar {
            return Some((self.planar_uv(local), self.extent));
        }
        let mut best: Option<(f32, Vec2, f32)> = None;
        for tri in indices.as_chunks::<3>().0 {
            let [Some(a), Some(b), Some(c)] = [0, 1, 2].map(|k| vertices.get(tri[k] as usize)) else {
                continue;
            };
            let (pa, pb, pc) = (
                Vec3::from_array(a.position),
                Vec3::from_array(b.position),
                Vec3::from_array(c.position),
            );
            let (q, bary) = closest_point_on_triangle(local, pa, pb, pc);
            let d = q.distance_squared(local);
            if best.is_some_and(|(bd, ..)| bd <= d) {
                continue;
            }
            let uv = Vec2::from_array(a.uv) * bary.x
                + Vec2::from_array(b.uv) * bary.y
                + Vec2::from_array(c.uv) * bary.z;
            let area = (pb - pa).cross(pc - pa).length();
            let (ua, ub, uc) = (
                Vec2::from_array(a.uv),
                Vec2::from_array(b.uv),
                Vec2::from_array(c.uv),
            );
            let uv_area = (ub - ua).perp_dot(uc - ua).abs();
            if uv_area <= 1e-12 {
                continue;
            }
            best = Some((d, uv, (area / uv_area).sqrt()));
        }
        best.map(|(_, uv, scale)| (uv, scale))
    }

    /// World → page UV (xy of the product) for a planar page under `model`.
    #[must_use]
    pub fn mask_from_world(&self, model: Mat4) -> Mat4 {
        let mut rows = [[0.0f32; 4]; 4];
        rows[0][self.axes[0]] = 1.0 / self.extent;
        rows[0][3] = -self.origin[0] / self.extent;
        rows[1][self.axes[1]] = 1.0 / self.extent;
        rows[1][3] = -self.origin[1] / self.extent;
        rows[3][3] = 1.0;
        let local_to_uv = Mat4::from_cols_array_2d(&rows).transpose();
        local_to_uv * model.inverse()
    }

    /// World directions of the page's +U and +V axes under `model` (planar), normalised.
    #[must_use]
    pub fn world_axes(&self, model: Mat4) -> (Vec3, Vec3) {
        let unit = |k: usize| {
            let mut v = [0.0f32; 3];
            v[k] = 1.0;
            model.transform_vector3(Vec3::from_array(v)).normalize_or_zero()
        };
        (unit(self.axes[0]), unit(self.axes[1]))
    }
}

/// Closest point on triangle `abc` to `p` and its barycentric weights (Ericson, *Real-Time
/// Collision Detection*, § 5.1.5).
#[must_use]
pub fn closest_point_on_triangle(p: Vec3, a: Vec3, b: Vec3, c: Vec3) -> (Vec3, Vec3) {
    let (ab, ac, ap) = (b - a, c - a, p - a);
    let (d1, d2) = (ab.dot(ap), ac.dot(ap));
    if d1 <= 0.0 && d2 <= 0.0 {
        return (a, Vec3::X);
    }
    let bp = p - b;
    let (d3, d4) = (ab.dot(bp), ac.dot(bp));
    if d3 >= 0.0 && d4 <= d3 {
        return (b, Vec3::Y);
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let v = d1 / (d1 - d3);
        return (a + ab * v, Vec3::new(1.0 - v, v, 0.0));
    }
    let cp = p - c;
    let (d5, d6) = (ab.dot(cp), ac.dot(cp));
    if d6 >= 0.0 && d5 <= d6 {
        return (c, Vec3::Z);
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let w = d2 / (d2 - d6);
        return (a + ac * w, Vec3::new(1.0 - w, 0.0, w));
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let w = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        return (b + (c - b) * w, Vec3::new(0.0, 1.0 - w, w));
    }
    let denom = 1.0 / (va + vb + vc);
    let (v, w) = (vb * denom, vc * denom);
    (a + ab * v + ac * w, Vec3::new(1.0 - v - w, v, w))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quad(w: f32, h: f32) -> (Vec<LabVertex>, Vec<u32>) {
        let v = |x: f32, y: f32, u: f32, vv: f32| LabVertex {
            position: [x, y, 0.0],
            normal: [0.0, 0.0, 1.0],
            uv: [u, vv],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        };
        (
            vec![
                v(-w, -h, 0.0, 1.0),
                v(w, -h, 1.0, 1.0),
                v(w, h, 1.0, 0.0),
                v(-w, h, 0.0, 0.0),
            ],
            vec![0, 1, 2, 0, 2, 3],
        )
    }

    #[test]
    fn planar_page_covers_the_pane_and_world_matrix_agrees() {
        let (verts, _) = quad(1.0, 0.5);
        let space = MaskSpace::from_mesh(&verts, DamageSpace::Planar);
        assert_eq!(space.axes, [0, 1]);
        assert!((space.extent - 2.0).abs() < 1e-5);
        let uv = space.planar_uv(Vec3::new(0.5, 0.25, 0.0));
        assert!((uv - Vec2::new(0.75, 0.625)).length() < 1e-5, "{uv}");
        let model = Mat4::from_rotation_translation(
            regia_core::math::Quat::from_rotation_y(0.7),
            Vec3::new(3.0, 1.0, -2.0),
        );
        let world = model.transform_point3(Vec3::new(0.5, 0.25, 0.0));
        let via = space.mask_from_world(model).transform_point3(world);
        assert!((via.truncate() - uv).length() < 1e-4, "{via}");
    }

    #[test]
    fn uv0_hit_interpolates_the_triangle() {
        let (verts, idx) = quad(1.0, 1.0);
        let space = MaskSpace::from_mesh(&verts, DamageSpace::Uv0);
        let (uv, scale) = space
            .hit_uv(&verts, &idx, Vec3::new(0.0, 0.0, 0.01))
            .expect("hit");
        assert!((uv - Vec2::new(0.5, 0.5)).length() < 1e-4, "{uv}");
        assert!((scale - 2.0).abs() < 1e-3, "{scale}");
    }
}
