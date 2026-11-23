// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/primitive/arrow.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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

use crate::draw::primitive::Line;
use crate::draw::primitive::Primitive;
use crate::draw::primitive::path;
use crate::draw::properties::spatial::{orientation, position};
use crate::draw::properties::{SetColor, SetOrientation, SetPosition, SetStroke};
use crate::draw::{self, Drawing};

/// A path containing only two points - a start and end.
///
/// A triangle is drawn on the end to indicate direction.
#[derive(Clone, Debug)]
pub struct Arrow {
    line: Line,
    head_length: Option<f32>,
    head_width: Option<f32>,
}

/// The drawing context for a line.
pub type DrawingArrow<'a> = Drawing<'a, Arrow>;

impl Arrow {
    /// Short-hand for the `stroke_weight` method.
    pub fn weight(self, weight: f32) -> Self {
        self.map_line(|l| l.weight(weight))
    }

    /// Short-hand for the `stroke_tolerance` method.
    pub fn tolerance(self, tolerance: f32) -> Self {
        self.map_line(|l| l.tolerance(tolerance))
    }

    /// Specify the start point of the arrow.
    pub fn start(self, start: Vec2) -> Self {
        self.map_line(|l| l.start(start))
    }

    /// Specify the end point of the arrow.
    pub fn end(self, end: Vec2) -> Self {
        self.map_line(|l| l.end(end))
    }

    /// Specify the start and end points of the arrow.
    pub fn points(self, start: Vec2, end: Vec2) -> Self {
        self.map_line(|l| l.points(start, end))
    }

    /// The length of the arrow head.
    ///
    /// By default, this is equal to `weight * 4.0`.
    ///
    /// This value will be clamped to the length of the line itself.
    pub fn head_length(mut self, length: f32) -> Self {
        self.head_length = Some(length);
        self
    }

    /// Distance from the shaft centerline to each corner of the arrow head.
    /// The full width of the base is twice this value.
    ///
    /// By default, this distance is `weight * 2.0`, so the full base is `weight * 4.0`.
    pub fn head_width(mut self, width: f32) -> Self {
        self.head_width = Some(width);
        self
    }

    // Map the inner `PathStroke<S>` using the given function.
    fn map_line<F>(self, map: F) -> Self
    where
        F: FnOnce(Line) -> Line,
    {
        let Arrow {
            line,
            head_length,
            head_width,
        } = self;
        let line = map(line);
        Arrow {
            line,
            head_length,
            head_width,
        }
    }
}

impl<'a> DrawingArrow<'a> {
    /// Short-hand for the `stroke_weight` method.
    pub fn weight(self, weight: f32) -> Self {
        self.stroke_weight(weight)
    }

    /// Short-hand for the `stroke_tolerance` method.
    pub fn tolerance(self, tolerance: f32) -> Self {
        self.stroke_tolerance(tolerance)
    }

    /// Specify the start point of the arrow.
    pub fn start(self, start: Vec2) -> Self {
        update_arrow(self.draw, self.index, |arrow| arrow.line.start = Some(start));
        self
    }

    /// Specify the end point of the arrow.
    pub fn end(self, end: Vec2) -> Self {
        update_arrow(self.draw, self.index, |arrow| arrow.line.end = Some(end));
        self
    }

    /// Specify the start and end points of the arrow.
    pub fn points(self, start: Vec2, end: Vec2) -> Self {
        self.start(start).end(end)
    }

    /// The length of the arrow head.
    ///
    /// By default, this is equal to `weight * 4.0`.
    ///
    /// This value will be clamped to the length of the line itself.
    pub fn head_length(self, length: f32) -> Self {
        update_arrow(self.draw, self.index, |arrow| arrow.head_length = Some(length));
        self
    }

    /// Distance from the shaft centerline to each corner of the arrow head.
    /// The full width of the base is twice this value.
    ///
    /// By default, this distance is `weight * 2.0`, so the full base is `weight * 4.0`.
    pub fn head_width(self, width: f32) -> Self {
        update_arrow(self.draw, self.index, |arrow| arrow.head_width = Some(width));
        self
    }
}

impl SetStroke for Arrow {
    fn stroke_options_mut(&mut self) -> &mut StrokeOptions {
        SetStroke::stroke_options_mut(&mut self.line)
    }
}

impl SetOrientation for Arrow {
    fn properties(&mut self) -> &mut orientation::Properties {
        SetOrientation::properties(&mut self.line)
    }
}

impl SetPosition for Arrow {
    fn properties(&mut self) -> &mut position::Properties {
        SetPosition::properties(&mut self.line)
    }
}

impl SetColor for Arrow {
    fn color_mut(&mut self) -> &mut Option<Color> {
        SetColor::color_mut(&mut self.line)
    }
}

impl From<Arrow> for Primitive {
    fn from(prim: Arrow) -> Self {
        Primitive::Arrow(prim)
    }
}

impl draw::render::RenderPrimitive for Arrow {
    fn render_primitive(self, ctxt: draw::render::RenderContext, mesh: &mut Mesh) {
        let Arrow {
            line,
            head_length,
            head_width,
        } = self;
        let start = line.start.unwrap_or(Vec2::new(0.0, 0.0));
        let end = line.end.unwrap_or(Vec2::new(0.0, 0.0));
        if start == end {
            return;
        }

        // Calculate the arrow head points.
        let line_w_2 = line.path.opts.line_width * 2.0;
        let line_w_4 = line_w_2 * 2.0;
        let head_width = head_width.unwrap_or(line_w_2);
        let head_length = head_length.unwrap_or(line_w_4);
        let line_dir = end - start;
        let line_dir_len = line_dir.length();
        let tri_len = head_length.min(line_dir_len);
        let tri_dir_norm = line_dir.normalize() * tri_len;
        let tri_start = end - tri_dir_norm;
        let tri_end = end;
        let line_start = start;
        let line_end = tri_start;
        let tri_a = tri_end;
        let tri_w_dir = Vec2::new(-tri_dir_norm.y, tri_dir_norm.x).normalize_or_zero() * head_width;
        let tri_b = tri_start + tri_w_dir;
        let tri_c = tri_start - tri_w_dir;
        // The line should only be drawn if there is space after drawing the triangle.
        let draw_line = line_dir_len > tri_len;

        // Determine the transform to apply to all points.
        let global_transform = *ctxt.transform;
        let local_transform = line.path.position.transform() * line.path.orientation.transform();
        let transform = global_transform * local_transform;

        // Plain butt or square caps: a 3-vertex head and one indexed quad for the shaft.
        // Round caps stay on lyon so the cap tessellation is unchanged.
        if draw::fast::stroke_is_plain(&line.path.opts) {
            let head_color = line
                .path
                .color
                .unwrap_or_else(|| ctxt.theme.fill(&draw::theme::Primitive::Arrow));
            let shaft_color = line
                .path
                .color
                .unwrap_or_else(|| ctxt.theme.stroke(&draw::theme::Primitive::Arrow));
            let head = [
                (tri_a, Vec2::new(0.5, 1.0)),
                (tri_b, Vec2::new(0.0, 0.0)),
                (tri_c, Vec2::new(1.0, 0.0)),
            ];
            draw::fast::push_convex_fill(mesh, &transform, &head, head_color);
            if draw_line {
                let d = line_end - line_start;
                let len = d.length();
                if len > 1e-8 {
                    let half = line.path.opts.line_width * 0.5;
                    let ext = if line.path.opts.start_cap == lyon::tessellation::LineCap::Square {
                        d / len * half
                    } else {
                        Vec2::ZERO
                    };
                    let n = Vec2::new(-d.y, d.x) / len * half;
                    let (a, b) = (line_start - ext, line_end + ext);
                    let quad = [a + n, a - n, b - n, b + n];
                    let uv = move |p: Vec2| {
                        let t = (p - line_start).dot(d) / (len * len);
                        [t, 0.0]
                    };
                    draw::fast::push_quad(mesh, &transform, quad, shaft_color, uv);
                }
            }
            return;
        }

        // Draw the tri.
        let tri_points = [tri_a, tri_b, tri_c];
        let tri_tex_coords = [
            Vec2::new(0.5, 1.0), // Tip of the arrowhead
            Vec2::new(0.0, 0.0), // Left corner
            Vec2::new(1.0, 0.0), // Right corner
        ];
        let tri_points = tri_points.iter().cloned().zip(tri_tex_coords.iter().copied());
        let close_tri = true;
        path::render_path_points_themed(
            tri_points,
            close_tri,
            line.path.color,
            transform,
            path::Options::Fill(Default::default()),
            ctxt.theme,
            &draw::theme::Primitive::Arrow,
            ctxt.fill_tessellator,
            ctxt.stroke_tessellator,
            mesh,
        );

        // Draw the line.
        if draw_line {
            let line_points = [line_start, line_end];
            let line_tex_coords = [Vec2::new(0.0, 0.0), Vec2::new(1.0, 0.0)];
            let line_points = line_points.iter().cloned().zip(line_tex_coords.iter().copied());
            let close_line = false;
            path::render_path_points_themed(
                line_points,
                close_line,
                line.path.color,
                transform,
                path::Options::Stroke(line.path.opts),
                ctxt.theme,
                &draw::theme::Primitive::Arrow,
                ctxt.fill_tessellator,
                ctxt.stroke_tessellator,
                mesh,
            );
        }
    }
}

impl Default for Arrow {
    fn default() -> Self {
        let line = Default::default();
        let head_length = Default::default();
        let head_width = Default::default();
        Arrow {
            line,
            head_length,
            head_width,
        }
    }
}

// Update the inner `Arrow` of the primitive being drawn at `index`.
fn update_arrow(draw: &crate::draw::Draw, index: usize, f: impl FnOnce(&mut Arrow)) {
    crate::draw::drawing::with_primitive(draw, index, |prim| match prim {
        Primitive::Arrow(arrow) => f(arrow),
        _ => crate::warn_once!("expected an `Arrow` primitive"),
    })
}
