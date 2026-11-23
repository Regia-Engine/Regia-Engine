// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_gizmos/src/circles.rs. Copyright (c) Bevy contributors, dual-licensed
// MIT OR Apache-2.0; used here under the MIT licence (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//! Ellipses, circles and spheres.

use std::f32::consts::TAU;

use regia_core::math::{Color, Isometry, Quat, Vec2, Vec3};

use super::buffer::{GizmoBuffer, IntoIsometry};
use super::config::GizmoConfigGroup;

/// Default number of segments of a full circle.
pub const DEFAULT_CIRCLE_RESOLUTION: u32 = 32;

fn ellipse_inner(half_size: Vec2, resolution: u32) -> impl Iterator<Item = Vec2> {
    let resolution = resolution.max(1);
    (0..resolution + 1).map(move |i| {
        let angle = i as f32 * TAU / resolution as f32;
        let (x, y) = angle.sin_cos();
        Vec2::new(x, y) * half_size
    })
}

impl<G: GizmoConfigGroup> GizmoBuffer<G> {
    /// Draws an ellipse in the local XY plane of `isometry`, with the given half-axes.
    #[inline]
    pub fn ellipse(
        &mut self,
        isometry: impl IntoIsometry,
        half_size: Vec2,
        color: impl Into<Color>,
    ) -> EllipseBuilder<'_, G> {
        EllipseBuilder {
            gizmos: self,
            isometry: isometry.into_isometry(),
            half_size,
            color: color.into(),
            resolution: DEFAULT_CIRCLE_RESOLUTION,
        }
    }

    /// Draws a circle of `radius` in the local XY plane of `isometry` (normal = local +Z).
    #[inline]
    pub fn circle(
        &mut self,
        isometry: impl IntoIsometry,
        radius: f32,
        color: impl Into<Color>,
    ) -> EllipseBuilder<'_, G> {
        self.ellipse(isometry, Vec2::splat(radius), color)
    }

    /// Draws a wire sphere: one great circle around each local axis.
    #[inline]
    pub fn sphere(
        &mut self,
        isometry: impl IntoIsometry,
        radius: f32,
        color: impl Into<Color>,
    ) -> SphereBuilder<'_, G> {
        SphereBuilder {
            gizmos: self,
            radius,
            isometry: isometry.into_isometry(),
            color: color.into(),
            resolution: DEFAULT_CIRCLE_RESOLUTION,
        }
    }
}

/// Builder of [`GizmoBuffer::ellipse`] / [`GizmoBuffer::circle`]; draws when dropped.
pub struct EllipseBuilder<'a, G: GizmoConfigGroup> {
    gizmos: &'a mut GizmoBuffer<G>,
    isometry: Isometry,
    half_size: Vec2,
    color: Color,
    resolution: u32,
}

impl<G: GizmoConfigGroup> EllipseBuilder<'_, G> {
    /// Number of line segments (default 32).
    pub fn resolution(mut self, resolution: u32) -> Self {
        self.resolution = resolution;
        self
    }
}

impl<G: GizmoConfigGroup> Drop for EllipseBuilder<'_, G> {
    fn drop(&mut self) {
        if !self.gizmos.enabled {
            return;
        }
        let isometry = self.isometry;
        let positions =
            ellipse_inner(self.half_size, self.resolution).map(|v| isometry.transform_point(v.extend(0.0)));
        self.gizmos.linestrip(positions, self.color);
    }
}

/// Builder of [`GizmoBuffer::sphere`]; draws when dropped.
pub struct SphereBuilder<'a, G: GizmoConfigGroup> {
    gizmos: &'a mut GizmoBuffer<G>,
    radius: f32,
    isometry: Isometry,
    color: Color,
    resolution: u32,
}

impl<G: GizmoConfigGroup> SphereBuilder<'_, G> {
    /// Segments per great circle (default 32).
    pub fn resolution(mut self, resolution: u32) -> Self {
        self.resolution = resolution;
        self
    }
}

impl<G: GizmoConfigGroup> Drop for SphereBuilder<'_, G> {
    fn drop(&mut self) {
        if !self.gizmos.enabled {
            return;
        }
        for axis in [Vec3::X, Vec3::Y, Vec3::Z] {
            let axis_rotation = Isometry::from_rotation(Quat::from_rotation_arc(Vec3::Z, axis));
            self.gizmos
                .circle(self.isometry * axis_rotation, self.radius, self.color)
                .resolution(self.resolution);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gizmos::DefaultGizmoConfigGroup;

    #[test]
    fn circle_is_a_closed_strip_of_resolution_segments() {
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        b.circle(Vec3::ZERO, 2.0, Color::WHITE).resolution(16);
        // 17 points (the last repeats the first) + separator.
        assert_eq!(b.strip_positions.len(), 18);
        assert!(b.strip_positions[0].abs_diff_eq(b.strip_positions[16], 1e-5));
        assert!(
            b.strip_positions[..17]
                .iter()
                .all(|p| (p.length() - 2.0).abs() < 1e-4 && p.z == 0.0)
        );
    }

    #[test]
    fn sphere_draws_three_great_circles() {
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        b.sphere(Vec3::new(1.0, 2.0, 3.0), 0.5, Color::WHITE)
            .resolution(8);
        assert_eq!(b.strip_positions.len(), 3 * (9 + 1));
        let centre = Vec3::new(1.0, 2.0, 3.0);
        assert!(
            b.strip_positions
                .iter()
                .filter(|p| !p.is_nan())
                .all(|p| ((*p - centre).length() - 0.5).abs() < 1e-4)
        );
    }
}
