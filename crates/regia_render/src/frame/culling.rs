// SPDX-License-Identifier: MPL-2.0
//! Mesh bounds and the view frustum used by CPU culling (moved out of `frame.rs` unchanged,
//! ADR-1552).

use super::*;

/// Object-space bounding sphere of a mesh (bind pose), computed at upload.
#[derive(Clone, Copy, Debug)]
pub struct MeshBounds {
    pub(super) center: [f32; 3],
    pub(super) radius: f32,
}

/// Computes a bounding sphere from the vertices (AABB centre + farthest corner).
pub(crate) fn mesh_bounds(vertices: &[LabVertex]) -> Option<MeshBounds> {
    let first = vertices.first()?;
    let mut lo = first.position;
    let mut hi = first.position;
    for vertex in &vertices[1..] {
        for c in 0..3 {
            lo[c] = lo[c].min(vertex.position[c]);
            hi[c] = hi[c].max(vertex.position[c]);
        }
    }
    let center = [
        (lo[0] + hi[0]) * 0.5,
        (lo[1] + hi[1]) * 0.5,
        (lo[2] + hi[2]) * 0.5,
    ];
    let center_v = regia_core::glam::Vec3::from_array(center);
    let radius = vertices
        .iter()
        .map(|v| (regia_core::glam::Vec3::from_array(v.position) - center_v).length())
        .fold(0.0_f32, f32::max);
    Some(MeshBounds { center, radius })
}

/// The six frustum planes of a view-projection matrix.
///
/// Gribb & Hartmann, "Fast Extraction of Viewing Frustum Planes from the
/// World-View-Projection Matrix" (1999); see `docs/provenance.md`.
#[derive(Clone, Copy, Debug)]
pub struct Frustum {
    pub(super) planes: [[f32; 4]; 6],
}

impl Frustum {
    /// Extracts the frustum planes from a (column-major) view-projection matrix.
    #[must_use]
    pub fn from_view_proj(view_proj: &regia_core::glam::Mat4) -> Self {
        let m = view_proj.to_cols_array_2d();
        // m[col][row]; row i = [m[0][i], m[1][i], m[2][i], m[3][i]].
        let row = |i: usize| [m[0][i], m[1][i], m[2][i], m[3][i]];
        let add = |a: [f32; 4], b: [f32; 4]| [a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]];
        let sub = |a: [f32; 4], b: [f32; 4]| [a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3]];
        // Depth range 0..1 (DirectX convention): near = row2, far = row3 - row2.
        let raw = [
            add(row(3), row(0)), // left
            sub(row(3), row(0)), // right
            add(row(3), row(1)), // bottom
            sub(row(3), row(1)), // top
            row(2),              // near
            sub(row(3), row(2)), // far
        ];
        let planes = raw.map(|p| {
            let len = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
            if len > 1.0e-8 {
                [p[0] / len, p[1] / len, p[2] / len, p[3] / len]
            } else {
                p
            }
        });
        Self { planes }
    }

    /// The six inward planes (Gribb–Hartmann). The GPU culler uploads these. A point inside
    /// the frustum has a positive distance to every plane.
    #[must_use]
    pub fn planes(&self) -> [[f32; 4]; 6] {
        self.planes
    }

    /// True when the sphere (center, radius) is (partially) inside the frustum.
    ///
    /// The Gribb–Hartmann planes point inward: a point inside satisfies
    /// `distance >= 0`. A sphere leaves the frustum only when a plane separates
    /// it by more than its radius (`distance < -radius`).
    #[must_use]
    pub fn intersects_sphere(&self, center: [f32; 3], radius: f32) -> bool {
        for plane in &self.planes {
            let distance = plane[0] * center[0] + plane[1] * center[1] + plane[2] * center[2] + plane[3];
            if distance < -radius {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed(plane: [f32; 4], point: [f32; 3]) -> f32 {
        plane[0] * point[0] + plane[1] * point[1] + plane[2] * point[2] + plane[3]
    }

    /// Positive distance is inside. The camera centre of a 0..1 perspective frustum is in
    /// front of the near plane; a point behind the camera is outside the near plane.
    #[test]
    fn frustum_planes_point_inward() {
        use regia_core::glam::Vec3;
        use regia_core::glam::camera::rh::proj::directx::perspective;
        use regia_core::glam::camera::rh::view::look_at_mat4;
        let view = look_at_mat4(Vec3::ZERO, Vec3::new(0.0, 0.0, -1.0), Vec3::Y);
        let proj = perspective(60.0_f32.to_radians(), 1.0, 0.1, 100.0);
        let frustum = Frustum::from_view_proj(&(proj * view));
        let inside = [0.0, 0.0, -1.0];
        for plane in frustum.planes() {
            let distance = signed(plane, inside);
            assert!(
                distance >= -1.0e-4,
                "inside point distance {distance} on {plane:?}"
            );
        }
        assert!(frustum.intersects_sphere(inside, 0.0));
        let behind = [0.0, 0.0, 1.0];
        let near = frustum.planes()[4];
        assert!(signed(near, behind) < 0.0, "near {:?}", signed(near, behind));
        assert!(!frustum.intersects_sphere(behind, 0.0));
    }
}
