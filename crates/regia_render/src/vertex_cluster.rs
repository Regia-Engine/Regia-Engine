// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/world/decimate.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (lines 1-128: `cluster` and `cluster_once`; its plane test is in vertex_cluster_tests.rs).

//! Vertex-cluster far LOD for multi-piece meshes (ADR-8464).
//!
//! A large hero model is often split into many pieces (one per UV tile or material) with no
//! light LOD of its own. Dropping pieces at distance leaves floating plates; simplifying each
//! piece alone opens cracks along the seams, because each piece snaps its border to its own
//! grid. This module keeps every piece and clusters the vertices of all pieces on **one shared
//! grid**: the full silhouette at a few percent of the triangles, with closed seams.
//!
//! The method is uniform-grid vertex clustering (Rossignac and Borrel, "Multi-resolution 3D
//! approximations for rendering complex scenes", 1993): every vertex snaps to the cell that
//! holds it, the cell's representative is the mean of its vertices, and triangles whose corners
//! land in fewer than three cells are dropped. Two refinements:
//!
//! - **Facing split.** Vertices of one cell are also keyed by the dominant axis and sign of
//!   their normal (six buckets), so the two sides of a thin plate stay apart and keep their
//!   own normals instead of averaging to zero.
//! - **Shared representatives.** The mean of a cell is taken over every piece's vertices in it,
//!   so a seam vertex that two pieces share by position snaps to the same point in both.
//!
//! Every input vertex moves to the mean of points inside its cell, so it moves at most one cell
//! diagonal; a triangle is an affine image of its corners, so every point of a kept triangle
//! moves at most as far as its farthest corner. [`ClusterLod::max_error`] reports that shift as
//! measured, and the silhouette of the result lies within it of the source's (features thinner
//! than a cell may vanish, as with any clustering).
//!
//! Adapted from Direct Order 27's far-rig decimation (`decimate.rs`, `regia-absorb-1` =
//! d6ed37677, owner copy exception ADR A-1). Changes, each for a named issue: one grid for all
//! pieces (seam cracks), duplicate triangles after the snap are dropped (z-fighting copies that
//! also count against the target), the 64-triangle floor and the pass schedule are
//! [`VertexClusterSettings`] fields (DO27 caps become settings), and the result reports its
//! error bound so a host can pick the switch distance.

use std::collections::HashMap;
use std::collections::HashSet;

use regia_core::math::Vec3;

use crate::LabVertex;

/// Tuning for [`cluster_pieces`]. [`Default`] is the far-rig schedule the source used.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VertexClusterSettings {
    /// Triangle budget for all pieces together.
    pub target_triangles: usize,
    /// Inputs with at most `max(target_triangles, min_triangles)` triangles are returned
    /// unchanged (default 64).
    pub min_triangles: usize,
    /// Grid passes before the coarsest result is returned (default 16).
    pub max_passes: u32,
    /// Factor applied to the cells per axis after a pass over budget, in `(0, 1)` (default 0.75).
    pub shrink: f32,
    /// The grid never gets coarser than this many cells along the longest axis (default 4).
    pub min_cells: f32,
    /// Key vertices by normal facing too, so thin plates keep both sides (default true).
    pub split_by_facing: bool,
}

impl Default for VertexClusterSettings {
    fn default() -> Self {
        Self {
            target_triangles: 2_000,
            min_triangles: 64,
            max_passes: 16,
            shrink: 0.75,
            min_cells: 4.0,
            split_by_facing: true,
        }
    }
}

impl VertexClusterSettings {
    /// Default settings with a triangle budget.
    #[must_use]
    pub fn with_target(target_triangles: usize) -> Self {
        Self {
            target_triangles,
            ..Self::default()
        }
    }
}

/// One piece of the input: positions, optional normals (empty or one per position) and a
/// triangle list indexing them.
#[derive(Clone, Copy, Debug)]
pub struct ClusterPiece<'a> {
    /// Object-space positions.
    pub positions: &'a [[f32; 3]],
    /// Normals, one per position, or empty (every vertex then shares one facing bucket).
    pub normals: &'a [[f32; 3]],
    /// Triangle list. A trailing partial triangle is ignored.
    pub indices: &'a [u32],
}

/// One clustered piece. Output vertex `k` sits at `positions[k]` and copies its other
/// attributes (UV, tangent, colour, skin) from source vertex `source_vertices[k]`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClusteredPiece {
    /// Representative positions (the mean of the cell's vertices over every piece).
    pub positions: Vec<[f32; 3]>,
    /// Representative normals (the normalized mean; the first vertex's normal if that is zero).
    pub normals: Vec<[f32; 3]>,
    /// For each output vertex, the first source vertex of this piece that landed in its cell.
    pub source_vertices: Vec<u32>,
    /// For each source vertex, the output vertex it became; `u32::MAX` for a vertex no
    /// triangle uses (such vertices take no part in the clustering).
    pub remap: Vec<u32>,
    /// Triangle list over the output vertices: no degenerate or repeated triangle.
    pub indices: Vec<u32>,
}

impl ClusteredPiece {
    /// Triangles in this piece.
    #[must_use]
    pub fn triangles(&self) -> usize {
        self.indices.len() / 3
    }
}

/// The result of [`cluster_pieces`]: one [`ClusteredPiece`] per input piece, in order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClusterLod {
    /// The clustered pieces.
    pub pieces: Vec<ClusteredPiece>,
    /// Edge of a grid cell in object units; 0 when the input was returned unchanged.
    pub cell_size: f32,
    /// Largest distance any source vertex moved, which bounds how far any point of a kept
    /// triangle moved: the geometric error of this level.
    pub max_error: f32,
    /// Triangles in the input.
    pub source_triangles: usize,
    /// Grid passes run (0 when the input was returned unchanged).
    pub passes: u32,
}

impl ClusterLod {
    /// Triangles over all pieces.
    #[must_use]
    pub fn triangles(&self) -> usize {
        self.pieces.iter().map(ClusteredPiece::triangles).sum()
    }
}

/// Why [`cluster_pieces`] refused its input.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum VertexClusterError {
    /// A piece's normals are neither empty nor one per position.
    #[error("piece {piece}: {normals} normals for {positions} positions")]
    NormalCount {
        /// Piece index.
        piece: usize,
        /// Normals given.
        normals: usize,
        /// Positions given.
        positions: usize,
    },
    /// A triangle indexes past the piece's positions.
    #[error("piece {piece}: index {index} is out of range for {positions} positions")]
    IndexOutOfRange {
        /// Piece index.
        piece: usize,
        /// The bad index.
        index: u32,
        /// Positions in the piece.
        positions: usize,
    },
    /// A position is NaN or infinite.
    #[error("piece {piece}: vertex {vertex} has a non-finite position")]
    NonFinitePosition {
        /// Piece index.
        piece: usize,
        /// Vertex index.
        vertex: usize,
    },
    /// A setting is out of range (`shrink` outside `(0, 1)`, `min_cells` below 1 or not finite).
    #[error("invalid settings: {0}")]
    Settings(&'static str),
}

/// Clusters every piece on one shared grid to about `settings.target_triangles` triangles in
/// total. The grid starts near the budget and gets coarser by `settings.shrink` per pass until
/// the result fits or `settings.min_cells` / `settings.max_passes` stops it; the last pass is
/// returned. Inputs at or under `max(target_triangles, min_triangles)` triangles come back
/// unchanged with zero error.
///
/// # Errors
///
/// [`VertexClusterError`] for a normal count, index or position the grid cannot use, or bad
/// settings.
pub fn cluster_pieces(
    pieces: &[ClusterPiece<'_>],
    settings: &VertexClusterSettings,
) -> Result<ClusterLod, VertexClusterError> {
    let _span = tracing::trace_span!("vertex_cluster", pieces = pieces.len()).entered();
    let shrink_ok = settings.shrink > 0.0 && settings.shrink < 1.0;
    if !shrink_ok {
        return Err(VertexClusterError::Settings("shrink must be in (0, 1)"));
    }
    let min_cells_ok = settings.min_cells.is_finite() && settings.min_cells >= 1.0;
    if !min_cells_ok {
        return Err(VertexClusterError::Settings(
            "min_cells must be finite and at least 1",
        ));
    }
    let mut lo = Vec3::splat(f32::MAX);
    let mut hi = Vec3::splat(f32::MIN);
    let mut source_triangles = 0;
    let mut used: Vec<Vec<bool>> = Vec::with_capacity(pieces.len());
    for (p, piece) in pieces.iter().enumerate() {
        if !piece.normals.is_empty() && piece.normals.len() != piece.positions.len() {
            return Err(VertexClusterError::NormalCount {
                piece: p,
                normals: piece.normals.len(),
                positions: piece.positions.len(),
            });
        }
        if let Some(&index) = piece
            .indices
            .iter()
            .find(|&&i| i as usize >= piece.positions.len())
        {
            return Err(VertexClusterError::IndexOutOfRange {
                piece: p,
                index,
                positions: piece.positions.len(),
            });
        }
        let piece_used = used_vertices(piece);
        for (v, position) in piece.positions.iter().enumerate() {
            if !piece_used[v] {
                continue;
            }
            let position = Vec3::from_array(*position);
            if !position.is_finite() {
                return Err(VertexClusterError::NonFinitePosition { piece: p, vertex: v });
            }
            lo = lo.min(position);
            hi = hi.max(position);
        }
        source_triangles += piece.indices.len() / 3;
        used.push(piece_used);
    }
    if source_triangles <= settings.target_triangles.max(settings.min_triangles) || lo.x > hi.x {
        return Ok(ClusterLod {
            pieces: pieces.iter().map(identity_piece).collect(),
            cell_size: 0.0,
            max_error: 0.0,
            source_triangles,
            passes: 0,
        });
    }
    let extent = (hi - lo).max_element().max(1.0e-3);
    // Surface cells grow with the square of the cells per axis: start near the budget.
    let mut cells = ((settings.target_triangles as f32 / 2.0).sqrt() * 1.6).max(8.0);
    let mut out = ClusterLod::default();
    let mut passes = 0;
    while passes < settings.max_passes.max(1) {
        passes += 1;
        let cell = extent / cells;
        let pieces_out = cluster_once(pieces, &used, lo, cell, settings.split_by_facing);
        out = ClusterLod {
            max_error: measured_error(pieces, &pieces_out),
            pieces: pieces_out,
            cell_size: cell,
            source_triangles,
            passes,
        };
        if out.triangles() <= settings.target_triangles || cells <= settings.min_cells {
            break;
        }
        cells = (cells * settings.shrink).max(settings.min_cells);
    }
    Ok(out)
}

/// Which vertices a whole triangle of `piece` references. Indices are already range-checked.
fn used_vertices(piece: &ClusterPiece<'_>) -> Vec<bool> {
    let mut used = vec![false; piece.positions.len()];
    for &i in piece.indices.as_chunks::<3>().0.as_flattened() {
        if let Some(slot) = used.get_mut(i as usize) {
            *slot = true;
        }
    }
    used
}

/// A piece returned unchanged: every vertex is its own representative.
fn identity_piece(piece: &ClusterPiece<'_>) -> ClusteredPiece {
    let count = piece.positions.len() as u32;
    let (tris, _) = piece.indices.as_chunks::<3>();
    ClusteredPiece {
        positions: piece.positions.to_vec(),
        normals: if piece.normals.is_empty() {
            vec![[0.0; 3]; piece.positions.len()]
        } else {
            piece.normals.to_vec()
        },
        source_vertices: (0..count).collect(),
        remap: (0..count).collect(),
        indices: tris.as_flattened().to_vec(),
    }
}

/// The facing bucket of a normal: dominant axis and sign, `0..6`.
fn facing(n: Vec3) -> u8 {
    let a = n.abs();
    if a.x >= a.y && a.x >= a.z {
        u8::from(n.x < 0.0)
    } else if a.y >= a.z {
        2 + u8::from(n.y < 0.0)
    } else {
        4 + u8::from(n.z < 0.0)
    }
}

type CellKey = (i32, i32, i32, u8);

/// One grid pass over every piece. Representatives are shared across pieces.
fn cluster_once(
    pieces: &[ClusterPiece<'_>],
    used: &[Vec<bool>],
    lo: Vec3,
    cell: f32,
    split_by_facing: bool,
) -> Vec<ClusteredPiece> {
    // Shared cells: position sum, normal sum and count over every piece. The map is only
    // looked up, never iterated, so output order follows the input order.
    let mut shared: HashMap<CellKey, usize> = HashMap::new();
    let mut sums: Vec<(Vec3, Vec3, u32)> = Vec::new();
    let mut keys: Vec<Vec<usize>> = Vec::with_capacity(pieces.len());
    for (piece, piece_used) in pieces.iter().zip(used) {
        let mut piece_keys = Vec::with_capacity(piece.positions.len());
        for (v, position) in piece.positions.iter().enumerate() {
            if !piece_used[v] {
                piece_keys.push(usize::MAX);
                continue;
            }
            let p = Vec3::from_array(*position);
            let n = piece.normals.get(v).map_or(Vec3::ZERO, |n| Vec3::from_array(*n));
            let q = ((p - lo) / cell).floor();
            let bucket = if split_by_facing { facing(n) } else { 0 };
            let key = (q.x as i32, q.y as i32, q.z as i32, bucket);
            let id = *shared.entry(key).or_insert_with(|| {
                sums.push((Vec3::ZERO, Vec3::ZERO, 0));
                sums.len() - 1
            });
            let sum = &mut sums[id];
            sum.0 += p;
            sum.1 += n;
            sum.2 += 1;
            piece_keys.push(id);
        }
        keys.push(piece_keys);
    }
    let means: Vec<(Vec3, Vec3)> = sums
        .iter()
        .map(|(p, n, count)| (*p / (*count).max(1) as f32, n.normalize_or_zero()))
        .collect();

    pieces
        .iter()
        .zip(&keys)
        .map(|(piece, piece_keys)| {
            let mut local: HashMap<usize, u32> = HashMap::new();
            let mut out = ClusteredPiece {
                remap: Vec::with_capacity(piece.positions.len()),
                ..ClusteredPiece::default()
            };
            for (v, &id) in piece_keys.iter().enumerate() {
                if id == usize::MAX {
                    out.remap.push(u32::MAX);
                    continue;
                }
                let k = *local.entry(id).or_insert_with(|| {
                    let (mean_p, mean_n) = means[id];
                    let first_n = piece.normals.get(v).copied().unwrap_or([0.0; 3]);
                    out.positions.push(mean_p.to_array());
                    out.normals.push(if mean_n == Vec3::ZERO {
                        first_n
                    } else {
                        mean_n.to_array()
                    });
                    out.source_vertices.push(v as u32);
                    (out.positions.len() - 1) as u32
                });
                out.remap.push(k);
            }
            let (tris, _) = piece.indices.as_chunks::<3>();
            let mut seen: HashSet<[u32; 3]> = HashSet::new();
            out.indices.reserve(piece.indices.len() / 4);
            for tri in tris {
                let [a, b, c] = tri.map(|i| out.remap[i as usize]);
                if a == b || b == c || a == c {
                    continue;
                }
                // The same triangle with the same winding from two source triangles: keep one.
                let key = if a < b && a < c {
                    [a, b, c]
                } else if b < c {
                    [b, c, a]
                } else {
                    [c, a, b]
                };
                if seen.insert(key) {
                    out.indices.extend_from_slice(&[a, b, c]);
                }
            }
            out
        })
        .collect()
}

/// The largest distance a source vertex moved to its representative.
fn measured_error(pieces: &[ClusterPiece<'_>], out: &[ClusteredPiece]) -> f32 {
    let mut max = 0.0f32;
    for (piece, clustered) in pieces.iter().zip(out) {
        for (position, &k) in piece.positions.iter().zip(&clustered.remap) {
            if k == u32::MAX {
                continue;
            }
            let moved =
                Vec3::from_array(*position).distance(Vec3::from_array(clustered.positions[k as usize]));
            max = max.max(moved);
        }
    }
    max
}

/// The output vertices of one clustered piece of a [`LabVertex`] mesh: representative position
/// and normal, every other attribute copied from the piece's source vertex.
#[must_use]
pub fn clustered_lab_vertices(source: &[LabVertex], piece: &ClusteredPiece) -> Vec<LabVertex> {
    piece
        .source_vertices
        .iter()
        .zip(piece.positions.iter().zip(&piece.normals))
        .filter_map(|(&s, (&position, &normal))| {
            source.get(s as usize).map(|v| LabVertex {
                position,
                normal,
                ..*v
            })
        })
        .collect()
}

/// Clusters one [`LabVertex`] mesh to about `target` triangles (the far-rig call of the source:
/// rigid meshes, default settings). Output vertices copy every attribute but position and
/// normal from their [`ClusteredPiece::source_vertices`] vertex. Returns the input when it is
/// small, and also when it is malformed (a bad index or position), so a far level never fails a
/// draw.
#[must_use]
pub fn cluster_lab_mesh(
    vertices: &[LabVertex],
    indices: &[u32],
    target: usize,
) -> (Vec<LabVertex>, Vec<u32>) {
    let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
    let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
    let piece = ClusterPiece {
        positions: &positions,
        normals: &normals,
        indices,
    };
    match cluster_pieces(&[piece], &VertexClusterSettings::with_target(target)) {
        Ok(lod) if lod.passes > 0 => {
            let Some(out) = lod.pieces.into_iter().next() else {
                return (vertices.to_vec(), indices.to_vec());
            };
            (clustered_lab_vertices(vertices, &out), out.indices)
        }
        _ => (vertices.to_vec(), indices.to_vec()),
    }
}

/// Builds a [`regia_asset::CpuMesh`] from one clustered piece of `source`: representative
/// positions and normals, and UV, tangent, colour and skin copied from each output vertex's
/// source vertex (arrays the source leaves empty stay empty). Morph targets are dropped: a far
/// level does not morph.
#[must_use]
pub fn clustered_cpu_mesh(source: &regia_asset::CpuMesh, piece: &ClusteredPiece) -> regia_asset::CpuMesh {
    fn pick<T: Copy>(values: &[T], sources: &[u32]) -> Vec<T> {
        if values.is_empty() {
            return Vec::new();
        }
        sources
            .iter()
            .filter_map(|&s| values.get(s as usize).copied())
            .collect()
    }
    let sources = &piece.source_vertices;
    let skin = source.skin.as_ref().map(|skin| regia_asset::CpuSkin {
        joint_names: skin.joint_names.clone(),
        joints: pick(&skin.joints, sources),
        weights: pick(&skin.weights, sources),
        joints1: pick(&skin.joints1, sources),
        weights1: pick(&skin.weights1, sources),
        eight_influences: skin.eight_influences,
    });
    regia_asset::CpuMesh {
        positions: piece.positions.clone(),
        normals: piece.normals.clone(),
        uvs: pick(&source.uvs, sources),
        tangents: pick(&source.tangents, sources),
        indices: piece.indices.clone(),
        skin,
        colors: pick(&source.colors, sources),
        morph_targets: Vec::new(),
    }
}

/// Clusters several [`regia_asset::CpuMesh`] pieces of one model on a shared grid and returns one
/// far-level mesh per piece (see [`clustered_cpu_mesh`]) with the level's report.
///
/// # Errors
///
/// As [`cluster_pieces`].
pub fn cluster_cpu_meshes(
    meshes: &[&regia_asset::CpuMesh],
    settings: &VertexClusterSettings,
) -> Result<(Vec<regia_asset::CpuMesh>, ClusterLod), VertexClusterError> {
    let pieces: Vec<ClusterPiece<'_>> = meshes
        .iter()
        .map(|mesh| ClusterPiece {
            positions: &mesh.positions,
            normals: &mesh.normals,
            indices: &mesh.indices,
        })
        .collect();
    let lod = cluster_pieces(&pieces, settings)?;
    let out = meshes
        .iter()
        .zip(&lod.pieces)
        .map(|(mesh, piece)| clustered_cpu_mesh(mesh, piece))
        .collect();
    Ok((out, lod))
}

#[cfg(test)]
#[path = "vertex_cluster_tests.rs"]
mod tests;
