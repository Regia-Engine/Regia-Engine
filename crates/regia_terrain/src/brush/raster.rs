// SPDX-License-Identifier: MIT
// Portions ported from Fyrox (https://github.com/FyroxEngine/Fyrox, commit 0876f67),
// fyrox-impl/src/scene/terrain/brushstroke/brushraster.rs.
// Copyright (c) 2019-present Dmitry Stepanov and Fyrox Engine contributors, MIT licence
// (see docs/ported-sources.toml).
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

//! Brush rasterisation: floating-point brush positions (in sample-grid coordinates) become
//! integer samples, each with a strength from how close it is to the brush centre and the
//! brush hardness. Soft brushes fall off from the centre to the edge; hard brushes keep full
//! strength until close to the edge.
//!
//! Regia Engine changes: glam instead of nalgebra, a small [`IRect`] instead of Fyrox's `Rect` /
//! `OptionRect`, and the line segment maths inlined.

use glam::{IVec2, Mat2, Vec2};

/// An integer rectangle: `position` is the minimum corner, `size` the extent. Iteration
/// ([`RectIter`]) covers `position ..= position + size` on both axes, as in the original.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct IRect {
    /// Minimum corner.
    pub position: IVec2,
    /// Extent (max corner minus min corner).
    pub size: IVec2,
}

impl IRect {
    /// A rectangle from its minimum corner and extent.
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self {
            position: IVec2::new(x, y),
            size: IVec2::new(w, h),
        }
    }

    /// Width.
    pub fn w(&self) -> i32 {
        self.size.x
    }

    /// Height.
    pub fn h(&self) -> i32 {
        self.size.y
    }

    /// Grows the rectangle to contain `other`.
    pub fn extend_to_contain(&mut self, other: IRect) {
        let min = self.position.min(other.position);
        let max = (self.position + self.size).max(other.position + other.size);
        self.position = min;
        self.size = max - min;
    }

    /// Number of samples [`RectIter`] visits (`(w + 1) * (h + 1)`), saturating.
    pub fn sample_count(&self) -> i64 {
        (i64::from(self.size.x) + 1).max(0) * (i64::from(self.size.y) + 1).max(0)
    }
}

/// Grows an optional rectangle to contain `rect`.
fn extend(bounds: &mut Option<IRect>, rect: IRect) {
    match bounds {
        Some(b) => b.extend_to_contain(rect),
        None => *bounds = Some(rect),
    }
}

/// Adjusts the strength of a brush sample for the brush hardness.
pub fn apply_hardness(hardness: f32, strength: f32) -> f32 {
    if strength == 0.0 {
        return 0.0;
    }
    let h = 1.0 - hardness;
    if strength < h { strength / h } else { 1.0 }
}

/// Something that assigns strengths to samples around a brush centre.
pub trait BrushRaster {
    /// Strength at `point`, measured from the brush centre (`(0, 0)` is the exact centre).
    fn strength_at(&self, point: Vec2) -> f32;
    /// Bounds `(min, max)` of every sample of the brush around `(0, 0)`.
    fn bounds(&self) -> (Vec2, Vec2);
    /// Integer bounds of the brush after `transform` and a translation to `center`.
    ///
    /// Corners are clamped to `±RASTER_LIMIT` samples first: a finite but enormous brush or
    /// centre would otherwise saturate the `i32` conversion and overflow the extent (a panic in
    /// debug builds). The clamped bounds still exceed the sanity limit, so such a stamp is
    /// dropped as before.
    fn transformed_bounds(&self, center: Vec2, transform: &Mat2) -> IRect {
        let mut bounds = None;
        let (min, max) = self.bounds();
        for p in [
            Vec2::new(min.x, min.y),
            Vec2::new(min.x, max.y),
            Vec2::new(max.x, min.y),
            Vec2::new(max.x, max.y),
        ] {
            let p1 = (*transform * p + center).clamp(Vec2::splat(-RASTER_LIMIT), Vec2::splat(RASTER_LIMIT));
            let ceil = p1.ceil().as_ivec2();
            let floor = p1.floor().as_ivec2();
            extend(
                &mut bounds,
                IRect::new(floor.x, floor.y, ceil.x - floor.x, ceil.y - floor.y),
            );
        }
        bounds.unwrap_or(IRect::new(center.x as i32, center.y as i32, 0, 0))
    }
}

/// Largest sample coordinate (either sign) a brush bound may reach: 2^24, so extents and
/// iteration positions stay far inside `i32`.
const RASTER_LIMIT: f32 = 16_777_216.0;

/// A round brush of the given radius (in samples).
#[derive(Debug, Clone)]
pub struct CircleRaster(pub f32);

impl BrushRaster for CircleRaster {
    fn strength_at(&self, point: Vec2) -> f32 {
        let radius = self.0;
        let dist_sqr = point.length_squared();
        if dist_sqr >= radius * radius {
            0.0
        } else {
            (1.0 - dist_sqr.sqrt() / radius).max(0.0)
        }
    }

    fn bounds(&self) -> (Vec2, Vec2) {
        (Vec2::splat(-self.0), Vec2::splat(self.0))
    }
}

/// A rectangular brush with the given x and y half-extents (in samples).
#[derive(Debug, Clone)]
pub struct RectRaster(pub f32, pub f32);

impl BrushRaster for RectRaster {
    fn strength_at(&self, point: Vec2) -> f32 {
        let radius = Vec2::new(self.0, self.1);
        // Fold onto the positive quadrant.
        let p = point.abs();
        let min = radius.min_element();
        let inner = Vec2::new(radius.x - min, radius.y - min);
        let outer = radius - inner;
        if outer.min_element() == 0.0 {
            return 0.0;
        }
        let p = (p - inner).max(Vec2::ZERO);
        if p.x > outer.x || p.y > outer.y {
            0.0
        } else {
            1.0 - (p.x / outer.x).max(p.y / outer.y)
        }
    }

    fn bounds(&self) -> (Vec2, Vec2) {
        (Vec2::new(-self.0, -self.1), Vec2::new(self.0, self.1))
    }
}

/// One sample under a brush.
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct BrushPixel {
    /// Sample position `(col, row)`.
    pub position: IVec2,
    /// Strength of the brush here: 0 outside the shape, 1 at full strength.
    pub strength: f32,
}

/// Iterates the integer coordinates of an [`IRect`], row by row (both ends inclusive).
#[derive(Debug, Clone)]
pub struct RectIter {
    bounds: IRect,
    next_pos: IVec2,
}

impl RectIter {
    /// An iterator over `bounds`.
    pub fn new(bounds: IRect) -> Self {
        Self {
            bounds,
            next_pos: IVec2::ZERO,
        }
    }

    /// The rectangle being scanned.
    pub fn bounds(&self) -> IRect {
        self.bounds
    }
}

impl Iterator for RectIter {
    type Item = IVec2;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_pos.y > self.bounds.size.y {
            return None;
        }
        let result = self.next_pos + self.bounds.position;
        if self.next_pos.x < self.bounds.size.x {
            self.next_pos.x += 1;
        } else {
            self.next_pos.y += 1;
            self.next_pos.x = 0;
        }
        Some(result)
    }
}

/// Splits `transform` into itself and its inverse, falling back to identity when singular.
fn with_inverse(transform: Mat2) -> (Mat2, Mat2) {
    if transform.determinant().abs() > f32::EPSILON && transform.is_finite() {
        (transform, transform.inverse())
    } else {
        (Mat2::IDENTITY, Mat2::IDENTITY)
    }
}

/// The samples of one brush stamp (zero-strength samples included).
#[derive(Debug, Clone)]
pub struct StampPixels<R> {
    brush_raster: R,
    center: Vec2,
    hardness: f32,
    inv_transform: Mat2,
    bounds_iter: RectIter,
}

impl<R: BrushRaster> StampPixels<R> {
    /// Bounds of every sample this iterator yields.
    pub fn bounds(&self) -> IRect {
        self.bounds_iter.bounds()
    }

    /// A stamp centred at `center` (sample coordinates).
    pub fn new(brush_raster: R, center: Vec2, hardness: f32, transform: Mat2) -> Self {
        let (transform, inv_transform) = with_inverse(transform);
        let bounds = brush_raster.transformed_bounds(center, &transform);
        Self {
            brush_raster,
            center,
            hardness,
            inv_transform,
            bounds_iter: RectIter::new(bounds),
        }
    }
}

impl<R: BrushRaster> Iterator for StampPixels<R> {
    type Item = BrushPixel;

    fn next(&mut self) -> Option<Self::Item> {
        let position = self.bounds_iter.next()?;
        let p = self.inv_transform * (position.as_vec2() - self.center);
        let strength = apply_hardness(self.hardness, self.brush_raster.strength_at(p));
        Some(BrushPixel { position, strength })
    }
}

/// The samples painted when a brush is dragged from `start` to `end`: a stamp swept along the
/// segment (each sample takes the strength from its nearest point on the segment).
#[derive(Debug, Clone)]
pub struct SmearPixels<R> {
    brush_raster: R,
    start: Vec2,
    end: Vec2,
    aspect_start: Vec2,
    aspect_end: Vec2,
    hardness: f32,
    inv_transform: Mat2,
    aspect_transform: Mat2,
    bounds_iter: RectIter,
}

impl<R> SmearPixels<R> {
    /// Bounds of every sample this iterator yields.
    pub fn bounds(&self) -> IRect {
        self.bounds_iter.bounds()
    }
}

impl<R: BrushRaster> SmearPixels<R> {
    /// A smear from `start` to `end` (sample coordinates).
    pub fn new(brush_raster: R, start: Vec2, end: Vec2, hardness: f32, transform: Mat2) -> Self {
        let (transform, inv_transform) = with_inverse(transform);
        let mut bounds = None;
        extend(&mut bounds, brush_raster.transformed_bounds(start, &transform));
        extend(&mut bounds, brush_raster.transformed_bounds(end, &transform));
        let (min, max) = brush_raster.bounds();
        let extent = (max - min).max(Vec2::splat(f32::EPSILON));
        let aspect = Mat2::from_diagonal(extent.recip());
        let aspect_transform = aspect * inv_transform;
        Self {
            brush_raster,
            start,
            end,
            aspect_start: aspect_transform * start,
            aspect_end: aspect_transform * end,
            hardness,
            inv_transform,
            aspect_transform,
            bounds_iter: RectIter::new(bounds.unwrap_or(IRect::new(start.x as i32, start.y as i32, 0, 0))),
        }
    }
}

/// Parameter of the point of segment `a..b` nearest to `p` (unclamped; 0 for a point segment).
fn nearest_t(a: Vec2, b: Vec2, p: Vec2) -> f32 {
    let d = b - a;
    let len_sq = d.length_squared();
    if len_sq <= 0.0 {
        0.0
    } else {
        (p - a).dot(d) / len_sq
    }
}

impl<R: BrushRaster> Iterator for SmearPixels<R> {
    type Item = BrushPixel;

    fn next(&mut self) -> Option<Self::Item> {
        let position = self.bounds_iter.next()?;
        let p = position.as_vec2();
        let t = nearest_t(self.aspect_start, self.aspect_end, self.aspect_transform * p);
        let center = self.start.lerp(self.end, t.clamp(0.0, 1.0));
        let local = self.inv_transform * (p - center);
        let strength = apply_hardness(self.hardness, self.brush_raster.strength_at(local));
        Some(BrushPixel { position, strength })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_extend_to_contain() {
        let mut rect = IRect::new(0, 0, 1, 1);
        rect.extend_to_contain(IRect::new(1, 1, 1, 1));
        assert_eq!(rect, IRect::new(0, 0, 2, 2));
        rect.extend_to_contain(IRect::new(-1, -1, 1, 1));
        assert_eq!(rect, IRect::new(-1, -1, 3, 3));
    }

    #[test]
    fn bounds_iter() {
        let result: Vec<IVec2> = RectIter::new(IRect::new(-1, -2, 3, 1)).collect();
        let expected: Vec<IVec2> = [
            (-1, -2),
            (0, -2),
            (1, -2),
            (2, -2),
            (-1, -1),
            (0, -1),
            (1, -1),
            (2, -1),
        ]
        .into_iter()
        .map(|(x, y)| IVec2::new(x, y))
        .collect();
        assert_eq!(result, expected);
    }

    #[test]
    fn finite_pixels() {
        let circle = StampPixels::new(CircleRaster(2.0), Vec2::ZERO, 1.0, Mat2::IDENTITY);
        assert!(circle.count() <= 100);
        let rect = StampPixels::new(RectRaster(2.0, 2.0), Vec2::ZERO, 1.0, Mat2::IDENTITY);
        assert!(rect.count() <= 100);
    }

    fn covered(iter: impl Iterator<Item = BrushPixel>) -> IRect {
        let mut rect: Option<IRect> = None;
        for p in iter {
            extend(&mut rect, IRect::new(p.position.x, p.position.y, 0, 0));
        }
        rect.unwrap_or(IRect::new(0, 0, 0, 0))
    }

    #[test]
    fn pixel_range_matches_shape() {
        let circle = StampPixels::new(CircleRaster(2.0), Vec2::ZERO, 1.0, Mat2::IDENTITY);
        assert_eq!(covered(circle), IRect::new(-2, -2, 4, 4));
        let rect = StampPixels::new(RectRaster(2.0, 2.0), Vec2::ZERO, 1.0, Mat2::IDENTITY);
        assert_eq!(covered(rect), IRect::new(-2, -2, 4, 4));
    }

    fn strength_at(iter: &StampPixels<RectRaster>, x: i32, y: i32) -> f32 {
        let p = IVec2::new(x, y);
        iter.clone()
            .find(|px| px.position == p)
            .map_or(0.0, |px| px.strength)
    }

    #[test]
    fn hard_rect_far_from_origin() {
        for c in [Vec2::new(1.0, 1.0), Vec2::new(1001.0, 2501.0)] {
            let iter = StampPixels::new(RectRaster(1.1, 1.1), c, 1.0, Mat2::IDENTITY);
            assert_eq!(iter.bounds().w(), 4, "{:?}", iter.bounds());
            assert_eq!(iter.bounds().h(), 4, "{:?}", iter.bounds());
            let (x, y) = (c.x as i32, c.y as i32);
            assert_eq!(strength_at(&iter, x, y), 1.0);
            assert_eq!(strength_at(&iter, x - 1, y), 1.0);
            assert_eq!(strength_at(&iter, x - 1, y + 1), 1.0);
            assert_eq!(strength_at(&iter, x - 1, y - 1), 1.0);
            assert_eq!(strength_at(&iter, x - 2, y), 0.0);
        }
    }

    #[test]
    fn soft_circle_falls_off_from_the_centre() {
        let iter = StampPixels::new(CircleRaster(4.0), Vec2::ZERO, 0.0, Mat2::IDENTITY);
        let at = |x: i32| {
            iter.clone()
                .find(|p| p.position == IVec2::new(x, 0))
                .map_or(0.0, |p| p.strength)
        };
        assert_eq!(at(0), 1.0);
        assert!(at(1) > at(2) && at(2) > at(3) && at(3) > 0.0);
        assert_eq!(at(4), 0.0);
    }

    #[test]
    fn smear_covers_both_ends_and_the_middle() {
        let smear: Vec<BrushPixel> = SmearPixels::new(
            CircleRaster(2.0),
            Vec2::new(0.0, 0.0),
            Vec2::new(10.0, 0.0),
            1.0,
            Mat2::IDENTITY,
        )
        .collect();
        let at = |x: i32| {
            smear
                .iter()
                .find(|p| p.position == IVec2::new(x, 0))
                .map_or(0.0, |p| p.strength)
        };
        assert_eq!(at(0), 1.0);
        assert_eq!(at(5), 1.0);
        assert_eq!(at(10), 1.0);
        assert_eq!(at(13), 0.0);
    }

    #[test]
    fn singular_transform_falls_back_to_identity() {
        let a: Vec<BrushPixel> = StampPixels::new(
            CircleRaster(3.0),
            Vec2::ZERO,
            0.5,
            Mat2::from_diagonal(Vec2::ZERO),
        )
        .collect();
        let b: Vec<BrushPixel> =
            StampPixels::new(CircleRaster(3.0), Vec2::ZERO, 0.5, Mat2::IDENTITY).collect();
        assert_eq!(a, b);
    }
}
