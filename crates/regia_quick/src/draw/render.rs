// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/render/mod.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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
use lyon::path::PathEvent;
use lyon::tessellation::{FillTessellator, StrokeTessellator};

use crate::draw;
use crate::draw::mesh::Mesh;
use crate::prelude_internal::*;

/// Draw API primitives that may be rendered into a [`Mesh`].
pub trait RenderPrimitive {
    /// Render self into the given mesh.
    fn render_primitive(self, ctxt: RenderContext, mesh: &mut Mesh);
}

/// The context provided to primitives to assist with the rendering process.
pub struct RenderContext<'a> {
    /// The draw context's transform.
    pub transform: &'a Mat4,
    /// Vertices and indices of mesh drawings.
    pub intermediary_mesh: &'a Mesh,
    /// Path events of path and polygon drawings.
    pub path_event_buffer: &'a [PathEvent],
    /// Per-point colour and texture coordinates of path and polygon drawings.
    pub path_points_vertex_buffer: &'a [(Vec2, Color, Vec2)],
    /// Text of text drawings.
    pub text_buffer: &'a str,
    /// Default colours.
    pub theme: &'a draw::Theme,
    /// Fill tessellator (reused across primitives).
    pub fill_tessellator: &'a mut FillTessellator,
    /// Stroke tessellator (reused across primitives).
    pub stroke_tessellator: &'a mut StrokeTessellator,
    /// Window size in logical units.
    pub output_attachment_size: Vec2,
    /// Physical pixels per logical unit.
    pub output_attachment_scale_factor: f32,
}

impl RenderPrimitive for draw::Primitive {
    fn render_primitive(self, ctxt: RenderContext, mesh: &mut Mesh) {
        match self {
            draw::Primitive::Arrow(prim) => prim.render_primitive(ctxt, mesh),
            draw::Primitive::Mesh(prim) => prim.render_primitive(ctxt, mesh),
            draw::Primitive::Path(prim) => prim.render_primitive(ctxt, mesh),
            draw::Primitive::Polygon(prim) => prim.render_primitive(ctxt, mesh),
            draw::Primitive::Tri(prim) => prim.render_primitive(ctxt, mesh),
            draw::Primitive::Ellipse(prim) => prim.render_primitive(ctxt, mesh),
            draw::Primitive::Quad(prim) => prim.render_primitive(ctxt, mesh),
            draw::Primitive::Rect(prim) => prim.render_primitive(ctxt, mesh),
            draw::Primitive::Line(prim) => prim.render_primitive(ctxt, mesh),
            // `Text` is rendered by the frame renderer directly (glyph quads textured by the
            // glyph atlas, not tessellated into the shared mesh); unfinished type-states
            // (`PathInit`, `PolygonInit`, a vertexless mesh) draw nothing.
            _ => {}
        }
    }
}
