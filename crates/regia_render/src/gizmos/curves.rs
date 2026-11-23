// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_gizmos/src/{curves,frustum}.rs. Copyright (c) Bevy contributors, dual-licensed
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
//! Sampled curves and view frusta.

use regia_core::math::{Color, Curve, Mat4, Vec3, Vec4};

use super::buffer::GizmoBuffer;
use super::config::GizmoConfigGroup;

impl<G: GizmoConfigGroup> GizmoBuffer<G> {
    /// Draws a keyframed [`Curve`] sampled at `times` as a line strip. Times outside the curve
    /// (no sample) are skipped.
    pub fn curve_3d(
        &mut self,
        curve: &Curve<Vec3>,
        times: impl IntoIterator<Item = f32>,
        color: impl Into<Color>,
    ) {
        self.linestrip(times.into_iter().filter_map(|t| curve.sample(t)), color);
    }

    /// Draws `f(t)` for every `t` in `times` as a line strip (splines, trajectories, paths).
    pub fn curve_fn_3d(
        &mut self,
        f: impl Fn(f32) -> Vec3,
        times: impl IntoIterator<Item = f32>,
        color: impl Into<Color>,
    ) {
        self.linestrip(times.into_iter().map(f), color);
    }

    /// Draws a curve sampled at `times`, each sample with its own colour.
    pub fn curve_gradient_3d<C: Into<Color>>(
        &mut self,
        curve: &Curve<Vec3>,
        times_with_colors: impl IntoIterator<Item = (f32, C)>,
    ) {
        self.linestrip_gradient(
            times_with_colors
                .into_iter()
                .filter_map(|(time, color)| curve.sample(time).map(|sample| (sample, color))),
        );
    }

    /// Draws the frustum of `view_projection` (clip = `view_projection * world`, wgpu depth 0..1):
    /// the near and far rectangles and the four connecting edges. Nothing is drawn when the
    /// matrix is not invertible.
    pub fn frustum(&mut self, view_projection: Mat4, color: impl Into<Color>) {
        if !self.enabled {
            return;
        }
        let Some([tln, trn, brn, bln, tlf, trf, brf, blf]) = frustum_corners(view_projection) else {
            return;
        };
        let color = color.into();
        self.linestrip([tln, trn, brn, bln, tln, tlf, trf, brf, blf, tlf], color);
        self.line(trn, trf, color);
        self.line(brn, brf, color);
        self.line(bln, blf, color);
    }
}

/// World-space corners of a frustum: near top-left, top-right, bottom-right, bottom-left, then
/// the same on the far plane.
pub(crate) fn frustum_corners(view_projection: Mat4) -> Option<[Vec3; 8]> {
    if view_projection.determinant().abs() < f32::EPSILON * f32::EPSILON {
        return None;
    }
    let inverse = view_projection.inverse();
    let mut out = [Vec3::ZERO; 8];
    for (i, (z, (x, y))) in [0.0f32, 1.0]
        .into_iter()
        .flat_map(|z| [(-1.0f32, 1.0f32), (1.0, 1.0), (1.0, -1.0), (-1.0, -1.0)].map(|xy| (z, xy)))
        .enumerate()
    {
        let p = inverse * Vec4::new(x, y, z, 1.0);
        if p.w.abs() < f32::EPSILON {
            return None;
        }
        out[i] = p.truncate() / p.w;
    }
    out.iter().all(|p| p.is_finite()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gizmos::DefaultGizmoConfigGroup;

    #[test]
    fn frustum_corners_lie_on_near_and_far() {
        let proj = regia_core::glam::camera::rh::proj::directx::perspective(1.0, 1.5, 0.5, 10.0);
        let view =
            regia_core::glam::camera::rh::view::look_at_mat4(Vec3::new(0.0, 0.0, 5.0), Vec3::ZERO, Vec3::Y);
        let corners = frustum_corners(proj * view).expect("invertible");
        for c in &corners[..4] {
            assert!((c.z - 4.5).abs() < 1e-3, "near corner {c}");
        }
        for c in &corners[4..] {
            assert!((c.z + 5.0).abs() < 1e-2, "far corner {c}");
        }
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        b.frustum(proj * view, Color::WHITE);
        assert_eq!(b.strip_positions.len(), 11);
        assert_eq!(b.list_positions.len(), 6);
        let mut none = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        none.frustum(Mat4::ZERO, Color::WHITE);
        assert!(none.is_empty());
    }

    #[test]
    fn curves_sample_into_one_strip() {
        let curve = Curve::linear([(0.0, Vec3::ZERO), (1.0, Vec3::X)]);
        let mut b = GizmoBuffer::<DefaultGizmoConfigGroup>::new();
        b.curve_3d(&curve, (0..=10).map(|i| i as f32 / 10.0), Color::WHITE);
        assert_eq!(b.strip_positions.len(), 12);
        b.curve_fn_3d(
            |t| Vec3::new(t.cos(), 0.0, t.sin()),
            (0..4).map(|i| i as f32),
            Color::WHITE,
        );
        assert_eq!(b.strip_positions.len(), 12 + 5);
    }
}
