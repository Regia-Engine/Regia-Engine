// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/mesh/builder.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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
//! Implementations of the lyon geometry builder traits for the `Mesh`.
//!
//! The aim is to allow for a tessellator to efficiently extend a mesh without using an
//! intermediary buffer.
//!
//! Lyon tessellators assume `f32` data, so we do the same in the following implementations.

use lyon::tessellation::geometry_builder::{FillGeometryBuilder, GeometryBuilder, StrokeGeometryBuilder};
use lyon::tessellation::{FillVertex, GeometryBuilderError, StrokeVertex, VertexId};

use crate::draw::mesh::Mesh;
use crate::prelude_internal::*;

/// Extends a [`Mesh`] with tessellated geometry, transforming every point.
pub struct MeshBuilder<'a, A> {
    /// The mesh that is to be extended.
    mesh: &'a mut Mesh,
    /// The number of vertices in the mesh when begin was called.
    begin_vertex_count: u32,
    /// The number of indices in the mesh when begin was called.
    begin_index_count: u32,
    /// Transform matrix that also integrates position and orientation here.
    transform: Mat4,
    /// The way in which vertex attributes should be sourced.
    attributes: A,
}

/// Every vertex gets one colour; texture coordinates come from the path's two attributes.
pub struct SingleColor(Color);
/// Colour and texture coordinates come from the path's six attributes (`r g b a u v`).
pub struct Vertex;

impl<'a, A> MeshBuilder<'a, A> {
    /// Begin extending the mesh.
    fn new(mesh: &'a mut Mesh, transform: Mat4, attributes: A) -> Self {
        MeshBuilder {
            mesh,
            begin_vertex_count: 0,
            begin_index_count: 0,
            transform,
            attributes,
        }
    }

    fn push(&mut self, position: lyon::math::Point, color: [f32; 4], tex_coords: [f32; 2]) -> VertexId {
        let id = VertexId::from_usize(self.mesh.count_vertices());
        let point = self
            .transform
            .transform_point3(Vec3::new(position.x, position.y, 0.0));
        self.mesh.vertices.push(regia_render::ImmediateVertex::textured(
            point.to_array(),
            tex_coords,
            color,
        ));
        id
    }
}

impl<'a> MeshBuilder<'a, SingleColor> {
    /// Begin extending a mesh rendered with a single colour.
    pub fn single_color(mesh: &'a mut Mesh, transform: Mat4, color: Color) -> Self {
        Self::new(mesh, transform, SingleColor(color))
    }
}

impl<'a> MeshBuilder<'a, Vertex> {
    /// Begin extending a mesh where the path interpolates a unique color per point.
    pub fn vertex_per_point(mesh: &'a mut Mesh, transform: Mat4) -> Self {
        Self::new(mesh, transform, Vertex)
    }
}

impl<A> GeometryBuilder for MeshBuilder<'_, A> {
    fn begin_geometry(&mut self) {
        self.begin_vertex_count = self.mesh.count_vertices() as u32;
        self.begin_index_count = self.mesh.count_indices() as u32;
    }

    fn add_triangle(&mut self, a: VertexId, b: VertexId, c: VertexId) {
        self.mesh.push_index(a.to_usize() as u32);
        self.mesh.push_index(b.to_usize() as u32);
        self.mesh.push_index(c.to_usize() as u32);
    }

    fn abort_geometry(&mut self) {
        // Drop the partial output of a failed tessellation.
        self.mesh.vertices.truncate(self.begin_vertex_count as usize);
        self.mesh.indices.truncate(self.begin_index_count as usize);
    }
}

/// The two leading interpolated attributes as texture coordinates (zero when absent).
fn uv_of(attr: &[f32]) -> [f32; 2] {
    match attr {
        [u, v, ..] => [*u, *v],
        _ => [0.0, 0.0],
    }
}

/// `r g b a u v` attributes.
fn color_uv_of(attr: &[f32]) -> ([f32; 4], [f32; 2]) {
    match attr {
        [r, g, b, a, u, v, ..] => ([*r, *g, *b, *a], [*u, *v]),
        _ => ([1.0; 4], [0.0, 0.0]),
    }
}

impl FillGeometryBuilder for MeshBuilder<'_, SingleColor> {
    fn add_fill_vertex(&mut self, mut vertex: FillVertex) -> Result<VertexId, GeometryBuilderError> {
        let position = vertex.position();
        let uv = uv_of(vertex.interpolated_attributes());
        let color = self.attributes.0.to_array();
        Ok(self.push(position, color, uv))
    }
}

impl StrokeGeometryBuilder for MeshBuilder<'_, SingleColor> {
    fn add_stroke_vertex(&mut self, mut vertex: StrokeVertex) -> Result<VertexId, GeometryBuilderError> {
        let position = vertex.position();
        let uv = uv_of(vertex.interpolated_attributes());
        let color = self.attributes.0.to_array();
        Ok(self.push(position, color, uv))
    }
}

impl FillGeometryBuilder for MeshBuilder<'_, Vertex> {
    fn add_fill_vertex(&mut self, mut vertex: FillVertex) -> Result<VertexId, GeometryBuilderError> {
        let position = vertex.position();
        let (color, uv) = color_uv_of(vertex.interpolated_attributes());
        Ok(self.push(position, color, uv))
    }
}

impl StrokeGeometryBuilder for MeshBuilder<'_, Vertex> {
    fn add_stroke_vertex(&mut self, mut vertex: StrokeVertex) -> Result<VertexId, GeometryBuilderError> {
        let position = vertex.position();
        let (color, uv) = color_uv_of(vertex.interpolated_attributes());
        Ok(self.push(position, color, uv))
    }
}
