// SPDX-License-Identifier: MPL-2.0
//! Crack-free dual contouring for volumetric terrain and caves.
//!
//! This module turns a signed-distance field ([`SdfField`]) into an indexed triangle mesh
//! over one regular-grid chunk, following dual contouring as published by Ju, Losasso,
//! Schaefer and Warren ("Dual Contouring of Hermite Data", SIGGRAPH 2002): every grid cell
//! whose corners straddle the surface contributes one dual vertex, placed by minimizing the
//! quadratic error function (QEF) of the Hermite data (edge crossing points and their
//! normals) collected on that cell's edges. The QEF is solved through the eigendecomposition
//! of the symmetric 3x3 normal matrix — for a symmetric positive-semidefinite matrix this is
//! its singular value decomposition — with small singular values dropped (a pseudo-inverse).
//! [`solve_qef`] clamps that minimizer into the box it is given and does not move it onto
//! the field. [`mesh_chunk`] solves unclamped: with [`MeshParams::clamp_to_cell`] (the
//! default) a minimizer outside the cell is replaced by the mass point of the crossings,
//! then [`MeshParams::project_to_surface`] (also the default) takes two Newton steps onto
//! the field and clamps those steps into the cell. Clamping the minimizer onto the cell
//! boundary would glue mirror-image cells onto one shared face. Because the vertex is the
//! least-squares intersection of the tangent planes, sharp edges and corners of the field
//! (where several planes meet inside one cell) are reproduced instead of rounded off,
//! unlike marching cubes.
//!
//! Sign convention: a sample is *inside* (solid) when it is negative, *outside* when it is
//! zero or positive, matching the usual signed-distance convention of this crate
//! ([`crate::sampler::TerrainMap::signed_distance`] is positive above the ground, negative
//! below it; see [`TerrainMapField`]).
//!
//! # How chunks are stitched at the same LOD
//!
//! Think of all chunks of a volume as cells of one global regular grid. A dual vertex is a
//! pure, deterministic function of its cell's world-space box and the field, so two chunks
//! that both need the vertex of a border cell compute bit-identical positions (all geometry
//! is built in `f64` from the chunk origin before the final cast to `f32`). Faces are dual to
//! sign-changing primal edges: the polygon of an edge joins the vertices of the cells
//! incident to that edge. Each such face is emitted by exactly one chunk — the chunk whose
//! half-open domain `[origin, origin + cells * cell_size)` contains the edge's midpoint — so
//! a chunk also emits faces whose polygon reaches one cell into a neighbor (a one-cell
//! halo), and the neighbor emits the complementary faces. Welded by exact position, the
//! union is the same manifold mesh one big chunk would have produced; no skirt geometry is
//! involved.
//!
//! # How a 2x LOD step is stitched without cracks (no skirts)
//!
//! When a neighbor chunk is one LOD coarser or finer (twice/half the cell size, see
//! [`NeighborLods`]), the two chunks' cells form a *graded* grid: fine cells on one side of
//! the shared plane, coarse cells on the other. The mesher enumerates primal edge segments
//! on the finest lattice involved and, for each sign-changing segment, collects the distinct
//! incident cells in cyclic order around the segment. Away from the step these are the usual
//! four cells (a quad). On the step, a segment can be incident to two fine cells on one side
//! and a single coarse cell on the other, giving a triangle, or to two fine and two coarse
//! cells, giving a quad that spans the step. Either way the polygon is a fan over vertices
//! both chunks compute identically: the fine chunk takes its border vertices on that face
//! from the coarse grid's dual cells (the halo cells across the plane are the neighbor's
//! coarse cells, and the coarse chunk in turn derives its halo vertices from the fine
//! cells). There is therefore no T-junction: every dual edge crossing the shared plane is
//! an edge of triangles from both chunks, with bit-identical endpoints. Graded-grid dual
//! faces of this kind are the regular-grid version of the transition faces used by balanced
//! octree contouring; here the grid stays regular (no octree), the transition is confined to
//! the chunk faces named in [`NeighborLods`], and LOD may change by at most one step across
//! a face. Coarse cells that border finer cells (*split* cells) take their Hermite data
//! from every half-size segment of their boundary -- edge halves and the segments through
//! each face center -- so a sheet that only clips the middle of a coarse edge or the center
//! of a coarse face still yields the coarse dual vertex the fine side's transition faces
//! need. Only the coarse cells touching a finer face are split; the rest of a coarse chunk
//! is meshed exactly as it would be without the finer neighbor, so it still welds to its
//! same-LOD neighbors. A face is emitted for a primal edge only when an odd number of its
//! finest segments change sign (a coarse edge crossed twice has no net crossing).
//!
//! Alignment requirement: with a coarser or finer neighbor, every entry of `cells` must be
//! even, and neighboring chunks must share their tangential origins, so the coarse lattice
//! of one chunk is a sublattice of the fine lattice of the other. A violation is reported as
//! [`MeshError::LodAlignment`] rather than meshed approximately.
//!
//! Where LOD steps meet along a chunk edge or at a corner (a fine chunk with coarser
//! neighbors on both `+x` and `+y`: the diagonal coarse chunk touches the fine one only
//! along an edge), every chunk involved must also describe its edge and corner
//! neighbors ([`NeighborLods::diagonals`]; [`NeighborLods::from_region_levels`] fills all
//! 26). A chunk described by its faces only cannot know about a finer chunk it touches
//! only along an edge, and the welded union then cracks along that edge.
//!
//! Known limitation: exact welding needs chunk origins and cell sizes whose sums are exact in
//! `f32`/`f64` (for example dyadic values): with origins such as `0.1 + 8 * 0.3` the two
//! chunks compute border vertices that differ in the last bits.
//!
//! Vertices of halo cells can lie up to one (neighbor-sized) cell outside the chunk's
//! nominal box. That is deliberate: those vertices belong to faces this chunk owns, and the
//! neighbor's faces reference the same positions from its own copies.
//!
//! # Determinism and failure behavior
//!
//! Output order and vertex bits depend only on grid indices (`BTreeMap` keys, fixed loop
//! order, a fixed Jacobi sweep budget and an IEEE-only rotation kernel): repeated calls
//! produce bit-identical meshes on every platform, whatever its libm. A non-finite field
//! sample or gradient, a non-positive or non-finite cell size, an empty
//! or oversized grid, a non-finite origin, invalid parameters and a misaligned LOD step all
//! return [`MeshError`]; empty (all-outside) and fully solid chunks return an empty mesh.
//! Degenerate Hermite data (a zero gradient, a single crossing, parallel planes) leaves
//! unconstrained axes at the mass point of the crossings. [`solve_qef`] then clamps into
//! its box. [`mesh_chunk`] uses that unclamped solve, and replaces a minimizer that falls
//! outside the cell with the mass point when `clamp_to_cell` is set.
//!
//! Source: T. Ju, F. Losasso, S. Schaefer, J. Warren, "Dual Contouring of Hermite Data",
//! ACM SIGGRAPH 2002. QEF formulation after the same paper; the symmetric eigensolver is the
//! classical cyclic Jacobi method (see `docs/provenance.md`).

use std::collections::BTreeMap;

use crate::sampler::TerrainMap;

/// A signed-distance field sampled by the mesher.
///
/// `sample` returns the signed distance at a world position: negative inside the solid,
/// positive outside, zero on the surface. Only the sign and the relative magnitudes along
/// grid edges matter for topology and crossing interpolation, but a true (or approximate)
/// distance gives the best vertex placement.
pub trait SdfField {
    /// Signed distance at world position `p` (negative inside, positive outside).
    fn sample(&self, p: [f32; 3]) -> f32;

    /// Gradient of the field at `p`, used as the surface normal of Hermite data.
    ///
    /// The provided default is a central finite difference of [`SdfField::sample`] with a
    /// fixed step of `1e-3`. Fields whose interesting features are far from unit scale, and
    /// fields with sharp features that must be preserved exactly, should override this with
    /// an analytic gradient.
    fn gradient(&self, p: [f32; 3]) -> [f32; 3] {
        const H: f32 = 1.0e-3;
        let mut g = [0.0_f32; 3];
        for axis in 0..3 {
            let mut hi = p;
            let mut lo = p;
            hi[axis] += H;
            lo[axis] -= H;
            g[axis] = (self.sample(hi) - self.sample(lo)) / (2.0 * H);
        }
        g
    }
}

/// Any closure `Fn([f32; 3]) -> f32` is a field, with the finite-difference gradient.
impl<F> SdfField for F
where
    F: Fn([f32; 3]) -> f32,
{
    fn sample(&self, p: [f32; 3]) -> f32 {
        self(p)
    }
}

/// [`SdfField`] adapter over this crate's heightfield distance
/// ([`TerrainMap::signed_distance`]): solid below the terrain surface, air above it.
///
/// Meshing a chunk with this field reproduces the heightfield surface as a volume mesh,
/// which is how a cave or overhang field combined with the heightfield (by `min`/`max` in
/// a user field) can share one meshing path with plain terrain.
#[derive(Clone, Copy, Debug)]
pub struct TerrainMapField<'a> {
    /// The heightfield whose signed distance is sampled.
    pub map: &'a TerrainMap,
}

impl SdfField for TerrainMapField<'_> {
    fn sample(&self, p: [f32; 3]) -> f32 {
        self.map.signed_distance(p)
    }
}

/// Errors returned by [`mesh_chunk`].
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
pub enum MeshError {
    /// `cell_size` was not finite and positive.
    #[error("volume mesh: cell_size must be finite and positive")]
    InvalidCellSize,
    /// A `cells` entry was zero, or the grid exceeded the configured cell limit.
    #[error("volume mesh: cells must be non-zero and within the cell limit")]
    InvalidCells,
    /// `origin` contained a non-finite component.
    #[error("volume mesh: origin must be finite")]
    InvalidOrigin,
    /// A meshing parameter was invalid (see [`MeshParams`]).
    #[error("volume mesh: invalid parameters")]
    InvalidParams,
    /// The field returned a non-finite sample at `position`.
    #[error("volume mesh: non-finite field sample at {position:?}")]
    NonFiniteSample {
        /// World position of the offending sample.
        position: [f32; 3],
    },
    /// The field returned a non-finite gradient at `position`.
    #[error("volume mesh: non-finite field gradient at {position:?}")]
    NonFiniteGradient {
        /// World position of the offending gradient.
        position: [f32; 3],
    },
    /// A coarser/finer LOD step was requested on a grid whose `cells` are not all even, so
    /// the coarse lattice is not a sublattice of the fine one.
    #[error("volume mesh: LOD step requires every cells entry to be even")]
    LodAlignment,
    /// A neighbor LOD differed from this chunk's LOD by more than one step.
    #[error("volume mesh: neighbor LOD differs by more than one step")]
    LodJumpTooLarge,
    /// The mesh would need more than `u32::MAX` vertices.
    #[error("volume mesh: vertex count overflow")]
    TooManyVertices,
}

/// Tunables of [`mesh_chunk`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeshParams {
    /// Relative singular-value threshold of the QEF pseudo-inverse: singular values below
    /// `singular_epsilon * largest_singular_value` are treated as zero, so directions the
    /// Hermite planes do not constrain keep the mass point's coordinate. Must be finite and
    /// in `[0, 1)`. Default `1e-6`.
    pub singular_epsilon: f32,
    /// Keep every dual vertex inside its own cell (the dual contouring paper's safeguard
    /// against a badly conditioned QEF throwing a vertex across the volume): a QEF
    /// minimizer outside the cell is replaced by the mass point of the cell's crossings,
    /// and the surface projection steps are clamped to the cell. Default `true`.
    pub clamp_to_cell: bool,
    /// After the QEF solve, pull the vertex onto the surface with two Newton steps
    /// `x -= f(x) * grad / |grad|^2`, re-clamping into the cell when `clamp_to_cell` is
    /// set. This removes most of the residual off-surface error a tangent-plane
    /// intersection has on curved fields, and is skipped per-vertex where the field is
    /// degenerate there (zero/non-finite gradient or sample, or an oversized step).
    /// Default `true`.
    pub project_to_surface: bool,
    /// Upper bound on `cells[0] * cells[1] * cells[2]` accepted by [`mesh_chunk`], guarding
    /// against accidental multi-gigabyte requests. Default `1 << 24`.
    pub cell_limit: usize,
}

impl Default for MeshParams {
    fn default() -> Self {
        Self {
            singular_epsilon: 1.0e-6,
            clamp_to_cell: true,
            project_to_surface: true,
            cell_limit: 1 << 24,
        }
    }
}

/// Resolution of one neighboring chunk relative to the chunk being meshed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NeighborLod {
    /// No chunk there (volume border). Halo cells are still generated at this chunk's
    /// resolution so border faces are well defined; the surface is simply cropped where it
    /// leaves the volume.
    Absent,
    /// Same cell size as this chunk. This is the default.
    #[default]
    Same,
    /// Twice this chunk's cell size (one LOD coarser).
    Coarser,
    /// Half this chunk's cell size (one LOD finer).
    Finer,
}

/// The relative LOD of the neighbors of a chunk, driving the crack-free LOD stitch
/// described in the module documentation.
///
/// The six face neighbors are named fields. The twelve edge and eight corner neighbors
/// live in [`NeighborLods::diagonals`]; set them (with [`NeighborLods::set_region`] or
/// [`NeighborLods::from_region_levels`]) wherever an LOD step meets a chunk edge or
/// corner, for example on the chunks diagonal to a finer chunk. Unset diagonals are
/// derived from the touching faces, which is crack free only where no LOD step meets an
/// edge or corner of this chunk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NeighborLods {
    /// Neighbor across the `-x` face.
    pub neg_x: NeighborLod,
    /// Neighbor across the `+x` face.
    pub pos_x: NeighborLod,
    /// Neighbor across the `-y` face.
    pub neg_y: NeighborLod,
    /// Neighbor across the `+y` face.
    pub pos_y: NeighborLod,
    /// Neighbor across the `-z` face.
    pub neg_z: NeighborLod,
    /// Neighbor across the `+z` face.
    pub pos_z: NeighborLod,
    /// Edge and corner neighbors, indexed `[dx + 1][dy + 1][dz + 1]` by the neighbor's
    /// chunk offset (each of `dx, dy, dz` in `-1..=1`). Entries with fewer than two
    /// non-zero offsets (this chunk and its face neighbors) are ignored. `None` derives
    /// the entry from the touching faces: coarser if any is coarser, finer if all are
    /// finer, otherwise same (or absent if all are absent).
    pub diagonals: [[[Option<NeighborLod>; 3]; 3]; 3],
}

impl NeighborLods {
    /// All neighbors at the same LOD as this chunk.
    #[must_use]
    pub const fn same() -> Self {
        Self {
            neg_x: NeighborLod::Same,
            pos_x: NeighborLod::Same,
            neg_y: NeighborLod::Same,
            pos_y: NeighborLod::Same,
            neg_z: NeighborLod::Same,
            pos_z: NeighborLod::Same,
            diagonals: [[[None; 3]; 3]; 3],
        }
    }

    /// No neighbors (isolated chunk / volume border on every face).
    #[must_use]
    pub const fn absent() -> Self {
        Self {
            neg_x: NeighborLod::Absent,
            pos_x: NeighborLod::Absent,
            neg_y: NeighborLod::Absent,
            pos_y: NeighborLod::Absent,
            neg_z: NeighborLod::Absent,
            pos_z: NeighborLod::Absent,
            diagonals: [[[None; 3]; 3]; 3],
        }
    }

    /// The six entries in `[-x, +x, -y, +y, -z, +z]` order.
    #[must_use]
    pub const fn as_array(&self) -> [NeighborLod; 6] {
        [
            self.neg_x, self.pos_x, self.neg_y, self.pos_y, self.neg_z, self.pos_z,
        ]
    }

    /// Builds relative LODs from absolute LOD levels, where a larger level means coarser
    /// (level `n` cells are `2^n` times the level-0 size).
    ///
    /// `levels` are in `[-x, +x, -y, +y, -z, +z]` order; `None` means no chunk there.
    ///
    /// # Errors
    ///
    /// Returns [`MeshError::LodJumpTooLarge`] when any present neighbor differs from
    /// `own_lod` by more than one level: stitch only one step at a time.
    pub fn from_neighbor_levels(own_lod: u32, levels: [Option<u32>; 6]) -> Result<Self, MeshError> {
        let mut out = [NeighborLod::Absent; 6];
        for (slot, level) in out.iter_mut().zip(levels.iter()) {
            *slot = match level {
                None => NeighborLod::Absent,
                Some(l) if *l == own_lod => NeighborLod::Same,
                Some(l) if *l >= 1 && *l - 1 == own_lod => NeighborLod::Coarser,
                Some(l) if own_lod >= 1 && *l == own_lod - 1 => NeighborLod::Finer,
                Some(_) => return Err(MeshError::LodJumpTooLarge),
            };
        }
        Ok(Self {
            neg_x: out[0],
            pos_x: out[1],
            neg_y: out[2],
            pos_y: out[3],
            neg_z: out[4],
            pos_z: out[5],
            diagonals: [[[None; 3]; 3]; 3],
        })
    }

    /// The relative LOD of the neighbor at chunk offset `offset` (each component's sign
    /// is used: `-1`, `0` or `+1`). The zero offset is this chunk ([`NeighborLod::Same`]);
    /// unset edge and corner entries are derived as documented on
    /// [`NeighborLods::diagonals`].
    #[must_use]
    pub fn region(&self, offset: [i32; 3]) -> NeighborLod {
        let o = [offset[0].signum(), offset[1].signum(), offset[2].signum()];
        let faces = self.as_array();
        let mut touching: Vec<NeighborLod> = Vec::with_capacity(3);
        for (axis, &d) in o.iter().enumerate() {
            if d < 0 {
                touching.push(faces[axis * 2]);
            } else if d > 0 {
                touching.push(faces[axis * 2 + 1]);
            }
        }
        match touching.len() {
            0 => NeighborLod::Same,
            1 => touching[0],
            _ => {
                let explicit = self.diagonals[(o[0] + 1) as usize][(o[1] + 1) as usize][(o[2] + 1) as usize];
                explicit.unwrap_or_else(|| derive_diagonal(&touching))
            }
        }
    }

    /// Sets the relative LOD of the neighbor at chunk offset `offset` (signs used, as in
    /// [`NeighborLods::region`]): a face field for one non-zero component, a
    /// [`NeighborLods::diagonals`] entry for two or three. The zero offset is ignored.
    pub fn set_region(&mut self, offset: [i32; 3], lod: NeighborLod) {
        let o = [offset[0].signum(), offset[1].signum(), offset[2].signum()];
        let nonzero = o.iter().filter(|d| **d != 0).count();
        match (nonzero, o) {
            (1, [-1, 0, 0]) => self.neg_x = lod,
            (1, [1, 0, 0]) => self.pos_x = lod,
            (1, [0, -1, 0]) => self.neg_y = lod,
            (1, [0, 1, 0]) => self.pos_y = lod,
            (1, [0, 0, -1]) => self.neg_z = lod,
            (1, [0, 0, 1]) => self.pos_z = lod,
            (2 | 3, _) => {
                self.diagonals[(o[0] + 1) as usize][(o[1] + 1) as usize][(o[2] + 1) as usize] = Some(lod);
            }
            _ => {}
        }
    }

    /// Builds relative LODs for all 26 neighbors from absolute LOD levels (larger is
    /// coarser), indexed `[dx + 1][dy + 1][dz + 1]` by chunk offset; `None` means no
    /// chunk there and the center entry is ignored.
    ///
    /// # Errors
    ///
    /// Returns [`MeshError::LodJumpTooLarge`] when any present neighbor differs from
    /// `own_lod` by more than one level.
    pub fn from_region_levels(own_lod: u32, levels: [[[Option<u32>; 3]; 3]; 3]) -> Result<Self, MeshError> {
        let mut out = Self::absent();
        for (ix, plane) in levels.iter().enumerate() {
            for (iy, row) in plane.iter().enumerate() {
                for (iz, level) in row.iter().enumerate() {
                    let offset = [ix as i32 - 1, iy as i32 - 1, iz as i32 - 1];
                    if offset == [0, 0, 0] {
                        continue;
                    }
                    let lod = match level {
                        None => NeighborLod::Absent,
                        Some(l) if *l == own_lod => NeighborLod::Same,
                        Some(l) if *l >= 1 && *l - 1 == own_lod => NeighborLod::Coarser,
                        Some(l) if own_lod >= 1 && *l == own_lod - 1 => NeighborLod::Finer,
                        Some(_) => return Err(MeshError::LodJumpTooLarge),
                    };
                    out.set_region(offset, lod);
                }
            }
        }
        Ok(out)
    }
}

/// The derived LOD of an unset edge/corner neighbor from its touching faces (see
/// [`NeighborLods::diagonals`]).
fn derive_diagonal(touching: &[NeighborLod]) -> NeighborLod {
    if touching.contains(&NeighborLod::Coarser) {
        NeighborLod::Coarser
    } else if touching.iter().all(|l| *l == NeighborLod::Finer) {
        NeighborLod::Finer
    } else if touching.contains(&NeighborLod::Same) || touching.contains(&NeighborLod::Finer) {
        NeighborLod::Same
    } else {
        NeighborLod::Absent
    }
}

/// An indexed triangle mesh produced by [`mesh_chunk`].
///
/// Winding is counter-clockwise when viewed from outside the solid (the side the field's
/// gradient points to), so face normals computed as `(b - a) x (c - a)` point outward.
/// `positions` and `normals` have the same length; `indices` length is a multiple of 3.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChunkMesh {
    /// Vertex positions in world space, one per dual cell that straddles the surface.
    pub positions: Vec<[f32; 3]>,
    /// Unit vertex normals (the normalized sum of the cell's Hermite normals).
    pub normals: Vec<[f32; 3]>,
    /// Triangle indices into `positions`/`normals`, three per triangle.
    pub indices: Vec<u32>,
}

impl ChunkMesh {
    /// Number of vertices.
    #[must_use]
    pub fn vertex_count(&self) -> usize {
        self.positions.len()
    }

    /// Number of triangles.
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    /// `true` when the mesh has no triangles.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
}

/// Solves a dual-contouring QEF: finds the point minimizing the sum of squared distances to
/// the planes `(points[i], normals[i])`, clamped into the axis-aligned box
/// `[clamp_min, clamp_max]`.
///
/// The minimizer is computed around the mass point (the average of `points`) through the
/// pseudo-inverse of the normal matrix `A^T A`: its eigendecomposition is computed with
/// cyclic Jacobi rotations, singular values at or below `singular_epsilon` times the
/// largest are dropped, and unconstrained directions keep the mass point's coordinate.
/// This is the unclamped pseudo-inverse [`mesh_chunk`] uses, then clamped into the box.
/// It does not apply [`mesh_chunk`]'s outside-cell mass-point replacement, and it does
/// not project onto a field ([`MeshParams::project_to_surface`] does that, in
/// [`mesh_chunk`] only).
///
/// Normals need not be unit length (they are normalized internally); zero or non-finite
/// normals are skipped, and if no usable plane remains the clamped mass point is returned.
///
/// Returns `None` when `points` is empty, when `points` and `normals` differ in length,
/// when any point or bound is non-finite, when `clamp_min` exceeds `clamp_max` on an axis,
/// or when `singular_epsilon` is not finite and in `[0, 1)`.
#[must_use]
pub fn solve_qef(
    points: &[[f32; 3]],
    normals: &[[f32; 3]],
    clamp_min: [f32; 3],
    clamp_max: [f32; 3],
    singular_epsilon: f32,
) -> Option<[f32; 3]> {
    if !singular_epsilon.is_finite() || !(0.0..1.0).contains(&singular_epsilon) {
        return None;
    }
    let lo = [
        f64::from(clamp_min[0]),
        f64::from(clamp_min[1]),
        f64::from(clamp_min[2]),
    ];
    let hi = [
        f64::from(clamp_max[0]),
        f64::from(clamp_max[1]),
        f64::from(clamp_max[2]),
    ];
    if lo.iter().chain(hi.iter()).any(|v| !v.is_finite()) {
        return None;
    }
    let pts: Vec<[f64; 3]> = points
        .iter()
        .map(|p| [f64::from(p[0]), f64::from(p[1]), f64::from(p[2])])
        .collect();
    let nrs: Vec<[f64; 3]> = normals
        .iter()
        .map(|n| [f64::from(n[0]), f64::from(n[1]), f64::from(n[2])])
        .collect();
    let solved = solve_qef_f64(&pts, &nrs, Some((lo, hi)), f64::from(singular_epsilon))?;
    Some([solved[0] as f32, solved[1] as f32, solved[2] as f32])
}

/// `f64` core of [`solve_qef`]. `clamp` is `None` for an unclamped minimizer.
fn solve_qef_f64(
    points: &[[f64; 3]],
    normals: &[[f64; 3]],
    clamp: Option<([f64; 3], [f64; 3])>,
    singular_epsilon: f64,
) -> Option<[f64; 3]> {
    if points.is_empty() || points.len() != normals.len() {
        return None;
    }
    if points.iter().flatten().any(|v| !v.is_finite()) {
        return None;
    }
    if let Some((lo, hi)) = clamp
        && (0..3).any(|a| !lo[a].is_finite() || !hi[a].is_finite() || lo[a] > hi[a])
    {
        return None;
    }
    // Mass point: the expansion center and the fallback for unconstrained directions.
    let mut mass = [0.0_f64; 3];
    for p in points {
        for a in 0..3 {
            mass[a] += p[a];
        }
    }
    let count = points.len() as f64;
    for m in &mut mass {
        *m /= count;
    }
    // Accumulate A^T A and A^T b around the mass point: minimize sum (n . (y - q))^2 with
    // q = p - mass, so A^T b becomes sum n * (n . q) and the solution is x = mass + y.
    let mut ata = [[0.0_f64; 3]; 3];
    let mut atb = [0.0_f64; 3];
    let mut usable = 0_usize;
    for (p, n) in points.iter().zip(normals.iter()) {
        let len2 = n[0] * n[0] + n[1] * n[1] + n[2] * n[2];
        if !len2.is_finite() || len2 <= 0.0 {
            continue;
        }
        let inv = 1.0 / len2.sqrt();
        let u = [n[0] * inv, n[1] * inv, n[2] * inv];
        let q = [p[0] - mass[0], p[1] - mass[1], p[2] - mass[2]];
        let d = u[0] * q[0] + u[1] * q[1] + u[2] * q[2];
        for r in 0..3 {
            atb[r] += u[r] * d;
            for c in 0..3 {
                ata[r][c] += u[r] * u[c];
            }
        }
        usable += 1;
    }
    let mut x = mass;
    if usable > 0 {
        let (values, vectors) = jacobi_eigen_symmetric(ata);
        let largest = values.iter().fold(0.0_f64, |m, v| m.max(v.max(0.0)));
        if largest > 0.0 {
            let threshold = singular_epsilon.max(0.0) * largest;
            // y = V diag(1/lambda) V^T atb, skipping dropped singular values.
            let mut y = [0.0_f64; 3];
            for k in 0..3 {
                let lambda = values[k].max(0.0);
                if lambda <= threshold {
                    continue;
                }
                // Column k of `vectors` is the eigenvector for `values[k]`.
                let v = [vectors[0][k], vectors[1][k], vectors[2][k]];
                let coeff = (v[0] * atb[0] + v[1] * atb[1] + v[2] * atb[2]) / lambda;
                for a in 0..3 {
                    y[a] += coeff * v[a];
                }
            }
            for a in 0..3 {
                x[a] = mass[a] + y[a];
            }
        }
    }
    if x.iter().any(|v| !v.is_finite()) {
        x = mass;
    }
    if let Some((lo, hi)) = clamp {
        for a in 0..3 {
            x[a] = x[a].clamp(lo[a], hi[a]);
        }
    }
    Some(x)
}

/// Eigendecomposition of a symmetric 3x3 matrix by cyclic Jacobi rotations.
///
/// Returns `(eigenvalues, v)` where column `k` of `v` is the eigenvector of
/// `eigenvalues[k]`. Always runs the full fixed sweep budget: the operation count must not
/// depend on a rounded residual, or two platforms can stop at different sweeps and diverge
/// by far more than an ulp. Each rotation uses only `f64` `+`, `*`, `/` and `sqrt`
/// (IEEE-754 correctly rounded on every target), so the result is bit-identical across
/// platforms, unlike an `atan2`/`sin_cos` formulation.
fn jacobi_eigen_symmetric(input: [[f64; 3]; 3]) -> ([f64; 3], [[f64; 3]; 3]) {
    let mut a = input;
    let mut v = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    for _sweep in 0..12 {
        for (p, q) in [(0_usize, 1_usize), (0, 2), (1, 2)] {
            let apq = a[p][q];
            if apq == 0.0 {
                continue;
            }
            // Rotation J in the (p, q) plane with J^T A J zeroing a[p][q]. With
            // w = (a_qq - a_pp) / (2 a_pq) = cot(2θ), t = tan(θ) is the smaller-magnitude
            // root of t² + 2wt - 1 = 0: t = sign(w) / (|w| + sqrt(w² + 1)).
            let w = (a[q][q] - a[p][p]) / (2.0 * apq);
            let mut t = 1.0 / (w.abs() + (w * w + 1.0).sqrt());
            if w.is_sign_negative() {
                t = -t;
            }
            let c = 1.0 / (t * t + 1.0).sqrt();
            let s = t * c;
            let mut j = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
            j[p][p] = c;
            j[q][q] = c;
            j[p][q] = s;
            j[q][p] = -s;
            a = mat3_mul(mat3_transpose(j), mat3_mul(a, j));
            v = mat3_mul(v, j);
        }
    }
    ([a[0][0], a[1][1], a[2][2]], v)
}

/// 3x3 matrix product `lhs * rhs` (row-major).
fn mat3_mul(lhs: [[f64; 3]; 3], rhs: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let mut out = [[0.0_f64; 3]; 3];
    for r in 0..3 {
        for c in 0..3 {
            out[r][c] = lhs[r][0] * rhs[0][c] + lhs[r][1] * rhs[1][c] + lhs[r][2] * rhs[2][c];
        }
    }
    out
}

/// 3x3 transpose.
fn mat3_transpose(m: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    [
        [m[0][0], m[1][0], m[2][0]],
        [m[0][1], m[1][1], m[2][1]],
        [m[0][2], m[1][2], m[2][2]],
    ]
}

/// Normalizes a vector, returning `None` for zero or non-finite input.
fn normalize3(v: [f64; 3]) -> Option<[f64; 3]> {
    let len2 = v[0] * v[0] + v[1] * v[1] + v[2] * v[2];
    if !len2.is_finite() || len2 <= 0.0 {
        return None;
    }
    let inv = 1.0 / len2.sqrt();
    Some([v[0] * inv, v[1] * inv, v[2] * inv])
}

/// Average of a non-empty point set (the QEF mass point).
fn mass_point(points: &[[f64; 3]]) -> [f64; 3] {
    let mut mass = [0.0_f64; 3];
    for p in points {
        for (m, v) in mass.iter_mut().zip(p.iter()) {
            *m += v;
        }
    }
    let count = points.len().max(1) as f64;
    [mass[0] / count, mass[1] / count, mass[2] / count]
}

/// Identifies one dual cell on the finest lattice: its minimum-corner node index (in units
/// of the finest cell involved, relative to the chunk origin) and its size in those units.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CellKey {
    origin: [i32; 3],
    size: i32,
}

/// Working state of one [`mesh_chunk`] call.
struct Mesher<'a, F: SdfField + ?Sized> {
    field: &'a F,
    origin: [f64; 3],
    /// Finest cell size involved (this chunk's, halved when a neighbor is finer).
    unit: f64,
    /// This chunk's cell size measured in `unit`s (1 or 2).
    own_units: i32,
    /// This chunk's extent measured in `unit`s.
    extent_units: [i32; 3],
    /// Resolved relative LOD of every neighbor region, indexed `[dx + 1][dy + 1][dz + 1]`
    /// (the center entry is this chunk, [`NeighborLod::Same`]).
    regions: [[[NeighborLod; 3]; 3]; 3],
    params: &'a MeshParams,
    node_cache: BTreeMap<[i32; 3], f32>,
    cell_cache: BTreeMap<CellKey, Option<u32>>,
    /// Dual face candidates keyed by their sorted incident cells (see [`PendingFace`]).
    pending_faces: BTreeMap<Vec<CellKey>, PendingFace>,
    mesh: ChunkMesh,
}

/// A dual face candidate: the incident cells of one primal edge in cyclic order, the first
/// sign-changing finest segment of that edge, and how many of its finest segments change
/// sign. A face is emitted only when that count is odd: a coarse edge crossed twice (in
/// and out again between its corners) has no net crossing and must not get a face, or the
/// dual edges around it would be used an odd number of times (an open mesh).
struct PendingFace {
    cells: Vec<CellKey>,
    start: [i32; 3],
    axis: usize,
    d0: f32,
    d1: f32,
    crossings: u32,
}

impl<F: SdfField + ?Sized> Mesher<'_, F> {
    /// World position of a lattice node given in (possibly fractional) units.
    fn node_world(&self, units: [f64; 3]) -> [f64; 3] {
        [
            self.origin[0] + units[0] * self.unit,
            self.origin[1] + units[1] * self.unit,
            self.origin[2] + units[2] * self.unit,
        ]
    }

    /// Cached field sample at an integer lattice node.
    fn node_sample(&mut self, node: [i32; 3]) -> Result<f32, MeshError> {
        if let Some(value) = self.node_cache.get(&node) {
            return Ok(*value);
        }
        let world = self.node_world([f64::from(node[0]), f64::from(node[1]), f64::from(node[2])]);
        let position = [world[0] as f32, world[1] as f32, world[2] as f32];
        let value = self.field.sample(position);
        if !value.is_finite() {
            return Err(MeshError::NonFiniteSample { position });
        }
        self.node_cache.insert(node, value);
        Ok(value)
    }

    /// Field gradient at a world position, rejecting non-finite components.
    fn gradient_at(&self, world: [f64; 3]) -> Result<[f64; 3], MeshError> {
        let position = [world[0] as f32, world[1] as f32, world[2] as f32];
        let g = self.field.gradient(position);
        if g.iter().any(|v| !v.is_finite()) {
            return Err(MeshError::NonFiniteGradient { position });
        }
        Ok([f64::from(g[0]), f64::from(g[1]), f64::from(g[2])])
    }

    /// Size, in units, of a cell of the neighbor across one face.
    fn neighbor_units(&self, lod: NeighborLod) -> i32 {
        match lod {
            NeighborLod::Absent | NeighborLod::Same => self.own_units,
            NeighborLod::Coarser => 2 * self.own_units,
            NeighborLod::Finer => 1,
        }
    }

    /// The dual cell containing a point given in units, under the graded tiling of the
    /// module documentation: inside the chunk cells are this chunk's, outside it they are
    /// those of the face, edge or corner neighbor whose region contains the point, all
    /// aligned to lattices anchored at the chunk origin.
    fn cell_for_point(&self, point: [f64; 3]) -> CellKey {
        let mut region = [1_usize; 3];
        for (axis, &coord) in point.iter().enumerate() {
            if coord < 0.0 {
                region[axis] = 0;
            } else if coord >= f64::from(self.extent_units[axis]) {
                region[axis] = 2;
            }
        }
        let size = self.neighbor_units(self.regions[region[0]][region[1]][region[2]]);
        let mut origin = [0_i32; 3];
        for axis in 0..3 {
            origin[axis] = (point[axis] / f64::from(size)).floor() as i32 * size;
        }
        CellKey { origin, size }
    }

    /// Whether a cell borders finer cells across an LOD step (see `compute_cell_vertex`).
    ///
    /// A cell larger than this chunk's own cells is a coarse halo cell of a coarser
    /// neighbor, which borders this chunk's cells. A cell of this chunk's size, when a
    /// neighbor is finer, is split when its closed box touches the region of a finer
    /// face, edge or corner neighbor. The coarse chunks around a finer chunk decide the
    /// same way from their own neighbor regions, so they agree on every shared cell as
    /// long as the edge and corner neighbors are described (see [`NeighborLods`]).
    fn cell_is_split(&self, key: CellKey) -> bool {
        if key.size > self.own_units {
            return true;
        }
        if key.size < self.own_units || self.own_units < 2 {
            return false;
        }
        for (ix, plane) in self.regions.iter().enumerate() {
            for (iy, row) in plane.iter().enumerate() {
                for (iz, lod) in row.iter().enumerate() {
                    if *lod != NeighborLod::Finer {
                        continue;
                    }
                    let touches = [ix, iy, iz].iter().enumerate().all(|(axis, &r)| {
                        let lo = key.origin[axis];
                        let hi = lo + key.size;
                        let extent = self.extent_units[axis];
                        match r {
                            0 => lo <= 0,
                            2 => hi >= extent,
                            _ => lo <= extent && hi >= 0,
                        }
                    });
                    if touches {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// The dual vertex of a cell, if the cell straddles the surface. Computed once per
    /// cell and cached; a pure function of the cell box and the field.
    fn cell_vertex(&mut self, key: CellKey) -> Result<Option<u32>, MeshError> {
        if let Some(cached) = self.cell_cache.get(&key) {
            return Ok(*cached);
        }
        let vertex = self.compute_cell_vertex(key)?;
        self.cell_cache.insert(key, vertex);
        Ok(vertex)
    }

    /// Hermite data collection and QEF placement for one cell.
    fn compute_cell_vertex(&mut self, key: CellKey) -> Result<Option<u32>, MeshError> {
        // Hermite data comes from the cell's boundary segments. An ordinary cell uses its
        // twelve edges (the textbook rule). A *split* cell -- a coarse cell bordering finer
        // cells across an LOD step -- uses every half-size segment of its boundary (edge
        // halves plus the four segments through each face center), because the finer
        // side's transition faces are dual to those segments and need this cell's vertex
        // whenever any of them changes sign. Which cells are split is decided from the
        // chunk's LOD faces only, so both chunks of a face-adjacent pair agree on it.
        let split = self.cell_is_split(key);
        let step = if split { key.size / 2 } else { key.size };
        let per_axis = key.size / step;
        let mut points: Vec<[f64; 3]> = Vec::new();
        let mut normals: Vec<[f64; 3]> = Vec::new();
        for axis in 0..3 {
            let (b_axis, c_axis) = ((axis + 1) % 3, (axis + 2) % 3);
            for ib in 0..=per_axis {
                for ic in 0..=per_axis {
                    let on_boundary = ib == 0 || ib == per_axis || ic == 0 || ic == per_axis;
                    if !on_boundary {
                        continue;
                    }
                    for ia in 0..per_axis {
                        let mut a = key.origin;
                        a[axis] += ia * step;
                        a[b_axis] += ib * step;
                        a[c_axis] += ic * step;
                        let mut b = a;
                        b[axis] += step;
                        let (d0, d1) = (self.node_sample(a)?, self.node_sample(b)?);
                        if (d0 < 0.0) == (d1 < 0.0) {
                            continue;
                        }
                        let denom = f64::from(d0) - f64::from(d1);
                        let t = if denom == 0.0 {
                            0.5
                        } else {
                            (f64::from(d0) / denom).clamp(0.0, 1.0)
                        };
                        let crossing = self.node_world([
                            f64::from(a[0]) + (f64::from(b[0]) - f64::from(a[0])) * t,
                            f64::from(a[1]) + (f64::from(b[1]) - f64::from(a[1])) * t,
                            f64::from(a[2]) + (f64::from(b[2]) - f64::from(a[2])) * t,
                        ]);
                        let gradient = self.gradient_at(crossing)?;
                        let normal = normalize3(gradient).unwrap_or_else(|| {
                            // Degenerate gradient: fall back to the edge direction, oriented
                            // from the inside endpoint towards the outside one.
                            let mut fallback = [0.0_f64; 3];
                            fallback[axis] = if d0 < 0.0 { 1.0 } else { -1.0 };
                            fallback
                        });
                        points.push(crossing);
                        normals.push(normal);
                    }
                }
            }
        }
        if points.is_empty() {
            return Ok(None);
        }
        let lo = self.node_world([
            f64::from(key.origin[0]),
            f64::from(key.origin[1]),
            f64::from(key.origin[2]),
        ]);
        let far = [
            key.origin[0] + key.size,
            key.origin[1] + key.size,
            key.origin[2] + key.size,
        ];
        let hi = self.node_world([f64::from(far[0]), f64::from(far[1]), f64::from(far[2])]);
        let Some(unconstrained) =
            solve_qef_f64(&points, &normals, None, f64::from(self.params.singular_epsilon))
        else {
            return Ok(None);
        };
        // A minimizer outside the cell falls back to the mass point of the crossings
        // (which lies inside the cell). Clamping it instead would push the vertices of
        // mirror-image cells (a field symmetric about a grid plane) onto the same point
        // of their shared face, giving coincident vertices and zero-area triangles.
        let mut solved = if self.params.clamp_to_cell {
            let tolerance = 1.0e-9 * f64::from(key.size) * self.unit;
            let inside = (0..3)
                .all(|a| unconstrained[a] >= lo[a] - tolerance && unconstrained[a] <= hi[a] + tolerance);
            if inside {
                [
                    unconstrained[0].clamp(lo[0], hi[0]),
                    unconstrained[1].clamp(lo[1], hi[1]),
                    unconstrained[2].clamp(lo[2], hi[2]),
                ]
            } else {
                mass_point(&points)
            }
        } else {
            unconstrained
        };
        if self.params.project_to_surface {
            // Two Newton steps onto the surface. Any degeneracy keeps the QEF point.
            let max_step = f64::from(key.size) * self.unit * 3.0_f64.sqrt();
            for _iteration in 0..2 {
                let position = [solved[0] as f32, solved[1] as f32, solved[2] as f32];
                let value = self.field.sample(position);
                let gradient = self.field.gradient(position);
                if !value.is_finite() || gradient.iter().any(|v| !v.is_finite()) {
                    break;
                }
                let g = [
                    f64::from(gradient[0]),
                    f64::from(gradient[1]),
                    f64::from(gradient[2]),
                ];
                let denom = g[0] * g[0] + g[1] * g[1] + g[2] * g[2];
                if !(denom > 0.0 && denom.is_finite()) {
                    break;
                }
                let step = [
                    f64::from(value) * g[0] / denom,
                    f64::from(value) * g[1] / denom,
                    f64::from(value) * g[2] / denom,
                ];
                let step_len = (step[0] * step[0] + step[1] * step[1] + step[2] * step[2]).sqrt();
                if !step_len.is_finite() || step_len > max_step {
                    break;
                }
                for a in 0..3 {
                    solved[a] -= step[a];
                    if self.params.clamp_to_cell {
                        solved[a] = solved[a].clamp(lo[a], hi[a]);
                    }
                }
            }
        }
        let mut normal_sum = [0.0_f64; 3];
        for n in &normals {
            for a in 0..3 {
                normal_sum[a] += n[a];
            }
        }
        let vertex_normal = normalize3(normal_sum).unwrap_or([0.0, 1.0, 0.0]);
        let index = u32::try_from(self.mesh.positions.len()).map_err(|_| MeshError::TooManyVertices)?;
        self.mesh
            .positions
            .push([solved[0] as f32, solved[1] as f32, solved[2] as f32]);
        self.mesh.normals.push([
            vertex_normal[0] as f32,
            vertex_normal[1] as f32,
            vertex_normal[2] as f32,
        ]);
        Ok(Some(index))
    }

    /// Records one sign-changing finest primal edge segment against its dual face
    /// candidate, if the segment has a genuine face in the graded grid (at least three
    /// distinct incident cells). Faces are emitted later by `emit_face`.
    fn collect_edge_face(&mut self, start: [i32; 3], axis: usize, d0: f32, d1: f32) {
        let others = [(axis + 1) % 3, (axis + 2) % 3];
        let mut midpoint = [f64::from(start[0]), f64::from(start[1]), f64::from(start[2])];
        midpoint[axis] += 0.5;
        // Cyclic order around the edge (either sense; winding is fixed up below).
        let quadrant_offsets = [(0.25, 0.25), (-0.25, 0.25), (-0.25, -0.25), (0.25, -0.25)];
        let mut cells: Vec<CellKey> = Vec::with_capacity(4);
        for (ob, oc) in quadrant_offsets {
            let mut point = midpoint;
            point[others[0]] += ob;
            point[others[1]] += oc;
            let key = self.cell_for_point(point);
            if cells.last() != Some(&key) {
                cells.push(key);
            }
        }
        if cells.len() > 1 && cells.first() == cells.last() {
            cells.pop();
        }
        if cells.len() < 3 {
            return;
        }
        // A repeated (non-consecutive) incident cell would make a bowtie polygon; such a
        // configuration only arises in mixed-size corner halos, and skipping the face is
        // the defined fallback there.
        let mut sorted = cells.clone();
        sorted.sort();
        sorted.dedup();
        if sorted.len() != cells.len() {
            return;
        }
        // A coarse primal edge is enumerated as several finest segments with the same
        // incident cells: they share one candidate, and its crossing parity decides.
        self.pending_faces
            .entry(sorted)
            .and_modify(|face| face.crossings += 1)
            .or_insert(PendingFace {
                cells,
                start,
                axis,
                d0,
                d1,
                crossings: 1,
            });
    }

    /// Emits the dual polygon of one face candidate as a triangle fan, oriented outward;
    /// skipped when an incident cell has no vertex.
    fn emit_face(&mut self, face: &PendingFace) -> Result<(), MeshError> {
        let PendingFace {
            ref cells,
            start,
            axis,
            d0,
            d1,
            ..
        } = *face;
        let mut polygon: Vec<u32> = Vec::with_capacity(cells.len());
        for key in cells {
            match self.cell_vertex(*key)? {
                Some(index) => polygon.push(index),
                None => return Ok(()),
            }
        }
        // Orient the polygon so its Newell normal agrees with the field gradient at the
        // edge crossing (outward). This fixes the winding sense left open above.
        let denom = f64::from(d0) - f64::from(d1);
        let t = if denom == 0.0 {
            0.5
        } else {
            (f64::from(d0) / denom).clamp(0.0, 1.0)
        };
        let mut crossing_units = [f64::from(start[0]), f64::from(start[1]), f64::from(start[2])];
        crossing_units[axis] += t;
        let crossing = self.node_world(crossing_units);
        let gradient = self.gradient_at(crossing)?;
        let outward = normalize3(gradient).unwrap_or_else(|| {
            let mut fallback = [0.0_f64; 3];
            fallback[axis] = if d0 < 0.0 { 1.0 } else { -1.0 };
            fallback
        });
        let mut newell = [0.0_f64; 3];
        for i in 0..polygon.len() {
            let pa = self.mesh.positions[polygon[i] as usize];
            let pb = self.mesh.positions[polygon[(i + 1) % polygon.len()] as usize];
            let a = [f64::from(pa[0]), f64::from(pa[1]), f64::from(pa[2])];
            let b = [f64::from(pb[0]), f64::from(pb[1]), f64::from(pb[2])];
            newell[0] += (a[1] - b[1]) * (a[2] + b[2]);
            newell[1] += (a[2] - b[2]) * (a[0] + b[0]);
            newell[2] += (a[0] - b[0]) * (a[1] + b[1]);
        }
        let agreement = newell[0] * outward[0] + newell[1] * outward[1] + newell[2] * outward[2];
        // A zero agreement (degenerate polygon) keeps the deterministic order.
        if agreement < 0.0 {
            polygon.reverse();
        }
        for i in 1..polygon.len().saturating_sub(1) {
            let tri = [polygon[0], polygon[i], polygon[i + 1]];
            if tri[0] == tri[1] || tri[1] == tri[2] || tri[0] == tri[2] {
                continue;
            }
            self.mesh.indices.extend_from_slice(&tri);
        }
        Ok(())
    }
}

/// Meshes one regular-grid chunk of a signed-distance field by dual contouring.
///
/// * `field` — the SDF (negative inside). Closures and [`TerrainMapField`] qualify.
/// * `origin` — world position of the chunk's minimum grid corner.
/// * `cell_size` — edge length of one (cubic) cell of this chunk, in world units.
/// * `cells` — number of cells along each axis; the chunk spans
///   `[origin, origin + cells * cell_size)`.
/// * `lod_neighbors` — relative LOD of the face (and edge/corner) neighbors; see the module
///   documentation for the stitch. With a coarser or finer neighbor every `cells` entry
///   must be even.
/// * `params` — QEF and safety tunables.
///
/// The returned mesh may contain vertices up to one neighbor-sized cell outside the
/// chunk's box (halo dual vertices of faces this chunk owns), and is open where the
/// surface is cropped by the chunk boundary unless neighboring chunks are welded to it
/// by exact position. See the module documentation for the full stitching contract.
///
/// # Errors
///
/// Returns [`MeshError`] for an invalid grid, origin, parameters or LOD alignment, and at
/// the first non-finite field sample or gradient encountered. Empty and fully solid
/// chunks are not errors: they yield an empty [`ChunkMesh`].
pub fn mesh_chunk<F: SdfField + ?Sized>(
    field: &F,
    origin: [f32; 3],
    cell_size: f32,
    cells: [u32; 3],
    lod_neighbors: NeighborLods,
    params: &MeshParams,
) -> Result<ChunkMesh, MeshError> {
    if !cell_size.is_finite() || cell_size <= 0.0 {
        return Err(MeshError::InvalidCellSize);
    }
    if origin.iter().any(|v| !v.is_finite()) {
        return Err(MeshError::InvalidOrigin);
    }
    if !params.singular_epsilon.is_finite() || !(0.0..1.0).contains(&params.singular_epsilon) {
        return Err(MeshError::InvalidParams);
    }
    if cells.iter().any(|c| *c == 0 || *c > (i32::MAX / 4) as u32) {
        return Err(MeshError::InvalidCells);
    }
    let total_cells = (0..3).try_fold(1_usize, |acc, a| {
        acc.checked_mul(usize::try_from(cells[a]).unwrap_or(usize::MAX))
    });
    match total_cells {
        Some(total) if total <= params.cell_limit => {}
        _ => return Err(MeshError::InvalidCells),
    }
    let _span = tracing::info_span!(
        target: "regia_terrain",
        "volume_mesh_chunk",
        cells_x = cells[0],
        cells_y = cells[1],
        cells_z = cells[2]
    )
    .entered();
    let mut regions = [[[NeighborLod::Same; 3]; 3]; 3];
    for (ix, plane) in regions.iter_mut().enumerate() {
        for (iy, row) in plane.iter_mut().enumerate() {
            for (iz, lod) in row.iter_mut().enumerate() {
                *lod = lod_neighbors.region([ix as i32 - 1, iy as i32 - 1, iz as i32 - 1]);
            }
        }
    }
    let neighbors: Vec<NeighborLod> = regions.iter().flatten().flatten().copied().collect();
    let has_step = neighbors
        .iter()
        .any(|l| matches!(l, NeighborLod::Coarser | NeighborLod::Finer));
    if has_step && cells.iter().any(|c| c % 2 != 0) {
        return Err(MeshError::LodAlignment);
    }
    let has_finer = neighbors.iter().any(|l| matches!(l, NeighborLod::Finer));
    let (unit, own_units): (f64, i32) = if has_finer {
        (f64::from(cell_size) * 0.5, 2)
    } else {
        (f64::from(cell_size), 1)
    };
    let extent_units = [
        cells[0] as i32 * own_units,
        cells[1] as i32 * own_units,
        cells[2] as i32 * own_units,
    ];
    let mut mesher = Mesher {
        field,
        origin: [f64::from(origin[0]), f64::from(origin[1]), f64::from(origin[2])],
        unit,
        own_units,
        extent_units,
        regions,
        params,
        node_cache: BTreeMap::new(),
        cell_cache: BTreeMap::new(),
        pending_faces: BTreeMap::new(),
        mesh: ChunkMesh::default(),
    };
    // One primal edge per (finest-lattice start node inside the chunk, axis): exactly the
    // edges whose midpoint lies in the chunk's half-open domain, so neighbors emit the
    // complementary set and no face is produced twice.
    for iz in 0..extent_units[2] {
        for iy in 0..extent_units[1] {
            for ix in 0..extent_units[0] {
                let start = [ix, iy, iz];
                for axis in 0..3 {
                    let mut end = start;
                    end[axis] += 1;
                    let d0 = mesher.node_sample(start)?;
                    let d1 = mesher.node_sample(end)?;
                    if (d0 < 0.0) == (d1 < 0.0) {
                        continue;
                    }
                    mesher.collect_edge_face(start, axis, d0, d1);
                }
            }
        }
    }
    let pending = std::mem::take(&mut mesher.pending_faces);
    for face in pending.values() {
        if !face.crossings.is_multiple_of(2) {
            mesher.emit_face(face)?;
        }
    }
    Ok(mesher.mesh)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::sampler::TerrainMap;

    /// An unsigned edge keyed by the bit patterns of its two endpoint positions.
    type BitEdge = ((u32, u32, u32), (u32, u32, u32));

    /// Sphere SDF using the trait's finite-difference gradient.
    struct Sphere {
        center: [f32; 3],
        radius: f32,
    }

    impl SdfField for Sphere {
        fn sample(&self, p: [f32; 3]) -> f32 {
            let d = [
                p[0] - self.center[0],
                p[1] - self.center[1],
                p[2] - self.center[2],
            ];
            (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() - self.radius
        }
    }

    /// Axis-aligned box SDF with an exact face normal as its gradient.
    struct BoxField {
        half: f32,
    }

    impl SdfField for BoxField {
        fn sample(&self, p: [f32; 3]) -> f32 {
            let q = [
                p[0].abs() - self.half,
                p[1].abs() - self.half,
                p[2].abs() - self.half,
            ];
            let outside = (q[0].max(0.0).powi(2) + q[1].max(0.0).powi(2) + q[2].max(0.0).powi(2)).sqrt();
            let inside = q[0].max(q[1]).max(q[2]).min(0.0);
            outside + inside
        }

        fn gradient(&self, p: [f32; 3]) -> [f32; 3] {
            let mut axis = 0_usize;
            for a in 1..3 {
                if p[a].abs() - self.half > p[axis].abs() - self.half {
                    axis = a;
                }
            }
            let mut g = [0.0_f32; 3];
            g[axis] = if p[axis] < 0.0 { -1.0 } else { 1.0 };
            g
        }
    }

    fn default_params() -> MeshParams {
        MeshParams::default()
    }

    fn position_bits(p: [f32; 3]) -> (u32, u32, u32) {
        (p[0].to_bits(), p[1].to_bits(), p[2].to_bits())
    }

    /// Unsigned edge -> incident triangle count, by vertex index.
    fn edge_counts(mesh: &ChunkMesh) -> BTreeMap<(u32, u32), usize> {
        let mut counts: BTreeMap<(u32, u32), usize> = BTreeMap::new();
        for tri in mesh.indices.as_chunks::<3>().0 {
            for (a, b) in [(tri[0], tri[1]), (tri[1], tri[2]), (tri[2], tri[0])] {
                *counts.entry((a.min(b), a.max(b))).or_insert(0) += 1;
            }
        }
        counts
    }

    /// Boundary edges (exactly one incident triangle), keyed by endpoint positions so two
    /// chunk meshes can be compared exactly.
    fn boundary_position_edges(mesh: &ChunkMesh) -> BTreeSet<BitEdge> {
        let mut counts: BTreeMap<BitEdge, usize> = BTreeMap::new();
        for tri in mesh.indices.as_chunks::<3>().0 {
            let pts = [
                position_bits(mesh.positions[tri[0] as usize]),
                position_bits(mesh.positions[tri[1] as usize]),
                position_bits(mesh.positions[tri[2] as usize]),
            ];
            for (a, b) in [(pts[0], pts[1]), (pts[1], pts[2]), (pts[2], pts[0])] {
                let key = if a <= b { (a, b) } else { (b, a) };
                *counts.entry(key).or_insert(0) += 1;
            }
        }
        counts
            .into_iter()
            .filter(|(_, count)| *count == 1)
            .map(|(edge, _)| edge)
            .collect()
    }

    /// Position-keyed edge counts over the welded union of several meshes.
    fn union_edge_counts(meshes: &[&ChunkMesh]) -> BTreeMap<BitEdge, usize> {
        let mut counts: BTreeMap<BitEdge, usize> = BTreeMap::new();
        for mesh in meshes {
            for tri in mesh.indices.as_chunks::<3>().0 {
                let pts = [
                    position_bits(mesh.positions[tri[0] as usize]),
                    position_bits(mesh.positions[tri[1] as usize]),
                    position_bits(mesh.positions[tri[2] as usize]),
                ];
                for (a, b) in [(pts[0], pts[1]), (pts[1], pts[2]), (pts[2], pts[0])] {
                    let key = if a <= b { (a, b) } else { (b, a) };
                    *counts.entry(key).or_insert(0) += 1;
                }
            }
        }
        counts
    }

    fn signed_volume(mesh: &ChunkMesh) -> f64 {
        let mut volume = 0.0_f64;
        for tri in mesh.indices.as_chunks::<3>().0 {
            let p: Vec<[f64; 3]> = tri
                .iter()
                .map(|i| {
                    let q = mesh.positions[*i as usize];
                    [f64::from(q[0]), f64::from(q[1]), f64::from(q[2])]
                })
                .collect();
            let cross = [
                p[1][1] * p[2][2] - p[1][2] * p[2][1],
                p[1][2] * p[2][0] - p[1][0] * p[2][2],
                p[1][0] * p[2][1] - p[1][1] * p[2][0],
            ];
            volume += (p[0][0] * cross[0] + p[0][1] * cross[1] + p[0][2] * cross[2]) / 6.0;
        }
        volume
    }

    fn assert_valid_indices(mesh: &ChunkMesh) {
        assert_eq!(mesh.positions.len(), mesh.normals.len());
        assert_eq!(mesh.indices.len() % 3, 0);
        for tri in mesh.indices.as_chunks::<3>().0 {
            for index in tri {
                assert!((*index as usize) < mesh.positions.len(), "index {index}");
            }
            assert!(tri[0] != tri[1] && tri[1] != tri[2] && tri[0] != tri[2]);
            let (a, b, c) = (
                mesh.positions[tri[0] as usize],
                mesh.positions[tri[1] as usize],
                mesh.positions[tri[2] as usize],
            );
            assert!(a != b && b != c && a != c, "degenerate positions");
        }
    }

    #[test]
    fn sphere_is_a_closed_manifold_with_euler_two() {
        let field = Sphere {
            center: [0.11, -0.07, 0.05],
            radius: 1.37,
        };
        let mesh = mesh_chunk(
            &field,
            [-2.0, -2.0, -2.0],
            0.125,
            [32, 32, 32],
            NeighborLods::absent(),
            &default_params(),
        )
        .expect("sphere mesh");
        assert_valid_indices(&mesh);
        assert!(mesh.vertex_count() > 500, "vertices {}", mesh.vertex_count());
        let counts = edge_counts(&mesh);
        assert!(
            counts.values().all(|count| *count == 2),
            "non-manifold edge in {:?}",
            counts.iter().find(|(_, c)| **c != 2)
        );
        let euler = mesh.vertex_count() as i64 - counts.len() as i64 + mesh.triangle_count() as i64;
        assert_eq!(euler, 2, "Euler characteristic");
        // Distance from every dual vertex to the true surface under 1% of the radius.
        for p in &mesh.positions {
            assert!(
                field.sample(*p).abs() < 0.01 * field.radius,
                "vertex {p:?} is {} from the surface",
                field.sample(*p)
            );
        }
    }

    #[test]
    fn sphere_winding_and_normals_point_outward() {
        let field = Sphere {
            center: [0.03, 0.0, -0.02],
            radius: 1.1,
        };
        let mesh = mesh_chunk(
            &field,
            [-1.5, -1.5, -1.5],
            0.25,
            [12, 12, 12],
            NeighborLods::absent(),
            &default_params(),
        )
        .expect("sphere mesh");
        let volume = signed_volume(&mesh);
        let expected = 4.0 / 3.0 * std::f64::consts::PI * 1.1_f64.powi(3);
        assert!(volume > 0.0, "inward winding, volume {volume}");
        assert!(
            (volume - expected).abs() < 0.15 * expected,
            "volume {volume} vs sphere {expected}"
        );
        for (p, n) in mesh.positions.iter().zip(mesh.normals.iter()) {
            let radial = [
                p[0] - field.center[0],
                p[1] - field.center[1],
                p[2] - field.center[2],
            ];
            let dot = radial[0] * n[0] + radial[1] * n[1] + radial[2] * n[2];
            assert!(dot > 0.0, "normal {n:?} at {p:?} points inward");
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            assert!((len - 1.0).abs() < 1.0e-5, "normal length {len}");
        }
    }

    #[test]
    fn cube_keeps_its_eight_sharp_corners() {
        let field = BoxField { half: 1.03 };
        let mesh = mesh_chunk(
            &field,
            [-2.0, -2.0, -2.0],
            0.25,
            [16, 16, 16],
            NeighborLods::absent(),
            &default_params(),
        )
        .expect("cube mesh");
        assert_valid_indices(&mesh);
        for sx in [-1.0_f32, 1.0] {
            for sy in [-1.0_f32, 1.0] {
                for sz in [-1.0_f32, 1.0] {
                    let corner = [sx * 1.03, sy * 1.03, sz * 1.03];
                    let found = mesh
                        .positions
                        .iter()
                        .any(|p| (0..3).all(|a| (p[a] - corner[a]).abs() < 1.0e-3));
                    assert!(found, "no vertex within 1e-3 of corner {corner:?}");
                }
            }
        }
    }

    #[test]
    fn adjacent_same_lod_chunks_share_identical_border_vertices() {
        let field = Sphere {
            center: [2.0, 0.07, -0.05],
            radius: 0.9,
        };
        let mut lod_a = NeighborLods::absent();
        lod_a.pos_x = NeighborLod::Same;
        let mut lod_b = NeighborLods::absent();
        lod_b.neg_x = NeighborLod::Same;
        let a = mesh_chunk(
            &field,
            [0.0, -1.5, -1.5],
            0.25,
            [8, 12, 12],
            lod_a,
            &default_params(),
        )
        .expect("chunk a");
        let b = mesh_chunk(
            &field,
            [2.0, -1.5, -1.5],
            0.25,
            [8, 12, 12],
            lod_b,
            &default_params(),
        )
        .expect("chunk b");
        let a_positions: BTreeSet<(u32, u32, u32)> = a.positions.iter().map(|p| position_bits(*p)).collect();
        let b_positions: BTreeSet<(u32, u32, u32)> = b.positions.iter().map(|p| position_bits(*p)).collect();
        // A chunk owns the primal edges whose start node is inside it, so the faces across
        // the seam are all owned by b (whose -x halo cells are a's border cells) and a
        // never references b's cells: there is nothing in a beyond x = 2 to compare.
        let seam_from_a: Vec<[f32; 3]> = a.positions.iter().copied().filter(|p| p[0] > 2.0).collect();
        for p in &seam_from_a {
            assert!(
                b_positions.contains(&position_bits(*p)),
                "a's border vertex {p:?} not bit-identical in b"
            );
        }
        let seam_from_b: Vec<[f32; 3]> = b.positions.iter().copied().filter(|p| p[0] < 2.0).collect();
        assert!(!seam_from_b.is_empty(), "b has halo vertices in a");
        for p in &seam_from_b {
            assert!(
                a_positions.contains(&position_bits(*p)),
                "b's border vertex {p:?} not bit-identical in a"
            );
        }
        // Exact-vertex seam test: each chunk's open boundary is exactly the other's.
        let a_boundary = boundary_position_edges(&a);
        assert!(!a_boundary.is_empty(), "a's surface is open at the seam");
        assert_eq!(a_boundary, boundary_position_edges(&b), "seam boundaries differ");
        // Welded union: the sphere closing across both chunks is closed and manifold.
        let union = union_edge_counts(&[&a, &b]);
        assert!(
            union.values().all(|count| *count == 2),
            "welded union is not closed/manifold"
        );
    }

    #[test]
    fn lod0_next_to_lod1_has_no_t_junction_cracks() {
        let field = Sphere {
            center: [4.0, 0.06, -0.04],
            radius: 0.85,
        };
        let mut lod_fine = NeighborLods::absent();
        lod_fine.pos_x = NeighborLod::Coarser;
        let mut lod_coarse = NeighborLods::absent();
        lod_coarse.neg_x = NeighborLod::Finer;
        let fine = mesh_chunk(
            &field,
            [0.0, -1.5, -1.5],
            0.25,
            [16, 12, 12],
            lod_fine,
            &default_params(),
        )
        .expect("fine chunk");
        let coarse = mesh_chunk(
            &field,
            [4.0, -1.5, -1.5],
            0.5,
            [8, 6, 6],
            lod_coarse,
            &default_params(),
        )
        .expect("coarse chunk");
        assert_valid_indices(&fine);
        assert_valid_indices(&coarse);
        // Exact-vertex test: every boundary edge of one chunk (by bit-exact endpoint
        // positions) must be a boundary edge of the other, and be shared by exactly two
        // triangles once the chunks are welded. The sphere lies inside the two chunks, so
        // all open edges are seam edges. (The delivered test only compared boundary edges
        // straddling the plane x = 4; no fine-chunk edge does that unless a vertex sits
        // exactly on the plane, which only clamping produced.)
        let fine_seam = boundary_position_edges(&fine);
        let coarse_seam = boundary_position_edges(&coarse);
        assert!(!fine_seam.is_empty(), "surface crosses the LOD seam");
        assert_eq!(
            fine_seam, coarse_seam,
            "seam boundary edges differ between LOD0 and LOD1 chunks"
        );
        let union = union_edge_counts(&[&fine, &coarse]);
        for edge in &fine_seam {
            assert_eq!(
                union.get(edge),
                Some(&2),
                "seam edge {edge:?} is not shared by exactly two welded triangles"
            );
        }
        // The whole welded sphere (both LODs) is closed and manifold: no cracks anywhere.
        assert!(
            union.values().all(|count| *count == 2),
            "welded LOD pair is not closed/manifold: {:?}",
            union.iter().find(|(_, c)| **c != 2)
        );
    }

    #[test]
    fn empty_and_fully_solid_chunks_are_empty_meshes() {
        for constant in [1.0_f32, -1.0, 1.0e30, -1.0e30] {
            let field = move |_p: [f32; 3]| constant;
            let mesh = mesh_chunk(
                &field,
                [0.0, 0.0, 0.0],
                0.5,
                [4, 4, 4],
                NeighborLods::same(),
                &default_params(),
            )
            .expect("constant field");
            assert!(mesh.is_empty(), "constant {constant}");
            assert_eq!(mesh.vertex_count(), 0);
        }
    }

    #[test]
    fn nan_and_infinite_fields_are_errors() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let field = move |_p: [f32; 3]| bad;
            let result = mesh_chunk(
                &field,
                [0.0, 0.0, 0.0],
                0.5,
                [4, 4, 4],
                NeighborLods::same(),
                &default_params(),
            );
            assert!(
                matches!(result, Err(MeshError::NonFiniteSample { .. })),
                "bad value {bad}: {result:?}"
            );
        }
    }

    #[test]
    fn nan_gradient_is_an_error() {
        struct NanGradient;
        impl SdfField for NanGradient {
            fn sample(&self, p: [f32; 3]) -> f32 {
                p[1] - 0.1
            }
            fn gradient(&self, _p: [f32; 3]) -> [f32; 3] {
                [f32::NAN, 0.0, 0.0]
            }
        }
        let result = mesh_chunk(
            &NanGradient,
            [-1.0, -1.0, -1.0],
            0.5,
            [4, 4, 4],
            NeighborLods::same(),
            &default_params(),
        );
        assert!(
            matches!(result, Err(MeshError::NonFiniteGradient { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn meshing_is_bitwise_deterministic_over_50_repeats() {
        let field = Sphere {
            center: [0.05, -0.03, 0.02],
            radius: 0.83,
        };
        let reference = mesh_chunk(
            &field,
            [-1.0, -1.0, -1.0],
            0.25,
            [8, 8, 8],
            NeighborLods::absent(),
            &default_params(),
        )
        .expect("reference");
        let reference_positions: Vec<(u32, u32, u32)> =
            reference.positions.iter().map(|p| position_bits(*p)).collect();
        let reference_normals: Vec<(u32, u32, u32)> =
            reference.normals.iter().map(|p| position_bits(*p)).collect();
        for _ in 0..50 {
            let again = mesh_chunk(
                &field,
                [-1.0, -1.0, -1.0],
                0.25,
                [8, 8, 8],
                NeighborLods::absent(),
                &default_params(),
            )
            .expect("repeat");
            let positions: Vec<(u32, u32, u32)> = again.positions.iter().map(|p| position_bits(*p)).collect();
            let normals: Vec<(u32, u32, u32)> = again.normals.iter().map(|p| position_bits(*p)).collect();
            assert_eq!(positions, reference_positions);
            assert_eq!(normals, reference_normals);
            assert_eq!(again.indices, reference.indices);
        }
    }

    fn assert_close(actual: [f32; 3], expected: [f32; 3], tolerance: f32) {
        for a in 0..3 {
            assert!(
                (actual[a] - expected[a]).abs() < tolerance,
                "axis {a}: {} vs {}",
                actual[a],
                expected[a]
            );
        }
    }

    #[test]
    fn qef_with_one_plane_returns_the_plane_point() {
        let solved = solve_qef(
            &[[1.0, 2.0, 3.0]],
            &[[0.0, 0.0, 1.0]],
            [-10.0, -10.0, -10.0],
            [10.0, 10.0, 10.0],
            1.0e-6,
        )
        .expect("one plane");
        assert_close(solved, [1.0, 2.0, 3.0], 1.0e-6);
    }

    #[test]
    fn qef_with_two_planes_lands_on_their_line() {
        // Planes x = 1 and y = 2; z is unconstrained and keeps the mass point's z (0).
        let solved = solve_qef(
            &[[1.0, 0.0, 0.0], [0.0, 2.0, 0.0]],
            &[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            [-10.0, -10.0, -10.0],
            [10.0, 10.0, 10.0],
            1.0e-6,
        )
        .expect("two planes");
        assert_close(solved, [1.0, 2.0, 0.0], 1.0e-6);
    }

    #[test]
    fn qef_with_three_planes_finds_the_corner() {
        let solved = solve_qef(
            &[[1.0, 0.0, 0.0], [0.0, 2.0, 0.0], [0.0, 0.0, 3.0]],
            &[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            [-10.0, -10.0, -10.0],
            [10.0, 10.0, 10.0],
            1.0e-6,
        )
        .expect("three planes");
        assert_close(solved, [1.0, 2.0, 3.0], 1.0e-6);
    }

    #[test]
    fn qef_with_four_consistent_planes_finds_the_same_corner() {
        // The fourth plane, x + y + z = 6, also passes through (1, 2, 3).
        let solved = solve_qef(
            &[[1.0, 0.0, 0.0], [0.0, 2.0, 0.0], [0.0, 0.0, 3.0], [2.0, 2.0, 2.0]],
            &[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0, 1.0]],
            [-10.0, -10.0, -10.0],
            [10.0, 10.0, 10.0],
            1.0e-6,
        )
        .expect("four planes");
        assert_close(solved, [1.0, 2.0, 3.0], 1.0e-5);
    }

    #[test]
    fn qef_with_near_parallel_planes_stays_finite_and_close() {
        // Two almost-parallel planes (normals 1e-3 apart) plus an orthogonal one: the
        // pseudo-inverse must not amplify the near-degeneracy into a wild point.
        let solved = solve_qef(
            &[[0.0, 0.0, 0.0], [0.0, 0.0, 0.001], [1.0, 0.0, 0.0]],
            &[[0.0, 0.0, 1.0], [0.001, 0.0, 1.0], [1.0, 0.0, 0.0]],
            [-5.0, -5.0, -5.0],
            [5.0, 5.0, 5.0],
            1.0e-6,
        )
        .expect("near-parallel planes");
        assert!(solved.iter().all(|v| v.is_finite()), "{solved:?}");
        for (a, v) in solved.iter().enumerate() {
            assert!((-5.0..=5.0).contains(v), "clamped axis {a}");
        }
        assert!((solved[0] - 1.0).abs() < 1.0e-3, "x on its plane: {solved:?}");
        assert!(solved[2].abs() < 0.01, "z near both planes: {solved:?}");
    }

    #[test]
    fn qef_rejects_empty_and_mismatched_inputs() {
        assert_eq!(solve_qef(&[], &[], [-1.0; 3], [1.0; 3], 1.0e-6), None, "empty");
        assert_eq!(
            solve_qef(&[[0.0; 3]], &[], [-1.0; 3], [1.0; 3], 1.0e-6),
            None,
            "mismatched lengths"
        );
        assert_eq!(
            solve_qef(
                &[[f32::NAN, 0.0, 0.0]],
                &[[1.0, 0.0, 0.0]],
                [-1.0; 3],
                [1.0; 3],
                1.0e-6
            ),
            None,
            "non-finite point"
        );
        assert_eq!(
            solve_qef(&[[0.0; 3]], &[[1.0, 0.0, 0.0]], [2.0; 3], [1.0; 3], 1.0e-6),
            None,
            "inverted clamp box"
        );
        assert_eq!(
            solve_qef(&[[0.0; 3]], &[[1.0, 0.0, 0.0]], [-1.0; 3], [1.0; 3], f32::NAN),
            None,
            "non-finite epsilon"
        );
    }

    #[test]
    fn qef_clamps_the_minimiser_into_the_box() {
        // Planes meet at (4, 5, 6), far outside the clamp box.
        let solved = solve_qef(
            &[[4.0, 0.0, 0.0], [0.0, 5.0, 0.0], [0.0, 0.0, 6.0]],
            &[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            1.0e-6,
        )
        .expect("clamped");
        assert_close(solved, [1.0, 1.0, 1.0], 1.0e-6);
    }

    /// `mesh_chunk` replaces an outside minimizer with the mass point. `solve_qef` clamps
    /// that same minimizer into the box and does not project. The default params then
    /// move a sphere's vertices off the unprojected solve.
    #[test]
    fn mesh_chunk_uses_the_mass_point_and_solve_qef_clamps() {
        struct OutsideMinimizer;
        impl SdfField for OutsideMinimizer {
            fn sample(&self, p: [f32; 3]) -> f32 {
                let on_grid = p.iter().all(|c| (c - c.round()).abs() < 1.0e-3);
                if on_grid && [p[0].round(), p[1].round(), p[2].round()] == [0.0, 0.0, 0.0] {
                    -1.0
                } else {
                    1.0
                }
            }

            fn gradient(&self, p: [f32; 3]) -> [f32; 3] {
                if (p[0] - 0.5).abs() < 0.05 && p[1].abs() < 0.05 && p[2].abs() < 0.05 {
                    [1.0, -2.0, 0.0]
                } else if p[0].abs() < 0.05 && (p[1] - 0.5).abs() < 0.05 && p[2].abs() < 0.05 {
                    [-2.0, 1.0, 0.0]
                } else if p[0].abs() < 0.05 && p[1].abs() < 0.05 && (p[2] - 0.5).abs() < 0.05 {
                    [0.0, 0.0, 1.0]
                } else {
                    [0.0, 1.0, 0.0]
                }
            }
        }

        let points = [[0.5, 0.0, 0.0], [0.0, 0.5, 0.0], [0.0, 0.0, 0.5]];
        let normals = [[1.0, -2.0, 0.0], [-2.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        let clamped = solve_qef(&points, &normals, [0.0; 3], [1.0; 3], 1.0e-6).expect("clamped");
        assert_close(clamped, [0.0, 0.0, 0.5], 1.0e-4);

        let params = MeshParams {
            project_to_surface: false,
            ..MeshParams::default()
        };
        let mesh = mesh_chunk(
            &OutsideMinimizer,
            [0.0; 3],
            1.0,
            [1, 1, 1],
            NeighborLods::absent(),
            &params,
        )
        .expect("cell");
        let inside: Vec<_> = mesh
            .positions
            .iter()
            .copied()
            .filter(|p| p.iter().all(|c| (0.0..1.0).contains(c)))
            .collect();
        assert_eq!(inside.len(), 1, "one vertex inside the unit cell, got {inside:?}");
        assert_close(inside[0], [1.0 / 6.0, 1.0 / 6.0, 1.0 / 6.0], 1.0e-4);

        let field = Sphere {
            center: [0.0; 3],
            radius: 0.6,
        };
        let flat = mesh_chunk(&field, [-1.0; 3], 0.5, [4, 4, 4], NeighborLods::absent(), &params)
            .expect("unprojected sphere");
        let projected = mesh_chunk(
            &field,
            [-1.0; 3],
            0.5,
            [4, 4, 4],
            NeighborLods::absent(),
            &MeshParams::default(),
        )
        .expect("projected sphere");
        assert_ne!(
            flat.positions, projected.positions,
            "default projection moves the sphere off the mass-point solve"
        );
    }

    #[test]
    fn qef_with_zero_normals_returns_the_mass_point() {
        let solved = solve_qef(
            &[[0.0, 0.0, 0.0], [2.0, 4.0, 6.0]],
            &[[0.0, 0.0, 0.0], [0.0, 0.0, 0.0]],
            [-10.0; 3],
            [10.0; 3],
            1.0e-6,
        )
        .expect("mass point fallback");
        assert_close(solved, [1.0, 2.0, 3.0], 1.0e-6);
    }

    #[test]
    fn qef_eigensolver_handles_scaled_and_rotated_planes() {
        // Three planes with unnormalized normals meeting at (-2, 0.5, 7), one of them
        // oblique (2x - y + 0.5z = -1), so A^T A is a full, non-diagonal matrix.
        let target = [-2.0_f32, 0.5, 7.0];
        let solved = solve_qef(
            &[[0.0, 0.5, 0.0], [0.0, 0.0, 7.0], [0.0, 0.0, -2.0]],
            &[[0.0, 3.0, 0.0], [0.0, 0.0, 0.25], [2.0, -1.0, 0.5]],
            [-20.0; 3],
            [20.0; 3],
            1.0e-6,
        )
        .expect("oblique planes");
        assert_close(solved, target, 1.0e-4);
    }

    #[test]
    fn invalid_cell_sizes_are_rejected() {
        let field = |p: [f32; 3]| p[1];
        for bad in [0.0_f32, -1.0, f32::NAN, f32::INFINITY] {
            let result = mesh_chunk(
                &field,
                [0.0; 3],
                bad,
                [4, 4, 4],
                NeighborLods::same(),
                &default_params(),
            );
            assert_eq!(result, Err(MeshError::InvalidCellSize), "size {bad}");
        }
    }

    #[test]
    fn invalid_grids_and_origins_are_rejected() {
        let field = |p: [f32; 3]| p[1];
        for cells in [[0, 4, 4], [4, 0, 4], [4, 4, 0]] {
            let result = mesh_chunk(
                &field,
                [0.0; 3],
                0.5,
                cells,
                NeighborLods::same(),
                &default_params(),
            );
            assert_eq!(result, Err(MeshError::InvalidCells), "cells {cells:?}");
        }
        let result = mesh_chunk(
            &field,
            [f32::NAN, 0.0, 0.0],
            0.5,
            [4, 4, 4],
            NeighborLods::same(),
            &default_params(),
        );
        assert_eq!(result, Err(MeshError::InvalidOrigin));
        let tiny_limit = MeshParams {
            cell_limit: 10,
            ..MeshParams::default()
        };
        let result = mesh_chunk(
            &field,
            [0.0; 3],
            0.5,
            [4, 4, 4],
            NeighborLods::same(),
            &tiny_limit,
        );
        assert_eq!(result, Err(MeshError::InvalidCells), "cell limit");
    }

    #[test]
    fn invalid_params_are_rejected() {
        let field = |p: [f32; 3]| p[1];
        for bad_eps in [f32::NAN, -0.1, 1.0, 2.0] {
            let params = MeshParams {
                singular_epsilon: bad_eps,
                ..MeshParams::default()
            };
            let result = mesh_chunk(&field, [0.0; 3], 0.5, [4, 4, 4], NeighborLods::same(), &params);
            assert_eq!(result, Err(MeshError::InvalidParams), "eps {bad_eps}");
        }
    }

    #[test]
    fn lod_step_on_an_odd_grid_is_rejected() {
        let field = Sphere {
            center: [0.0; 3],
            radius: 0.5,
        };
        let mut lod = NeighborLods::absent();
        lod.pos_x = NeighborLod::Coarser;
        let result = mesh_chunk(&field, [-1.0; 3], 0.25, [7, 8, 8], lod, &default_params());
        assert_eq!(result, Err(MeshError::LodAlignment));
    }

    #[test]
    fn neighbor_levels_convert_and_reject_large_jumps() {
        let lods = NeighborLods::from_neighbor_levels(1, [Some(1), Some(2), Some(0), None, Some(1), Some(2)])
            .expect("valid levels");
        assert_eq!(lods.neg_x, NeighborLod::Same);
        assert_eq!(lods.pos_x, NeighborLod::Coarser);
        assert_eq!(lods.neg_y, NeighborLod::Finer);
        assert_eq!(lods.pos_y, NeighborLod::Absent);
        let jumped = NeighborLods::from_neighbor_levels(0, [Some(2), None, None, None, None, None]);
        assert_eq!(jumped, Err(MeshError::LodJumpTooLarge));
        assert_eq!(NeighborLods::default(), NeighborLods::same());
    }

    #[test]
    fn plane_vertices_lie_on_the_plane() {
        let field = |p: [f32; 3]| p[1] - 0.31;
        let mesh = mesh_chunk(
            &field,
            [-1.0, -1.0, -1.0],
            0.25,
            [8, 8, 8],
            NeighborLods::absent(),
            &default_params(),
        )
        .expect("plane mesh");
        assert_valid_indices(&mesh);
        assert!(!mesh.is_empty());
        for p in &mesh.positions {
            assert!((p[1] - 0.31).abs() < 1.0e-4, "vertex {p:?} off the plane");
        }
        for n in &mesh.normals {
            assert!(n[1] > 0.999, "plane normal {n:?}");
        }
    }

    #[test]
    fn zero_gradient_field_falls_back_without_panicking() {
        struct Step;
        impl SdfField for Step {
            fn sample(&self, p: [f32; 3]) -> f32 {
                if p[1] < 0.13 { -1.0 } else { 1.0 }
            }
            fn gradient(&self, _p: [f32; 3]) -> [f32; 3] {
                [0.0; 3]
            }
        }
        let mesh = mesh_chunk(
            &Step,
            [-1.0, -1.0, -1.0],
            0.25,
            [8, 8, 8],
            NeighborLods::absent(),
            &default_params(),
        )
        .expect("step field");
        assert_valid_indices(&mesh);
        assert!(!mesh.is_empty());
    }

    #[test]
    fn default_gradient_matches_the_analytic_sphere_gradient() {
        let field = Sphere {
            center: [0.0; 3],
            radius: 2.0,
        };
        let p = [1.2_f32, 0.9, 0.4];
        let len = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
        let expected = [p[0] / len, p[1] / len, p[2] / len];
        let got = field.gradient(p);
        assert_close(got, expected, 1.0e-3);
    }

    #[test]
    fn trait_objects_and_closures_both_mesh() {
        let boxed: Box<dyn SdfField> = Box::new(Sphere {
            center: [0.0; 3],
            radius: 0.6,
        });
        let from_object = mesh_chunk(
            &*boxed,
            [-1.0; 3],
            0.25,
            [8, 8, 8],
            NeighborLods::absent(),
            &default_params(),
        )
        .expect("trait object");
        assert!(!from_object.is_empty());
        let closure = |p: [f32; 3]| (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt() - 0.6;
        let from_closure = mesh_chunk(
            &closure,
            [-1.0; 3],
            0.25,
            [8, 8, 8],
            NeighborLods::absent(),
            &default_params(),
        )
        .expect("closure");
        assert_eq!(from_closure.indices, from_object.indices);
        assert_eq!(from_closure.positions, from_object.positions);
    }

    #[test]
    fn terrain_map_field_matches_heightfield_distance() {
        let map = TerrainMap::from_heights(2, 2, vec![0.0; 4], 10.0, 1.0, 0.0).expect("map");
        let field = TerrainMapField { map: &map };
        assert!(field.sample([0.0, 2.0, 0.0]) > 0.0, "above ground is outside");
        assert!(field.sample([0.0, -2.0, 0.0]) < 0.0, "below ground is solid");
        let mesh = mesh_chunk(
            &field,
            [-1.0, -1.0, -1.0],
            0.5,
            [4, 4, 4],
            NeighborLods::absent(),
            &default_params(),
        )
        .expect("heightfield chunk");
        assert!(!mesh.is_empty());
        for p in &mesh.positions {
            assert!(p[1].abs() < 0.05, "vertex near y=0 surface: {p:?}");
        }
    }

    #[test]
    fn single_cell_and_tiny_chunks_do_not_panic() {
        let field = Sphere {
            center: [0.1, 0.0, 0.0],
            radius: 0.4,
        };
        for cells in [[1, 1, 1], [1, 5, 2], [3, 1, 3]] {
            let mesh = mesh_chunk(
                &field,
                [-0.5; 3],
                0.3,
                cells,
                NeighborLods::absent(),
                &default_params(),
            )
            .expect("tiny chunk");
            assert_valid_indices(&mesh);
        }
    }

    #[test]
    fn two_disjoint_spheres_have_euler_four() {
        struct TwoSpheres;
        impl SdfField for TwoSpheres {
            fn sample(&self, p: [f32; 3]) -> f32 {
                let a = Sphere {
                    center: [-1.4, 0.13, 0.0],
                    radius: 0.9,
                };
                let b = Sphere {
                    center: [1.5, -0.11, 0.07],
                    radius: 0.8,
                };
                a.sample(p).min(b.sample(p))
            }
        }
        let mesh = mesh_chunk(
            &TwoSpheres,
            [-3.0, -3.0, -3.0],
            0.25,
            [24, 24, 24],
            NeighborLods::absent(),
            &default_params(),
        )
        .expect("two spheres");
        assert_valid_indices(&mesh);
        let counts = edge_counts(&mesh);
        assert!(counts.values().all(|c| *c == 2), "manifold components");
        let euler = mesh.vertex_count() as i64 - counts.len() as i64 + mesh.triangle_count() as i64;
        assert_eq!(euler, 4, "two closed components");
    }

    #[test]
    fn unclamped_params_still_produce_a_valid_mesh() {
        let field = Sphere {
            center: [0.0; 3],
            radius: 0.7,
        };
        let params = MeshParams {
            clamp_to_cell: false,
            ..MeshParams::default()
        };
        let mesh = mesh_chunk(
            &field,
            [-1.0; 3],
            0.25,
            [8, 8, 8],
            NeighborLods::absent(),
            &params,
        )
        .expect("unclamped");
        assert_valid_indices(&mesh);
        assert!(!mesh.is_empty());
        for p in &mesh.positions {
            assert!(p.iter().all(|v| v.is_finite()), "finite vertex {p:?}");
        }
    }

    /// The audit's reference sphere (used by `trait_objects_and_closures_both_mesh`).
    fn audit_sphere() -> Sphere {
        Sphere {
            center: [0.0; 3],
            radius: 0.6,
        }
    }

    /// FNV-1a over the little-endian bytes of every `f32::to_bits` word of the reference
    /// sphere chunk's positions, captured from the libm-free Jacobi kernel. Pins the QEF
    /// solve to one exact result on every platform: a rotation through the platform
    /// `atan2`/`sin_cos` (or a sweep count that depends on a rounded residual) does not
    /// reproduce these bits, so the test fails if the deterministic kernel is replaced.
    const PINNED_SPHERE_POSITION_FNV1A: u64 = 0x7c74_45a2_2ba7_61b5;
    /// The pinned bits of the first vertex, for a concrete example.
    const PINNED_SPHERE_FIRST_VERTEX_BITS: (u32, u32, u32) = (3205803636, 3185188541, 1037705144);

    fn sphere_position_bits(mesh: &ChunkMesh) -> Vec<u32> {
        mesh.positions.iter().flatten().map(|c| c.to_bits()).collect()
    }

    fn fnv1a(words: &[u32]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for word in words {
            for byte in word.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        hash
    }

    /// Direct `f64` pin of the Jacobi kernel on an ill-conditioned matrix (eigenvalue gaps
    /// ~1e-12 against a magnitude of 1): the algebraic rotation formulation is locked to these
    /// bits, and the `atan2`/`sin_cos` formulation it replaced is checked to differ in `f64`
    /// on this very matrix — before the `f32` cast can mask it — so reverting the kernel fails
    /// here, not just in the sphere's f32 pattern (Muse review of fix-terrain1-determinism
    /// finding 2).
    #[test]
    fn jacobi_eigen_symmetric_is_pinned_in_f64() {
        // Q D Q^T with D = diag(1, 1 + 1e-12, 1 + 2e-12) and a small rotation, as literals:
        // building it at runtime would put a platform libm in the test's own inputs.
        let a: [[f64; 3]; 3] = [
            [
                f64::from_bits(0x3feeef975c5f9697),
                f64::from_bits(0x3f463790480d5280),
                f64::from_bits(0x3f6af02bbdb42f40),
            ],
            [
                f64::from_bits(0x3f463790480d5280),
                f64::from_bits(0x3feef7ec327ab6f1),
                f64::from_bits(0x3f7af02bbdad46c0),
            ],
            [
                f64::from_bits(0x3f6af02bbdb42f80),
                f64::from_bits(0x3f7af02bbdad46c0),
                f64::from_bits(0x3feff21d45d34487),
            ],
        ];
        let (values, vectors) = jacobi_eigen_symmetric(a);
        assert_eq!(
            values.map(f64::to_bits),
            [0x3feeecd06a56958a, 0x3feeecd06a56b849, 0x3ff0000000002220,],
            "the algebraic Jacobi kernel drifted from its pinned f64 bits"
        );
        // The pin is a real eigendecomposition: A v = λ v at f64 rounding level.
        for k in 0..3 {
            let mut av = [0.0f64; 3];
            for (r, av_r) in av.iter_mut().enumerate() {
                *av_r = a[r][0] * vectors[0][k] + a[r][1] * vectors[1][k] + a[r][2] * vectors[2][k];
            }
            let residual: f64 = (0..3).map(|r| (av[r] - values[k] * vectors[r][k]).abs()).sum();
            assert!(
                residual < 1e-12,
                "eigenpair {k} residual {residual:e}: the pin is not an eigendecomposition"
            );
        }
        // Discrimination: the atan2 formulation (the pre-fix kernel) must land on different
        // f64 bits for this matrix, so this test provably fails on the old code.
        let atan2_values = {
            let mut b = a;
            for _sweep in 0..12 {
                for (p, q) in [(0_usize, 1_usize), (0, 2), (1, 2)] {
                    let apq = b[p][q];
                    if apq == 0.0 {
                        continue;
                    }
                    let theta = 0.5 * f64::atan2(2.0 * apq, b[q][q] - b[p][p]);
                    let (c, s) = (theta.cos(), theta.sin());
                    let mut j = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
                    j[p][p] = c;
                    j[q][q] = c;
                    j[p][q] = s;
                    j[q][p] = -s;
                    b = mat3_mul(mat3_transpose(j), mat3_mul(b, j));
                }
            }
            [b[0][0], b[1][1], b[2][2]]
        };
        assert_ne!(
            atan2_values.map(f64::to_bits),
            values.map(f64::to_bits),
            "the matrix no longer separates the algebraic and atan2 rotations: pick a more \
             ill-conditioned one"
        );
    }

    #[test]
    fn sphere_vertices_match_a_pinned_bit_pattern() {
        let mesh = mesh_chunk(
            &audit_sphere(),
            [-1.0; 3],
            0.25,
            [8, 8, 8],
            NeighborLods::absent(),
            &default_params(),
        )
        .expect("sphere");
        assert!(!mesh.positions.is_empty());
        let bits = sphere_position_bits(&mesh);
        assert_eq!(
            fnv1a(&bits),
            PINNED_SPHERE_POSITION_FNV1A,
            "sphere vertices changed: the QEF solve is not pinned to the deterministic kernel"
        );
        let first = mesh.positions[0];
        assert_eq!(
            position_bits(first),
            PINNED_SPHERE_FIRST_VERTEX_BITS,
            "first vertex of the reference sphere changed"
        );
    }

    #[test]
    fn adjacent_chunks_weld_bit_identically_at_the_shared_border() {
        // Two chunks meeting at the plane x = 1, both with a `Same` seam neighbor: b's
        // -x halo cells are a's border cells, so both solve the same QEF for them. The
        // origins and the cell size are dyadic, so both chunks sample bit-identical Hermite
        // data; the vertices must then match exactly or the welded union cracks. The sphere
        // is centred on the seam so the surface crosses it.
        let field = Sphere {
            center: [1.0, 0.0, 0.0],
            radius: 0.6,
        };
        let mut lod_a = NeighborLods::absent();
        lod_a.pos_x = NeighborLod::Same;
        let mut lod_b = NeighborLods::absent();
        lod_b.neg_x = NeighborLod::Same;
        let a = mesh_chunk(&field, [-1.0; 3], 0.25, [8, 8, 8], lod_a, &default_params()).expect("chunk a");
        let b = mesh_chunk(
            &field,
            [1.0, -1.0, -1.0],
            0.25,
            [8, 8, 8],
            lod_b,
            &default_params(),
        )
        .expect("chunk b");
        let a_positions: std::collections::BTreeSet<(u32, u32, u32)> =
            a.positions.iter().map(|p| position_bits(*p)).collect();
        let seam_from_b: Vec<[f32; 3]> = b.positions.iter().copied().filter(|p| p[0] < 1.0).collect();
        assert!(
            !seam_from_b.is_empty(),
            "b has {} positions; expected halo vertices in a ({} positions)",
            b.positions.len(),
            a.positions.len()
        );
        for p in &seam_from_b {
            assert!(
                a_positions.contains(&position_bits(*p)),
                "b's border vertex {p:?} not bit-identical in a"
            );
        }
    }

    #[test]
    fn projection_can_be_disabled() {
        let field = Sphere {
            center: [0.0; 3],
            radius: 0.7,
        };
        let params = MeshParams {
            project_to_surface: false,
            ..MeshParams::default()
        };
        let mesh = mesh_chunk(
            &field,
            [-1.0; 3],
            0.25,
            [8, 8, 8],
            NeighborLods::absent(),
            &params,
        )
        .expect("no projection");
        assert_valid_indices(&mesh);
        assert!(!mesh.is_empty());
    }

    #[test]
    fn surface_exactly_on_grid_nodes_does_not_panic() {
        // Zero samples count as outside; a plane through grid nodes is the classic
        // degenerate case for contouring sign tables.
        let field = |p: [f32; 3]| p[1];
        let mesh = mesh_chunk(
            &field,
            [-1.0, -1.0, -1.0],
            0.5,
            [4, 4, 4],
            NeighborLods::absent(),
            &default_params(),
        )
        .expect("nodal plane");
        assert_valid_indices(&mesh);
    }

    // ---- Verification-lane adversarial tests (sprint-muse-verify-dc) ----

    /// Ellipsoid SDF (approximate distance: unit-sphere distance scaled by the smallest
    /// semi-axis), finite-difference gradient.
    struct Ellipsoid {
        center: [f32; 3],
        radii: [f32; 3],
    }

    impl SdfField for Ellipsoid {
        fn sample(&self, p: [f32; 3]) -> f32 {
            let q = [
                (p[0] - self.center[0]) / self.radii[0],
                (p[1] - self.center[1]) / self.radii[1],
                (p[2] - self.center[2]) / self.radii[2],
            ];
            let min_radius = self.radii[0].min(self.radii[1]).min(self.radii[2]);
            ((q[0] * q[0] + q[1] * q[1] + q[2] * q[2]).sqrt() - 1.0) * min_radius
        }
    }

    /// Torus around the y axis (major radius `major`, tube radius `minor`).
    struct Torus {
        major: f32,
        minor: f32,
    }

    impl SdfField for Torus {
        fn sample(&self, p: [f32; 3]) -> f32 {
            let ring = (p[0] * p[0] + p[2] * p[2]).sqrt() - self.major;
            (ring * ring + p[1] * p[1]).sqrt() - self.minor
        }
    }

    /// Number of position-keyed edges of the welded union not shared by exactly two
    /// triangles (0 for a closed, edge-manifold union).
    fn open_or_nonmanifold_edges(meshes: &[&ChunkMesh]) -> usize {
        union_edge_counts(meshes)
            .values()
            .filter(|count| **count != 2)
            .count()
    }

    #[test]
    fn verify_torus_is_closed_with_euler_zero() {
        let field = Torus {
            major: 1.0,
            minor: 0.4,
        };
        let mesh = mesh_chunk(
            &field,
            [-1.75, -1.0, -1.75],
            0.125,
            [28, 16, 28],
            NeighborLods::absent(),
            &default_params(),
        )
        .expect("torus mesh");
        assert_valid_indices(&mesh);
        let counts = edge_counts(&mesh);
        assert!(counts.values().all(|c| *c == 2), "torus not closed/manifold");
        let euler = mesh.vertex_count() as i64 - counts.len() as i64 + mesh.triangle_count() as i64;
        assert_eq!(euler, 0, "genus-one surface");
    }

    #[test]
    fn verify_coarse_chunk_with_a_finer_face_welds_to_its_same_lod_neighbor() {
        // Fine | coarse A (finer neighbor on -x) | coarse B. The sphere straddles only
        // the A/B seam, which is a same-LOD seam: it must weld exactly like one.
        let field = Sphere {
            center: [8.0, 0.06, -0.04],
            radius: 0.85,
        };
        let a = mesh_chunk(
            &field,
            [4.0, -1.5, -1.5],
            0.5,
            [8, 6, 6],
            NeighborLods {
                neg_x: NeighborLod::Finer,
                pos_x: NeighborLod::Same,
                ..NeighborLods::absent()
            },
            &default_params(),
        )
        .expect("coarse chunk a");
        let b = mesh_chunk(
            &field,
            [8.0, -1.5, -1.5],
            0.5,
            [8, 6, 6],
            NeighborLods {
                neg_x: NeighborLod::Same,
                ..NeighborLods::absent()
            },
            &default_params(),
        )
        .expect("coarse chunk b");
        assert_valid_indices(&a);
        assert_valid_indices(&b);
        assert!(!a.is_empty() && !b.is_empty());
        let a_positions: BTreeSet<(u32, u32, u32)> = a.positions.iter().map(|p| position_bits(*p)).collect();
        for p in b.positions.iter().filter(|p| p[0] < 8.0) {
            assert!(
                a_positions.contains(&position_bits(*p)),
                "b's halo vertex {p:?} differs from a's own vertex"
            );
        }
        assert_eq!(open_or_nonmanifold_edges(&[&a, &b]), 0, "A/B seam cracks");
    }

    #[test]
    fn verify_tiny_feature_at_a_coarse_face_center_on_the_lod_seam_leaves_no_hole() {
        // A small blob around the one fine lattice node the coarse cell's corners and
        // edge midpoints do not sample: the center of a coarse face on the seam.
        let field = Sphere {
            center: [2.0, -0.25, -0.25],
            radius: 0.1,
        };
        let fine = mesh_chunk(
            &field,
            [0.0, -1.0, -1.0],
            0.25,
            [8, 8, 8],
            NeighborLods {
                pos_x: NeighborLod::Coarser,
                ..NeighborLods::absent()
            },
            &default_params(),
        )
        .expect("fine chunk");
        let coarse = mesh_chunk(
            &field,
            [2.0, -1.0, -1.0],
            0.5,
            [4, 4, 4],
            NeighborLods {
                neg_x: NeighborLod::Finer,
                ..NeighborLods::absent()
            },
            &default_params(),
        )
        .expect("coarse chunk");
        assert_valid_indices(&fine);
        assert_valid_indices(&coarse);
        assert_eq!(
            open_or_nonmanifold_edges(&[&fine, &coarse]),
            0,
            "hole at the LOD seam"
        );
    }

    #[test]
    fn verify_double_crossing_on_a_coarse_edge_stays_closed() {
        // A thin disk that only contains the midpoints of coarse edges (no coarse lattice
        // node) inside a chunk that has a finer neighbor far away on -x.
        let field = Ellipsoid {
            center: [0.13, 0.25, 0.07],
            radii: [1.2, 0.15, 1.2],
        };
        let mesh = mesh_chunk(
            &field,
            [-2.0, -2.0, -2.0],
            0.5,
            [8, 8, 8],
            NeighborLods {
                neg_x: NeighborLod::Finer,
                ..NeighborLods::absent()
            },
            &default_params(),
        )
        .expect("disk chunk");
        assert_valid_indices(&mesh);
        assert_eq!(open_or_nonmanifold_edges(&[&mesh]), 0, "open sheet");
    }

    /// Meshes a 2x2x2 block of chunks of world size `4.0` starting at `origin`: chunks
    /// for which `is_fine(offset)` holds use 0.25 cells (LOD 0), the others 0.5 cells
    /// (LOD 1); each chunk describes all 26 neighbors with `from_region_levels`.
    fn mesh_lod_block(
        field: &impl SdfField,
        origin: [f32; 3],
        is_fine: impl Fn([i32; 3]) -> bool,
    ) -> Vec<ChunkMesh> {
        let level = |o: [i32; 3]| -> Option<u32> {
            if o.iter().all(|c| (0..2).contains(c)) {
                Some(if is_fine(o) { 0 } else { 1 })
            } else {
                None
            }
        };
        let mut meshes = Vec::new();
        for ox in 0..2 {
            for oy in 0..2 {
                for oz in 0..2 {
                    let o = [ox, oy, oz];
                    let own = level(o).expect("in block");
                    let mut levels = [[[None; 3]; 3]; 3];
                    for (ix, plane) in levels.iter_mut().enumerate() {
                        for (iy, row) in plane.iter_mut().enumerate() {
                            for (iz, entry) in row.iter_mut().enumerate() {
                                *entry = level([ox + ix as i32 - 1, oy + iy as i32 - 1, oz + iz as i32 - 1]);
                            }
                        }
                    }
                    let lods = NeighborLods::from_region_levels(own, levels).expect("levels");
                    let cell = if own == 0 { 0.25 } else { 0.5 };
                    let cells = if own == 0 { 16 } else { 8 };
                    let chunk_origin = [
                        origin[0] + 4.0 * ox as f32,
                        origin[1] + 4.0 * oy as f32,
                        origin[2] + 4.0 * oz as f32,
                    ];
                    let mesh = mesh_chunk(field, chunk_origin, cell, [cells; 3], lods, &default_params())
                        .expect("block chunk");
                    assert_valid_indices(&mesh);
                    meshes.push(mesh);
                }
            }
        }
        meshes
    }

    #[test]
    fn verify_two_coarser_faces_meeting_at_an_edge_are_crack_free() {
        // Fine chunks along the z column at x, y < 4; coarse elsewhere: the LOD step turns
        // a corner along the line x = y = 4, where the diagonal coarse chunks touch the
        // fine ones only along an edge.
        let field = Sphere {
            center: [4.03, 3.98, 4.05],
            radius: 0.9,
        };
        let meshes = mesh_lod_block(&field, [0.0; 3], |o| o[0] == 0 && o[1] == 0);
        let refs: Vec<&ChunkMesh> = meshes.iter().collect();
        assert_eq!(
            open_or_nonmanifold_edges(&refs),
            0,
            "cracks where two LOD seams meet"
        );
    }

    #[test]
    fn verify_lod_corner_where_three_coarser_faces_meet_is_crack_free() {
        // One fine chunk, seven coarse ones: three LOD seams meet at the point (4, 4, 4).
        let field = Sphere {
            center: [3.97, 4.04, 4.02],
            radius: 0.9,
        };
        let meshes = mesh_lod_block(&field, [0.0; 3], |o| o == [0, 0, 0]);
        let refs: Vec<&ChunkMesh> = meshes.iter().collect();
        assert_eq!(open_or_nonmanifold_edges(&refs), 0, "cracks at the LOD corner");
    }

    #[test]
    fn verify_concave_lod_corner_is_crack_free() {
        // One coarse chunk, seven fine ones: the coarse chunk has finer face, edge and
        // corner neighbors all around its +x/+y/+z corner.
        let field = Sphere {
            center: [4.02, 3.96, 4.03],
            radius: 0.9,
        };
        let meshes = mesh_lod_block(&field, [0.0; 3], |o| o != [0, 0, 0]);
        let refs: Vec<&ChunkMesh> = meshes.iter().collect();
        assert_eq!(open_or_nonmanifold_edges(&refs), 0, "cracks at the LOD corner");
    }

    #[test]
    fn verify_region_levels_and_set_region_agree() {
        let mut levels = [[[Some(1_u32); 3]; 3]; 3];
        levels[0][0][1] = Some(0);
        levels[2][1][1] = Some(2);
        levels[1][1][0] = None;
        let lods = NeighborLods::from_region_levels(1, levels).expect("levels");
        assert_eq!(lods.region([-1, -1, 0]), NeighborLod::Finer);
        assert_eq!(lods.pos_x, NeighborLod::Coarser);
        assert_eq!(lods.region([1, 0, 0]), NeighborLod::Coarser);
        assert_eq!(lods.neg_z, NeighborLod::Absent);
        assert_eq!(lods.region([0, 0, 0]), NeighborLod::Same);
        let mut manual = NeighborLods::same();
        manual.set_region([-5, -2, 0], NeighborLod::Finer);
        assert_eq!(manual.region([-1, -1, 0]), NeighborLod::Finer);
        // Unset diagonals derive from the touching faces.
        let mut faces = NeighborLods::same();
        faces.pos_x = NeighborLod::Coarser;
        assert_eq!(faces.region([1, 1, 0]), NeighborLod::Coarser);
        assert_eq!(faces.region([-1, 1, 1]), NeighborLod::Same);
        levels[1][1][2] = Some(3);
        assert_eq!(
            NeighborLods::from_region_levels(1, levels),
            Err(MeshError::LodJumpTooLarge)
        );
    }
}
