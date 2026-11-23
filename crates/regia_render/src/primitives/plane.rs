// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_mesh/src/primitives/dim3/plane.rs. Copyright (c) Bevy contributors.
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

//! Subdivided plane mesher, rotated so its front face points along `normal`.

use regia_core::math::{Quat, Vec2, Vec3};

use super::MeshParts;
use crate::runtime::CpuMesh;

/// A flat rectangle centred at the origin (single-sided: its front faces `normal`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plane {
    /// Facing direction (normalised when built; +Y when zero).
    pub normal: Vec3,
    /// Half the size along the plane's local X and Z, metres.
    pub half_size: Vec2,
    /// Extra vertex columns along local X.
    pub subdivisions_x: u32,
    /// Extra vertex rows along local Z.
    pub subdivisions_z: u32,
}

impl Default for Plane {
    fn default() -> Self {
        Self {
            normal: Vec3::Y,
            half_size: Vec2::splat(0.5),
            subdivisions_x: 0,
            subdivisions_z: 0,
        }
    }
}

impl Plane {
    /// Builds the mesh: `(subdivisions_x + 2) × (subdivisions_z + 2)` vertices and
    /// `2 × (subdivisions_x + 1) × (subdivisions_z + 1)` triangles.
    #[must_use]
    pub fn build(&self) -> CpuMesh {
        let z_vertex_count = self.subdivisions_z + 2;
        let x_vertex_count = self.subdivisions_x + 2;
        let num_vertices = (z_vertex_count * x_vertex_count) as usize;
        let num_indices = ((z_vertex_count - 1) * (x_vertex_count - 1) * 6) as usize;
        let mut parts = MeshParts::with_capacity(num_vertices, num_indices);

        let normal = self.normal.normalize_or(Vec3::Y);
        let rotation = Quat::from_rotation_arc(Vec3::Y, normal);
        let size = self.half_size.abs() * 2.0;

        for z in 0..z_vertex_count {
            for x in 0..x_vertex_count {
                let tx = x as f32 / (x_vertex_count - 1) as f32;
                let tz = z as f32 / (z_vertex_count - 1) as f32;
                let pos = rotation * Vec3::new((-0.5 + tx) * size.x, 0.0, (-0.5 + tz) * size.y);
                parts.push(pos.to_array(), normal.to_array(), [tx, tz]);
            }
        }

        for z in 0..z_vertex_count - 1 {
            for x in 0..x_vertex_count - 1 {
                let quad = z * x_vertex_count + x;
                parts.indices.extend_from_slice(&[
                    quad + x_vertex_count + 1,
                    quad + 1,
                    quad + x_vertex_count,
                    quad,
                    quad + x_vertex_count,
                    quad + 1,
                ]);
            }
        }
        parts.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::check;

    #[test]
    fn plane_counts_and_open_boundary() {
        for (sx, sz) in [(0, 0), (3, 1), (7, 7)] {
            let mesh = Plane {
                half_size: Vec2::new(2.0, 1.0),
                subdivisions_x: sx,
                subdivisions_z: sz,
                ..Plane::default()
            }
            .build();
            let what = format!("plane {sx}x{sz}");
            check::assert_well_formed(&mesh, &what);
            assert_eq!(
                check::triangles(&mesh),
                (2 * (sx + 1) * (sz + 1)) as usize,
                "{what}"
            );
            // Only the rim is open: 2 × (cells along X + cells along Z) edges.
            assert_eq!(
                check::boundary_edges(&mesh),
                (2 * (sx + 1 + sz + 1)) as usize,
                "{what}"
            );
        }
    }

    #[test]
    fn plane_faces_its_normal() {
        let mesh = Plane {
            normal: Vec3::new(0.0, 0.0, -2.0),
            ..Plane::default()
        }
        .build();
        check::assert_well_formed(&mesh, "plane -z");
        assert!(mesh.vertices.iter().all(|v| v.normal == [0.0, 0.0, -1.0]));
        assert!(mesh.vertices.iter().all(|v| v.position[2].abs() < 1.0e-6));
    }
}
