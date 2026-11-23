// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_draw/src/draw/mesh/mod.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
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
//! Items related to the custom mesh type used by the `Draw` API.

use regia_render::ImmediateVertex;

use crate::prelude_internal::*;

pub use self::builder::MeshBuilder;

pub mod builder;

/// A coloured, textured triangle mesh: the intermediary storage of mesh and path drawings and the
/// output of primitive rendering.
#[derive(Clone, Debug, Default)]
pub struct Mesh {
    /// Vertices (position, texture coordinate, linear straight-alpha colour).
    pub vertices: Vec<ImmediateVertex>,
    /// Triangle-list indices into `vertices`.
    pub indices: Vec<u32>,
}

impl Mesh {
    /// Removes every vertex and index, keeping the allocations.
    pub fn clear(&mut self) {
        self.vertices.clear();
        self.indices.clear();
    }

    /// Number of vertices.
    #[must_use]
    pub fn count_vertices(&self) -> usize {
        self.vertices.len()
    }

    /// Number of indices.
    #[must_use]
    pub fn count_indices(&self) -> usize {
        self.indices.len()
    }

    /// Index `index` (0 when out of range).
    #[must_use]
    pub fn get_index(&self, index: usize) -> u32 {
        self.indices.get(index).copied().unwrap_or(0)
    }

    /// Appends an index.
    pub fn push_index(&mut self, index: u32) {
        self.indices.push(index);
    }

    /// Appends a vertex.
    pub fn push_vertex(&mut self, point: Vec3, color: Color, tex_coords: Vec2) {
        self.vertices.push(ImmediateVertex::textured(
            point.to_array(),
            tex_coords.to_array(),
            color.to_array(),
        ));
    }

    /// Position of vertex `i`.
    #[must_use]
    pub fn point(&self, i: usize) -> Vec3 {
        self.vertices
            .get(i)
            .map_or(Vec3::ZERO, |v| Vec3::from_array(v.position))
    }

    /// Texture coordinate of vertex `i`.
    #[must_use]
    pub fn tex_coord(&self, i: usize) -> Vec2 {
        self.vertices
            .get(i)
            .map_or(Vec2::ZERO, |v| Vec2::from_array(v.uv))
    }

    /// Colour of vertex `i`.
    #[must_use]
    pub fn color(&self, i: usize) -> Color {
        self.vertices
            .get(i)
            .map_or(Color::WHITE, |v| Color::from(v.color))
    }
}
