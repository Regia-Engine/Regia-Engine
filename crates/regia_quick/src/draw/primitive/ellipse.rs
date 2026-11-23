// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/primitive/ellipse.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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

use crate::draw;
use crate::draw::Drawing;
use crate::draw::primitive::Primitive;
use crate::draw::primitive::polygon::{PolygonInit, PolygonOptions, SetPolygon};
use crate::draw::properties::spatial::{dimension, orientation, position};
use crate::draw::properties::{SetColor, SetDimensions, SetOrientation, SetPosition, SetStroke, spatial};

/// Properties related to drawing an **Ellipse**.
#[derive(Clone, Debug, Default)]
pub struct Ellipse {
    dimensions: spatial::dimension::Properties,
    resolution: Option<f32>,
    polygon: PolygonInit,
}

/// The drawing context for an ellipse.
pub type DrawingEllipse<'a> = Drawing<'a, Ellipse>;

// Ellipse-specific methods.

impl Ellipse {
    /// Stroke the outline with the given color.
    pub fn stroke<C>(self, color: C) -> Self
    where
        C: Into<Color>,
    {
        self.stroke_color(color)
    }

    /// Specify the width and height of the **Ellipse** via a given **radius**.
    pub fn radius(self, radius: f32) -> Self {
        let side = radius * 2.0;
        self.w_h(side, side)
    }

    /// The number of sides used to draw the ellipse.
    ///
    /// By default, ellipse does not use a resolution. The side count comes from the stroke
    /// tolerance (`StrokeOptions::tolerance`, 0.1 when the caller leaves it) so the polygon
    /// stays within that distance of the curve.
    pub fn resolution(mut self, resolution: f32) -> Self {
        self.resolution = Some(resolution);
        self
    }
}

// Trait implementations.

impl draw::render::RenderPrimitive for Ellipse {
    fn render_primitive(self, ctxt: draw::render::RenderContext, mesh: &mut Mesh) {
        let Ellipse {
            dimensions,
            polygon,
            resolution,
        } = self;

        // First get the dimensions of the ellipse (a depth is ignored: ellipses are flat).
        let w = dimensions.x.map(f32::abs).unwrap_or(100.0);
        let h = dimensions.y.map(f32::abs).unwrap_or(100.0);
        if w <= 0.0 && h <= 0.0 {
            return;
        }
        let opts = &polygon.opts;
        let m = *ctxt.transform * opts.position.transform() * opts.orientation.transform();
        // A caller-set stroke tolerance replaces the 0.1 px default. No stroke keeps 0.1.
        // An explicit resolution (side count) wins over both.
        let radius = w.max(h) * 0.5;
        let scale = ctxt.output_attachment_scale_factor;
        let segments = resolution.map_or_else(
            || match opts.stroke.map(|stroke| stroke.tolerance) {
                Some(tolerance) => draw::fast::circle_segments_with_tolerance(radius, &m, scale, tolerance),
                None => draw::fast::circle_segments(radius, &m, scale),
            },
            |r| (r.max(3.0) as u32).min(1024),
        );
        let rect = [-w * 0.5, -h * 0.5, w, h];
        // Texture coordinates span the bounding box, v down.
        let uv = |p: [f32; 2]| [p[0] / w.max(1e-6) + 0.5, 0.5 - p[1] / h.max(1e-6)];
        if !opts.no_fill {
            let color = opts
                .color
                .unwrap_or_else(|| ctxt.theme.fill(&draw::theme::Primitive::Ellipse));
            draw::fast::push_ellipse_fill(mesh, &m, rect, segments, color, uv);
        }
        if let Some(stroke) = opts.stroke {
            let color = opts
                .stroke_color
                .unwrap_or_else(|| ctxt.theme.stroke(&draw::theme::Primitive::Ellipse));
            draw::fast::push_ellipse_ring(mesh, &m, rect, segments, stroke.line_width * 0.5, color, uv);
        }
    }
}

impl SetOrientation for Ellipse {
    fn properties(&mut self) -> &mut orientation::Properties {
        SetOrientation::properties(&mut self.polygon)
    }
}

impl SetPosition for Ellipse {
    fn properties(&mut self) -> &mut position::Properties {
        SetPosition::properties(&mut self.polygon)
    }
}

impl SetDimensions for Ellipse {
    fn properties(&mut self) -> &mut dimension::Properties {
        SetDimensions::properties(&mut self.dimensions)
    }
}

impl SetColor for Ellipse {
    fn color_mut(&mut self) -> &mut Option<Color> {
        SetColor::color_mut(&mut self.polygon)
    }
}

impl SetStroke for Ellipse {
    fn stroke_options_mut(&mut self) -> &mut StrokeOptions {
        SetStroke::stroke_options_mut(&mut self.polygon)
    }
}

impl SetPolygon for Ellipse {
    fn polygon_options_mut(&mut self) -> &mut PolygonOptions {
        SetPolygon::polygon_options_mut(&mut self.polygon)
    }
}

// Primitive conversion.

impl From<Ellipse> for Primitive {
    fn from(prim: Ellipse) -> Self {
        Primitive::Ellipse(prim)
    }
}

// Drawing methods.

impl<'a> DrawingEllipse<'a> {
    /// Stroke the outline with the given color.
    pub fn stroke<C>(self, color: C) -> Self
    where
        C: Into<Color>,
    {
        self.stroke_color(color)
    }

    /// Specify the width and height of the **Ellipse** via a given **radius**.
    pub fn radius(self, radius: f32) -> Self {
        let side = radius * 2.0;
        self.w_h(side, side)
    }

    /// The number of sides used to draw the ellipse.
    pub fn resolution(self, resolution: f32) -> Self {
        update_ellipse(self.draw, self.index, |ellipse| {
            ellipse.resolution = Some(resolution)
        });
        self
    }
}

// Update the inner `Ellipse` of the primitive being drawn at `index`.
fn update_ellipse(draw: &crate::draw::Draw, index: usize, f: impl FnOnce(&mut Ellipse)) {
    crate::draw::drawing::with_primitive(draw, index, |prim| match prim {
        Primitive::Ellipse(ellipse) => f(ellipse),
        _ => crate::warn_once!("expected an `Ellipse` primitive"),
    })
}
