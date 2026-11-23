// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_mesh/src/primitives/dim3/torus.rs. Copyright (c) Bevy contributors.
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

//! Torus mesher: a ring around the Y axis, optionally a partial arc.
//!
//! Bevy adapted it from the Apparat engine blog's procedural torus
//! (<http://apparat-engine.blogspot.com/2013/04/procedural-meshes-torus.html>).

use regia_core::math::Vec3;

use super::MeshParts;
use crate::runtime::CpuMesh;

/// A torus centred at the origin, its ring in the XZ plane.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Torus {
    /// Tube radius, metres.
    pub minor_radius: f32,
    /// Distance from the centre to the middle of the tube, metres.
    pub major_radius: f32,
    /// Segments around the tube (at least 3).
    pub minor_resolution: u32,
    /// Segments around the ring (at least 3).
    pub major_resolution: u32,
    /// Start and end of the ring arc, radians (`0..TAU` is closed; a shorter arc leaves both
    /// tube ends open).
    pub angle_range: (f32, f32),
}

impl Default for Torus {
    fn default() -> Self {
        Self {
            minor_radius: 0.25,
            major_radius: 0.75,
            minor_resolution: 24,
            major_resolution: 32,
            angle_range: (0.0, std::f32::consts::TAU),
        }
    }
}

impl Torus {
    /// Triangle count: `2 × major_resolution × minor_resolution`.
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        2 * self.major_resolution.max(3) as usize * self.minor_resolution.max(3) as usize
    }

    /// Builds the mesh.
    #[must_use]
    pub fn build(&self) -> CpuMesh {
        let major_resolution = self.major_resolution.max(3);
        let minor_resolution = self.minor_resolution.max(3);
        let minor_radius = self.minor_radius.abs();
        let major_radius = self.major_radius.abs();
        let n_vertices = ((major_resolution + 1) * (minor_resolution + 1)) as usize;
        let mut parts = MeshParts::with_capacity(n_vertices, self.triangle_count() * 3);

        let (start_angle, end_angle) = self.angle_range;
        let segment_stride = (end_angle - start_angle) / major_resolution as f32;
        let side_stride = std::f32::consts::TAU / minor_resolution as f32;

        for segment in 0..=major_resolution {
            let theta = start_angle + segment_stride * segment as f32;
            let (sin_theta, cos_theta) = theta.sin_cos();
            for side in 0..=minor_resolution {
                let phi = side_stride * side as f32;
                let (sin_phi, cos_phi) = phi.sin_cos();
                let radius = major_radius + minor_radius * cos_phi;
                let position = Vec3::new(cos_theta * radius, minor_radius * sin_phi, sin_theta * radius);
                // The tube-centre direction, so a zero minor radius still has a normal.
                let normal = Vec3::new(cos_theta * cos_phi, sin_phi, sin_theta * cos_phi);
                parts.push(
                    position.to_array(),
                    normal.normalize().to_array(),
                    [
                        segment as f32 / major_resolution as f32,
                        side as f32 / minor_resolution as f32,
                    ],
                );
            }
        }

        let n_vertices_per_row = minor_resolution + 1;
        for segment in 0..major_resolution {
            for side in 0..minor_resolution {
                let lt = side + segment * n_vertices_per_row;
                let rt = (side + 1) + segment * n_vertices_per_row;
                let lb = side + (segment + 1) * n_vertices_per_row;
                let rb = (side + 1) + (segment + 1) * n_vertices_per_row;
                parts.indices.extend_from_slice(&[lt, rt, lb, rt, rb, lb]);
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
    fn torus_is_closed_and_counts_match() {
        for (minor, major) in [(3, 3), (12, 24), (24, 32)] {
            let torus = Torus {
                minor_resolution: minor,
                major_resolution: major,
                ..Torus::default()
            };
            let mesh = torus.build();
            let what = format!("torus {minor}x{major}");
            check::assert_well_formed(&mesh, &what);
            check::assert_watertight(&mesh, &what);
            assert_eq!(check::triangles(&mesh), torus.triangle_count(), "{what}");
        }
    }

    #[test]
    fn half_torus_has_two_open_ends() {
        let mesh = Torus {
            minor_resolution: 10,
            major_resolution: 16,
            angle_range: (0.0, std::f32::consts::PI),
            ..Torus::default()
        }
        .build();
        check::assert_well_formed(&mesh, "half torus");
        assert_eq!(check::boundary_edges(&mesh), 20);
    }
}
