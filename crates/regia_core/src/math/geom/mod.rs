// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_core/src/geom/mod.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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
//! 2D and 3D geometry on glam (`f32` by default, generic over the scalar): rectangles with
//! alignment and padding, ranges, ellipses, triangles, quads, polygons and cuboids, plus vertex
//! traits and angle / range helpers ([`util`]). The quick draw API (`regia_quick`) builds on it.
//!
//! Each module provides a set of general tools for working with the named geometry including:
//!
//! - A typed, object representation.
//! - Functions for producing vertices, triangles and triangulation indices.
//! - Functions for checking whether or not the geometry contains a point.
//! - Functions for determining the bounding rectangle or cuboid.
//! - A function for finding the centroid.

pub use glam::{
    DVec2, DVec3, DVec4, IVec2, IVec3, IVec4, Vec2, Vec3, Vec4, dvec2, dvec3, dvec4, ivec2, ivec3, ivec4,
    vec2, vec3, vec4,
};
pub use num_traits;

pub use self::cuboid::Cuboid;
pub use self::ellipse::Ellipse;
pub use self::point::{Point2, Point3, Point4, pt2, pt3, pt4};
pub use self::polygon::Polygon;
pub use self::quad::Quad;
pub use self::range::{Align, Edge, Range};
pub use self::rect::{Corner, Padding, Rect};
pub use self::scalar::Scalar;
pub use self::tri::Tri;
pub use self::util::{
    ConvertAngle, Mat4LookTo, Vec2Angle, Vec2Rotate, clamp, deg_to_rad, fmod, map_range, partial_max,
    partial_min, rad_to_deg, rad_to_turns, turns_to_rad,
};
pub use self::vertex::{Vertex, Vertex2d, Vertex3d};

/// Axis-aligned boxes in 3D.
pub mod cuboid;
/// Ellipses: circumference points, sections and triangulation.
pub mod ellipse;
/// Point type aliases over glam vectors.
pub mod point;
/// Simple polygons: triangulation, bounds and containment.
pub mod polygon;
/// Quads (four vertices, two triangles).
pub mod quad;
/// 1D ranges with alignment and padding (the axes of [`Rect`] and [`Cuboid`]).
pub mod range;
/// Axis-aligned rectangles: alignment, padding, subdivision and containment.
pub mod rect;
/// The numeric trait every geometry scalar implements.
pub mod scalar;
/// Triangles: centroid, bounds and point containment.
pub mod tri;
pub mod util;
/// Vertex traits over glam vectors and arrays.
pub mod vertex;

// General geometry utility functions

/// The `Rect` that bounds the given sequence of vertices.
///
/// Returns `None` if the given iterator is empty.
pub fn bounding_rect<I>(vertices: I) -> Option<Rect<<I::Item as Vertex>::Scalar>>
where
    I: IntoIterator,
    I::Item: Vertex2d,
{
    let mut vertices = vertices.into_iter();
    vertices.next().map(|first| {
        let [x, y] = first.point2();
        let bounds = Rect {
            x: Range::new(x, x),
            y: Range::new(y, y),
        };
        vertices.fold(bounds, |b, v| b.stretch_to_point(v.point2()))
    })
}

/// The `Cuboid` that bounds the given sequence of vertices.
///
/// Returns `None` if the given iterator is empty.
pub fn bounding_cuboid<I>(vertices: I) -> Option<Cuboid<<I::Item as Vertex>::Scalar>>
where
    I: IntoIterator,
    I::Item: Vertex3d,
{
    let mut vertices = vertices.into_iter();
    vertices.next().map(|first| {
        let [x, y, z] = first.point3();
        let bounds = Cuboid {
            x: Range::new(x, x),
            y: Range::new(y, y),
            z: Range::new(z, z),
        };
        vertices.fold(bounds, |b, v| b.stretch_to_point(v.point3()))
    })
}

/// The `centroid` (average position) of all vertices in the given iterator.
///
/// Returns `None` if the given iterator contains no vertices.
pub fn centroid<I>(vertices: I) -> Option<I::Item>
where
    I: IntoIterator,
    I::Item: vertex::Average,
{
    <I::Item as vertex::Average>::average(vertices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec2_angle_matches_upstream() {
        // nannou/tests/geom_tests.rs
        assert_eq!(vec2(1.0, 1.0).angle(), std::f32::consts::FRAC_PI_4);
        assert_eq!(vec2(5.0, 2.0).angle(), 0.380_506_37);
        assert_eq!(vec2(-3.0, 4.0).angle(), 2.214_297_5);
        assert_eq!(vec2(-2.1, -6.7).angle(), -1.874_531);
        assert_eq!(vec2(70.7, -60.8).angle(), -0.710_254_7);
    }

    #[test]
    fn rect_alignment_padding_and_overlap() {
        let outer = Rect::from_w_h(100.0f32, 50.0);
        let inner = Rect::from_w_h(10.0f32, 10.0).top_left_of(outer);
        assert_eq!(inner.top_left(), outer.top_left());
        assert_eq!(inner.xy(), vec2(-45.0, 20.0));
        let mid = Rect::from_w_h(10.0f32, 10.0).middle_of(outer.shift(vec2(5.0, 5.0)));
        assert_eq!(mid.xy(), vec2(5.0, 5.0));
        let padded = outer.pad(5.0);
        assert_eq!(padded.wh(), vec2(90.0, 40.0));
        let a = Rect::from_corners(vec2(0.0, 0.0), vec2(10.0, 10.0));
        let b = Rect::from_corners(vec2(5.0, 5.0), vec2(20.0, 20.0));
        assert_eq!(a.overlap(b).map(|r| r.wh()), Some(vec2(5.0, 5.0)));
        assert!(a.contains(vec2(1.0, 9.0)));
        assert!(!a.contains(vec2(11.0, 9.0)));
        let quarters = a.subdivisions();
        assert_eq!(quarters.len(), 4);
        assert!(quarters.iter().all(|q| q.wh() == vec2(5.0, 5.0)));
    }

    #[test]
    fn range_mapping_and_clamping() {
        let r = Range::new(0.0f32, 10.0);
        assert_eq!(r.map_value(2.5, &Range::new(0.0, 1.0)), 0.25);
        assert_eq!(r.clamp_value(12.0), 10.0);
        assert_eq!(r.middle(), 5.0);
        assert_eq!(Range::new(10.0f32, 0.0).absolute(), r);
        assert_eq!(map_range(3, 0, 10, 0.0, 1.0), 0.3);
        assert_eq!(clamp(1.5, 0.0, 1.0), 1.0);
        assert!((deg_to_rad(180.0f32) - std::f32::consts::PI).abs() < 1e-6);
    }

    #[test]
    fn ellipse_circumference_and_triangles() {
        let rect = Rect::from_w_h(20.0f32, 10.0);
        let points: Vec<[f32; 2]> = Ellipse::new(rect, 16.0).circumference().collect();
        // `resolution` segments: the outline closes on its first point.
        assert_eq!(points.len(), 17);
        assert!((points[0][0] - points[16][0]).abs() < 1e-4 && (points[0][1] - points[16][1]).abs() < 1e-4);
        for [x, y] in &points {
            let e = (x / 10.0).powi(2) + (y / 5.0).powi(2);
            assert!((e - 1.0).abs() < 1e-4, "{x},{y} off the ellipse");
        }
        assert_eq!(Ellipse::new(rect, 16.0).triangles().count(), 16);
    }

    #[test]
    fn tri_polygon_and_cuboid() {
        let t = Tri([vec2(0.0, 0.0), vec2(4.0, 0.0), vec2(0.0, 4.0)]);
        assert!(t.contains(&vec2(1.0, 1.0)));
        assert!(!t.contains(&vec2(3.0, 3.0)));
        assert_eq!(t.centroid(), vec2(4.0 / 3.0, 4.0 / 3.0));
        let square = [vec2(0.0, 0.0), vec2(2.0, 0.0), vec2(2.0, 2.0), vec2(0.0, 2.0)];
        let tris = polygon::triangles(square.iter().copied()).map(Iterator::count);
        assert_eq!(tris, Some(2));
        assert!(polygon::contains(square.iter().copied(), &vec2(1.5, 0.5)).is_some());
        let bounds = bounding_rect(square.iter().copied()).map(|r| r.wh());
        assert_eq!(bounds, Some(vec2(2.0, 2.0)));
        let c = Cuboid::from_xyz_whd(Vec3::ZERO, vec3(2.0, 4.0, 6.0));
        assert_eq!(c.volume(), 48.0);
        assert_eq!(c.faces().len(), 6);
        assert_eq!(c.triangles_iter().count(), 12);
    }
}
