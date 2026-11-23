// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/primitive/rect.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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
use crate::draw::properties::tex_coords::{self, SetTexCoords};
use crate::draw::properties::{SetColor, SetDimensions, SetOrientation, SetPosition, SetStroke};
use crate::draw::{self, Drawing};

/// Properties related to drawing a **Rect**.
#[derive(Clone, Debug)]
pub struct Rect {
    dimensions: dimension::Properties,
    tex_coords: Option<geom::Rect>,
    polygon: PolygonInit,
}

/// The drawing context for a Rect.
pub type DrawingRect<'a> = Drawing<'a, Rect>;

// Trait implementations.

impl Rect {
    /// Stroke the outline with the given color.
    pub fn stroke<C>(self, color: C) -> Self
    where
        C: Into<Color>,
    {
        self.stroke_color(color)
    }
}

impl<'a> DrawingRect<'a> {
    /// Stroke the outline with the given color.
    pub fn stroke<C>(self, color: C) -> Self
    where
        C: Into<Color>,
    {
        self.stroke_color(color)
    }

    /// Texture coordinates: sample `area` (in `0..1` texture space) of the image.
    pub fn area(self, area: geom::Rect) -> Self {
        tex_coords::set_tex_coords_area(self.draw, self.index, area);
        self
    }
}

impl draw::render::RenderPrimitive for Rect {
    fn render_primitive(self, ctxt: draw::render::RenderContext, mesh: &mut Mesh) {
        let Rect {
            tex_coords,
            polygon,
            dimensions,
        } = self;

        // If dimensions were specified, scale the points to those dimensions (a depth is
        // ignored: rectangles are flat).
        let w = dimensions.x.unwrap_or(100.0);
        let h = dimensions.y.unwrap_or(100.0);
        let rect = geom::Rect::from_wh([w, h].into());

        // Fast path: a plain filled rectangle is four vertices, no tessellator.
        if polygon.opts.stroke.is_none() && !polygon.opts.no_fill {
            let opts = &polygon.opts;
            let m = *ctxt.transform * opts.position.transform() * opts.orientation.transform();
            let color = opts
                .color
                .unwrap_or_else(|| ctxt.theme.fill(&draw::theme::Primitive::Rect));
            let uv = tex_coords.map_or([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]], |area| {
                [
                    area.bottom_left(),
                    area.bottom_right(),
                    area.top_right(),
                    area.top_left(),
                ]
                .map(|p| [p.x, p.y])
            });
            let corners = rect.corners().0;
            let base = mesh.vertices.len() as u32;
            for (corner, uv) in corners.iter().zip(uv) {
                let p = m.transform_point3(Vec3::new(corner[0], corner[1], 0.0));
                mesh.vertices.push(regia_render::ImmediateVertex::textured(
                    p.to_array(),
                    uv,
                    color.to_array(),
                ));
            }
            mesh.indices
                .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
            return;
        }

        let tex_coords = tex_coords
            .map(|area| {
                // `area` is in texture space (v down), the corners run top-left, top-right,
                // bottom-right, bottom-left: the visual top samples the area's smaller v.
                [
                    area.bottom_left(),
                    area.bottom_right(),
                    area.top_right(),
                    area.top_left(),
                ]
            })
            .unwrap_or([
                Vec2::new(0.0, 0.0), // Top-left
                Vec2::new(1.0, 0.0), // Top-right
                Vec2::new(1.0, 1.0), // Bottom-right
                Vec2::new(0.0, 1.0), // Bottom-left
            ]);

        // A plain miter stroke is the fill plus an eight-vertex ring. Round caps and clipping
        // miters fall through to lyon.
        if let Some(stroke) = polygon.opts.stroke
            && crate::draw::fast::stroke_is_plain(&stroke)
        {
            let opts = &polygon.opts;
            let m = *ctxt.transform * opts.position.transform() * opts.orientation.transform();
            let corners = rect.corners().0;
            let poly: [(Vec2, Vec2); 4] =
                std::array::from_fn(|i| (Vec2::new(corners[i][0], corners[i][1]), tex_coords[i]));
            let fill = (!opts.no_fill).then(|| {
                opts.color
                    .unwrap_or_else(|| ctxt.theme.fill(&draw::theme::Primitive::Rect))
            });
            let stroke_color = opts
                .stroke_color
                .unwrap_or_else(|| ctxt.theme.stroke(&draw::theme::Primitive::Rect));
            if crate::draw::fast::push_fill_and_miter_ring(
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

        let points = rect
            .corners()
            .vertices()
            .map(Vec2::from)
            .zip(tex_coords.iter().copied());

        polygon::render_points_themed(
            polygon.opts,
            true,
            points,
            ctxt,
            &draw::theme::Primitive::Rect,
            mesh,
        );
    }
}

impl From<geom::Rect<f32>> for Rect {
    fn from(r: geom::Rect<f32>) -> Self {
        let (x, y, w, h) = r.x_y_w_h();
        Self::default().x_y(x, y).w_h(w, h)
    }
}

impl Default for Rect {
    fn default() -> Self {
        let dimensions = <_>::default();
        let polygon = <_>::default();
        Rect {
            dimensions,
            tex_coords: None,
            polygon,
        }
    }
}

impl SetOrientation for Rect {
    fn properties(&mut self) -> &mut orientation::Properties {
        SetOrientation::properties(&mut self.polygon)
    }
}

impl SetPosition for Rect {
    fn properties(&mut self) -> &mut position::Properties {
        SetPosition::properties(&mut self.polygon)
    }
}

impl SetDimensions for Rect {
    fn properties(&mut self) -> &mut dimension::Properties {
        SetDimensions::properties(&mut self.dimensions)
    }
}

impl SetColor for Rect {
    fn color_mut(&mut self) -> &mut Option<Color> {
        SetColor::color_mut(&mut self.polygon)
    }
}

impl SetStroke for Rect {
    fn stroke_options_mut(&mut self) -> &mut StrokeOptions {
        SetStroke::stroke_options_mut(&mut self.polygon)
    }
}

impl SetPolygon for Rect {
    fn polygon_options_mut(&mut self) -> &mut PolygonOptions {
        SetPolygon::polygon_options_mut(&mut self.polygon)
    }
}

impl SetTexCoords for Rect {
    fn tex_coords_mut(&mut self) -> &mut Option<geom::Rect> {
        SetTexCoords::tex_coords_mut(&mut self.tex_coords)
    }
}

// Primitive conversions.

impl From<Rect> for Primitive {
    fn from(prim: Rect) -> Self {
        Primitive::Rect(prim)
    }
}
