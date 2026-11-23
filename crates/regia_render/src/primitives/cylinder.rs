// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_mesh/src/primitives/dim3/cylinder.rs. Copyright (c) Bevy contributors.
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

//! Cylinder mesher: subdivided barrel plus optional cap discs, axis along Y.

use super::MeshParts;
use crate::runtime::CpuMesh;

/// Where a [`Cylinder`]'s origin sits along its axis.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CylinderAnchor {
    /// Halfway between the caps.
    #[default]
    MidPoint,
    /// At the top cap (the cylinder hangs below the origin).
    Top,
    /// At the bottom cap (the cylinder stands on the origin).
    Bottom,
}

/// A cylinder centred on the Y axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cylinder {
    /// Radius, metres.
    pub radius: f32,
    /// Half the height, metres.
    pub half_height: f32,
    /// Sides around the axis (at least 3).
    pub resolution: u32,
    /// Barrel segments along the axis (at least 1).
    pub segments: u32,
    /// Close the ends with discs (an open tube when false).
    pub caps: bool,
    /// Origin placement.
    pub anchor: CylinderAnchor,
}

impl Default for Cylinder {
    fn default() -> Self {
        Self {
            radius: 0.5,
            half_height: 0.5,
            resolution: 32,
            segments: 1,
            caps: true,
            anchor: CylinderAnchor::MidPoint,
        }
    }
}

impl Cylinder {
    /// Triangle count: `2 × resolution × segments` plus `2 × (resolution − 2)` with caps.
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        let resolution = self.resolution.max(3) as usize;
        let segments = self.segments.max(1) as usize;
        2 * resolution * segments + if self.caps { 2 * (resolution - 2) } else { 0 }
    }

    /// Builds the mesh.
    #[must_use]
    pub fn build(&self) -> CpuMesh {
        let resolution = self.resolution.max(3);
        let segments = self.segments.max(1);
        let radius = self.radius.abs();
        let half_height = self.half_height.abs();

        let num_rings = segments + 1;
        let num_vertices = (resolution * 2 + num_rings * (resolution + 1)) as usize;
        let mut parts = MeshParts::with_capacity(num_vertices, self.triangle_count() * 3);

        let step_theta = std::f32::consts::TAU / resolution as f32;
        let step_y = 2.0 * half_height / segments as f32;

        // Rings.
        for ring in 0..num_rings {
            let y = -half_height + ring as f32 * step_y;
            for segment in 0..=resolution {
                let theta = segment as f32 * step_theta;
                let (sin, cos) = theta.sin_cos();
                parts.push(
                    [radius * cos, y, radius * sin],
                    [cos, 0.0, sin],
                    [segment as f32 / resolution as f32, ring as f32 / segments as f32],
                );
            }
        }

        // Barrel skin.
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

        if self.caps {
            build_cap(&mut parts, resolution, radius, half_height, true);
            build_cap(&mut parts, resolution, radius, -half_height, false);
        }

        let shift = match self.anchor {
            CylinderAnchor::Top => -half_height,
            CylinderAnchor::Bottom => half_height,
            CylinderAnchor::MidPoint => 0.0,
        };
        parts.positions.iter_mut().for_each(|p| p[1] += shift);
        parts.finish()
    }
}

/// A fan-triangulated disc at height `y`, facing +Y when `top`, −Y otherwise.
pub(super) fn build_cap(parts: &mut MeshParts, resolution: u32, radius: f32, y: f32, top: bool) {
    let offset = parts.positions.len() as u32;
    let (normal_y, winding) = if top { (1.0, (1, 0)) } else { (-1.0, (0, 1)) };
    let step_theta = std::f32::consts::TAU / resolution as f32;
    for i in 0..resolution {
        let theta = i as f32 * step_theta;
        let (sin, cos) = theta.sin_cos();
        parts.push(
            [cos * radius, y, sin * radius],
            [0.0, normal_y, 0.0],
            [0.5 * (cos + 1.0), 1.0 - 0.5 * (sin + 1.0)],
        );
    }
    for i in 1..(resolution - 1) {
        parts
            .indices
            .extend_from_slice(&[offset, offset + i + winding.0, offset + i + winding.1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::check;

    #[test]
    fn cylinder_is_closed_and_counts_match() {
        for (resolution, segments) in [(3, 1), (8, 4), (32, 1), (0, 0)] {
            let cylinder = Cylinder {
                radius: 0.3,
                half_height: 1.2,
                resolution,
                segments,
                ..Cylinder::default()
            };
            let mesh = cylinder.build();
            let what = format!("cylinder {resolution}x{segments}");
            check::assert_well_formed(&mesh, &what);
            check::assert_watertight(&mesh, &what);
            assert_eq!(check::triangles(&mesh), cylinder.triangle_count(), "{what}");
        }
    }

    #[test]
    fn open_tube_has_two_rims() {
        let cylinder = Cylinder {
            resolution: 12,
            caps: false,
            anchor: CylinderAnchor::Bottom,
            ..Cylinder::default()
        };
        let mesh = cylinder.build();
        check::assert_well_formed(&mesh, "tube");
        assert_eq!(check::boundary_edges(&mesh), 24);
        assert_eq!(check::triangles(&mesh), 24);
        let lowest = mesh
            .vertices
            .iter()
            .map(|v| v.position[1])
            .fold(f32::MAX, f32::min);
        assert_eq!(lowest, 0.0);
    }
}
