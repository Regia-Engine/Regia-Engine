// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_gizmos/src/{arrows,cross}.rs. Copyright (c) Bevy contributors, dual-licensed
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
//! Arrows, axes triads and crosses.

use regia_core::math::{Color, Quat, Vec3};

use super::buffer::{GizmoBuffer, IntoIsometry, TransformPoint};
use super::config::GizmoConfigGroup;

/// Builder of [`GizmoBuffer::arrow`]; draws when dropped.
pub struct ArrowBuilder<'a, G: GizmoConfigGroup> {
    gizmos: &'a mut GizmoBuffer<G>,
    start: Vec3,
    end: Vec3,
    color: Color,
    double_ended: bool,
    tip_length: f32,
}

impl<G: GizmoConfigGroup> ArrowBuilder<'_, G> {
    /// Length of the head's four tip lines (default: a tenth of the arrow).
    pub fn with_tip_length(mut self, length: f32) -> Self {
        self.tip_length = length;
        self
    }

    /// Adds a head at the start too.
    pub fn with_double_end(mut self) -> Self {
        self.double_ended = true;
        self
    }
}

const TIPS: [Vec3; 4] = [
    Vec3::new(-1.0, 1.0, 0.0),
    Vec3::new(-1.0, 0.0, 1.0),
    Vec3::new(-1.0, -1.0, 0.0),
    Vec3::new(-1.0, 0.0, -1.0),
];

impl<G: GizmoConfigGroup> Drop for ArrowBuilder<'_, G> {
    fn drop(&mut self) {
        if !self.gizmos.enabled {
            return;
        }
        if self.double_ended {
            // Two halves animate better for dashed styles.
            let center = (self.start + self.end) / 2.0;
            self.gizmos.line(center, self.start, self.color);
            self.gizmos.line(center, self.end, self.color);
        } else {
            self.gizmos.line(self.start, self.end, self.color);
        }
        let Some(pointing_end) = (self.end - self.start).try_normalize() else {
            return;
        };
        // Frame where the arrow points along +X and ends at the origin, then into place.
        let rotation_end = Quat::from_rotation_arc(Vec3::X, pointing_end);
        for tip in TIPS {
            let v = rotation_end * (tip.normalize() * self.tip_length) + self.end;
            self.gizmos.line(self.end, v, self.color);
        }
        if self.double_ended {
            let rotation_start = Quat::from_rotation_arc(Vec3::X, -pointing_end);
            for tip in TIPS {
                let v = rotation_start * (tip.normalize() * self.tip_length) + self.start;
                self.gizmos.line(self.start, v, self.color);
            }
        }
    }
}

impl<G: GizmoConfigGroup> GizmoBuffer<G> {
    /// Draws an arrow from `start` to `end` with a four-line head.
    pub fn arrow(&mut self, start: Vec3, end: Vec3, color: impl Into<Color>) -> ArrowBuilder<'_, G> {
        let length = (end - start).length();
        ArrowBuilder {
            gizmos: self,
            start,
            end,
            color: color.into(),
            double_ended: false,
            tip_length: length / 10.0,
        }
    }

    /// Draws the local X (red), Y (green) and Z (blue) axes of `transform` as arrows of
    /// `base_length` (scaled by the transform).
    pub fn axes(&mut self, transform: impl TransformPoint, base_length: f32) {
        let start = transform.transform_point(Vec3::ZERO);
        let end_x = transform.transform_point(base_length * Vec3::X);
        let end_y = transform.transform_point(base_length * Vec3::Y);
        let end_z = transform.transform_point(base_length * Vec3::Z);
        self.arrow(start, end_x, Color::RED);
        self.arrow(start, end_y, Color::GREEN);
        self.arrow(start, end_z, Color::BLUE);
    }

    /// Draws three axis-aligned lines of length `2 × half_size` crossing at `isometry`.
    pub fn cross(&mut self, isometry: impl IntoIsometry, half_size: f32, color: impl Into<Color>) {
        let isometry = isometry.into_isometry();
        let color = color.into();
        for axis in [Vec3::X, Vec3::Y, Vec3::Z] {
            let axis = axis * half_size;
            self.line(
                isometry.transform_point(axis),
                isometry.transform_point(-axis),
                color,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gizmos::DefaultGizmoConfigGroup;

    #[test]
    fn arrow_is_a_shaft_and_four_tips() {
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        b.arrow(Vec3::ZERO, Vec3::new(0.0, 0.0, -10.0), Color::WHITE);
        assert_eq!(b.list_positions.len(), 2 * 5);
        // Tips point back from the end, a tenth of the length long.
        for pair in b.list_positions[2..].chunks(2) {
            assert!(pair[0].abs_diff_eq(Vec3::new(0.0, 0.0, -10.0), 1e-5));
            assert!((pair[1].distance(pair[0]) - 1.0).abs() < 1e-4);
            assert!(pair[1].z > -10.0);
        }
        let mut d = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        d.arrow(Vec3::ZERO, Vec3::X, Color::WHITE).with_double_end();
        assert_eq!(d.list_positions.len(), 2 * (2 + 8));
    }

    #[test]
    fn zero_length_arrow_draws_only_the_shaft() {
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        b.arrow(Vec3::ONE, Vec3::ONE, Color::WHITE);
        assert_eq!(b.list_positions.len(), 2);
        assert!(b.list_positions.iter().all(|p| p.is_finite()));
    }

    #[test]
    fn axes_and_cross() {
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        b.axes(regia_core::math::Mat4::IDENTITY, 1.0);
        assert_eq!(b.list_positions.len(), 3 * 10);
        assert_eq!(b.list_colors[0], Color::RED);
        let mut c = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        c.cross(Vec3::ZERO, 0.5, Color::WHITE);
        assert_eq!(c.list_positions.len(), 6);
    }
}
