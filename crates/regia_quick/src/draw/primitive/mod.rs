// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/primitive/mod.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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
use lyon::tessellation::{FillOptions, StrokeOptions};
use regia_core::math::Color;

use regia_core::math::geom::{Vec2, Vec3};

use crate::draw::primitive::polygon::{PolygonOptions, SetPolygon};
use crate::draw::properties::spatial::{dimension, orientation, position};
use crate::draw::properties::tex_coords::SetTexCoords;
use crate::draw::properties::{SetColor, SetDimensions, SetFill, SetOrientation, SetPosition, SetStroke};

pub use self::arrow::Arrow;
pub use self::ellipse::Ellipse;
pub use self::line::Line;
pub use self::mesh::PrimitiveMesh;
pub use self::path::{Path, PathFill, PathInit, PathStroke};
pub use self::polygon::{Polygon, PolygonInit};
pub use self::quad::Quad;
pub use self::rect::Rect;
pub use self::text::Text;
pub use self::tri::Tri;

/// Arrows: a line with a triangular head.
pub mod arrow;
/// Ellipses and circles.
pub mod ellipse;
/// Straight line segments.
pub mod line;
/// Meshes from user vertices and indices.
pub mod mesh;
/// Filled or stroked paths.
pub mod path;
/// Polygons from points or path events.
pub mod polygon;
/// Four-cornered polygons.
pub mod quad;
/// Axis-aligned rectangles.
pub mod rect;
pub mod text;
/// Triangles.
pub mod tri;

type Vertex = (Vec3, Color, Vec2);

/// A wrapper around all primitive sets of properties so that they may be stored within the
/// **Draw**'s `drawing` field while they are being drawn.
///
/// This also allows us to flush all pending drawings to the mesh if `Draw::to_frame` is called
/// before their respective **Drawing** types are dropped.
#[derive(Clone, Debug)]
pub enum Primitive {
    /// An arrow.
    Arrow(Arrow),
    /// An ellipse.
    Ellipse(Ellipse),
    /// A line.
    Line(Line),
    /// A mesh before its vertices are given.
    MeshVertexless(mesh::Vertexless),
    /// A mesh.
    Mesh(PrimitiveMesh),
    /// A path before fill or stroke is chosen.
    PathInit(PathInit),
    /// A filled path before its points are given.
    PathFill(PathFill),
    /// A stroked path before its points are given.
    PathStroke(PathStroke),
    /// A path.
    Path(Path),
    /// A polygon before its points are given.
    PolygonInit(PolygonInit),
    /// A polygon.
    Polygon(Polygon),
    /// A quad.
    Quad(Quad),
    /// A rectangle.
    Rect(Rect),
    /// Text.
    Text(Text),
    /// A triangle.
    Tri(Tri),
}

/// The cheapest possible variant, used as a placeholder when temporarily taking ownership of a
/// stored primitive (e.g. for type-state transitions).
impl Default for Primitive {
    fn default() -> Self {
        Primitive::PathInit(PathInit)
    }
}

// Total accessors over all variants for each property channel.
//
// Each returns `Some` exactly for the variants whose inner type implements the corresponding
// `Set*` trait, `None` otherwise. The marker-gated `Drawing` API only exposes a property method
// when the marker type implements the trait, so `None` is unreachable via the public builders -
// it exists to keep these functions total.
impl Primitive {
    /// A short lowercase name of the primitive's kind (`rect`, `ellipse`, `text`, …).
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Primitive::Arrow(_) => "arrow",
            Primitive::Ellipse(_) => "ellipse",
            Primitive::Line(_) => "line",
            Primitive::MeshVertexless(_) | Primitive::Mesh(_) => "mesh",
            Primitive::PathInit(_)
            | Primitive::PathFill(_)
            | Primitive::PathStroke(_)
            | Primitive::Path(_) => "path",
            Primitive::PolygonInit(_) | Primitive::Polygon(_) => "polygon",
            Primitive::Quad(_) => "quad",
            Primitive::Rect(_) => "rect",
            Primitive::Text(_) => "text",
            Primitive::Tri(_) => "tri",
        }
    }

    pub(crate) fn color_mut(&mut self) -> Option<&mut Option<Color>> {
        match self {
            Primitive::Arrow(p) => Some(SetColor::color_mut(p)),
            Primitive::Ellipse(p) => Some(SetColor::color_mut(p)),
            Primitive::Line(p) => Some(SetColor::color_mut(p)),
            Primitive::Mesh(p) => Some(SetColor::color_mut(p)),
            Primitive::PathFill(p) => Some(SetColor::color_mut(p)),
            Primitive::PathStroke(p) => Some(SetColor::color_mut(p)),
            Primitive::Path(p) => Some(SetColor::color_mut(p)),
            Primitive::PolygonInit(p) => Some(SetColor::color_mut(p)),
            Primitive::Polygon(p) => Some(SetColor::color_mut(p)),
            Primitive::Quad(p) => Some(SetColor::color_mut(p)),
            Primitive::Rect(p) => Some(SetColor::color_mut(p)),
            Primitive::Text(p) => Some(SetColor::color_mut(p)),
            Primitive::Tri(p) => Some(SetColor::color_mut(p)),
            Primitive::MeshVertexless(_) | Primitive::PathInit(_) => None,
        }
    }

    pub(crate) fn position_mut(&mut self) -> Option<&mut position::Properties> {
        match self {
            Primitive::Arrow(p) => Some(SetPosition::properties(p)),
            Primitive::Ellipse(p) => Some(SetPosition::properties(p)),
            Primitive::Line(p) => Some(SetPosition::properties(p)),
            Primitive::Mesh(p) => Some(SetPosition::properties(p)),
            Primitive::PathFill(p) => Some(SetPosition::properties(p)),
            Primitive::PathStroke(p) => Some(SetPosition::properties(p)),
            Primitive::Path(p) => Some(SetPosition::properties(p)),
            Primitive::PolygonInit(p) => Some(SetPosition::properties(p)),
            Primitive::Polygon(p) => Some(SetPosition::properties(p)),
            Primitive::Quad(p) => Some(SetPosition::properties(p)),
            Primitive::Rect(p) => Some(SetPosition::properties(p)),
            Primitive::Text(p) => Some(SetPosition::properties(p)),
            Primitive::Tri(p) => Some(SetPosition::properties(p)),
            Primitive::MeshVertexless(_) | Primitive::PathInit(_) => None,
        }
    }

    pub(crate) fn orientation_mut(&mut self) -> Option<&mut orientation::Properties> {
        match self {
            Primitive::Arrow(p) => Some(SetOrientation::properties(p)),
            Primitive::Ellipse(p) => Some(SetOrientation::properties(p)),
            Primitive::Line(p) => Some(SetOrientation::properties(p)),
            Primitive::Mesh(p) => Some(SetOrientation::properties(p)),
            Primitive::PathFill(p) => Some(SetOrientation::properties(p)),
            Primitive::PathStroke(p) => Some(SetOrientation::properties(p)),
            Primitive::Path(p) => Some(SetOrientation::properties(p)),
            Primitive::PolygonInit(p) => Some(SetOrientation::properties(p)),
            Primitive::Polygon(p) => Some(SetOrientation::properties(p)),
            Primitive::Quad(p) => Some(SetOrientation::properties(p)),
            Primitive::Rect(p) => Some(SetOrientation::properties(p)),
            Primitive::Text(p) => Some(SetOrientation::properties(p)),
            Primitive::Tri(p) => Some(SetOrientation::properties(p)),
            Primitive::MeshVertexless(_) | Primitive::PathInit(_) => None,
        }
    }

    pub(crate) fn dimensions_mut(&mut self) -> Option<&mut dimension::Properties> {
        match self {
            Primitive::Ellipse(p) => Some(SetDimensions::properties(p)),
            Primitive::Quad(p) => Some(SetDimensions::properties(p)),
            Primitive::Rect(p) => Some(SetDimensions::properties(p)),
            Primitive::Text(p) => Some(SetDimensions::properties(p)),
            Primitive::Tri(p) => Some(SetDimensions::properties(p)),
            Primitive::Arrow(_)
            | Primitive::Line(_)
            | Primitive::Mesh(_)
            | Primitive::MeshVertexless(_)
            | Primitive::PathInit(_)
            | Primitive::PathFill(_)
            | Primitive::PathStroke(_)
            | Primitive::Path(_)
            | Primitive::PolygonInit(_)
            | Primitive::Polygon(_) => None,
        }
    }

    pub(crate) fn stroke_options_mut(&mut self) -> Option<&mut StrokeOptions> {
        match self {
            Primitive::Arrow(p) => Some(SetStroke::stroke_options_mut(p)),
            Primitive::Ellipse(p) => Some(SetStroke::stroke_options_mut(p)),
            Primitive::Line(p) => Some(SetStroke::stroke_options_mut(p)),
            Primitive::PathStroke(p) => Some(SetStroke::stroke_options_mut(p)),
            Primitive::PolygonInit(p) => Some(SetStroke::stroke_options_mut(p)),
            Primitive::Quad(p) => Some(SetStroke::stroke_options_mut(p)),
            Primitive::Rect(p) => Some(SetStroke::stroke_options_mut(p)),
            Primitive::Tri(p) => Some(SetStroke::stroke_options_mut(p)),
            Primitive::Mesh(_)
            | Primitive::MeshVertexless(_)
            | Primitive::PathInit(_)
            | Primitive::PathFill(_)
            | Primitive::Path(_)
            | Primitive::Polygon(_)
            | Primitive::Text(_) => None,
        }
    }

    pub(crate) fn fill_options_mut(&mut self) -> Option<&mut FillOptions> {
        match self {
            Primitive::PathFill(p) => Some(SetFill::fill_options_mut(p)),
            _ => None,
        }
    }

    pub(crate) fn polygon_options_mut(&mut self) -> Option<&mut PolygonOptions> {
        match self {
            Primitive::Ellipse(p) => Some(SetPolygon::polygon_options_mut(p)),
            Primitive::PolygonInit(p) => Some(SetPolygon::polygon_options_mut(p)),
            Primitive::Quad(p) => Some(SetPolygon::polygon_options_mut(p)),
            Primitive::Rect(p) => Some(SetPolygon::polygon_options_mut(p)),
            Primitive::Tri(p) => Some(SetPolygon::polygon_options_mut(p)),
            Primitive::Arrow(_)
            | Primitive::Line(_)
            | Primitive::Mesh(_)
            | Primitive::MeshVertexless(_)
            | Primitive::PathInit(_)
            | Primitive::PathFill(_)
            | Primitive::PathStroke(_)
            | Primitive::Path(_)
            | Primitive::Polygon(_)
            | Primitive::Text(_) => None,
        }
    }

    pub(crate) fn tex_coords_mut(&mut self) -> Option<&mut Option<regia_core::math::geom::Rect>> {
        match self {
            Primitive::Rect(p) => Some(SetTexCoords::tex_coords_mut(p)),
            _ => None,
        }
    }
}
