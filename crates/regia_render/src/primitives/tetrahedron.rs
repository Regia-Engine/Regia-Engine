// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_mesh/src/primitives/dim3/tetrahedron.rs. Copyright (c) Bevy contributors.
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

//! Tetrahedron mesher: four flat faces with unskewed per-face UVs (Bevy's `Triangle3d` UV mapping).

use regia_core::math::{Mat3, Vec3};

use super::MeshParts;
use crate::runtime::CpuMesh;

/// A tetrahedron given by its four corners.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tetrahedron {
    /// Corner positions, metres.
    pub vertices: [Vec3; 4],
}

impl Default for Tetrahedron {
    /// The regular tetrahedron inscribed in the unit cube centred at the origin.
    fn default() -> Self {
        Self {
            vertices: [
                Vec3::new(0.5, 0.5, 0.5),
                Vec3::new(-0.5, 0.5, -0.5),
                Vec3::new(-0.5, -0.5, 0.5),
                Vec3::new(0.5, -0.5, -0.5),
            ],
        }
    }
}

impl Tetrahedron {
    /// Signed volume; negative when the corners are in clockwise order.
    #[must_use]
    pub fn signed_volume(&self) -> f32 {
        let [a, b, c, d] = self.vertices;
        Mat3::from_cols(b - a, c - a, d - a).determinant() / 6.0
    }

    /// The four faces, each wound outward for a positively oriented tetrahedron.
    fn faces(&self) -> [[Vec3; 3]; 4] {
        let [a, b, c, d] = self.vertices;
        [[b, c, d], [d, c, a], [a, b, d], [c, b, a]]
    }

    /// Builds the mesh: 12 vertices, 4 triangles.
    #[must_use]
    pub fn build(&self) -> CpuMesh {
        let mut faces = self.faces();
        // A negatively oriented tetrahedron: reverse every face so they still face outward.
        if self.signed_volume().is_sign_negative() {
            faces.iter_mut().for_each(|f| f.swap(0, 2));
        }
        let mut parts = MeshParts::with_capacity(12, 12);
        for face in faces {
            let normal = (face[1] - face[0])
                .cross(face[2] - face[0])
                .try_normalize()
                .unwrap_or(Vec3::Y);
            for (position, uv) in face.iter().zip(uv_coords(&face)) {
                let index = parts.push(position.to_array(), normal.to_array(), uv);
                parts.indices.push(index);
            }
        }
        parts.finish()
    }
}

/// Unskewed UVs for a triangle: `a→b` is the U axis, the height over it is V, and the triangle is
/// fitted into the unit square without shear.
fn uv_coords(triangle: &[Vec3; 3]) -> [[f32; 2]; 3] {
    let [a, b, c] = *triangle;
    let main_length = a.distance(b);
    let Some(x) = (b - a).try_normalize() else {
        return [[0., 0.], [1., 0.], [0., 1.]];
    };
    let y = c - a;
    // The orthogonalisation of `y` against `x` gives the second direction; `offset` is the
    // U coordinate of `c` in units of `main_length`.
    let y_proj = y.project_onto_normalized(x);
    let offset = y_proj.dot(x) / main_length;
    if offset < 0. {
        // Obtuse, leaning left: the U range extends left of `a`.
        let total_length = 1. - offset;
        [[offset.abs() / total_length, 0.], [1., 0.], [0., 1.]]
    } else if offset > 1. {
        // Obtuse, leaning right: the U range extends right of `b`.
        [[0., 0.], [1. / offset, 0.], [1., 1.]]
    } else {
        [[0., 0.], [1., 0.], [offset, 1.]]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::check;

    #[test]
    fn tetrahedron_is_closed_either_orientation() {
        let regular = Tetrahedron::default();
        let mut flipped = regular;
        flipped.vertices.swap(0, 1);
        assert!(regular.signed_volume() * flipped.signed_volume() < 0.0);
        for (what, tetra) in [("regular", regular), ("flipped", flipped)] {
            let mesh = tetra.build();
            check::assert_well_formed(&mesh, what);
            check::assert_watertight(&mesh, what);
            assert_eq!(check::triangles(&mesh), 4);
            // Outward: each face normal points away from the centroid.
            let centroid: Vec3 = tetra.vertices.iter().sum::<Vec3>() / 4.0;
            for v in &mesh.vertices {
                let n = Vec3::from_array(v.normal);
                assert!(n.dot(Vec3::from_array(v.position) - centroid) > 0.0, "{what}");
            }
        }
    }

    #[test]
    fn obtuse_triangle_uvs_stay_in_the_unit_square() {
        let tri = [Vec3::ZERO, Vec3::X, Vec3::new(-2.0, 1.0, 0.0)];
        for uv in uv_coords(&tri) {
            assert!(uv.iter().all(|c| (0.0..=1.0).contains(c)), "{uv:?}");
        }
        let tri = [Vec3::ZERO, Vec3::X, Vec3::new(3.0, 1.0, 0.0)];
        for uv in uv_coords(&tri) {
            assert!(uv.iter().all(|c| (0.0..=1.0).contains(c)), "{uv:?}");
        }
    }
}
