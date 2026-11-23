// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_mesh/src/primitives/dim3/cuboid.rs. Copyright (c) Bevy contributors.
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

//! Box mesher: six faces, four vertices each, per-face UVs.

use regia_core::math::Vec3;

use super::MeshParts;
use crate::runtime::CpuMesh;

/// An axis-aligned box centred at the origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cuboid {
    /// Half the size along each axis, metres.
    pub half_size: Vec3,
}

impl Default for Cuboid {
    fn default() -> Self {
        Self {
            half_size: Vec3::splat(0.5),
        }
    }
}

impl Cuboid {
    /// A box of the given full size.
    #[must_use]
    pub fn new(size: Vec3) -> Self {
        Self {
            half_size: size * 0.5,
        }
    }

    /// Builds the mesh: 24 vertices, 12 triangles.
    #[must_use]
    pub fn build(&self) -> CpuMesh {
        let min = -self.half_size.abs();
        let max = self.half_size.abs();
        // Y up, right-handed, camera looking from +Z towards -Z.
        let vertices: [([f32; 3], [f32; 3], [f32; 2]); 24] = [
            // Front (+Z)
            ([min.x, min.y, max.z], [0.0, 0.0, 1.0], [0.0, 0.0]),
            ([max.x, min.y, max.z], [0.0, 0.0, 1.0], [1.0, 0.0]),
            ([max.x, max.y, max.z], [0.0, 0.0, 1.0], [1.0, 1.0]),
            ([min.x, max.y, max.z], [0.0, 0.0, 1.0], [0.0, 1.0]),
            // Back (-Z)
            ([min.x, max.y, min.z], [0.0, 0.0, -1.0], [1.0, 0.0]),
            ([max.x, max.y, min.z], [0.0, 0.0, -1.0], [0.0, 0.0]),
            ([max.x, min.y, min.z], [0.0, 0.0, -1.0], [0.0, 1.0]),
            ([min.x, min.y, min.z], [0.0, 0.0, -1.0], [1.0, 1.0]),
            // Right (+X)
            ([max.x, min.y, min.z], [1.0, 0.0, 0.0], [0.0, 0.0]),
            ([max.x, max.y, min.z], [1.0, 0.0, 0.0], [1.0, 0.0]),
            ([max.x, max.y, max.z], [1.0, 0.0, 0.0], [1.0, 1.0]),
            ([max.x, min.y, max.z], [1.0, 0.0, 0.0], [0.0, 1.0]),
            // Left (-X)
            ([min.x, min.y, max.z], [-1.0, 0.0, 0.0], [1.0, 0.0]),
            ([min.x, max.y, max.z], [-1.0, 0.0, 0.0], [0.0, 0.0]),
            ([min.x, max.y, min.z], [-1.0, 0.0, 0.0], [0.0, 1.0]),
            ([min.x, min.y, min.z], [-1.0, 0.0, 0.0], [1.0, 1.0]),
            // Top (+Y)
            ([max.x, max.y, min.z], [0.0, 1.0, 0.0], [1.0, 0.0]),
            ([min.x, max.y, min.z], [0.0, 1.0, 0.0], [0.0, 0.0]),
            ([min.x, max.y, max.z], [0.0, 1.0, 0.0], [0.0, 1.0]),
            ([max.x, max.y, max.z], [0.0, 1.0, 0.0], [1.0, 1.0]),
            // Bottom (-Y)
            ([max.x, min.y, max.z], [0.0, -1.0, 0.0], [0.0, 0.0]),
            ([min.x, min.y, max.z], [0.0, -1.0, 0.0], [1.0, 0.0]),
            ([min.x, min.y, min.z], [0.0, -1.0, 0.0], [1.0, 1.0]),
            ([max.x, min.y, min.z], [0.0, -1.0, 0.0], [0.0, 1.0]),
        ];
        let mut parts = MeshParts::with_capacity(24, 36);
        for (position, normal, uv) in vertices {
            parts.push(position, normal, uv);
        }
        for face in 0..6u32 {
            let b = face * 4;
            parts
                .indices
                .extend_from_slice(&[b, b + 1, b + 2, b + 2, b + 3, b]);
        }
        parts.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::check;

    #[test]
    fn cuboid_is_closed_and_well_formed() {
        let mesh = Cuboid::new(Vec3::new(2.0, 1.0, 0.5)).build();
        check::assert_well_formed(&mesh, "cuboid");
        check::assert_watertight(&mesh, "cuboid");
        assert_eq!(mesh.vertices.len(), 24);
        assert_eq!(check::triangles(&mesh), 12);
        let max_x = mesh
            .vertices
            .iter()
            .map(|v| v.position[0])
            .fold(f32::MIN, f32::max);
        assert_eq!(max_x, 1.0);
    }
}
