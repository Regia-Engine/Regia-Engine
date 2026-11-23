// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_core/src/geom/polygon.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
// MIT OR Apache-2.0, used here under the MIT licence (see docs/ported-sources.toml).
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
use crate::math::geom::tri::{self, Tri};
use crate::math::geom::{Cuboid, Rect, Vertex, Vertex2d, Vertex3d};

/// A simple type wrapper around a list of points that describe a polygon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Polygon<I> {
    /// The iterator yielding all points in the polygon.
    pub points: I,
}

/// An iterator yielding indices into a polygon's vertices required to triangulate the polygon.
#[derive(Clone, Debug)]
pub struct TriangleIndices {
    index: usize,
    n_points: usize,
}

impl<I> Polygon<I>
where
    I: Iterator,
{
    /// Construct a new polygon from the given list of points describing its vertices.
    pub fn new<P>(points: P) -> Self
    where
        P: IntoIterator<IntoIter = I, Item = I::Item>,
    {
        let points = points.into_iter();
        Polygon { points }
    }

    /// Triangulate the polygon given as a list of `Point`s describing its sides.
    ///
    /// Returns `None` if the polygon's iterator yields less than two points.
    pub fn triangles(self) -> Option<Triangles<I>> {
        triangles(self.points)
    }

    /// Returns `Some` with the touched triangle if the given `Point` is over the polygon described
    /// by the given series of points.
    ///
    /// This uses the `triangles` function internally.
    pub fn contains(self, p: &I::Item) -> Option<Tri<I::Item>>
    where
        I::Item: Vertex2d,
    {
        contains(self.points, p)
    }

    /// The `Rect` that bounds the polygon.
    ///
    /// Returns `None` if the polygon's point iterator is empty.
    pub fn bounding_rect(self) -> Option<Rect<<I::Item as Vertex>::Scalar>>
    where
        I::Item: Vertex2d,
    {
        super::bounding_rect(self.points)
    }

    /// The `Cuboid that bounds the polygon.
    ///
    /// Returns `None` if the polygon's point iterator is empty.
    pub fn bounding_cuboid(self) -> Option<Cuboid<<I::Item as Vertex>::Scalar>>
    where
        I::Item: Vertex3d,
    {
        super::bounding_cuboid(self.points)
    }
}

/// An iterator that triangulates a polygon represented by a sequence of points describing its
/// edges.
#[derive(Clone, Debug)]
pub struct Triangles<I>
where
    I: Iterator,
{
    first: I::Item,
    prev: I::Item,
    points: I,
}

/// Triangulate the polygon given as a list of `Point`s describing its sides.
///
/// Returns `None` if the given iterator yields less than two points.
pub fn triangles<I>(points: I) -> Option<Triangles<I::IntoIter>>
where
    I: IntoIterator,
{
    let mut points = points.into_iter();
    let first = points.next()?;
    let prev = points.next()?;
    Some(Triangles { first, prev, points })
}

/// An iterator yielding indices into a polygon's vertices required to triangulate the polygon.
pub fn triangle_indices(n_points: usize) -> TriangleIndices {
    let index = 0;
    TriangleIndices { index, n_points }
}

/// Returns `Some` with the touched triangle if the given `Point` is over the polygon described by
/// the given series of points.
///
/// This uses the `triangles` function internally.
pub fn contains<I>(points: I, point: &I::Item) -> Option<Tri<I::Item>>
where
    I: IntoIterator,
    I::Item: Vertex2d,
{
    triangles(points).and_then(|ts| tri::iter_contains(ts, point))
}

impl<I> Iterator for Triangles<I>
where
    I: Iterator,
    I::Item: Vertex,
{
    type Item = Tri<I::Item>;
    fn next(&mut self) -> Option<Self::Item> {
        self.points.next().map(|point| {
            let t = Tri([self.first, self.prev, point]);
            self.prev = point;
            t
        })
    }
}

impl Iterator for TriangleIndices {
    type Item = usize;
    fn next(&mut self) -> Option<Self::Item> {
        let index = self.index;
        let tri_index = index / 3;
        if self.n_points < tri_index + 3 {
            return None;
        }
        self.index += 1;
        match index % 3 {
            0 => Some(0),
            remainder => Some(tri_index + remainder),
        }
    }
}
