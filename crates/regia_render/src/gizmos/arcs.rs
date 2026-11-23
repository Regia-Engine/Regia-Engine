// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_gizmos/src/arcs.rs. Copyright (c) Bevy contributors, dual-licensed
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
//! Circular arcs in 3D.

use std::f32::consts::TAU;

use regia_core::math::{Color, Isometry, Quat, Vec3};

use super::buffer::{GizmoBuffer, IntoIsometry};
use super::circles::DEFAULT_CIRCLE_RESOLUTION;
use super::config::GizmoConfigGroup;

impl<G: GizmoConfigGroup> GizmoBuffer<G> {
    /// Draws an arc of `angle` radians and `radius` in the local XZ plane of `isometry`, starting
    /// at local +X and turning about local +Y.
    #[inline]
    pub fn arc_3d(
        &mut self,
        angle: f32,
        radius: f32,
        isometry: impl IntoIsometry,
        color: impl Into<Color>,
    ) -> Arc3dBuilder<'_, G> {
        Arc3dBuilder {
            gizmos: self,
            start_vertex: Vec3::X,
            isometry: isometry.into_isometry(),
            angle,
            radius,
            color: color.into(),
            resolution: None,
        }
    }

    /// Draws the shorter arc around `center` from `from` to `to` (radius = `|from - center|`).
    #[inline]
    pub fn short_arc_3d_between(
        &mut self,
        center: Vec3,
        from: Vec3,
        to: Vec3,
        color: impl Into<Color>,
    ) -> Arc3dBuilder<'_, G> {
        self.arc_from_to(center, from, to, color, |angle| angle)
    }

    /// Draws the longer arc around `center` from `from` to `to`.
    #[inline]
    pub fn long_arc_3d_between(
        &mut self,
        center: Vec3,
        from: Vec3,
        to: Vec3,
        color: impl Into<Color>,
    ) -> Arc3dBuilder<'_, G> {
        // The other way round: the complement, turning in the opposite direction. (Bevy at
        // fd98063 returns `TAU - angle` with the same sign, which ends the arc at the mirror image
        // of `to`; the unit test below pins the corrected end point.)
        self.arc_from_to(center, from, to, color, |angle| {
            if angle > 0.0 {
                angle - TAU
            } else if angle < 0.0 {
                angle + TAU
            } else {
                0.0
            }
        })
    }

    #[inline]
    fn arc_from_to(
        &mut self,
        center: Vec3,
        from: Vec3,
        to: Vec3,
        color: impl Into<Color>,
        angle_fn: impl Fn(f32) -> f32,
    ) -> Arc3dBuilder<'_, G> {
        // `from` and `to` may coincide: nothing visible is drawn and the axis ambiguity is moot.
        let from_axis = (from - center).normalize_or_zero();
        let to_axis = (to - center).normalize_or_zero();
        let (up, angle) = Quat::from_rotation_arc(from_axis, to_axis).to_axis_angle();
        let angle = angle_fn(angle);
        let radius = center.distance(from);
        let rotation = Quat::from_rotation_arc(Vec3::Y, up);
        let start_vertex = rotation.inverse() * from_axis;
        Arc3dBuilder {
            gizmos: self,
            start_vertex,
            isometry: Isometry::new(rotation, center),
            angle,
            radius,
            color: color.into(),
            resolution: None,
        }
    }
}

/// Builder of the 3D arc calls; draws when dropped.
pub struct Arc3dBuilder<'a, G: GizmoConfigGroup> {
    gizmos: &'a mut GizmoBuffer<G>,
    // Where the arc starts, in the local XZ plane, unit length (an internal invariant).
    start_vertex: Vec3,
    isometry: Isometry,
    angle: f32,
    radius: f32,
    color: Color,
    resolution: Option<u32>,
}

impl<G: GizmoConfigGroup> Arc3dBuilder<'_, G> {
    /// Number of segments (default: 32 per full turn, rounded up).
    pub fn resolution(mut self, resolution: u32) -> Self {
        self.resolution = Some(resolution);
        self
    }
}

impl<G: GizmoConfigGroup> Drop for Arc3dBuilder<'_, G> {
    fn drop(&mut self) {
        if !self.gizmos.enabled {
            return;
        }
        let resolution = self
            .resolution
            .unwrap_or_else(|| resolution_from_angle(self.angle))
            .max(1);
        let positions = arc_3d_inner(
            self.start_vertex,
            self.isometry,
            self.angle,
            self.radius,
            resolution,
        );
        self.gizmos.linestrip(positions, self.color);
    }
}

fn arc_3d_inner(
    start_vertex: Vec3,
    isometry: Isometry,
    angle: f32,
    radius: f32,
    resolution: u32,
) -> impl Iterator<Item = Vec3> {
    // Past a full turn the overlap is invisible and only costs detail.
    let angle = angle.clamp(-TAU, TAU);
    (0..=resolution).map(move |step| {
        let frac_angle = angle * step as f32 / resolution as f32;
        isometry.transform_point(Quat::from_axis_angle(Vec3::Y, frac_angle) * start_vertex * radius)
    })
}

/// Default resolution of an arc: [`DEFAULT_CIRCLE_RESOLUTION`] per full turn.
pub(crate) fn resolution_from_angle(angle: f32) -> u32 {
    ((angle.abs() / TAU) * DEFAULT_CIRCLE_RESOLUTION as f32).ceil() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gizmos::DefaultGizmoConfigGroup;

    #[test]
    fn quarter_arc_has_eight_segments_and_ends_on_target() {
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        b.short_arc_3d_between(Vec3::ZERO, Vec3::X, Vec3::Z, Color::WHITE);
        assert_eq!(b.strip_positions.len(), 8 + 1 + 1);
        assert!(b.strip_positions[0].abs_diff_eq(Vec3::X, 1e-5));
        assert!(b.strip_positions[8].abs_diff_eq(Vec3::Z, 1e-5));
    }

    #[test]
    fn long_arc_goes_the_other_way() {
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        b.long_arc_3d_between(Vec3::ZERO, Vec3::X, Vec3::Z, Color::WHITE);
        // Three quarters of a turn: 24 segments; the midpoint is on the far side.
        assert_eq!(b.strip_positions.len(), 24 + 2);
        assert!(b.strip_positions[24].abs_diff_eq(Vec3::Z, 1e-4));
        assert!(b.strip_positions[12].x < -0.5);
    }
}
