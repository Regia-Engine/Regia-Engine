// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/primitive/tri.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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
use crate::draw::mesh::Mesh;
use crate::prelude_internal::*;
use lyon::tessellation::StrokeOptions;

use regia_core::math::geom;

use crate::draw::primitive::Primitive;
use crate::draw::primitive::polygon::{self, PolygonInit, PolygonOptions, SetPolygon};
use crate::draw::properties::spatial::{dimension, orientation, position};
use crate::draw::properties::{SetColor, SetDimensions, SetOrientation, SetPosition, SetStroke};
use crate::draw::{self, Drawing};

/// Properties related to drawing a **Tri**.
#[derive(Clone, Debug)]
pub struct Tri {
    tri: geom::Tri<Vec2>,
    dimensions: dimension::Properties,
    polygon: PolygonInit,
}

/// The drawing context for a `Tri`.
pub type DrawingTri<'a> = Drawing<'a, Tri>;

// Tri-specific methods.

impl Tri {
    /// Stroke the outline with the given color.
    pub fn stroke<C>(self, color: C) -> Self
    where
        C: Into<Color>,
    {
        self.stroke_color(color)
    }

    /// Use the given three points as the vertices (corners) of the triangle.
    pub fn points<P>(mut self, a: P, b: P, c: P) -> Self
    where
        P: Into<Vec2>,
    {
        let a = a.into();
        let b = b.into();
        let c = c.into();
        self.tri = geom::Tri([a, b, c]);
        self
    }
}

// Drawing methods.

impl<'a> DrawingTri<'a> {
    /// Stroke the outline with the given color.
    pub fn stroke<C>(self, color: C) -> Self
    where
        C: Into<Color>,
    {
        self.stroke_color(color)
    }

    /// Use the given points as the vertices (corners) of the triangle.
    pub fn points<P>(self, a: P, b: P, c: P) -> Self
    where
        P: Into<Vec2>,
    {
        set_points(self.draw, self.index, geom::Tri([a.into(), b.into(), c.into()]));
        self
    }
}

// Set the corner points of the `Tri` primitive being drawn at `index`.
//
// Non-generic so that the body compiles here rather than in the caller's crate.
fn set_points(draw: &crate::draw::Draw, index: usize, points: geom::Tri<Vec2>) {
    crate::draw::drawing::with_primitive(draw, index, |prim| match prim {
        Primitive::Tri(tri) => tri.tri = points,
        _ => crate::warn_once!("expected a `Tri` primitive"),
    })
}

// Trait implementations.

impl draw::render::RenderPrimitive for Tri {
    fn render_primitive(self, ctxt: draw::render::RenderContext, mesh: &mut Mesh) {
        let Tri {
            mut tri,
            dimensions,
            polygon,
        } = self;
        let (maybe_x, maybe_y, _maybe_z) = (dimensions.x, dimensions.y, dimensions.z);
        // If dimensions were specified, scale the points to those dimensions.
        if maybe_x.is_some() || maybe_y.is_some() {
            let cuboid = tri.bounding_rect();
            let centroid = tri.centroid();
            let x_scale = maybe_x
                .filter(|_| cuboid.w() != 0.0)
                .map(|x| x / cuboid.w())
                .unwrap_or(1.0);
            let y_scale = maybe_y
                .filter(|_| cuboid.h() != 0.0)
                .map(|y| y / cuboid.h())
                .unwrap_or(1.0);
            let scale = Vec2::new(x_scale, y_scale);
            let (a, b, c) = tri.into();
            let translate = |v: Vec2| centroid + ((v - centroid) * scale);
            let new_a = translate(a);
            let new_b = translate(b);
            let new_c = translate(c);
            tri = geom::Tri([new_a, new_b, new_c]);
        }

        let tex_coords = [
            Vec2::new(0.0, 0.0), // Vertex A
            Vec2::new(1.0, 0.0), // Vertex B
            Vec2::new(0.5, 1.0), // Vertex C
        ];

        // Fast path: a plain filled convex shape is an indexed fan, no tessellator.
        let corners = tri.0;
        if polygon.opts.stroke.is_none() && !polygon.opts.no_fill {
            let opts = &polygon.opts;
            let m = *ctxt.transform * opts.position.transform() * opts.orientation.transform();
            let color = opts
                .color
                .unwrap_or_else(|| ctxt.theme.fill(&draw::theme::Primitive::Tri));
            let points: [(Vec2, Vec2); 3] = std::array::from_fn(|i| (corners[i], tex_coords[i]));
            draw::fast::push_convex_fill(mesh, &m, &points, color);
            return;
        }
        if let Some(stroke) = polygon.opts.stroke
            && draw::fast::stroke_is_plain(&stroke)
        {
            let opts = &polygon.opts;
            let m = *ctxt.transform * opts.position.transform() * opts.orientation.transform();
            let poly: [(Vec2, Vec2); 3] = std::array::from_fn(|i| (corners[i], tex_coords[i]));
            let fill = (!opts.no_fill).then(|| {
                opts.color
                    .unwrap_or_else(|| ctxt.theme.fill(&draw::theme::Primitive::Tri))
            });
            let stroke_color = opts
                .stroke_color
                .unwrap_or_else(|| ctxt.theme.stroke(&draw::theme::Primitive::Tri));
            if draw::fast::push_fill_and_miter_ring(
                mesh,
                &m,
                &poly,
                stroke.line_width * 0.5,
                stroke.miter_limit,
                fill,
                stroke_color,
            ) {
                return;
            }
        }
        let points = tri.vertices().zip(tex_coords.iter().copied());

        polygon::render_points_themed(
            polygon.opts,
            true,
            points,
            ctxt,
            &draw::theme::Primitive::Tri,
            mesh,
        );
    }
}

impl From<geom::Tri<Vec2>> for Tri {
    fn from(tri: geom::Tri<Vec2>) -> Self {
        let dimensions = <_>::default();
        let polygon = <_>::default();
        Tri {
            tri,
            dimensions,
            polygon,
        }
    }
}

impl Default for Tri {
    fn default() -> Self {
        // Create a triangle pointing towards 0.0 radians.
        let fifty = 50.0;
        let thirty_three = 33.0;
        let a = Vec2::new(-fifty, thirty_three);
        let b = Vec2::new(fifty, 0.0);
        let c = Vec2::new(-fifty, -thirty_three);
        Tri::from(geom::Tri([a, b, c]))
    }
}

impl SetOrientation for Tri {
    fn properties(&mut self) -> &mut orientation::Properties {
        SetOrientation::properties(&mut self.polygon)
    }
}

impl SetPosition for Tri {
    fn properties(&mut self) -> &mut position::Properties {
        SetPosition::properties(&mut self.polygon)
    }
}

impl SetDimensions for Tri {
    fn properties(&mut self) -> &mut dimension::Properties {
        SetDimensions::properties(&mut self.dimensions)
    }
}

impl SetColor for Tri {
    fn color_mut(&mut self) -> &mut Option<Color> {
        SetColor::color_mut(&mut self.polygon)
    }
}

impl SetStroke for Tri {
    fn stroke_options_mut(&mut self) -> &mut StrokeOptions {
        SetStroke::stroke_options_mut(&mut self.polygon)
    }
}

impl SetPolygon for Tri {
    fn polygon_options_mut(&mut self) -> &mut PolygonOptions {
        SetPolygon::polygon_options_mut(&mut self.polygon)
    }
}

// Primitive conversions.

impl From<Tri> for Primitive {
    fn from(prim: Tri) -> Self {
        Primitive::Tri(prim)
    }
}
