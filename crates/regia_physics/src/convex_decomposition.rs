// SPDX-License-Identifier: MPL-2.0
// Source: K. Mamou, F. Ghorbel, "A simple and efficient approach for 3D mesh approximate convex
// decomposition", IEEE ICIP 2009; the public V-HACD project description (K. Mamou, BSD-3:
// voxelize, cut by the axis-aligned plane that minimizes concavity, merge hulls to a limit; ideas
// only, no code read); C. B. Barber, D. P. Dobkin, H. Huhdanpaa, "The Quickhull Algorithm for
// Convex Hulls", ACM TOMS 22(4), 1996; I. E. Sutherland, G. W. Hodgman, "Reentrant polygon
// clipping", CACM 17(1), 1974.

//! Approximate convex decomposition: [`decompose`] splits a triangle mesh into at most
//! [`DecompositionParams::max_hulls`] convex hulls, for dynamic bodies that need a concave shape
//! (a dynamic body cannot use a `TriMesh` collider). [`compound_shape`] turns the hulls into a
//! [`ColliderShape::Compound`].
//!
//! # How it works
//!
//! 1. **Voxelize.** A grid with [`DecompositionParams::resolution`] cells along the mesh's longest
//!    axis. Cells a triangle touches are surface cells; a flood fill from the grid's padding marks
//!    the outside; the remaining cells are interior. Winding does not matter, and a mesh with holes
//!    smaller than a cell still has an inside.
//! 2. **Cut.** The part with the largest concavity (its hull's volume minus its solid volume) is cut
//!    by the axis-aligned plane, on a cell boundary, that minimizes the summed concavity of the two
//!    sides, plus two small biases: toward cuts that split the part's cells evenly, and toward cuts
//!    across its longer extents (so a ring is cut into arcs, not trimmed or sliced flat). Planes are
//!    searched coarse to fine (16 positions per axis, then every position around the best). Cutting stops when every part's concavity is at most
//!    [`DecompositionParams::max_concavity`] of the mesh's volume, when a part reaches
//!    [`DecompositionParams::max_depth`] cuts, or at four times `max_hulls` parts (at most 64).
//! 3. **Hull.** Each part's hull is built from the mesh polygons clipped to the part's box (plus
//!    the box corners inside the solid), so hulls follow the mesh, not the voxel staircase.
//! 4. **Merge.** The pair whose merged hull adds the least volume is merged until at most
//!    `max_hulls` remain, and further while a merge adds at most `max_concavity` of the volume.
//! 5. Each hull is capped at [`DecompositionParams::max_hull_vertices`] vertices (farthest points
//!    first, so a capped hull lies inside the uncapped one).
//!
//! The result is deterministic: the same mesh and parameters give the same hulls, in order of
//! decreasing volume. The work is single-threaded and allocates; run it at import or load time,
//! not per frame.

mod quickhull;
mod voxels;

use glam::DVec3;
use regia_core::math::Vec3;
use regia_core::mesh_query::{TriMesh, triangle};
use regia_scene::Transform;

use crate::components::ColliderShape;
use quickhull::{Hull, quickhull};
use voxels::Grid;

/// Smallest [`DecompositionParams::resolution`].
pub const MIN_RESOLUTION: u32 = 8;
/// Largest [`DecompositionParams::resolution`] (the grid then has up to 259³ cells).
pub const MAX_RESOLUTION: u32 = 256;
/// Largest [`DecompositionParams::max_hulls`].
pub const MAX_HULLS: u32 = 64;
/// Largest [`DecompositionParams::max_depth`].
pub const MAX_DEPTH: u32 = 16;
/// Largest nonzero [`DecompositionParams::max_hull_vertices`].
pub const MAX_HULL_VERTICES: u32 = 4096;

/// Most parts the cutting stage makes before merging.
const MAX_PARTS: usize = 64;

/// Settings of [`decompose`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecompositionParams {
    /// Most hulls to return, 1 to [`MAX_HULLS`]. With 1 the result is the hull of every vertex.
    pub max_hulls: u32,
    /// Voxel cells along the mesh's longest axis, [`MIN_RESOLUTION`] to [`MAX_RESOLUTION`]. Higher
    /// finds smaller concave features and costs about the cube of the value in memory and time.
    pub resolution: u32,
    /// Concavity a part may keep without being cut, and that a merge may add, as a fraction of the
    /// mesh's volume (0 to 1). Lower makes more, tighter hulls.
    pub max_concavity: f32,
    /// Most cuts on the way to any one part, 0 to [`MAX_DEPTH`].
    pub max_depth: u32,
    /// Most vertices per hull: 0 for no cap, otherwise 4 to [`MAX_HULL_VERTICES`].
    pub max_hull_vertices: u32,
}

impl Default for DecompositionParams {
    fn default() -> Self {
        Self {
            max_hulls: 16,
            resolution: 64,
            max_concavity: 0.01,
            max_depth: 10,
            max_hull_vertices: 64,
        }
    }
}

impl DecompositionParams {
    /// Checks every field's range.
    pub fn validate(&self) -> Result<(), DecompositionError> {
        let bad = |name, reason| Err(DecompositionError::InvalidParameter { name, reason });
        if !(1..=MAX_HULLS).contains(&self.max_hulls) {
            return bad("max_hulls", "must be 1 to 64");
        }
        if !(MIN_RESOLUTION..=MAX_RESOLUTION).contains(&self.resolution) {
            return bad("resolution", "must be 8 to 256");
        }
        if !(self.max_concavity.is_finite() && (0.0..=1.0).contains(&self.max_concavity)) {
            return bad("max_concavity", "must be a finite number from 0 to 1");
        }
        if self.max_depth > MAX_DEPTH {
            return bad("max_depth", "must be 0 to 16");
        }
        if self.max_hull_vertices != 0 && !(4..=MAX_HULL_VERTICES).contains(&self.max_hull_vertices) {
            return bad("max_hull_vertices", "must be 0 (no cap) or 4 to 4096");
        }
        Ok(())
    }
}

/// Why [`decompose`] returned no hulls.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DecompositionError {
    /// The mesh has no triangles.
    #[error("the mesh has no triangles")]
    EmptyMesh,
    /// Every triangle has zero area (coincident or collinear corners).
    #[error("every triangle of the mesh has zero area")]
    ZeroArea,
    /// The mesh's vertices are coplanar (within about one `f32` rounding step of their extent), so
    /// it encloses no volume.
    #[error("the mesh is flat: its vertices are coplanar")]
    Flat,
    /// A [`DecompositionParams`] field is out of range.
    #[error("parameter `{name}` {reason}")]
    InvalidParameter {
        /// The field's name.
        name: &'static str,
        /// The allowed range.
        reason: &'static str,
    },
}

impl DecompositionError {
    /// A stable lowercase code for tools and the agent protocol: `empty_mesh`, `zero_area`,
    /// `flat_mesh`, or `bad_` plus the parameter name (`bad_max_hulls`, `bad_resolution`,
    /// `bad_max_concavity`, `bad_max_depth`, `bad_max_hull_vertices`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyMesh => "empty_mesh",
            Self::ZeroArea => "zero_area",
            Self::Flat => "flat_mesh",
            Self::InvalidParameter { name, .. } => match *name {
                "max_hulls" => "bad_max_hulls",
                "resolution" => "bad_resolution",
                "max_concavity" => "bad_max_concavity",
                "max_depth" => "bad_max_depth",
                _ => "bad_max_hull_vertices",
            },
        }
    }
}

/// One convex piece of a decomposition: vertices and outward counter-clockwise triangles.
#[derive(Debug, Clone, PartialEq)]
pub struct ConvexHull {
    /// The hull's vertices (each used by a triangle).
    pub vertices: Vec<Vec3>,
    /// Index triples into `vertices`, counter-clockwise seen from outside.
    pub triangles: Vec<[u32; 3]>,
}

impl ConvexHull {
    /// The hull in `f32`. Its triangles are rebuilt from the rounded vertices so they stay convex
    /// and closed after rounding.
    fn from_hull(hull: &Hull) -> Self {
        let rounded: Vec<DVec3> = hull.vertices.iter().map(|p| p.as_vec3().as_dvec3()).collect();
        let rebuilt = quickhull(&rounded, 0);
        let hull = rebuilt.as_ref().unwrap_or(hull);
        Self {
            vertices: hull.vertices.iter().map(|p| p.as_vec3()).collect(),
            triangles: hull.triangles.clone(),
        }
    }

    /// Whether every triangle index names a vertex (the fields are public, so a hand-built hull
    /// may not).
    fn indices_valid(&self) -> bool {
        self.triangles
            .iter()
            .all(|t| t.iter().all(|&i| (i as usize) < self.vertices.len()))
    }

    fn corners(&self, [a, b, c]: [u32; 3]) -> [DVec3; 3] {
        [a, b, c].map(|i| self.vertices[i as usize].as_dvec3())
    }

    /// The enclosed volume.
    pub fn volume(&self) -> f32 {
        if !self.indices_valid() {
            return 0.0;
        }
        let Some(origin) = self.vertices.first().map(|p| p.as_dvec3()) else {
            return 0.0;
        };
        let six: f64 = self
            .triangles
            .iter()
            .map(|&tri| {
                let [a, b, c] = self.corners(tri).map(|p| p - origin);
                a.dot(b.cross(c))
            })
            .sum();
        (six / 6.0) as f32
    }

    /// The center of mass of the solid hull (uniform density); the vertex average when the hull
    /// encloses no volume.
    pub fn centroid(&self) -> Vec3 {
        let Some(origin) = self.vertices.first().map(|p| p.as_dvec3()) else {
            return Vec3::ZERO;
        };
        let mut weighted = DVec3::ZERO;
        let mut six = 0.0;
        let valid = self.indices_valid();
        for &tri in self.triangles.iter().filter(|_| valid) {
            let [a, b, c] = self.corners(tri).map(|p| p - origin);
            let v = a.dot(b.cross(c));
            weighted += (a + b + c) * (v / 4.0);
            six += v;
        }
        if six.abs() > f64::MIN_POSITIVE {
            (origin + weighted / six).as_vec3()
        } else {
            let sum: Vec3 = self.vertices.iter().copied().sum();
            sum / self.vertices.len() as f32
        }
    }

    /// `true` when `point` is inside the hull or within `tolerance` of it (behind every face plane
    /// by at least `-tolerance`).
    pub fn contains(&self, point: Vec3, tolerance: f32) -> bool {
        if !self.indices_valid() {
            return false;
        }
        let p = point.as_dvec3();
        self.triangles.iter().all(|&tri| {
            let [a, b, c] = self.corners(tri);
            let n = (b - a).cross(c - a);
            let len = n.length();
            len <= f64::MIN_POSITIVE || n.dot(p - a) / len <= f64::from(tolerance)
        })
    }

    /// The hull as a collider shape (`ColliderShape::ConvexHull` over its vertices).
    pub fn to_collider_shape(&self) -> ColliderShape {
        ColliderShape::ConvexHull {
            points: self.vertices.clone(),
        }
    }
}

/// The hulls as one collider: `None` for no hulls, the hull itself for one, otherwise a
/// [`ColliderShape::Compound`] of convex hulls at identity offsets (the hulls are already in the
/// mesh's space).
pub fn compound_shape(hulls: &[ConvexHull]) -> Option<ColliderShape> {
    match hulls {
        [] => None,
        [one] => Some(one.to_collider_shape()),
        many => Some(ColliderShape::Compound {
            shapes: many
                .iter()
                .map(|hull| (Transform::IDENTITY, hull.to_collider_shape()))
                .collect(),
        }),
    }
}

/// The convex hull of a point cloud with the same quickhull [`decompose`] uses for its parts (the
/// importer's `collision_convex_*` proxies, ADR-2341). `max_vertices` caps the vertex count
/// (0 = no cap, otherwise at least 4; farthest points first, so a capped hull lies inside the
/// uncapped one). `None` when the points are fewer than four, not finite, or flat (coplanar,
/// collinear or coincident). Deterministic: the same points give the same hull.
pub fn convex_hull_of_points(points: &[Vec3], max_vertices: u32) -> Option<ConvexHull> {
    let points: Vec<DVec3> = points.iter().map(|p| p.as_dvec3()).collect();
    let cap = if max_vertices == 0 {
        0
    } else {
        max_vertices.max(4) as usize
    };
    quickhull(&points, cap).map(|hull| ConvexHull::from_hull(&hull))
}

/// Splits `mesh` into at most `params.max_hulls` convex hulls (see the module docs for the steps).
///
/// The hulls cover the mesh's solid approximately: each follows the mesh where it has surface and
/// the cut planes elsewhere, and they may overlap a little where merges joined parts. Errors: an
/// out-of-range parameter, a mesh with no triangles, only zero-area triangles, or coplanar
/// vertices.
pub fn decompose(
    mesh: &TriMesh,
    params: &DecompositionParams,
) -> Result<Vec<ConvexHull>, DecompositionError> {
    let _span = tracing::info_span!("convex_decompose", triangles = mesh.triangle_count()).entered();
    params.validate()?;
    if mesh.is_empty() {
        return Err(DecompositionError::EmptyMesh);
    }
    let positions = mesh.positions();
    let corner = |i: u32| positions[i as usize];
    if mesh
        .triangles()
        .iter()
        .all(|&[a, b, c]| triangle::is_degenerate(corner(a), corner(b), corner(c)))
    {
        return Err(DecompositionError::ZeroArea);
    }
    let mut used = vec![false; positions.len()];
    for &i in mesh.triangles().iter().flatten() {
        used[i as usize] = true;
    }
    let points: Vec<DVec3> = positions
        .iter()
        .zip(&used)
        .filter(|&(_, &u)| u)
        .map(|(p, _)| p.as_dvec3())
        .collect();
    let whole = quickhull(&points, 0).ok_or(DecompositionError::Flat)?;
    let cap = params.max_hull_vertices as usize;
    let capped_whole = || {
        quickhull(&whole.vertices, cap)
            .map_or_else(|| ConvexHull::from_hull(&whole), |h| ConvexHull::from_hull(&h))
    };
    if params.max_hulls == 1 {
        return Ok(vec![capped_whole()]);
    }

    let bounds = mesh.bounds();
    let grid = Grid::build(
        mesh,
        bounds.min.as_dvec3(),
        bounds.max.as_dvec3(),
        params.resolution,
    );
    let root = grid.root_part(mesh);
    let threshold = f64::from(params.max_concavity) * root.solid_volume;
    let max_hulls = params.max_hulls as usize;
    let max_parts = (4 * max_hulls).min(MAX_PARTS).max(max_hulls);

    // Cut the most concave part until every part is convex enough or a limit is reached.
    let mut parts = vec![root];
    while parts.len() < max_parts {
        let mut pick: Option<(usize, f64)> = None;
        for (i, part) in parts.iter().enumerate() {
            let concavity = part.concavity();
            if part.splittable
                && part.depth < params.max_depth
                && concavity > threshold
                && pick.is_none_or(|(_, best)| concavity > best)
            {
                pick = Some((i, concavity));
            }
        }
        let Some((i, _)) = pick else { break };
        let split = parts[i]
            .best_cut()
            .and_then(|(axis, c)| parts[i].split(&grid, axis, c));
        match split {
            Some((first, second)) => {
                parts[i] = first;
                parts.push(second);
            }
            None => parts[i].splittable = false,
        }
    }
    let cut_parts = parts.len();

    // Merge the cheapest pairs down to `max_hulls`, then while a merge stays under the threshold.
    let mut clusters: Vec<Option<Cluster>> = parts
        .into_iter()
        .map(|part| {
            Some(Cluster {
                points: part.hull_points,
                volume: part.hull_volume,
            })
        })
        .collect();
    let n = clusters.len();
    let mut cost = vec![f64::INFINITY; n * n];
    for (i, a) in clusters.iter().enumerate() {
        for (j, b) in clusters.iter().enumerate().skip(i + 1) {
            if let (Some(a), Some(b)) = (a, b) {
                cost[i * n + j] = merge_cost(a, b);
            }
        }
    }
    let mut live = n;
    while live > 1 {
        let mut best: Option<(usize, usize, f64)> = None;
        for (i, row) in cost.chunks(n.max(1)).enumerate() {
            if clusters[i].is_none() {
                continue;
            }
            for (j, &c) in row.iter().enumerate().skip(i + 1) {
                if clusters[j].is_some() && best.is_none_or(|(.., b)| c < b) {
                    best = Some((i, j, c));
                }
            }
        }
        let Some((i, j, c)) = best else { break };
        if live <= max_hulls && c > threshold {
            break;
        }
        let (Some(a), Some(b)) = (clusters[i].take(), clusters[j].take()) else {
            break;
        };
        clusters[i] = Some(merge(a, b));
        live -= 1;
        let merged = clusters[i].as_ref();
        let updated: Vec<(usize, f64)> = clusters
            .iter()
            .enumerate()
            .filter(|&(k, _)| k != i)
            .map(|(k, other)| match (merged, other) {
                (Some(a), Some(b)) => (k, merge_cost(a, b)),
                _ => (k, f64::INFINITY),
            })
            .collect();
        for (k, c) in updated {
            cost[k.min(i) * n + k.max(i)] = c;
        }
    }

    let mut hulls: Vec<(f32, ConvexHull)> = clusters
        .into_iter()
        .flatten()
        .filter_map(|cluster| quickhull(&cluster.points, cap))
        .map(|hull| {
            let hull = ConvexHull::from_hull(&hull);
            (hull.volume(), hull)
        })
        .collect();
    if hulls.is_empty() {
        let hull = capped_whole();
        hulls.push((hull.volume(), hull));
    }
    hulls.sort_by(|a, b| b.0.total_cmp(&a.0));
    tracing::debug!(cut_parts, hulls = hulls.len(), "convex decomposition");
    Ok(hulls.into_iter().map(|(_, hull)| hull).collect())
}

/// A merge candidate: hull vertices and hull volume.
struct Cluster {
    points: Vec<DVec3>,
    volume: f64,
}

/// The hull volume a merge adds beyond the two hulls.
fn merge_cost(a: &Cluster, b: &Cluster) -> f64 {
    let points: Vec<DVec3> = a.points.iter().chain(&b.points).copied().collect();
    quickhull(&points, 0).map_or(f64::INFINITY, |h| h.volume() - a.volume - b.volume)
}

fn merge(a: Cluster, b: Cluster) -> Cluster {
    let mut points = a.points;
    points.extend(b.points);
    match quickhull(&points, 0) {
        Some(hull) => Cluster {
            volume: hull.volume(),
            points: hull.vertices,
        },
        None => Cluster { points, volume: 0.0 },
    }
}

#[cfg(test)]
mod tests;
