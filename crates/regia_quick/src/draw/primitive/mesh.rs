// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/primitive/mesh.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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
use std::ops;

use crate::prelude_internal::*;

use regia_core::math::geom;

use crate::draw::QuickImage;
use crate::draw::mesh::Mesh;
use crate::draw::primitive::{Primitive, Vertex};
use crate::draw::properties::spatial::{orientation, position};
use crate::draw::properties::{SetColor, SetOrientation, SetPosition};
use crate::draw::{self, Drawing};

/// The mesh type prior to being initialised with vertices or indices.
#[derive(Clone, Debug, Default)]
pub struct Vertexless;

/// Properties related to drawing an arbitrary mesh of colours, geometry and texture.
#[derive(Clone, Debug)]
pub struct PrimitiveMesh {
    position: position::Properties,
    orientation: orientation::Properties,
    vertex_range: ops::Range<usize>,
    index_range: ops::Range<usize>,
    fill_color: Option<FillColor>,
}

#[derive(Clone, Debug, Default)]
struct FillColor(Option<Color>);

/// The drawing context for a mesh with vertices.
pub type DrawingMesh<'a> = Drawing<'a, PrimitiveMesh>;

impl Vertexless {
    /// Describe the mesh with a sequence of textured points.
    ///
    /// Each of the vertices must be represented as a tuple containing the point and tex
    /// coordinates in that order, e.g. `(point, tex_coords)`. `point` may be of any type that
    /// implements `Into<Vec3>` and `tex_coords` may be of any type that implements
    /// `Into<Vec2>`.
    pub fn points_textured<I, P, T>(self, inner_mesh: &mut Mesh, points: I) -> PrimitiveMesh
    where
        I: IntoIterator<Item = (P, T)>,
        P: Into<Vec3>,
        T: Into<Vec2>,
    {
        let points = points.into_iter().map(|(p, t)| {
            let point = p.into();
            let color = Color::WHITE;
            let tex_coords = t.into();
            (point, color, tex_coords)
        });
        self.points_inner(inner_mesh, points)
    }

    /// Describe the mesh with a sequence of colored points.
    ///
    /// Each of the points must be represented as a tuple containing the point and the color in
    /// that order, e.g. `(point, color)`. `point` may be of any type that implements
    /// `Into<Vec3>` and `color` may be of any type that implements `IntoColor`.
    pub fn points_colored<I, P, C>(self, inner_mesh: &mut Mesh, points: I) -> PrimitiveMesh
    where
        I: IntoIterator<Item = (P, C)>,
        P: Into<Vec3>,
        C: Into<Color>,
    {
        let vertices = points.into_iter().map(|(p, c)| {
            let point = p.into();
            let color = c.into();
            let tex_coords = Vec2::ZERO;
            (point, color, tex_coords)
        });
        self.points_inner(inner_mesh, vertices)
    }

    /// Describe the mesh with a sequence of points.
    ///
    /// The given iterator may yield any type that can be converted directly into `Vec3`s.
    ///
    /// This method assumes that the entire mesh should be coloured with a single colour. If a
    /// colour is not specified via one of the builder methods, a default colour will be retrieved
    /// from the inner `Theme`.
    pub fn points<I>(self, inner_mesh: &mut Mesh, points: I) -> PrimitiveMesh
    where
        I: IntoIterator,
        I::Item: Into<Vec3>,
    {
        let vertices = points.into_iter().map(|p| {
            let point = p.into();
            let color = Color::WHITE;
            let tex_coords = Vec2::ZERO;
            (point, color, tex_coords)
        });
        let mut mesh = self.points_inner(inner_mesh, vertices);
        mesh.fill_color = Some(FillColor(None));
        mesh
    }

    fn points_inner<I>(self, inner_mesh: &mut Mesh, vertices: I) -> PrimitiveMesh
    where
        I: Iterator<Item = Vertex>,
    {
        let v_start = inner_mesh.count_vertices();
        let i_start = inner_mesh.count_indices();
        let vertices: Vec<Vertex> = vertices.collect();
        // Whole triangles only: a 1-2 vertex tail would shift the next same-state draw's triangles.
        let index_count = vertices.len() - vertices.len() % 3;
        for (i, (point, color, tex_coords)) in vertices.into_iter().enumerate() {
            inner_mesh.push_vertex(point, color, tex_coords);
            // Indices are absolute into the intermediary mesh; render rebases them per primitive.
            if i < index_count {
                inner_mesh.push_index((v_start + i) as u32);
            }
        }
        let v_end = inner_mesh.count_vertices();
        let i_end = inner_mesh.count_indices();
        PrimitiveMesh::new(v_start..v_end, i_start..i_end)
    }

    /// Describe the mesh with a sequence of textured triangles.
    ///
    /// Each of the vertices must be represented as a tuple containing the point and tex
    /// coordinates in that order, e.g. `(point, tex_coords)`. `point` may be of any type that
    /// implements `Into<Vec3>` and `tex_coords` may be of any type that implements
    /// `Into<Vec2>`.
    pub fn tris_textured<I, P, T>(self, inner_mesh: &mut Mesh, tris: I) -> PrimitiveMesh
    where
        I: IntoIterator<Item = geom::Tri<(P, T)>>,
        P: Into<Vec3>,
        T: Into<Vec2>,
    {
        let points = tris
            .into_iter()
            .map(|t| t.map_vertices(|(p, t)| (p.into(), t.into())))
            .flat_map(geom::Tri::vertices);
        self.points_textured(inner_mesh, points)
    }

    /// Describe the mesh with a sequence of colored triangles.
    ///
    /// Each of the vertices must be represented as a tuple containing the point and the color in
    /// that order, e.g. `(point, color)`. `point` may be of any type that implements `Into<Vec3>`
    /// and `color` may be of any type that implements `IntoColor`.
    pub fn tris_colored<I, P, C>(self, inner_mesh: &mut Mesh, tris: I) -> PrimitiveMesh
    where
        I: IntoIterator<Item = geom::Tri<(P, C)>>,
        P: Into<Vec3>,
        C: Into<Color>,
    {
        let points = tris
            .into_iter()
            .map(|t| t.map_vertices(|(p, c)| (p.into(), c.into())))
            .flat_map(geom::Tri::vertices);
        self.points_colored(inner_mesh, points)
    }

    /// Describe the mesh with a sequence of triangles.
    ///
    /// Each triangle may be composed of any vertex type that may be converted directly into
    /// `Vec3`s.
    ///
    /// This method assumes that the entire mesh should be coloured with a single colour. If a
    /// colour is not specified via one of the builder methods, a default colour will be retrieved
    /// from the inner `Theme`.
    pub fn tris<I, V>(self, inner_mesh: &mut Mesh, tris: I) -> PrimitiveMesh
    where
        I: IntoIterator<Item = geom::Tri<V>>,
        V: Into<Vec3>,
    {
        let points = tris
            .into_iter()
            .map(|t| t.map_vertices(Into::into))
            .flat_map(geom::Tri::vertices);
        self.points(inner_mesh, points)
    }

    /// Describe the mesh with the given indexed, textured points.
    ///
    /// Each trio of `indices` describes a single triangle made up of colored `points`.
    ///
    /// Each of the `points` must be represented as a tuple containing the point and the texture
    /// coordinates in that order, e.g. `(point, tex_coords)`. `point` may be of any type that
    /// implements `Into<Vec3>` and `tex_coords` may be of any type that implements
    /// `Into<Vec2>`.
    pub fn indexed_textured<V, I, P, T>(self, inner_mesh: &mut Mesh, points: V, indices: I) -> PrimitiveMesh
    where
        V: IntoIterator<Item = (P, T)>,
        I: IntoIterator<Item = usize>,
        P: Into<Vec3>,
        T: Into<Vec2>,
    {
        let vertices = points.into_iter().map(|(p, t)| {
            let point = p.into();
            let color = Color::WHITE;
            let tex_coords = t.into();
            (point, color, tex_coords)
        });
        self.indexed_inner(inner_mesh, vertices, indices)
    }

    /// Describe the mesh with the given indexed, colored points.
    ///
    /// Each trio of `indices` describes a single triangle made up of colored `points`.
    ///
    /// Each of the `points` must be represented as a tuple containing the point and the color in
    /// that order, e.g. `(point, color)`. `point` may be of any type that implements
    /// `Into<Vec3>` and `color` may be of any type that implements `IntoColor`.
    pub fn indexed_colored<V, I, P, C>(self, inner_mesh: &mut Mesh, points: V, indices: I) -> PrimitiveMesh
    where
        V: IntoIterator<Item = (P, C)>,
        I: IntoIterator<Item = usize>,
        P: Into<Vec3>,
        C: Into<Color>,
    {
        let vertices = points.into_iter().map(|(p, c)| {
            let point = p.into();
            let color = c.into();
            let tex_coords = Vec2::ZERO;
            (point, color, tex_coords)
        });
        self.indexed_inner(inner_mesh, vertices, indices)
    }

    /// Describe the mesh with the given indexed points.
    ///
    /// Each trio of `indices` describes a single triangle made up of `points`.
    ///
    /// Each point may be any type that may be converted directly into the `Vec3` type.
    pub fn indexed<V, I>(self, inner_mesh: &mut Mesh, points: V, indices: I) -> PrimitiveMesh
    where
        V: IntoIterator,
        V::Item: Into<Vec3>,
        I: IntoIterator<Item = usize>,
    {
        let vertices = points.into_iter().map(|p| {
            let point = p.into();
            let color = Color::WHITE;
            let tex_coords = Vec2::ZERO;
            (point, color, tex_coords)
        });
        let mut mesh = self.indexed_inner(inner_mesh, vertices, indices);
        mesh.fill_color = Some(FillColor(None));
        mesh
    }

    fn indexed_inner<V, I>(self, inner_mesh: &mut Mesh, vertices: V, indices: I) -> PrimitiveMesh
    where
        V: IntoIterator<Item = Vertex>,
        I: IntoIterator<Item = usize>,
    {
        let v_start = inner_mesh.count_vertices();
        let i_start = inner_mesh.count_indices();

        for (point, color, tex_coords) in vertices.into_iter() {
            inner_mesh.push_vertex(point, color, tex_coords);
        }
        let v_end = inner_mesh.count_vertices();
        // Caller indices are local to this mesh: make them absolute, and drop any triangle that
        // references a vertex the mesh does not have.
        let local: Vec<usize> = indices.into_iter().collect();
        let (tris, _) = local.as_chunks::<3>();
        for tri in tris {
            if tri
                .iter()
                .all(|&i| v_start.checked_add(i).is_some_and(|abs| abs < v_end))
            {
                for &i in tri {
                    inner_mesh.push_index((v_start + i) as u32);
                }
            }
        }
        let i_end = inner_mesh.count_indices();
        PrimitiveMesh::new(v_start..v_end, i_start..i_end)
    }
}

impl PrimitiveMesh {
    // Initialise a new `Mesh` with its ranges into the intermediary mesh, ready for drawing.
    fn new(vertex_range: ops::Range<usize>, index_range: ops::Range<usize>) -> Self {
        let orientation = Default::default();
        let position = Default::default();
        let fill_color = None;
        PrimitiveMesh {
            orientation,
            position,
            vertex_range,
            index_range,
            fill_color,
        }
    }
}

impl<'a> Drawing<'a, Vertexless> {
    /// Describe the mesh with a sequence of points.
    ///
    /// The given iterator may yield any type that can be converted directly into `Vec3`s.
    ///
    /// This method assumes that the entire mesh should be coloured with a single colour. If a
    /// colour is not specified via one of the builder methods, a default colour will be retrieved
    /// from the inner `Theme`.
    pub fn points<I>(self, points: I) -> DrawingMesh<'a>
    where
        I: IntoIterator,
        I::Item: Into<Vec3>,
    {
        let mut vertices = points.into_iter().map(|p| (p.into(), Color::WHITE, Vec2::ZERO));
        mesh_points(self.draw, self.index, true, &mut vertices);
        self.transition()
    }

    /// Describe the mesh with a sequence of colored points.
    ///
    /// Each of the points must be represented as a tuple containing the point and the color in
    /// that order, e.g. `(point, color)`. `point` may be of any type that implements
    /// `Into<Vec3>` and `color` may be of any type that implements `IntoColor`.
    pub fn points_colored<I, P, C>(self, points: I) -> DrawingMesh<'a>
    where
        I: IntoIterator<Item = (P, C)>,
        P: Into<Vec3>,
        C: Into<Color>,
    {
        let mut vertices = points.into_iter().map(|(p, c)| (p.into(), c.into(), Vec2::ZERO));
        mesh_points(self.draw, self.index, false, &mut vertices);
        self.transition()
    }

    /// Describe the mesh with a sequence of textured points.
    ///
    /// Each of the vertices must be represented as a tuple containing the point and tex
    /// coordinates in that order, e.g. `(point, tex_coords)`. `point` may be of any type that
    /// implements `Into<Vec3>` and `tex_coords` may be of any type that implements
    /// `Into<Vec2>`.
    pub fn points_textured<I, P, T>(self, texture: QuickImage, points: I) -> DrawingMesh<'a>
    where
        I: IntoIterator<Item = (P, T)>,
        P: Into<Vec3>,
        T: Into<Vec2>,
    {
        let this = self.texture(texture);
        let mut vertices = points
            .into_iter()
            .map(|(p, t)| (p.into(), Color::WHITE, t.into()));
        mesh_points(this.draw, this.index, false, &mut vertices);
        this.transition()
    }

    /// Describe the mesh with a sequence of triangles.
    ///
    /// Each triangle may be composed of any vertex type that may be converted directly into
    /// `Vec3`s.
    ///
    /// This method assumes that the entire mesh should be coloured with a single colour. If a
    /// colour is not specified via one of the builder methods, a default colour will be retrieved
    /// from the inner `Theme`.
    pub fn tris<I, V>(self, tris: I) -> DrawingMesh<'a>
    where
        I: IntoIterator<Item = geom::Tri<V>>,
        V: Into<Vec3>,
    {
        let mut vertices = tris
            .into_iter()
            .map(|t| t.map_vertices(Into::into))
            .flat_map(geom::Tri::vertices)
            .map(|p| (p, Color::WHITE, Vec2::ZERO));
        mesh_points(self.draw, self.index, true, &mut vertices);
        self.transition()
    }

    /// Describe the mesh with a sequence of colored triangles.
    ///
    /// Each of the vertices must be represented as a tuple containing the point and the color in
    /// that order, e.g. `(point, color)`. `point` may be of any type that implements `Into<Vec3>`
    /// and `color` may be of any type that implements `IntoColor`.
    pub fn tris_colored<I, P, C>(self, tris: I) -> DrawingMesh<'a>
    where
        I: IntoIterator<Item = geom::Tri<(P, C)>>,
        P: Into<Vec3>,
        C: Into<Color>,
    {
        let mut vertices = tris
            .into_iter()
            .map(|t| t.map_vertices(|(p, c)| (p.into(), c.into())))
            .flat_map(geom::Tri::vertices)
            .map(|(p, c)| (p, c, Vec2::ZERO));
        mesh_points(self.draw, self.index, false, &mut vertices);
        self.transition()
    }

    /// Describe the mesh with a sequence of textured triangles.
    ///
    /// Each of the vertices must be represented as a tuple containing the point and tex
    /// coordinates in that order, e.g. `(point, tex_coords)`. `point` may be of any type that
    /// implements `Into<Vec3>` and `tex_coords` may be of any type that implements
    /// `Into<Vec2>`.
    pub fn tris_textured<I, P, T>(self, texture: QuickImage, tris: I) -> DrawingMesh<'a>
    where
        I: IntoIterator<Item = geom::Tri<(P, T)>>,
        P: Into<Vec3>,
        T: Into<Vec2>,
    {
        let this = self.texture(texture);
        let mut vertices = tris
            .into_iter()
            .map(|t| t.map_vertices(|(p, t)| (p.into(), t.into())))
            .flat_map(geom::Tri::vertices)
            .map(|(p, t)| (p, Color::WHITE, t));
        mesh_points(this.draw, this.index, false, &mut vertices);
        this.transition()
    }

    /// Describe the mesh with the given indexed points.
    ///
    /// Each trio of `indices` describes a single triangle made up of `points`.
    ///
    /// Each point may be any type that may be converted directly into the `Vec3` type.
    pub fn indexed<V, I>(self, points: V, indices: I) -> DrawingMesh<'a>
    where
        V: IntoIterator,
        V::Item: Into<Vec3>,
        I: IntoIterator<Item = usize>,
    {
        let mut vertices = points.into_iter().map(|p| (p.into(), Color::WHITE, Vec2::ZERO));
        let mut indices = indices.into_iter();
        mesh_indexed(self.draw, self.index, true, &mut vertices, &mut indices);
        self.transition()
    }

    /// Describe the mesh with the given indexed, colored points.
    ///
    /// Each trio of `indices` describes a single triangle made up of colored `points`.
    ///
    /// Each of the `points` must be represented as a tuple containing the point and the color in
    /// that order, e.g. `(point, color)`. `point` may be of any type that implements
    /// `Into<Vec3>` and `color` may be of any type that implements `IntoColor`.
    pub fn indexed_colored<V, I, P, C>(self, points: V, indices: I) -> DrawingMesh<'a>
    where
        V: IntoIterator<Item = (P, C)>,
        I: IntoIterator<Item = usize>,
        P: Into<Vec3>,
        C: Into<Color>,
    {
        let mut vertices = points.into_iter().map(|(p, c)| (p.into(), c.into(), Vec2::ZERO));
        let mut indices = indices.into_iter();
        mesh_indexed(self.draw, self.index, false, &mut vertices, &mut indices);
        self.transition()
    }

    /// Describe the mesh with the given indexed, textured points.
    ///
    /// Each trio of `indices` describes a single triangle made up of colored `points`.
    ///
    /// Each of the `points` must be represented as a tuple containing the point and the texture
    /// coordinates in that order, e.g. `(point, tex_coords)`. `point` may be of any type that
    /// implements `Into<Vec3>` and `tex_coords` may be of any type that implements
    /// `Into<Vec2>`.
    pub fn indexed_textured<V, I, P, T>(self, texture: QuickImage, points: V, indices: I) -> DrawingMesh<'a>
    where
        V: IntoIterator<Item = (P, T)>,
        I: IntoIterator<Item = usize>,
        P: Into<Vec3>,
        T: Into<Vec2>,
    {
        let this = self.texture(texture);
        let mut vertices = points
            .into_iter()
            .map(|(p, t)| (p.into(), Color::WHITE, t.into()));
        let mut indices = indices.into_iter();
        mesh_indexed(this.draw, this.index, false, &mut vertices, &mut indices);
        this.transition()
    }
}

// Submit the mesh's vertices, transitioning the primitive from `Vertexless` to `Mesh`.
//
// `themed_fill` indicates that the vertices carry no colour of their own and the mesh should be
// filled with a single colour (the one set via the builder methods, or the theme's default).
fn mesh_points(
    draw: &crate::draw::Draw,
    index: usize,
    themed_fill: bool,
    vertices: &mut dyn Iterator<Item = Vertex>,
) {
    // Pull the user's iterator before taking the `Draw` lock: it may call back into `Draw`.
    let vertices: Vec<Vertex> = vertices.collect();
    crate::draw::drawing::with_primitive_ctxt(draw, index, |prim, ctxt| match prim {
        Primitive::MeshVertexless(v) => {
            let mut mesh = v.points_inner(ctxt.mesh, vertices.into_iter());
            if themed_fill {
                mesh.fill_color = Some(FillColor(None));
            }
            Primitive::Mesh(mesh)
        }
        other => {
            crate::warn_once!("expected a `Vertexless` mesh primitive");
            other
        }
    })
}

// The same as `mesh_points`, but with explicit indices.
fn mesh_indexed(
    draw: &crate::draw::Draw,
    index: usize,
    themed_fill: bool,
    vertices: &mut dyn Iterator<Item = Vertex>,
    indices: &mut dyn Iterator<Item = usize>,
) {
    // Pull the user's iterators before taking the `Draw` lock: they may call back into `Draw`.
    let vertices: Vec<Vertex> = vertices.collect();
    let indices: Vec<usize> = indices.collect();
    crate::draw::drawing::with_primitive_ctxt(draw, index, |prim, ctxt| match prim {
        Primitive::MeshVertexless(v) => {
            let mut mesh = v.indexed_inner(ctxt.mesh, vertices, indices);
            if themed_fill {
                mesh.fill_color = Some(FillColor(None));
            }
            Primitive::Mesh(mesh)
        }
        other => {
            crate::warn_once!("expected a `Vertexless` mesh primitive");
            other
        }
    })
}

impl draw::render::RenderPrimitive for PrimitiveMesh {
    fn render_primitive(self, ctxt: draw::render::RenderContext, mesh: &mut Mesh) {
        let PrimitiveMesh {
            orientation,
            position,
            vertex_range,
            index_range,
            fill_color,
        } = self;

        // Determine the transform to apply to vertices.
        let global_transform = *ctxt.transform;
        let local_transform = position.transform() * orientation.transform();
        let transform = global_transform * local_transform;

        // We need to update the indices to point to where vertices will be in the new mesh.
        let old_mesh_vertex_start = vertex_range.start as u32;
        let new_mesh_vertex_start = mesh.count_vertices() as u32;
        let indices = index_range
            .map(|i| ctxt.intermediary_mesh.get_index(i))
            .map(|i| new_mesh_vertex_start + i - old_mesh_vertex_start);

        // A small function for transforming a point via the transform matrix.
        let transform_point = |p: Vec3| -> Vec3 { transform.transform_point3(p) };

        // Color the vertices based on whether or not we should fill, then extend the mesh!
        match fill_color {
            Some(fill) => {
                let theme_prim = draw::theme::Primitive::Mesh;
                let color = fill.0.unwrap_or_else(|| ctxt.theme.fill(&theme_prim));
                let vertices = vertex_range.map(|i| {
                    let point = transform_point(ctxt.intermediary_mesh.point(i));
                    let tex_coords: Vec2 = ctxt.intermediary_mesh.tex_coord(i);
                    (point, color, tex_coords)
                });

                for (point, color, tex_coords) in vertices {
                    mesh.push_vertex(point, color, tex_coords);
                }
                for index in indices {
                    mesh.push_index(index);
                }
            }
            None => {
                let vertices = vertex_range.map(|i| {
                    let point = transform_point(ctxt.intermediary_mesh.point(i));
                    let color = ctxt.intermediary_mesh.color(i);
                    let tex_coords: Vec2 = ctxt.intermediary_mesh.tex_coord(i);
                    (point, color, tex_coords)
                });

                for (point, color, tex_coords) in vertices {
                    mesh.push_vertex(point, color, tex_coords);
                }
                for index in indices {
                    mesh.push_index(index);
                }
            }
        }
    }
}

impl SetOrientation for PrimitiveMesh {
    fn properties(&mut self) -> &mut orientation::Properties {
        SetOrientation::properties(&mut self.orientation)
    }
}

impl SetPosition for PrimitiveMesh {
    fn properties(&mut self) -> &mut position::Properties {
        SetPosition::properties(&mut self.position)
    }
}

impl SetColor for PrimitiveMesh {
    fn color_mut(&mut self) -> &mut Option<Color> {
        &mut self.fill_color.get_or_insert_with(Default::default).0
    }
}

impl From<Vertexless> for Primitive {
    fn from(prim: Vertexless) -> Self {
        Primitive::MeshVertexless(prim)
    }
}

impl From<PrimitiveMesh> for Primitive {
    fn from(prim: PrimitiveMesh) -> Self {
        Primitive::Mesh(prim)
    }
}
