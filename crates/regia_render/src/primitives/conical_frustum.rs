// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_mesh/src/primitives/dim3/conical_frustum.rs. Copyright (c) Bevy contributors.
// Bevy is dual-licensed MIT OR Apache-2.0; this file uses it under the MIT licence:
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! Conical frustum mesher (a cone with its top cut off), axis along Y.

use regia_core::math::Vec3;

use super::MeshParts;
use super::cylinder::build_cap;
use crate::runtime::CpuMesh;

/// A conical frustum centred at the origin, bottom on −Y and top on +Y.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConicalFrustum {
    /// Top radius, metres.
    pub radius_top: f32,
    /// Bottom radius, metres.
    pub radius_bottom: f32,
    /// Height, metres.
    pub height: f32,
    /// Sides around the axis (at least 3).
    pub resolution: u32,
    /// Segments along the axis (at least 1).
    pub segments: u32,
}

impl Default for ConicalFrustum {
    fn default() -> Self {
        Self {
            radius_top: 0.25,
            radius_bottom: 0.5,
            height: 0.5,
            resolution: 32,
            segments: 1,
        }
    }
}

impl ConicalFrustum {
    /// Triangle count: `2 × resolution × segments + 2 × (resolution − 2)`.
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        let resolution = self.resolution.max(3) as usize;
        let segments = self.segments.max(1) as usize;
        2 * resolution * segments + 2 * (resolution - 2)
    }

    /// Builds the mesh.
    #[must_use]
    pub fn build(&self) -> CpuMesh {
        let resolution = self.resolution.max(3);
        let segments = self.segments.max(1);
        let radius_top = self.radius_top.abs();
        let radius_bottom = self.radius_bottom.abs();
        let height = self.height.abs();
        let half_height = height / 2.0;

        let num_rings = segments + 1;
        let num_vertices = (resolution * 2 + num_rings * (resolution + 1)) as usize;
        let mut parts = MeshParts::with_capacity(num_vertices, self.triangle_count() * 3);

        let step_theta = std::f32::consts::TAU / resolution as f32;
        let step_y = height / segments as f32;
        let step_radius = (radius_top - radius_bottom) / segments as f32;
        let slope = if height > 0.0 {
            (radius_bottom - radius_top) / height
        } else {
            0.0
        };

        // Rings.
        for ring in 0..num_rings {
            let y = -half_height + ring as f32 * step_y;
            let radius = radius_bottom + ring as f32 * step_radius;
            for segment in 0..=resolution {
                let theta = segment as f32 * step_theta;
                let (sin, cos) = theta.sin_cos();
                parts.push(
                    [radius * cos, y, radius * sin],
                    Vec3::new(cos, slope, sin).normalize().to_array(),
                    [segment as f32 / resolution as f32, ring as f32 / segments as f32],
                );
            }
        }

        // Lateral surface.
        for i in 0..segments {
            let ring = i * (resolution + 1);
            let next_ring = (i + 1) * (resolution + 1);
            for j in 0..resolution {
                parts.indices.extend_from_slice(&[
                    ring + j,
                    next_ring + j,
                    ring + j + 1,
                    next_ring + j,
                    next_ring + j + 1,
                    ring + j + 1,
                ]);
            }
        }

        build_cap(&mut parts, resolution, radius_top, half_height, true);
        build_cap(&mut parts, resolution, radius_bottom, -half_height, false);
        parts.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::check;

    #[test]
    fn frustum_is_closed_and_counts_match() {
        for (resolution, segments) in [(3, 1), (16, 3), (32, 1)] {
            let frustum = ConicalFrustum {
                radius_top: 0.2,
                radius_bottom: 0.6,
                height: 0.9,
                resolution,
                segments,
            };
            let mesh = frustum.build();
            let what = format!("frustum {resolution}x{segments}");
            check::assert_well_formed(&mesh, &what);
            check::assert_watertight(&mesh, &what);
            assert_eq!(check::triangles(&mesh), frustum.triangle_count(), "{what}");
        }
    }

    #[test]
    fn inverted_frustum_normals_lean_down() {
        let mesh = ConicalFrustum {
            radius_top: 1.0,
            radius_bottom: 0.25,
            ..ConicalFrustum::default()
        }
        .build();
        check::assert_well_formed(&mesh, "inverted frustum");
        assert!(mesh.vertices[0].normal[1] < 0.0);
    }
}
