// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_gizmos/src/rounded_box.rs. Copyright (c) Bevy contributors, dual-licensed
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
//! Rectangles and cuboids with rounded corners and edges.

use std::f32::consts::FRAC_PI_2;

use regia_core::math::{Color, Isometry, Mat4, Quat, Vec2, Vec3};

use super::buffer::{GizmoBuffer, IntoIsometry};
use super::config::GizmoConfigGroup;

const DEFAULT_ARC_RESOLUTION: u32 = 8;
const DEFAULT_CORNER_RADIUS: f32 = 0.1;

struct RoundedBoxConfig {
    isometry: Isometry,
    color: Color,
    corner_radius: f32,
    arc_resolution: u32,
}

/// Builder of [`GizmoBuffer::rounded_rect`]; draws when dropped.
pub struct RoundedRectBuilder<'a, G: GizmoConfigGroup> {
    size: Vec2,
    gizmos: &'a mut GizmoBuffer<G>,
    config: RoundedBoxConfig,
}

/// Builder of [`GizmoBuffer::rounded_cuboid`]; draws when dropped.
pub struct RoundedCuboidBuilder<'a, G: GizmoConfigGroup> {
    size: Vec3,
    gizmos: &'a mut GizmoBuffer<G>,
    config: RoundedBoxConfig,
}

impl<G: GizmoConfigGroup> RoundedRectBuilder<'_, G> {
    /// Corner radius (default: a tenth of the shorter side). Negative values curve inwards.
    pub fn corner_radius(mut self, corner_radius: f32) -> Self {
        self.config.corner_radius = corner_radius;
        self
    }

    /// Segments per corner arc (default 8).
    pub fn arc_resolution(mut self, arc_resolution: u32) -> Self {
        self.config.arc_resolution = arc_resolution;
        self
    }
}

impl<G: GizmoConfigGroup> RoundedCuboidBuilder<'_, G> {
    /// Edge radius (default: a tenth of the shortest side). Negative values curve inwards.
    pub fn edge_radius(mut self, edge_radius: f32) -> Self {
        self.config.corner_radius = edge_radius;
        self
    }

    /// Segments per edge arc (default 8).
    pub fn arc_resolution(mut self, arc_resolution: u32) -> Self {
        self.config.arc_resolution = arc_resolution;
        self
    }
}

impl<G: GizmoConfigGroup> Drop for RoundedRectBuilder<'_, G> {
    fn drop(&mut self) {
        if !self.gizmos.enabled {
            return;
        }
        let config = &self.config;
        let iso = config.isometry;
        // Inner and outer half sizes; the radius never exceeds a half side.
        let mut outer_half_size = self.size.abs() / 2.0;
        let inner_half_size = (outer_half_size - Vec2::splat(config.corner_radius.abs())).max(Vec2::ZERO);
        let corner_radius = (outer_half_size - inner_half_size).min_element();
        let mut inner_half_size = outer_half_size - Vec2::splat(corner_radius);
        if config.corner_radius < 0.0 {
            std::mem::swap(&mut outer_half_size, &mut inner_half_size);
        }

        // Collapsed rectangles are a line or a plain rectangle.
        if outer_half_size.x * outer_half_size.y == 0.0 {
            self.gizmos.line(
                iso.transform_point(-outer_half_size.extend(0.0)),
                iso.transform_point(outer_half_size.extend(0.0)),
                config.color,
            );
            return;
        }
        if corner_radius == 0.0 {
            self.gizmos.rect(iso, self.size, config.color);
            return;
        }

        let vertices = [
            // top right
            Vec3::new(inner_half_size.x, outer_half_size.y, 0.0),
            Vec3::new(inner_half_size.x, inner_half_size.y, 0.0),
            Vec3::new(outer_half_size.x, inner_half_size.y, 0.0),
            // bottom right
            Vec3::new(outer_half_size.x, -inner_half_size.y, 0.0),
            Vec3::new(inner_half_size.x, -inner_half_size.y, 0.0),
            Vec3::new(inner_half_size.x, -outer_half_size.y, 0.0),
            // bottom left
            Vec3::new(-inner_half_size.x, -outer_half_size.y, 0.0),
            Vec3::new(-inner_half_size.x, -inner_half_size.y, 0.0),
            Vec3::new(-outer_half_size.x, -inner_half_size.y, 0.0),
            // top left
            Vec3::new(-outer_half_size.x, inner_half_size.y, 0.0),
            Vec3::new(-inner_half_size.x, inner_half_size.y, 0.0),
            Vec3::new(-inner_half_size.x, outer_half_size.y, 0.0),
        ]
        .map(|v| iso.transform_point(v));

        let (color, arc_resolution, positive) =
            (config.color, config.arc_resolution, config.corner_radius > 0.0);
        for corner in vertices.as_chunks::<3>().0 {
            self.gizmos
                .short_arc_3d_between(corner[1], corner[0], corner[2], color)
                .resolution(arc_resolution);
        }
        let edges = if positive {
            [
                (vertices[2], vertices[3]),
                (vertices[5], vertices[6]),
                (vertices[8], vertices[9]),
                (vertices[11], vertices[0]),
            ]
        } else {
            [
                (vertices[0], vertices[5]),
                (vertices[3], vertices[8]),
                (vertices[6], vertices[11]),
                (vertices[9], vertices[2]),
            ]
        };
        for (start, end) in edges {
            self.gizmos.line(start, end, color);
        }
    }
}

impl<G: GizmoConfigGroup> Drop for RoundedCuboidBuilder<'_, G> {
    fn drop(&mut self) {
        if !self.gizmos.enabled {
            return;
        }
        let config = &self.config;
        let outer_half_size = self.size.abs() / 2.0;
        let inner_half_size = (outer_half_size - Vec3::splat(config.corner_radius.abs())).max(Vec3::ZERO);
        let mut edge_radius = (outer_half_size - inner_half_size).min_element();
        let inner_half_size = outer_half_size - Vec3::splat(edge_radius);
        edge_radius *= config.corner_radius.signum();

        if edge_radius == 0.0 {
            let transform = Mat4::from_scale_rotation_translation(
                self.size,
                config.isometry.rotation,
                config.isometry.translation,
            );
            self.gizmos.cube(transform, config.color);
            return;
        }

        let rects = [
            (
                Vec3::X,
                Vec2::new(self.size.z, self.size.y),
                Quat::from_rotation_y(FRAC_PI_2),
            ),
            (
                Vec3::Y,
                Vec2::new(self.size.x, self.size.z),
                Quat::from_rotation_x(FRAC_PI_2),
            ),
            (Vec3::Z, Vec2::new(self.size.x, self.size.y), Quat::IDENTITY),
        ];
        let (iso, color, arc_resolution) = (config.isometry, config.color, config.arc_resolution);
        for (position, size, rotation) in rects {
            let local_position = position * inner_half_size;
            for offset in [local_position, -local_position] {
                self.gizmos
                    .rounded_rect(iso * Isometry::new(rotation, offset), size, color)
                    .arc_resolution(arc_resolution)
                    .corner_radius(edge_radius);
            }
        }
    }
}

impl<G: GizmoConfigGroup> GizmoBuffer<G> {
    /// Draws a rectangle with rounded corners in the local XY plane of `isometry`.
    pub fn rounded_rect(
        &mut self,
        isometry: impl IntoIsometry,
        size: Vec2,
        color: impl Into<Color>,
    ) -> RoundedRectBuilder<'_, G> {
        let corner_radius = size.min_element() * DEFAULT_CORNER_RADIUS;
        RoundedRectBuilder {
            gizmos: self,
            config: RoundedBoxConfig {
                isometry: isometry.into_isometry(),
                color: color.into(),
                corner_radius,
                arc_resolution: DEFAULT_ARC_RESOLUTION,
            },
            size,
        }
    }

    /// Draws a cuboid of `size` with rounded edges, placed by `isometry`.
    pub fn rounded_cuboid(
        &mut self,
        isometry: impl IntoIsometry,
        size: Vec3,
        color: impl Into<Color>,
    ) -> RoundedCuboidBuilder<'_, G> {
        let corner_radius = size.min_element() * DEFAULT_CORNER_RADIUS;
        RoundedCuboidBuilder {
            gizmos: self,
            config: RoundedBoxConfig {
                isometry: isometry.into_isometry(),
                color: color.into(),
                corner_radius,
                arc_resolution: DEFAULT_ARC_RESOLUTION,
            },
            size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gizmos::DefaultGizmoConfigGroup;

    #[test]
    fn rounded_rect_is_four_arcs_and_four_edges() {
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        b.rounded_rect(Vec3::ZERO, Vec2::new(4.0, 2.0), Color::WHITE)
            .corner_radius(0.5)
            .arc_resolution(4);
        assert_eq!(b.list_positions.len(), 4 * 2);
        // Four arcs of 4 segments: 5 points + separator each.
        assert_eq!(b.strip_positions.len(), 4 * 6);
        let finite = b
            .strip_positions
            .iter()
            .chain(&b.list_positions)
            .filter(|p| !p.is_nan());
        assert!(
            finite
                .into_iter()
                .all(|p| p.x.abs() <= 2.0 + 1e-5 && p.y.abs() <= 1.0 + 1e-5)
        );
    }

    #[test]
    fn zero_radius_cuboid_is_a_cube() {
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        b.rounded_cuboid(Vec3::ZERO, Vec3::new(2.0, 1.0, 1.0), Color::WHITE)
            .edge_radius(0.0);
        assert_eq!(b.strip_positions.len(), 11);
        assert_eq!(b.list_positions.len(), 6);
        let mut r = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        r.rounded_cuboid(Vec3::ZERO, Vec3::ONE, Color::WHITE);
        // Six rounded faces of four edges each.
        assert_eq!(r.list_positions.len(), 6 * 4 * 2);
    }
}
