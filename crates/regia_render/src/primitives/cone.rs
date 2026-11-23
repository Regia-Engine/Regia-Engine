// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_mesh/src/primitives/dim3/cone.rs. Copyright (c) Bevy contributors.
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

//! Cone mesher: lateral surface plus a base disc, axis along Y.
//!
//! Regia Engine change: Bevy shares one tip vertex with a zero normal (relying on NaN shading in a
//! vertex shader that does not normalise). Regia Engine's shaders normalise, so the tip is split into
//! one vertex per side with the normal of the side's mid angle; the lateral surface stays smooth.

use super::MeshParts;
use crate::runtime::CpuMesh;

/// Where a [`Cone`]'s origin sits along its axis.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConeAnchor {
    /// Halfway between base and tip.
    #[default]
    MidPoint,
    /// At the tip (the cone hangs below the origin).
    Tip,
    /// At the base centre (the cone stands on the origin).
    Base,
}

/// A cone with its base on −Y and its tip on +Y.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cone {
    /// Base radius, metres.
    pub radius: f32,
    /// Base-to-tip height, metres.
    pub height: f32,
    /// Sides around the axis (at least 3).
    pub resolution: u32,
    /// Origin placement.
    pub anchor: ConeAnchor,
}

impl Default for Cone {
    fn default() -> Self {
        Self {
            radius: 0.5,
            height: 1.0,
            resolution: 32,
            anchor: ConeAnchor::MidPoint,
        }
    }
}

impl Cone {
    /// Triangle count: `resolution` sides plus `resolution − 2` base triangles.
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        let resolution = self.resolution.max(3) as usize;
        2 * resolution - 2
    }

    /// Builds the mesh.
    #[must_use]
    pub fn build(&self) -> CpuMesh {
        let resolution = self.resolution.max(3);
        let radius = self.radius.abs();
        let height = self.height.abs();
        let half_height = height / 2.0;
        let mut parts = MeshParts::with_capacity(resolution as usize * 3, self.triangle_count() * 3);

        // Slope of the side normal, normalised once (the normal is (cos, slope, sin) / len).
        let normal_slope = if height > 0.0 { radius / height } else { 0.0 };
        let normalization_factor = (1.0 + normal_slope * normal_slope).sqrt().recip();
        let step_theta = std::f32::consts::TAU / resolution as f32;
        let side_normal = |theta: f32| {
            let (sin, cos) = theta.sin_cos();
            [
                cos * normalization_factor,
                normal_slope * normalization_factor,
                sin * normalization_factor,
            ]
        };

        // Lateral surface. UVs are polar, like projecting a circle texture from above, with the
        // tip at the centre.
        let first_rim = parts.positions.len() as u32;
        for segment in 0..resolution {
            let theta = segment as f32 * step_theta;
            let (sin, cos) = theta.sin_cos();
            parts.push(
                [radius * cos, -half_height, radius * sin],
                side_normal(theta),
                [0.5 + cos * 0.5, 0.5 + sin * 0.5],
            );
        }
        for segment in 0..resolution {
            let tip = parts.push(
                [0.0, half_height, 0.0],
                side_normal((segment as f32 + 0.5) * step_theta),
                [0.5, 0.5],
            );
            let here = first_rim + segment;
            let next = first_rim + (segment + 1) % resolution;
            parts.indices.extend_from_slice(&[tip, next, here]);
        }

        // Base disc.
        let index_offset = parts.positions.len() as u32;
        for i in 0..resolution {
            let theta = i as f32 * step_theta;
            let (sin, cos) = theta.sin_cos();
            parts.push(
                [cos * radius, -half_height, sin * radius],
                [0.0, -1.0, 0.0],
                [0.5 * (cos + 1.0), 1.0 - 0.5 * (sin + 1.0)],
            );
        }
        for i in 1..(resolution - 1) {
            parts
                .indices
                .extend_from_slice(&[index_offset, index_offset + i, index_offset + i + 1]);
        }

        let shift = match self.anchor {
            ConeAnchor::Tip => -half_height,
            ConeAnchor::Base => half_height,
            ConeAnchor::MidPoint => 0.0,
        };
        parts.positions.iter_mut().for_each(|p| p[1] += shift);
        parts.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::check;

    #[test]
    fn cone_is_closed_and_counts_match() {
        for resolution in [3, 4, 17, 64] {
            let cone = Cone {
                radius: 0.7,
                height: 1.3,
                resolution,
                anchor: ConeAnchor::MidPoint,
            };
            let mesh = cone.build();
            let what = format!("cone {resolution}");
            check::assert_well_formed(&mesh, &what);
            check::assert_watertight(&mesh, &what);
            assert_eq!(check::triangles(&mesh), cone.triangle_count(), "{what}");
        }
    }

    #[test]
    fn cone_anchors_shift_the_base() {
        let lowest = |anchor| {
            Cone {
                anchor,
                ..Cone::default()
            }
            .build()
            .vertices
            .iter()
            .map(|v| v.position[1])
            .fold(f32::MAX, f32::min)
        };
        assert_eq!(lowest(ConeAnchor::MidPoint), -0.5);
        assert_eq!(lowest(ConeAnchor::Base), 0.0);
        assert_eq!(lowest(ConeAnchor::Tip), -1.0);
    }

    #[test]
    fn cone_side_normals_match_bevy_slope() {
        // Bevy's unit test: a 0.5 × 1.0 cone's side normal is perp(0.5, −1) normalised.
        let mesh = Cone {
            resolution: 4,
            ..Cone::default()
        }
        .build();
        let n = mesh.vertices[0].normal;
        let expected = regia_core::math::Vec2::new(0.5, -1.0).perp().normalize();
        assert!((n[0] - expected.x).abs() < 1.0e-6 && (n[1] - expected.y).abs() < 1.0e-6);
    }
}
