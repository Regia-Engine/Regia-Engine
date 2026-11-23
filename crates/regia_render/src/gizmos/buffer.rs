// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_gizmos/src/gizmos.rs. Copyright (c) Bevy contributors, dual-licensed
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
//! The [`Gizmos`] system parameter and its [`GizmoBuffer`] of line-list and line-strip vertices.

use std::iter;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

use regia_core::math::{Affine3A, Color, Isometry, Mat4, Quat, Vec2, Vec3};
use regia_ecs::ecs::system::{Deferred, SystemBuffer, SystemMeta, SystemParam};
use regia_ecs::ecs::world::DeferredWorld;
use regia_ecs::prelude::*;
use regia_scene::{GlobalTransform, Transform};

use super::config::{DefaultGizmoConfigGroup, GizmoConfig, GizmoConfigGroup, GizmoConfigStore};

/// The NaN colour that terminates a line strip (pairs with the NaN strip separator position).
pub(crate) const NAN_COLOR: Color = Color::new(f32::NAN, f32::NAN, f32::NAN, f32::NAN);

/// Anything that places a gizmo: a rotation and a translation (scale is ignored).
///
/// Implemented for [`Isometry`], [`Vec3`] (translation only), [`Quat`] (rotation only),
/// `(Vec3, Quat)`, [`Transform`] and [`GlobalTransform`].
pub trait IntoIsometry {
    /// The isometry.
    fn into_isometry(self) -> Isometry;
}

impl IntoIsometry for Isometry {
    fn into_isometry(self) -> Isometry {
        self
    }
}

impl IntoIsometry for Vec3 {
    fn into_isometry(self) -> Isometry {
        Isometry::from_translation(self)
    }
}

impl IntoIsometry for Quat {
    fn into_isometry(self) -> Isometry {
        Isometry::from_rotation(self)
    }
}

impl IntoIsometry for (Vec3, Quat) {
    fn into_isometry(self) -> Isometry {
        Isometry::new(self.1, self.0)
    }
}

impl IntoIsometry for Transform {
    fn into_isometry(self) -> Isometry {
        Isometry::new(self.rotation, self.translation)
    }
}

impl IntoIsometry for GlobalTransform {
    fn into_isometry(self) -> Isometry {
        Isometry::from_affine(&self.0)
    }
}

/// Anything that maps local points to world space, scale included (cubes, AABBs, axes).
pub trait TransformPoint {
    /// Transforms `point` from local to world space.
    fn transform_point(&self, point: Vec3) -> Vec3;
}

impl TransformPoint for Transform {
    fn transform_point(&self, point: Vec3) -> Vec3 {
        self.rotation * (self.scale * point) + self.translation
    }
}

impl TransformPoint for GlobalTransform {
    fn transform_point(&self, point: Vec3) -> Vec3 {
        self.0.transform_point3(point)
    }
}

impl TransformPoint for Affine3A {
    fn transform_point(&self, point: Vec3) -> Vec3 {
        self.transform_point3(point)
    }
}

impl TransformPoint for Mat4 {
    fn transform_point(&self, point: Vec3) -> Vec3 {
        self.transform_point3(point)
    }
}

impl TransformPoint for Isometry {
    fn transform_point(&self, point: Vec3) -> Vec3 {
        Isometry::transform_point(self, point)
    }
}

/// The accumulated gizmos of group `G` for the current frame.
///
/// [`Gizmos`] buffers flush here when their system's commands apply; the renderer drains it once
/// per frame (`Render`).
#[derive(Resource)]
pub struct GizmoStorage<G: GizmoConfigGroup> {
    pub(crate) list_positions: Vec<Vec3>,
    pub(crate) list_colors: Vec<Color>,
    pub(crate) strip_positions: Vec<Vec3>,
    pub(crate) strip_colors: Vec<Color>,
    marker: PhantomData<G>,
}

impl<G: GizmoConfigGroup> Default for GizmoStorage<G> {
    fn default() -> Self {
        Self {
            list_positions: Vec::new(),
            list_colors: Vec::new(),
            strip_positions: Vec::new(),
            strip_colors: Vec::new(),
            marker: PhantomData,
        }
    }
}

impl<G: GizmoConfigGroup> GizmoStorage<G> {
    /// Drops every queued gizmo.
    pub fn clear(&mut self) {
        self.list_positions.clear();
        self.list_colors.clear();
        self.strip_positions.clear();
        self.strip_colors.clear();
    }

    /// `true` when nothing is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.list_positions.is_empty() && self.strip_positions.is_empty()
    }

    /// Read-only view of the queued vertices.
    #[must_use]
    pub fn view(&self) -> GizmoBufferView<'_> {
        GizmoBufferView {
            list_positions: &self.list_positions,
            list_colors: &self.list_colors,
            strip_positions: &self.strip_positions,
            strip_colors: &self.strip_colors,
        }
    }
}

/// A [`SystemParam`] for drawing gizmos (debug lines and shapes) in immediate mode.
///
/// Everything drawn is shown for one frame: draw again every frame to keep it visible. Draw from
/// `Update` or `PostUpdate`; gizmos drawn in `FixedUpdate` show only in frames where the fixed
/// step ran.
///
/// ```
/// use regia_core::math::{Color, Vec3};
/// use regia_render::gizmos::Gizmos;
///
/// fn draw(mut gizmos: Gizmos) {
///     gizmos.line(Vec3::ZERO, Vec3::X, Color::RED);
///     gizmos.sphere(Vec3::Y, 0.5, Color::GREEN).resolution(48);
/// }
/// # regia_ecs::ecs::system::assert_is_system(draw);
/// ```
#[derive(SystemParam)]
pub struct Gizmos<'w, 's, G: GizmoConfigGroup = DefaultGizmoConfigGroup> {
    buffer: Deferred<'s, GizmoBuffer<G>>,
    store: Res<'w, GizmoConfigStore>,
}

impl<G: GizmoConfigGroup> Gizmos<'_, '_, G> {
    /// This group's configuration (`None` when the group was never registered).
    #[must_use]
    pub fn config(&self) -> Option<&GizmoConfig> {
        self.store.config::<G>().map(|(config, _)| config)
    }

    /// This group's value (the custom settings of a config group type).
    #[must_use]
    pub fn config_ext(&self) -> Option<&G> {
        self.store.config::<G>().map(|(_, ext)| ext)
    }
}

impl<G: GizmoConfigGroup> Deref for Gizmos<'_, '_, G> {
    type Target = GizmoBuffer<G>;

    fn deref(&self) -> &Self::Target {
        &self.buffer
    }
}

impl<G: GizmoConfigGroup> DerefMut for Gizmos<'_, '_, G> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // Cached per access so a disabled group returns before building any geometry.
        let enabled = self.store.is_enabled::<G>();
        let buffer = &mut *self.buffer;
        buffer.enabled = enabled;
        buffer
    }
}

/// Line-list and line-strip vertices of gizmo group `G`.
///
/// Strips are separated by a NaN position (and a NaN colour). Colours are linear RGBA.
#[derive(Debug, Clone)]
pub struct GizmoBuffer<G: GizmoConfigGroup> {
    pub(crate) enabled: bool,
    /// Positions of line segment end points (pairs).
    pub list_positions: Vec<Vec3>,
    /// Colours of line segment end points.
    pub list_colors: Vec<Color>,
    /// Positions of line strip vertices (strips end with a NaN position).
    pub strip_positions: Vec<Vec3>,
    /// Colours of line strip vertices.
    pub strip_colors: Vec<Color>,
    marker: PhantomData<G>,
}

impl<G: GizmoConfigGroup> Default for GizmoBuffer<G> {
    fn default() -> Self {
        Self::new()
    }
}

/// Read-only view of gizmo vertex data.
#[derive(Clone, Copy, Debug)]
pub struct GizmoBufferView<'a> {
    /// Line-list positions.
    pub list_positions: &'a [Vec3],
    /// Line-list colours.
    pub list_colors: &'a [Color],
    /// Line-strip positions.
    pub strip_positions: &'a [Vec3],
    /// Line-strip colours.
    pub strip_colors: &'a [Color],
}

impl<G: GizmoConfigGroup> SystemBuffer for GizmoBuffer<G> {
    fn apply(&mut self, _system_meta: &SystemMeta, world: &mut World) {
        flush(self, world.get_resource_mut::<GizmoStorage<G>>().as_deref_mut());
    }

    fn queue(&mut self, _system_meta: &SystemMeta, mut world: DeferredWorld) {
        flush(self, world.get_resource_mut::<GizmoStorage<G>>().as_deref_mut());
    }
}

fn flush<G: GizmoConfigGroup>(buffer: &mut GizmoBuffer<G>, storage: Option<&mut GizmoStorage<G>>) {
    match storage {
        Some(storage) => {
            storage.list_positions.append(&mut buffer.list_positions);
            storage.list_colors.append(&mut buffer.list_colors);
            storage.strip_positions.append(&mut buffer.strip_positions);
            storage.strip_colors.append(&mut buffer.strip_colors);
        }
        // An unregistered group: drop the data so the buffer cannot grow without bound.
        None => buffer.clear(),
    }
}

impl<G: GizmoConfigGroup> GizmoBuffer<G> {
    /// An empty, enabled buffer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            enabled: true,
            list_positions: Vec::new(),
            list_colors: Vec::new(),
            strip_positions: Vec::new(),
            strip_colors: Vec::new(),
            marker: PhantomData,
        }
    }

    /// Drops all data.
    pub fn clear(&mut self) {
        self.list_positions.clear();
        self.list_colors.clear();
        self.strip_positions.clear();
        self.strip_colors.clear();
    }

    /// `true` when the buffer holds no vertices.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.list_positions.is_empty() && self.strip_positions.is_empty()
    }

    /// Read-only view of the data.
    #[must_use]
    pub fn buffer(&self) -> GizmoBufferView<'_> {
        GizmoBufferView {
            list_positions: &self.list_positions,
            list_colors: &self.list_colors,
            strip_positions: &self.strip_positions,
            strip_colors: &self.strip_colors,
        }
    }

    /// Draws a line from `start` to `end`.
    #[inline]
    pub fn line(&mut self, start: Vec3, end: Vec3, color: impl Into<Color>) {
        if !self.enabled {
            return;
        }
        self.list_positions.extend([start, end]);
        self.add_list_color(color, 2);
    }

    /// Draws a line from `start` to `end`, fading from `start_color` to `end_color`.
    #[inline]
    pub fn line_gradient<C: Into<Color>>(&mut self, start: Vec3, end: Vec3, start_color: C, end_color: C) {
        if !self.enabled {
            return;
        }
        self.list_positions.extend([start, end]);
        self.list_colors.extend([start_color.into(), end_color.into()]);
    }

    /// Draws a line from `start` to `start + vector`.
    #[inline]
    pub fn ray(&mut self, start: Vec3, vector: Vec3, color: impl Into<Color>) {
        self.line(start, start + vector, color);
    }

    /// Draws a line from `start` to `start + vector` with a colour gradient.
    #[inline]
    pub fn ray_gradient<C: Into<Color>>(&mut self, start: Vec3, vector: Vec3, start_color: C, end_color: C) {
        self.line_gradient(start, start + vector, start_color, end_color);
    }

    /// Draws a connected line through `positions`.
    #[inline]
    pub fn linestrip(&mut self, positions: impl IntoIterator<Item = Vec3>, color: impl Into<Color>) {
        if !self.enabled {
            return;
        }
        self.strip_positions.extend(positions);
        let len = self.strip_positions.len();
        let color = color.into();
        // `len` counts every strip so far; the newest strip's colours fill up to it.
        self.strip_colors.resize(len, color);
        self.strip_positions.push(Vec3::NAN);
        self.strip_colors.push(NAN_COLOR);
    }

    /// Draws a closed loop through `positions` (the last point connects back to the first).
    #[inline]
    pub fn lineloop(&mut self, positions: impl IntoIterator<Item = Vec3>, color: impl Into<Color>) {
        if !self.enabled {
            return;
        }
        // Loop back to the start; the second point again so the first corner gets a joint.
        let mut positions = positions.into_iter();
        let first = positions.next();
        let second = positions.next();
        self.linestrip(
            first
                .into_iter()
                .chain(second)
                .chain(positions)
                .chain(first)
                .chain(second),
            color,
        );
    }

    /// Draws a connected line through `points`, each with its own colour.
    #[inline]
    pub fn linestrip_gradient<C: Into<Color>>(&mut self, points: impl IntoIterator<Item = (Vec3, C)>) {
        if !self.enabled {
            return;
        }
        let points = points.into_iter();
        let (min, _) = points.size_hint();
        self.strip_positions.reserve(min + 1);
        self.strip_colors.reserve(min + 1);
        for (position, color) in points {
            self.strip_positions.push(position);
            self.strip_colors.push(color.into());
        }
        self.strip_positions.push(Vec3::NAN);
        self.strip_colors.push(NAN_COLOR);
    }

    /// Draws a rectangle of `size` in the local XY plane of `isometry`.
    #[inline]
    pub fn rect(&mut self, isometry: impl IntoIsometry, size: Vec2, color: impl Into<Color>) {
        if !self.enabled {
            return;
        }
        let isometry = isometry.into_isometry();
        let [tl, tr, br, bl] = rect_inner(size).map(|v| isometry.transform_point(v.extend(0.0)));
        self.lineloop([tl, tr, br, bl], color);
    }

    /// Draws the unit cube (side 1, centred on the origin) placed by `transform` (scale applies).
    #[inline]
    pub fn cube(&mut self, transform: impl TransformPoint, color: impl Into<Color>) {
        if !self.enabled {
            return;
        }
        let color = color.into();
        let rect = rect_inner(Vec2::ONE);
        let [tlf, trf, brf, blf] = rect.map(|v| transform.transform_point(v.extend(0.5)));
        let [tlb, trb, brb, blb] = rect.map(|v| transform.transform_point(v.extend(-0.5)));
        self.linestrip([tlf, trf, brf, blf, tlf, tlb, trb, brb, blb, tlb], color);
        self.list_positions.extend([trf, trb, brf, brb, blf, blb]);
        self.add_list_color(color, 6);
    }

    /// Draws an axis-aligned box (`min`..`max` in local space) placed by `transform`.
    #[inline]
    pub fn aabb_3d(
        &mut self,
        aabb: regia_core::math::Aabb,
        transform: impl TransformPoint,
        color: impl Into<Color>,
    ) {
        if !self.enabled {
            return;
        }
        let color = color.into();
        let (min, max) = (aabb.min, aabb.max);
        let [tlf, trf, brf, blf, tlb, trb, brb, blb] = [
            Vec3::new(min.x, max.y, max.z),
            Vec3::new(max.x, max.y, max.z),
            Vec3::new(max.x, min.y, max.z),
            Vec3::new(min.x, min.y, max.z),
            Vec3::new(min.x, max.y, min.z),
            Vec3::new(max.x, max.y, min.z),
            Vec3::new(max.x, min.y, min.z),
            Vec3::new(min.x, min.y, min.z),
        ]
        .map(|v| transform.transform_point(v));
        self.linestrip([tlf, trf, brf, blf, tlf, tlb, trb, brb, blb, tlb], color);
        self.list_positions.extend([trf, trb, brf, brb, blf, blb]);
        self.add_list_color(color, 6);
    }

    /// Appends another buffer's data (any group) to this one.
    pub fn append<H: GizmoConfigGroup>(&mut self, other: &GizmoBuffer<H>) {
        if !self.enabled {
            return;
        }
        self.list_positions.extend_from_slice(&other.list_positions);
        self.list_colors.extend_from_slice(&other.list_colors);
        self.strip_positions.extend_from_slice(&other.strip_positions);
        self.strip_colors.extend_from_slice(&other.strip_colors);
    }

    #[inline]
    fn add_list_color(&mut self, color: impl Into<Color>, count: usize) {
        self.list_colors.extend(iter::repeat_n(color.into(), count));
    }
}

pub(crate) fn rect_inner(size: Vec2) -> [Vec2; 4] {
    let half = size / 2.0;
    [
        Vec2::new(-half.x, half.y),
        Vec2::new(half.x, half.y),
        Vec2::new(half.x, -half.y),
        Vec2::new(-half.x, -half.y),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    type Buffer = GizmoBuffer<DefaultGizmoConfigGroup>;

    #[test]
    fn line_adds_one_segment() {
        let mut b = Buffer::new();
        b.line(Vec3::ZERO, Vec3::X, Color::RED);
        assert_eq!(b.list_positions.len(), 2);
        assert_eq!(b.list_colors.len(), 2);
        assert!(b.strip_positions.is_empty());
    }

    #[test]
    fn strips_end_with_a_nan_separator_and_colours_stay_aligned() {
        let mut b = Buffer::new();
        b.linestrip([Vec3::ZERO, Vec3::X, Vec3::Y], Color::RED);
        b.linestrip([Vec3::Z, Vec3::ONE], Color::BLUE);
        assert_eq!(b.strip_positions.len(), 3 + 1 + 2 + 1);
        assert_eq!(b.strip_colors.len(), b.strip_positions.len());
        assert!(b.strip_positions[3].is_nan());
        assert!(b.strip_positions[6].is_nan());
        assert_eq!(b.strip_colors[4], Color::BLUE);
        assert_eq!(b.strip_colors[2], Color::RED);
    }

    #[test]
    fn lineloop_closes_and_repeats_the_second_point() {
        let mut b = Buffer::new();
        b.lineloop([Vec3::ZERO, Vec3::X, Vec3::Y], Color::WHITE);
        // 3 points + first + second + separator.
        assert_eq!(b.strip_positions.len(), 6);
        assert_eq!(b.strip_positions[3], Vec3::ZERO);
        assert_eq!(b.strip_positions[4], Vec3::X);
    }

    #[test]
    fn cube_and_aabb_have_twelve_edges() {
        let mut b = Buffer::new();
        b.cube(Transform::IDENTITY, Color::WHITE);
        // A 10-point strip (9 segments, one doubling back over an edge) + 6 list points = 3 segments.
        assert_eq!(b.strip_positions.len(), 11);
        assert_eq!(b.list_positions.len(), 6);
        let mut a = Buffer::new();
        a.aabb_3d(
            regia_core::math::Aabb::new(Vec3::splat(-1.0), Vec3::splat(1.0)),
            Mat4::IDENTITY,
            Color::WHITE,
        );
        assert_eq!(a.strip_positions.len(), 11);
        assert!(
            a.strip_positions[..10]
                .iter()
                .all(|p| p.abs().max_element() == 1.0)
        );
    }

    #[test]
    fn disabled_buffers_record_nothing() {
        let mut b = Buffer::new();
        b.enabled = false;
        b.line(Vec3::ZERO, Vec3::X, Color::RED);
        b.linestrip([Vec3::ZERO, Vec3::X], Color::RED);
        b.cube(Mat4::IDENTITY, Color::RED);
        assert!(b.is_empty());
    }
}
