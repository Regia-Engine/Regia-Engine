// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/primitive/line.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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
use lyon::tessellation::{LineCap, StrokeOptions};
use regia_render::primitives::triangulation;

use crate::draw::primitive::path;
use crate::draw::primitive::{PathStroke, Primitive};
use crate::draw::properties::spatial::{orientation, position};
use crate::draw::properties::{SetColor, SetOrientation, SetPosition, SetStroke};
use crate::draw::{self, Drawing};

/// A path containing only two points - a start and end.
///
/// The usage of this type is almost identical to `PathStroke` but provides `start`, `end` and
/// `points(a, b)` methods.
#[derive(Clone, Debug, Default)]
pub struct Line {
    /// The stroke options, colour and placement.
    pub path: PathStroke,
    /// Start point (origin when unset).
    pub start: Option<Vec2>,
    /// End point (origin when unset).
    pub end: Option<Vec2>,
}

/// The drawing context for a line.
pub type DrawingLine<'a> = Drawing<'a, Line>;

impl Line {
    /// Short-hand for the `stroke_weight` method.
    pub fn weight(self, weight: f32) -> Self {
        self.map_path(|p| p.stroke_weight(weight))
    }

    /// Short-hand for the `stroke_tolerance` method.
    pub fn tolerance(self, tolerance: f32) -> Self {
        self.map_path(|p| p.stroke_tolerance(tolerance))
    }

    /// Specify the start point of the line.
    pub fn start(mut self, start: Vec2) -> Self {
        self.start = Some(start);
        self
    }

    /// Specify the end point of the line.
    pub fn end(mut self, end: Vec2) -> Self {
        self.end = Some(end);
        self
    }

    /// Specify the start and end points of the line.
    pub fn points(self, start: Vec2, end: Vec2) -> Self {
        self.start(start).end(end)
    }

    // Map the inner `PathStroke<S>` using the given function.
    fn map_path<F>(self, map: F) -> Self
    where
        F: FnOnce(PathStroke) -> PathStroke,
    {
        let Line { path, start, end } = self;
        let path = map(path);
        Line { path, start, end }
    }
}

impl<'a> DrawingLine<'a> {
    /// Short-hand for the `stroke_weight` method.
    pub fn weight(self, weight: f32) -> Self {
        self.stroke_weight(weight)
    }

    /// Short-hand for the `stroke_tolerance` method.
    pub fn tolerance(self, tolerance: f32) -> Self {
        self.stroke_tolerance(tolerance)
    }

    /// Specify the start point of the line.
    pub fn start(self, start: Vec2) -> Self {
        update_line(self.draw, self.index, |line| line.start = Some(start));
        self
    }

    /// Specify the end point of the line.
    pub fn end(self, end: Vec2) -> Self {
        update_line(self.draw, self.index, |line| line.end = Some(end));
        self
    }

    /// Specify the start and end points of the line.
    pub fn points(self, start: Vec2, end: Vec2) -> Self {
        self.start(start).end(end)
    }
}

// Update the inner `Line` of the primitive being drawn at `index`.
fn update_line(draw: &crate::draw::Draw, index: usize, f: impl FnOnce(&mut Line)) {
    crate::draw::drawing::with_primitive(draw, index, |prim| match prim {
        Primitive::Line(line) => f(line),
        _ => crate::warn_once!("expected a `Line` primitive"),
    })
}

impl SetStroke for Line {
    fn stroke_options_mut(&mut self) -> &mut StrokeOptions {
        SetStroke::stroke_options_mut(&mut self.path)
    }
}

impl SetOrientation for Line {
    fn properties(&mut self) -> &mut orientation::Properties {
        SetOrientation::properties(&mut self.path)
    }
}

impl SetPosition for Line {
    fn properties(&mut self) -> &mut position::Properties {
        SetPosition::properties(&mut self.path)
    }
}

impl SetColor for Line {
    fn color_mut(&mut self) -> &mut Option<Color> {
        SetColor::color_mut(&mut self.path)
    }
}

impl From<Line> for Primitive {
    fn from(prim: Line) -> Self {
        Primitive::Line(prim)
    }
}

impl draw::render::RenderPrimitive for Line {
    fn render_primitive(self, ctxt: draw::render::RenderContext, mesh: &mut Mesh) {
        let Line { path, start, end } = self;
        let start = start.unwrap_or(Vec2::new(0.0, 0.0));
        let end = end.unwrap_or(Vec2::new(0.0, 0.0));
        if start == end || !start.is_finite() || !end.is_finite() {
            return;
        }
        // A non-positive or non-finite width draws nothing, like a polyline stroke.
        let width = path.opts.line_width;
        if !(width.is_finite() && width > 0.0) {
            return;
        }
        // Fast path: one segment with equal caps is a quad or a capsule.
        let opts = &path.opts;
        if opts.start_cap == opts.end_cap {
            let m = *ctxt.transform * path.position.transform() * path.orientation.transform();
            // A line is a stroke: the equal-cap fast path uses the stroke theme, as the lyon path does.
            let color = path
                .color
                .unwrap_or_else(|| ctxt.theme.stroke(&draw::theme::Primitive::Line));
            let half = opts.line_width * 0.5;
            let d = end - start;
            let len = d.length();
            let uv = move |p: [f32; 2]| {
                let t = (Vec2::from(p) - start).dot(d) / (len * len);
                [t, 0.0]
            };
            match opts.start_cap {
                LineCap::Round => {
                    let segments = draw::fast::circle_segments_with_tolerance(
                        half,
                        &m,
                        ctxt.output_attachment_scale_factor,
                        opts.tolerance,
                    );
                    triangulation::with_round_border_line_tri_list(
                        (segments / 2).max(2),
                        draw::fast::LOCAL,
                        [start.x, start.y, end.x, end.y],
                        half,
                        |tris| draw::fast::push_tris(mesh, &m, tris, color, uv),
                    );
                }
                cap => {
                    let ext = if cap == LineCap::Square {
                        d / len * half
                    } else {
                        Vec2::ZERO
                    };
                    let n = Vec2::new(-d.y, d.x) / len * half;
                    let (a, b) = (start - ext, end + ext);
                    let quad = [a + n, a - n, b - n, a + n, b - n, b + n].map(|p| [p.x, p.y]);
                    draw::fast::push_tris(mesh, &m, &quad, color, uv);
                }
            }
            return;
        }
        let close = false;
        let points = [start, end];
        let tex_coords = [Vec2::new(0.0, 0.0), Vec2::new(1.0, 0.0)];
        let points = points.iter().cloned().zip(tex_coords.iter().copied());

        // Determine the transform to apply to all points.
        let global_transform = *ctxt.transform;
        let local_transform = path.position.transform() * path.orientation.transform();
        let transform = global_transform * local_transform;

        path::render_path_points_themed(
            points,
            close,
            path.color,
            transform,
            path::Options::Stroke(path.opts),
            ctxt.theme,
            &draw::theme::Primitive::Line,
            ctxt.fill_tessellator,
            ctxt.stroke_tessellator,
            mesh,
        );
    }
}
