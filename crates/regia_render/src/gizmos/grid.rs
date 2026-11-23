// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_gizmos/src/grid.rs. Copyright (c) Bevy contributors, dual-licensed
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
//! Planar and volumetric line grids.

use regia_core::math::{Color, Isometry, UVec2, UVec3, Vec2, Vec3};

use super::buffer::{GizmoBuffer, IntoIsometry};
use super::config::GizmoConfigGroup;

/// Builder of [`GizmoBuffer::grid_3d`]; draws when dropped.
pub struct GridBuilder3d<'a, G: GizmoConfigGroup> {
    gizmos: &'a mut GizmoBuffer<G>,
    isometry: Isometry,
    spacing: Vec3,
    cell_count: UVec3,
    skew: Vec3,
    outer_edges: [bool; 3],
    color: Color,
}

/// Builder of [`GizmoBuffer::grid`]; draws when dropped.
pub struct GridBuilder2d<'a, G: GizmoConfigGroup> {
    gizmos: &'a mut GizmoBuffer<G>,
    isometry: Isometry,
    spacing: Vec2,
    cell_count: UVec2,
    skew: Vec2,
    outer_edges: [bool; 2],
    color: Color,
}

impl<G: GizmoConfigGroup> GridBuilder3d<'_, G> {
    /// Skew of the grid along each axis, radians.
    pub fn skew(mut self, skew: Vec3) -> Self {
        self.skew = skew;
        self
    }

    /// Also draw the outer edges along every axis.
    pub fn outer_edges(mut self) -> Self {
        self.outer_edges.fill(true);
        self
    }

    /// Also draw the outer edges along X.
    pub fn outer_edges_x(mut self) -> Self {
        self.outer_edges[0] = true;
        self
    }

    /// Also draw the outer edges along Y.
    pub fn outer_edges_y(mut self) -> Self {
        self.outer_edges[1] = true;
        self
    }

    /// Also draw the outer edges along Z.
    pub fn outer_edges_z(mut self) -> Self {
        self.outer_edges[2] = true;
        self
    }
}

impl<G: GizmoConfigGroup> GridBuilder2d<'_, G> {
    /// Skew of the grid along X and Y, radians.
    pub fn skew(mut self, skew: Vec2) -> Self {
        self.skew = skew;
        self
    }

    /// Also draw the outer edges.
    pub fn outer_edges(mut self) -> Self {
        self.outer_edges.fill(true);
        self
    }

    /// Also draw the outer edges along X.
    pub fn outer_edges_x(mut self) -> Self {
        self.outer_edges[0] = true;
        self
    }

    /// Also draw the outer edges along Y.
    pub fn outer_edges_y(mut self) -> Self {
        self.outer_edges[1] = true;
        self
    }
}

impl<G: GizmoConfigGroup> Drop for GridBuilder3d<'_, G> {
    fn drop(&mut self) {
        draw_grid(
            self.gizmos,
            self.isometry,
            self.spacing,
            self.cell_count,
            self.skew,
            self.outer_edges,
            self.color,
        );
    }
}

impl<G: GizmoConfigGroup> Drop for GridBuilder2d<'_, G> {
    fn drop(&mut self) {
        draw_grid(
            self.gizmos,
            self.isometry,
            self.spacing.extend(0.0),
            self.cell_count.extend(0),
            self.skew.extend(0.0),
            [self.outer_edges[0], self.outer_edges[1], true],
            self.color,
        );
    }
}

impl<G: GizmoConfigGroup> GizmoBuffer<G> {
    /// Draws a grid of `cell_count` cells of `spacing` in the local XY plane of `isometry`,
    /// centred on it. Rotate by `Quat::from_rotation_x(-FRAC_PI_2)` for a ground grid.
    pub fn grid(
        &mut self,
        isometry: impl IntoIsometry,
        cell_count: UVec2,
        spacing: Vec2,
        color: impl Into<Color>,
    ) -> GridBuilder2d<'_, G> {
        GridBuilder2d {
            gizmos: self,
            isometry: isometry.into_isometry(),
            spacing,
            cell_count,
            skew: Vec2::ZERO,
            outer_edges: [false, false],
            color: color.into(),
        }
    }

    /// Draws a 3D lattice of `cell_count` cells of `spacing`, centred on `isometry`.
    pub fn grid_3d(
        &mut self,
        isometry: impl IntoIsometry,
        cell_count: UVec3,
        spacing: Vec3,
        color: impl Into<Color>,
    ) -> GridBuilder3d<'_, G> {
        GridBuilder3d {
            gizmos: self,
            isometry: isometry.into_isometry(),
            spacing,
            cell_count,
            skew: Vec3::ZERO,
            outer_edges: [false, false, false],
            color: color.into(),
        }
    }
}

fn draw_grid<G: GizmoConfigGroup>(
    gizmos: &mut GizmoBuffer<G>,
    isometry: Isometry,
    spacing: Vec3,
    cell_count: UVec3,
    skew: Vec3,
    outer_edges: [bool; 3],
    color: Color,
) {
    if !gizmos.enabled {
        return;
    }

    #[inline]
    fn or_zero(cond: bool, val: Vec3) -> Vec3 {
        if cond { val } else { Vec3::ZERO }
    }

    // Offset between adjacent cells along each axis, skew included.
    let skew_tan = Vec3::from(skew.to_array().map(f32::tan));
    let dx = or_zero(
        cell_count.x != 0,
        spacing.x * Vec3::new(1.0, skew_tan.y, skew_tan.z),
    );
    let dy = or_zero(
        cell_count.y != 0,
        spacing.y * Vec3::new(skew_tan.x, 1.0, skew_tan.z),
    );
    let dz = or_zero(
        cell_count.z != 0,
        spacing.z * Vec3::new(skew_tan.x, skew_tan.y, 1.0),
    );

    // Bottom-left-front corner.
    let half = cell_count.as_vec3() * 0.5;
    let grid_start = -half.x * dx - half.y * dy - half.z * dz;

    #[inline]
    fn cell_count_to_line_count(include_outer: bool, cell_count: u32) -> u32 {
        if include_outer {
            cell_count.saturating_add(1)
        } else {
            cell_count.saturating_sub(1).max(1)
        }
    }

    let x_line_count = UVec2::new(
        cell_count_to_line_count(outer_edges[0], cell_count.y),
        cell_count_to_line_count(outer_edges[0], cell_count.z),
    );
    let y_line_count = UVec2::new(
        cell_count_to_line_count(outer_edges[1], cell_count.z),
        cell_count_to_line_count(outer_edges[1], cell_count.x),
    );
    let z_line_count = UVec2::new(
        cell_count_to_line_count(outer_edges[2], cell_count.x),
        cell_count_to_line_count(outer_edges[2], cell_count.y),
    );

    let x_start = grid_start + or_zero(!outer_edges[0], dy + dz);
    let y_start = grid_start + or_zero(!outer_edges[1], dx + dz);
    let z_start = grid_start + or_zero(!outer_edges[2], dx + dy);

    fn iter_lines(
        delta_a: Vec3,
        delta_b: Vec3,
        delta_c: Vec3,
        line_count: UVec2,
        cell_count: u32,
        start: Vec3,
    ) -> impl Iterator<Item = [Vec3; 2]> {
        let dline = delta_a * cell_count as f32;
        // No cells along this axis: its lines would have zero length (Bevy draws them anyway).
        let line_count = if cell_count == 0 { UVec2::ZERO } else { line_count };
        (0..line_count.x).map(|v| v as f32).flat_map(move |b| {
            (0..line_count.y).map(|v| v as f32).map(move |c| {
                let line_start = start + b * delta_b + c * delta_c;
                [line_start, line_start + dline]
            })
        })
    }

    let x_lines = iter_lines(dx, dy, dz, x_line_count, cell_count.x, x_start);
    let y_lines = iter_lines(dy, dz, dx, y_line_count, cell_count.y, y_start);
    let z_lines = iter_lines(dz, dx, dy, z_line_count, cell_count.z, z_start);
    for [start, end] in x_lines.chain(y_lines).chain(z_lines) {
        gizmos.line(
            isometry.transform_point(start),
            isometry.transform_point(end),
            color,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gizmos::DefaultGizmoConfigGroup;

    fn segments(b: &GizmoBuffer<DefaultGizmoConfigGroup>) -> Vec<[Vec3; 2]> {
        b.list_positions.chunks(2).map(|p| [p[0], p[1]]).collect()
    }

    #[test]
    fn grid_with_outer_edges_has_n_plus_one_lines_per_axis() {
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        b.grid(Vec3::ZERO, UVec2::new(4, 2), Vec2::splat(1.0), Color::WHITE)
            .outer_edges();
        let lines = segments(&b);
        // Lines along X: 3 (y cells + 1); along Y: 5; none along the flat Z axis.
        assert_eq!(lines.len(), 3 + 5);
        assert!(lines.iter().all(|[a, b]| a.distance(*b) > 0.0));
        assert!(
            lines
                .iter()
                .flatten()
                .all(|p| p.x.abs() <= 2.0 + 1e-6 && p.y.abs() <= 1.0 + 1e-6)
        );
    }

    #[test]
    fn grid_without_outer_edges_skips_the_border() {
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        b.grid(Vec3::ZERO, UVec2::new(4, 2), Vec2::splat(1.0), Color::WHITE);
        assert_eq!(segments(&b).len(), 1 + 3);
    }
}
