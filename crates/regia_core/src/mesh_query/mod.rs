// SPDX-License-Identifier: MPL-2.0
//! Spatial queries on triangle meshes and point sets (ADR-1461, book `systems/mesh-queries.md`).
//!
//! - [`TriMesh`]: an indexed triangle mesh (positions plus index triples), validated on creation.
//! - [`TriBvh`]: a bounding-box tree over the triangles: nearest ray hit, closest point, triangles in
//!   a box or a sphere, and overlap tests between two trees (optionally under a transform).
//! - [`WindingNumberTree`]: the fast generalized winding number (Barill et al. 2018) on a
//!   [`TriBvh`], for inside/outside tests that tolerate holes and soups; [`winding_number_exact`] is
//!   the direct sum (Jacobson et al. 2013) it approximates.
//! - [`SignedDistanceGrid`]: a dense grid of signed distances to a mesh (exact narrow band, fast
//!   sweeping, sign from the winding number), with trilinear sampling and gradient.
//! - [`PointHash`]: a uniform hash grid over points, for neighbors within a radius.
//! - [`triangle`]: the single-triangle primitives the queries use.
//!
//! The tree, winding-number, distance-grid and point-hash code is ported from geometry3Sharp by
//! Ryan Schmidt (Boost Software License 1.0, see `docs/ported-sources.toml`); the triangle
//! primitives are written from the textbook sources cited in [`triangle`]. Positions are `f32`;
//! every per-triangle computation (distances, ray hits, solid angles, winding coefficients) runs in
//! `f64` like the upstream code, and results convert back to `f32`.
//!
//! All of it is single-threaded CPU code with no allocation per query beyond the result vectors,
//! usable from runtime crates and from the asset pipeline (it lives in the foundation layer).

mod bvh;
mod grid;
mod point_hash;
mod sdf;
pub mod triangle;
mod winding;

pub use bvh::{ClosestPoint, QueryCounts, RayHit, TriBvh};
pub use grid::DenseGrid3;
pub use point_hash::PointHash;
pub use sdf::{SdfParams, SignedDistanceGrid};
pub use winding::{WindingNumberTree, winding_number_exact};

use glam::Vec3;

use crate::math::Aabb;

/// Why a mesh query structure could not be built.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum MeshQueryError {
    /// The mesh has no triangles; a tree, winding number or distance grid needs at least one.
    #[error("the mesh has no triangles")]
    EmptyMesh,
    /// A flat index list's length is not a multiple of three.
    #[error("{0} indices do not form whole triangles (not a multiple of 3)")]
    IndexCount(usize),
    /// A triangle names a vertex the mesh does not have.
    #[error("triangle {triangle} uses vertex {index}, but the mesh has {vertex_count} vertices")]
    IndexOutOfRange {
        /// The triangle's index.
        triangle: u32,
        /// The out-of-range vertex index.
        index: u32,
        /// How many vertices the mesh has.
        vertex_count: u32,
    },
    /// A vertex position or point has a NaN or infinite coordinate.
    #[error("point {0} has a non-finite coordinate")]
    NonFinite(u32),
    /// More vertices, points or triangles than a `u32` index can name.
    #[error("{0} elements are more than a u32 index can address")]
    TooLarge(usize),
    /// A grid cell size is zero, negative or not finite.
    #[error("cell size {0} must be finite and greater than zero")]
    InvalidCellSize(f32),
    /// The distance grid would have more sample points than the limit in [`SdfParams`].
    #[error("the grid would have {points} points, more than the limit of {limit}")]
    GridTooLarge {
        /// Sample points the grid would need.
        points: u64,
        /// The configured limit ([`SdfParams::max_points`]).
        limit: u64,
    },
    /// The winding-number accuracy parameter `beta` is not finite and positive.
    #[error("winding number beta {0} must be finite and greater than zero")]
    InvalidBeta(f32),
}

/// An indexed triangle mesh: vertex positions and counter-clockwise index triples (the outward side
/// of a closed mesh is the one its triangles are counter-clockwise from). Every index is checked
/// against the positions and every position is finite; degenerate (zero-area) triangles are kept
/// and handled by every query.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TriMesh {
    positions: Vec<Vec3>,
    triangles: Vec<[u32; 3]>,
}

impl TriMesh {
    /// A mesh from positions and index triples.
    pub fn new(positions: Vec<Vec3>, triangles: Vec<[u32; 3]>) -> Result<Self, MeshQueryError> {
        let vertex_count =
            u32::try_from(positions.len()).map_err(|_| MeshQueryError::TooLarge(positions.len()))?;
        u32::try_from(triangles.len()).map_err(|_| MeshQueryError::TooLarge(triangles.len()))?;
        if let Some(bad) = positions.iter().position(|p| !p.is_finite()) {
            return Err(MeshQueryError::NonFinite(bad as u32));
        }
        for (t, tri) in triangles.iter().enumerate() {
            if let Some(&index) = tri.iter().find(|&&i| i >= vertex_count) {
                return Err(MeshQueryError::IndexOutOfRange {
                    triangle: t as u32,
                    index,
                    vertex_count,
                });
            }
        }
        Ok(Self { positions, triangles })
    }

    /// A mesh from positions and a flat index list (three indices per triangle, as in a GPU index
    /// buffer).
    pub fn from_indices(positions: Vec<Vec3>, indices: &[u32]) -> Result<Self, MeshQueryError> {
        let (triangles, rest) = indices.as_chunks::<3>();
        if !rest.is_empty() {
            return Err(MeshQueryError::IndexCount(indices.len()));
        }
        Self::new(positions, triangles.to_vec())
    }

    /// Vertex positions.
    pub fn positions(&self) -> &[Vec3] {
        &self.positions
    }

    /// Index triples, one per triangle.
    pub fn triangles(&self) -> &[[u32; 3]] {
        &self.triangles
    }

    /// Number of triangles.
    pub fn triangle_count(&self) -> usize {
        self.triangles.len()
    }

    /// `true` if the mesh has no triangles.
    pub fn is_empty(&self) -> bool {
        self.triangles.is_empty()
    }

    /// The three corner positions of triangle `t`.
    ///
    /// # Panics
    /// If `t` is not below [`TriMesh::triangle_count`]; use [`TriMesh::get_triangle`] for untrusted ids.
    pub fn triangle(&self, t: u32) -> [Vec3; 3] {
        let [a, b, c] = self.triangles[t as usize];
        [
            self.positions[a as usize],
            self.positions[b as usize],
            self.positions[c as usize],
        ]
    }

    /// The three corner positions of triangle `t`, or `None` if `t` is not below
    /// [`TriMesh::triangle_count`].
    pub fn get_triangle(&self, t: u32) -> Option<[Vec3; 3]> {
        let [a, b, c] = *self.triangles.get(t as usize)?;
        Some([
            *self.positions.get(a as usize)?,
            *self.positions.get(b as usize)?,
            *self.positions.get(c as usize)?,
        ])
    }

    /// The box around every vertex a triangle uses ([`Aabb::EMPTY`] for an empty mesh).
    pub fn bounds(&self) -> Aabb {
        let mut b = Aabb::EMPTY;
        for tri in &self.triangles {
            for &i in tri {
                b.extend(self.positions[i as usize]);
            }
        }
        b
    }

    /// Reverses every triangle's winding (swaps inside and outside).
    pub fn flip(&mut self) {
        for tri in &mut self.triangles {
            tri.swap(1, 2);
        }
    }
}

#[cfg(test)]
mod get_triangle_tests {
    use super::*;

    #[test]
    fn get_triangle_is_none_out_of_range() {
        let Ok(mesh) = TriMesh::new(vec![Vec3::ZERO, Vec3::X, Vec3::Y], vec![[0, 1, 2]]) else {
            panic!("valid mesh rejected");
        };
        assert!(mesh.get_triangle(0).is_some());
        assert!(mesh.get_triangle(mesh.triangle_count() as u32).is_none());
        assert!(mesh.get_triangle(u32::MAX).is_none());
    }
}
