// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_mesh/src/primitives/dim3/sphere.rs. Copyright (c) Bevy contributors.
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

//! UV sphere mesher (sectors × stacks), poles on ±Y.
//!
//! The vertex grid follows Song Ho Ahn's sphere article (<http://www.songho.ca/opengl/gl_sphere.html>)
//! as Bevy's `SphereMeshBuilder::uv` does; Bevy's poles sit on ±Z and are rotated onto ±Y here
//! (a proper rotation, so the winding is unchanged). Bevy's icosphere needs `hexasphere`, which
//! Regia Engine does not depend on, so it is not ported.

use std::f32::consts::PI;

use super::MeshParts;
use crate::runtime::CpuMesh;

/// A UV sphere centred at the origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sphere {
    /// Radius, metres.
    pub radius: f32,
    /// Segments around the Y axis (at least 3).
    pub sectors: u32,
    /// Segments from pole to pole (at least 2).
    pub stacks: u32,
}

impl Default for Sphere {
    fn default() -> Self {
        Self {
            radius: 0.5,
            sectors: 32,
            stacks: 18,
        }
    }
}

impl Sphere {
    /// Builds the mesh: `(stacks + 1) × (sectors + 1)` vertices and
    /// `2 × sectors × (stacks − 1)` triangles (one triangle per sector in each polar row).
    #[must_use]
    pub fn build(&self) -> CpuMesh {
        let sectors = self.sectors.max(3);
        let stacks = self.stacks.max(2);
        let radius = self.radius.abs();
        let sector_step = 2.0 * PI / sectors as f32;
        let stack_step = PI / stacks as f32;
        let n_vertices = ((stacks + 1) * (sectors + 1)) as usize;
        let mut parts = MeshParts::with_capacity(n_vertices, n_vertices * 6);

        for i in 0..=stacks {
            let stack_angle = PI / 2.0 - i as f32 * stack_step;
            let (sin_stack, cos_stack) = stack_angle.sin_cos();
            for j in 0..=sectors {
                let sector_angle = j as f32 * sector_step;
                let (sin_sector, cos_sector) = sector_angle.sin_cos();
                // Bevy's (x, y, z) with z up, rotated −90° about X: (x, z, −y).
                let normal = [cos_stack * cos_sector, sin_stack, -cos_stack * sin_sector];
                parts.push(
                    normal.map(|c| c * radius),
                    normal,
                    [j as f32 / sectors as f32, i as f32 / stacks as f32],
                );
            }
        }

        //  k1--k1+1
        //  |  / |
        //  | /  |
        //  k2--k2+1
        for i in 0..stacks {
            for j in 0..sectors {
                let k1 = i * (sectors + 1) + j;
                let k2 = k1 + sectors + 1;
                if i != 0 {
                    parts.indices.extend_from_slice(&[k1, k2, k1 + 1]);
                }
                if i != stacks - 1 {
                    parts.indices.extend_from_slice(&[k1 + 1, k2, k2 + 1]);
                }
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
    fn sphere_is_closed_and_counts_match() {
        for (sectors, stacks) in [(3, 2), (8, 6), (32, 18)] {
            let mesh = Sphere {
                radius: 1.5,
                sectors,
                stacks,
            }
            .build();
            let what = format!("sphere {sectors}x{stacks}");
            check::assert_well_formed(&mesh, &what);
            check::assert_watertight(&mesh, &what);
            assert_eq!(
                check::triangles(&mesh),
                (2 * sectors * (stacks - 1)) as usize,
                "{what}"
            );
            for v in &mesh.vertices {
                let r = v.position.iter().map(|c| c * c).sum::<f32>().sqrt();
                assert!((r - 1.5).abs() < 1.0e-4, "{what}: radius {r}");
            }
        }
    }

    #[test]
    fn sphere_poles_are_on_y() {
        let mesh = Sphere::default().build();
        assert!((mesh.vertices[0].position[1] - 0.5).abs() < 1.0e-6);
        let last = mesh.vertices.last().map(|v| v.position[1]);
        assert!(last.is_some_and(|y| (y + 0.5).abs() < 1.0e-6));
    }
}
