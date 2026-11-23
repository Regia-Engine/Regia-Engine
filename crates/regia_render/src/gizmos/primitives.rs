// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_gizmos/src/primitives/{dim3,helpers}.rs. Copyright (c) Bevy contributors,
// dual-licensed MIT OR Apache-2.0; used here under the MIT licence (see docs/ported-sources.toml).
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
//! Wireframe 3D primitives: cuboids, capsules, cylinders, cones, conical frusta, tori,
//! tetrahedra, triangles and planes.
//!
//! Bevy draws these through a `GizmoPrimitive3d<P>` trait over its `bevy_shape` types; Regia Engine
//! has no shape crate, so each primitive is a method taking its dimensions. Every primitive is
//! centred on its isometry with its axis along local +Y.

use std::f32::consts::{FRAC_PI_2, TAU};

use regia_core::math::{Color, Isometry, Quat, UVec2, Vec2, Vec3};

use super::buffer::{GizmoBuffer, IntoIsometry};
use super::config::GizmoConfigGroup;

const DEFAULT_RESOLUTION: u32 = 5;
const DEFAULT_CAPSULE_RESOLUTION: u32 = 4;

fn single_circle_coordinate(radius: f32, resolution: u32, nth_point: u32) -> Vec2 {
    let angle = nth_point as f32 * TAU / resolution.max(1) as f32;
    let (x, y) = angle.sin_cos();
    Vec2::new(x, y) * radius
}

/// `resolution + 1` points around a circle in the XZ plane; the last repeats the first.
fn circle_coordinates_closed(radius: f32, resolution: u32) -> impl Iterator<Item = Vec2> {
    (0..=resolution).map(move |p| single_circle_coordinate(radius, resolution, p))
}

fn ring(radius: f32, resolution: u32, height: f32, iso: Isometry) -> Vec<Vec3> {
    circle_coordinates_closed(radius, resolution)
        .map(|v| iso.transform_point(Vec3::new(v.x, height, v.y)))
        .collect()
}

impl<G: GizmoConfigGroup> GizmoBuffer<G> {
    /// Draws a cuboid with half extents `half_size` (12 edges).
    pub fn cuboid(&mut self, isometry: impl IntoIsometry, half_size: Vec3, color: impl Into<Color>) {
        if !self.enabled {
            return;
        }
        let iso = isometry.into_isometry();
        let vertices @ [a, b, c, d, e, f, g, h] = [
            [1.0, 1.0, 1.0],
            [-1.0, 1.0, 1.0],
            [-1.0, -1.0, 1.0],
            [1.0, -1.0, 1.0],
            [1.0, 1.0, -1.0],
            [-1.0, 1.0, -1.0],
            [-1.0, -1.0, -1.0],
            [1.0, -1.0, -1.0],
        ]
        .map(|v| iso.transform_point(Vec3::from(v) * half_size));
        let color = color.into();
        let upper = [a, b, c, d].into_iter().zip([b, c, d, a]);
        let lower = [e, f, g, h].into_iter().zip([f, g, h, e]);
        let connections = vertices.into_iter().zip(vertices.into_iter().skip(4));
        for (start, end) in upper.chain(lower).chain(connections) {
            self.line(start, end, color);
        }
    }

    /// Draws a triangle (a closed loop through `vertices`, local space).
    pub fn triangle(&mut self, isometry: impl IntoIsometry, vertices: [Vec3; 3], color: impl Into<Color>) {
        if !self.enabled {
            return;
        }
        let iso = isometry.into_isometry();
        self.lineloop(vertices.map(|v| iso.transform_point(v)), color);
    }

    /// Draws the six edges of a tetrahedron (local-space `vertices`).
    pub fn tetrahedron(&mut self, isometry: impl IntoIsometry, vertices: [Vec3; 4], color: impl Into<Color>) {
        if !self.enabled {
            return;
        }
        let iso = isometry.into_isometry();
        let [a, b, c, d] = vertices.map(|v| iso.transform_point(v));
        let color = color.into();
        for (start, end) in [(a, b), (a, c), (a, d), (b, c), (b, d), (c, d)] {
            self.line(start, end, color);
        }
    }

    /// Draws a plane through `isometry` with local normal `normal`: the normal as an arrow plus a
    /// small grid (3×3 cells of 1 m by default).
    pub fn plane(
        &mut self,
        isometry: impl IntoIsometry,
        normal: Vec3,
        color: impl Into<Color>,
    ) -> Plane3dBuilder<'_, G> {
        Plane3dBuilder {
            gizmos: self,
            normal: normal.try_normalize().unwrap_or(Vec3::Y),
            isometry: isometry.into_isometry(),
            color: color.into(),
            cell_count: UVec2::splat(3),
            spacing: Vec2::splat(1.0),
        }
    }

    /// Draws a cylinder of `radius` and `half_height` along local +Y.
    pub fn cylinder(
        &mut self,
        isometry: impl IntoIsometry,
        radius: f32,
        half_height: f32,
        color: impl Into<Color>,
    ) -> ConicalFrustum3dBuilder<'_, G> {
        self.conical_frustum(isometry, radius, radius, half_height * 2.0, color)
    }

    /// Draws a capsule: a cylinder of `half_length` along local +Y with hemispherical caps of
    /// `radius`.
    pub fn capsule(
        &mut self,
        isometry: impl IntoIsometry,
        radius: f32,
        half_length: f32,
        color: impl Into<Color>,
    ) -> Capsule3dBuilder<'_, G> {
        Capsule3dBuilder {
            gizmos: self,
            radius,
            half_length,
            isometry: isometry.into_isometry(),
            color: color.into(),
            resolution: DEFAULT_CAPSULE_RESOLUTION,
        }
    }

    /// Draws a cone of base `radius` and `height`, apex at local `+height/2` on Y.
    pub fn cone(
        &mut self,
        isometry: impl IntoIsometry,
        radius: f32,
        height: f32,
        color: impl Into<Color>,
    ) -> Cone3dBuilder<'_, G> {
        Cone3dBuilder {
            gizmos: self,
            radius,
            height,
            isometry: isometry.into_isometry(),
            color: color.into(),
            base_resolution: DEFAULT_RESOLUTION,
            height_resolution: DEFAULT_RESOLUTION,
        }
    }

    /// Draws a conical frustum (truncated cone) along local +Y.
    pub fn conical_frustum(
        &mut self,
        isometry: impl IntoIsometry,
        radius_top: f32,
        radius_bottom: f32,
        height: f32,
        color: impl Into<Color>,
    ) -> ConicalFrustum3dBuilder<'_, G> {
        ConicalFrustum3dBuilder {
            gizmos: self,
            radius_top,
            radius_bottom,
            height,
            isometry: isometry.into_isometry(),
            color: color.into(),
            resolution: DEFAULT_RESOLUTION,
        }
    }

    /// Draws a torus in the local XZ plane (ring radius `major_radius`, tube `minor_radius`).
    pub fn torus(
        &mut self,
        isometry: impl IntoIsometry,
        minor_radius: f32,
        major_radius: f32,
        color: impl Into<Color>,
    ) -> Torus3dBuilder<'_, G> {
        Torus3dBuilder {
            gizmos: self,
            minor_radius,
            major_radius,
            isometry: isometry.into_isometry(),
            color: color.into(),
            minor_resolution: DEFAULT_RESOLUTION,
            major_resolution: DEFAULT_RESOLUTION,
        }
    }
}

/// Builder of [`GizmoBuffer::plane`]; draws when dropped.
pub struct Plane3dBuilder<'a, G: GizmoConfigGroup> {
    gizmos: &'a mut GizmoBuffer<G>,
    normal: Vec3,
    isometry: Isometry,
    color: Color,
    cell_count: UVec2,
    spacing: Vec2,
}

impl<G: GizmoConfigGroup> Plane3dBuilder<'_, G> {
    /// Number of grid cells along the plane's two axes (default 3×3).
    pub fn cell_count(mut self, cell_count: UVec2) -> Self {
        self.cell_count = cell_count;
        self
    }

    /// Cell size (default 1 m).
    pub fn spacing(mut self, spacing: Vec2) -> Self {
        self.spacing = spacing;
        self
    }
}

impl<G: GizmoConfigGroup> Drop for Plane3dBuilder<'_, G> {
    fn drop(&mut self) {
        if !self.gizmos.enabled {
            return;
        }
        let iso = self.isometry;
        self.gizmos.arrow(
            iso.transform_point(Vec3::ZERO),
            iso.transform_point(self.normal),
            self.color,
        );
        // The grid's own normal is +Z.
        let rotation = Quat::from_rotation_arc(Vec3::Z, self.normal);
        self.gizmos.grid(
            Isometry::new(iso.rotation * rotation, iso.translation),
            self.cell_count,
            self.spacing,
            self.color,
        );
    }
}

/// Builder of [`GizmoBuffer::capsule`]; draws when dropped.
pub struct Capsule3dBuilder<'a, G: GizmoConfigGroup> {
    gizmos: &'a mut GizmoBuffer<G>,
    radius: f32,
    half_length: f32,
    isometry: Isometry,
    color: Color,
    resolution: u32,
}

impl<G: GizmoConfigGroup> Capsule3dBuilder<'_, G> {
    /// Number of meridian lines (default 4).
    pub fn resolution(mut self, resolution: u32) -> Self {
        self.resolution = resolution;
        self
    }
}

impl<G: GizmoConfigGroup> Drop for Capsule3dBuilder<'_, G> {
    fn drop(&mut self) {
        if !self.gizmos.enabled {
            return;
        }
        let iso = self.isometry;
        let [upper_apex, lower_apex] =
            [-1.0, 1.0].map(|sign| iso.transform_point(Vec3::Y * sign * (self.half_length + self.radius)));
        let [upper_center, lower_center] =
            [-1.0, 1.0].map(|sign| iso.transform_point(Vec3::Y * sign * self.half_length));
        let [upper_points, lower_points] =
            [-1.0, 1.0].map(|sign| ring(self.radius, self.resolution, sign * self.half_length, iso));

        for &start in upper_points.iter().skip(1) {
            self.gizmos
                .short_arc_3d_between(upper_center, start, upper_apex, self.color);
        }
        for &start in lower_points.iter().skip(1) {
            self.gizmos
                .short_arc_3d_between(lower_center, start, lower_apex, self.color);
        }
        let circle_rotation = iso.rotation * Quat::from_rotation_x(FRAC_PI_2);
        self.gizmos.circle(
            Isometry::new(circle_rotation, upper_center),
            self.radius,
            self.color,
        );
        self.gizmos.circle(
            Isometry::new(circle_rotation, lower_center),
            self.radius,
            self.color,
        );
        for (start, end) in upper_points.into_iter().zip(lower_points).skip(1) {
            self.gizmos.line(start, end, self.color);
        }
    }
}

/// Builder of [`GizmoBuffer::cone`]; draws when dropped.
pub struct Cone3dBuilder<'a, G: GizmoConfigGroup> {
    gizmos: &'a mut GizmoBuffer<G>,
    radius: f32,
    height: f32,
    isometry: Isometry,
    color: Color,
    base_resolution: u32,
    height_resolution: u32,
}

impl<G: GizmoConfigGroup> Cone3dBuilder<'_, G> {
    /// Sets both the base and the height resolution.
    pub fn resolution(mut self, resolution: u32) -> Self {
        self.base_resolution = resolution;
        self.height_resolution = resolution;
        self
    }

    /// Segments of the base circle (default 5).
    pub fn base_resolution(mut self, resolution: u32) -> Self {
        self.base_resolution = resolution;
        self
    }

    /// Lines from the base to the apex (default 5).
    pub fn height_resolution(mut self, resolution: u32) -> Self {
        self.height_resolution = resolution;
        self
    }
}

impl<G: GizmoConfigGroup> Drop for Cone3dBuilder<'_, G> {
    fn drop(&mut self) {
        if !self.gizmos.enabled {
            return;
        }
        let iso = self.isometry;
        let half_height = self.height * 0.5;
        let apex = iso.transform_point(Vec3::Y * half_height);
        for &start in ring(self.radius, self.height_resolution, -half_height, iso)
            .iter()
            .skip(1)
        {
            self.gizmos.line(start, apex, self.color);
        }
        let base = ring(self.radius, self.base_resolution, -half_height, iso);
        for pair in base.windows(2) {
            self.gizmos.line(pair[0], pair[1], self.color);
        }
    }
}

/// Builder of [`GizmoBuffer::conical_frustum`] and [`GizmoBuffer::cylinder`]; draws when dropped.
pub struct ConicalFrustum3dBuilder<'a, G: GizmoConfigGroup> {
    gizmos: &'a mut GizmoBuffer<G>,
    radius_top: f32,
    radius_bottom: f32,
    height: f32,
    isometry: Isometry,
    color: Color,
    resolution: u32,
}

impl<G: GizmoConfigGroup> ConicalFrustum3dBuilder<'_, G> {
    /// Segments of each end circle, and lines between them (default 5).
    pub fn resolution(mut self, resolution: u32) -> Self {
        self.resolution = resolution;
        self
    }
}

impl<G: GizmoConfigGroup> Drop for ConicalFrustum3dBuilder<'_, G> {
    fn drop(&mut self) {
        if !self.gizmos.enabled {
            return;
        }
        let iso = self.isometry;
        let half_height = self.height * 0.5;
        let lower = ring(self.radius_bottom, self.resolution, -half_height, iso);
        let upper = ring(self.radius_top, self.resolution, half_height, iso);
        for pair in lower.windows(2).chain(upper.windows(2)) {
            self.gizmos.line(pair[0], pair[1], self.color);
        }
        for (start, end) in lower.into_iter().zip(upper).skip(1) {
            self.gizmos.line(start, end, self.color);
        }
    }
}

/// Builder of [`GizmoBuffer::torus`]; draws when dropped.
pub struct Torus3dBuilder<'a, G: GizmoConfigGroup> {
    gizmos: &'a mut GizmoBuffer<G>,
    minor_radius: f32,
    major_radius: f32,
    isometry: Isometry,
    color: Color,
    minor_resolution: u32,
    major_resolution: u32,
}

impl<G: GizmoConfigGroup> Torus3dBuilder<'_, G> {
    /// Segments of each quarter tube arc (default 5).
    pub fn minor_resolution(mut self, resolution: u32) -> Self {
        self.minor_resolution = resolution;
        self
    }

    /// Segments around the ring (default 5).
    pub fn major_resolution(mut self, resolution: u32) -> Self {
        self.major_resolution = resolution;
        self
    }
}

impl<G: GizmoConfigGroup> Drop for Torus3dBuilder<'_, G> {
    fn drop(&mut self) {
        if !self.gizmos.enabled {
            return;
        }
        let iso = self.isometry;
        let (minor, major) = (self.minor_radius, self.major_radius);
        // Four rings: inner, outer, top and bottom of the tube.
        let [inner, outer, top, bottom] = [
            (major - minor, 0.0),
            (major + minor, 0.0),
            (major, minor),
            (major, -minor),
        ]
        .map(|(radius, height)| ring(radius, self.major_resolution, height, iso));
        for points in [&inner, &outer, &top, &bottom] {
            for pair in points.windows(2) {
                self.gizmos.line(pair[0], pair[1], self.color);
            }
        }
        for (((inner, top), outer), bottom) in inner.into_iter().zip(top).zip(outer).zip(bottom) {
            let center = (inner + top + outer + bottom) * 0.25;
            for (from, to) in [(inner, top), (top, outer), (outer, bottom), (bottom, inner)] {
                self.gizmos
                    .short_arc_3d_between(center, from, to, self.color)
                    .resolution(self.minor_resolution);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gizmos::DefaultGizmoConfigGroup;

    type Buffer = GizmoBuffer<DefaultGizmoConfigGroup>;

    fn segment_count(b: &Buffer) -> usize {
        let strip_segments: usize = b
            .strip_positions
            .split(|p| p.is_nan())
            .map(|run| run.len().saturating_sub(1))
            .sum();
        b.list_positions.len() / 2 + strip_segments
    }

    #[test]
    fn cuboid_has_twelve_edges() {
        let mut b = Buffer::new();
        b.cuboid(Vec3::ZERO, Vec3::new(1.0, 2.0, 3.0), Color::WHITE);
        assert_eq!(segment_count(&b), 12);
        assert!(
            b.list_positions
                .iter()
                .all(|p| p.abs() == Vec3::new(1.0, 2.0, 3.0))
        );
    }

    #[test]
    fn capsule_counts_and_extent() {
        let mut b = Buffer::new();
        b.capsule(Vec3::ZERO, 0.5, 1.0, Color::WHITE).resolution(4);
        // 2×4 cap arcs, 2 rings, 4 side lines.
        assert_eq!(b.list_positions.len(), 4 * 2);
        let strips = b
            .strip_positions
            .split(|p| p.is_nan())
            .filter(|r| !r.is_empty())
            .count();
        assert_eq!(strips, 8 + 2);
        let top = b
            .strip_positions
            .iter()
            .filter(|p| !p.is_nan())
            .map(|p| p.y)
            .fold(f32::MIN, f32::max);
        assert!(
            (top - 1.5).abs() < 1e-4,
            "apex at half_length + radius, got {top}"
        );
    }

    #[test]
    fn cylinder_cone_frustum_torus_segment_counts() {
        let mut b = Buffer::new();
        b.cylinder(Vec3::ZERO, 1.0, 1.0, Color::WHITE).resolution(8);
        assert_eq!(segment_count(&b), 8 + 8 + 8);
        let mut c = Buffer::new();
        c.cone(Vec3::ZERO, 1.0, 2.0, Color::WHITE).resolution(6);
        assert_eq!(segment_count(&c), 6 + 6);
        let mut t = Buffer::new();
        t.torus(Vec3::ZERO, 0.25, 1.0, Color::WHITE)
            .major_resolution(6)
            .minor_resolution(3);
        // 4 rings of 6 segments + (6 + 1) cross-sections × 4 arcs × 3 segments.
        assert_eq!(segment_count(&t), 4 * 6 + 7 * 4 * 3);
    }

    #[test]
    fn triangle_and_tetrahedron_close() {
        let mut b = Buffer::new();
        b.triangle(Vec3::ZERO, [Vec3::ZERO, Vec3::X, Vec3::Y], Color::WHITE);
        // A closed loop: the first point comes back.
        assert_eq!(b.strip_positions[3], Vec3::ZERO);
        let mut t = Buffer::new();
        t.tetrahedron(Vec3::ZERO, [Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z], Color::WHITE);
        assert_eq!(segment_count(&t), 6);
    }

    #[test]
    fn plane_draws_normal_arrow_and_grid() {
        let mut b = Buffer::new();
        b.plane(Vec3::ZERO, Vec3::Y, Color::WHITE);
        // Arrow (5 lines) + 3×3 grid without outer edges (2 + 2 lines).
        assert_eq!(b.list_positions.len() / 2, 5 + 4);
    }
}
