// SPDX-License-Identifier: MPL-2.0
// Source: the cube-into-five-tetrahedra split with alternating parity so neighboring cubes'
// face diagonals agree (standard marching-tetrahedra decomposition, e.g. G. Treece, R. Prager,
// A. Gee, "Regularised marching tetrahedra", Computers & Graphics 23(4), 1999). Inside test:
// ray parity (Jordan curve theorem) along +X. Original code (spec 55 § 2.8).

//! The voxel tet generator for `Tetra` soft bodies (spec 55 § 2.8), a desktop authoring tool:
//! voxelize a closed surface at a chosen cell size, keep the cells whose centers are inside,
//! split each cube into 5 tetrahedra with alternating parity (so faces conform), and bind each
//! surface vertex to the tet that contains it (or the nearest one, by the largest smallest
//! barycentric weight). Open meshes are refused (`soft_not_closed`).

use std::collections::BTreeMap;

use regia_core::math::Vec3;
use regia_physics::cloth::ClothError;
use regia_physics::cloth::asset::{TetBinding, TetMeshInput};

/// Most voxel cells the generator considers (a 96³ grid); a finer cell is refused.
pub const MAX_TET_CELLS: usize = 96 * 96 * 96;

/// What [`generate_tets`] made.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TetGenReport {
    /// Tetrahedra.
    pub tets: usize,
    /// Tet mesh vertices.
    pub vertices: usize,
    /// Smallest tet volume over the largest (0.5 for this split: corner tets are a sixth of a
    /// cube, the center one a third).
    pub min_volume_ratio: f32,
    /// Surface vertices that lie outside every tet (bound to the nearest one).
    pub outside_vertices: usize,
}

fn tet_volume(p: [Vec3; 4]) -> f32 {
    (p[1] - p[0]).dot((p[2] - p[0]).cross(p[3] - p[0])) / 6.0
}

/// Barycentric weights of `x` in tet `p` (sum 1; all non-negative when inside).
fn barycentric(p: [Vec3; 4], x: Vec3) -> [f32; 4] {
    let v = tet_volume(p);
    if v.abs() <= f32::EPSILON {
        return [0.25; 4];
    }
    let w1 = tet_volume([p[0], x, p[2], p[3]]) / v;
    let w2 = tet_volume([p[0], p[1], x, p[3]]) / v;
    let w3 = tet_volume([p[0], p[1], p[2], x]) / v;
    [1.0 - w1 - w2 - w3, w1, w2, w3]
}

/// Whether `p` is inside the closed surface (ray parity along +X, the ray's start nudged off
/// grid-aligned features by a fixed fraction of `nudge`).
fn inside(positions: &[Vec3], triangles: &[[u32; 3]], p: Vec3, nudge: f32) -> bool {
    let (y, z) = (p.y + nudge * 0.000_123_7, p.z + nudge * 0.000_317_1);
    let mut crossings = 0u32;
    for t in triangles {
        let [a, b, c] = t.map(|i| positions[i as usize]);
        // Barycentric of (y, z) in the triangle's YZ projection.
        let d = (b.y - a.y) * (c.z - a.z) - (c.y - a.y) * (b.z - a.z);
        if d.abs() <= f32::EPSILON {
            continue;
        }
        let u = ((y - a.y) * (c.z - a.z) - (c.y - a.y) * (z - a.z)) / d;
        let v = ((b.y - a.y) * (z - a.z) - (y - a.y) * (b.z - a.z)) / d;
        if u < 0.0 || v < 0.0 || u + v > 1.0 {
            continue;
        }
        let x = a.x + (b.x - a.x) * u + (c.x - a.x) * v;
        if x > p.x {
            crossings += 1;
        }
    }
    !crossings.is_multiple_of(2)
}

/// Corner `k` (`dx + 2 dy + 4 dz`) of the cube at `(i, j, l)`.
fn corner(i: usize, j: usize, l: usize, k: usize) -> (usize, usize, usize) {
    (i + (k & 1), j + ((k >> 1) & 1), l + ((k >> 2) & 1))
}

/// The five tets of a cube by corner index, for even and odd parity.
const SPLIT: [[[usize; 4]; 5]; 2] = [
    [
        [0, 3, 5, 6],
        [1, 0, 3, 5],
        [2, 0, 6, 3],
        [4, 0, 5, 6],
        [7, 3, 6, 5],
    ],
    [
        [1, 2, 4, 7],
        [0, 1, 2, 4],
        [3, 1, 7, 2],
        [5, 1, 4, 7],
        [6, 2, 7, 4],
    ],
];

/// Voxelizes a closed surface at `cell` meters into a tet mesh with a binding per surface
/// vertex (see the module docs).
///
/// # Errors
/// [`ClothError::NotClosed`] when an edge is not shared by exactly two triangles (or there
/// are none), [`ClothError::BadParam`] (`"cell"`) for a cell that is not positive, too fine
/// (over [`MAX_TET_CELLS`]) or too coarse (no cell center inside), [`ClothError::NonFinite`]
/// for a bad position.
pub fn generate_tets(
    positions: &[Vec3],
    triangles: &[[u32; 3]],
    cell: f32,
) -> Result<(TetMeshInput, TetGenReport), ClothError> {
    if !(cell.is_finite() && cell > 0.0) {
        return Err(ClothError::BadParam { what: "cell" });
    }
    if positions.iter().any(|p| !p.is_finite()) {
        return Err(ClothError::NonFinite { what: "position" });
    }
    let mut uses: BTreeMap<(u32, u32), u32> = BTreeMap::new();
    for t in triangles {
        if t.iter().any(|&i| i as usize >= positions.len()) {
            return Err(ClothError::ParticleOutOfRange {
                index: t.iter().map(|&i| i as usize).max().unwrap_or(0),
                len: positions.len(),
            });
        }
        for k in 0..3 {
            let (a, b) = (t[k], t[(k + 1) % 3]);
            *uses.entry((a.min(b), a.max(b))).or_default() += 1;
        }
    }
    if triangles.is_empty() || uses.values().any(|&u| u != 2) {
        return Err(ClothError::NotClosed);
    }
    let (lo, hi) = positions
        .iter()
        .fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(lo, hi), p| {
            (lo.min(*p), hi.max(*p))
        });
    let dims = ((hi - lo) / cell).ceil().max(Vec3::ONE);
    let (nx, ny, nz) = (dims.x as usize, dims.y as usize, dims.z as usize);
    if nx.saturating_mul(ny).saturating_mul(nz) > MAX_TET_CELLS {
        return Err(ClothError::BadParam { what: "cell" });
    }
    // Center the grid on the surface's bounds.
    let origin = (lo + hi) * 0.5 - Vec3::new(nx as f32, ny as f32, nz as f32) * cell * 0.5;
    let mut index_of: BTreeMap<(usize, usize, usize), u32> = BTreeMap::new();
    let mut vertices: Vec<Vec3> = Vec::new();
    let mut tets: Vec<[u32; 4]> = Vec::new();
    for l in 0..nz {
        for j in 0..ny {
            for i in 0..nx {
                let center = origin + Vec3::new(i as f32 + 0.5, j as f32 + 0.5, l as f32 + 0.5) * cell;
                if !inside(positions, triangles, center, cell) {
                    continue;
                }
                let mut ids = [0u32; 8];
                for (k, id) in ids.iter_mut().enumerate() {
                    let key = corner(i, j, l, k);
                    *id = *index_of.entry(key).or_insert_with(|| {
                        vertices.push(origin + Vec3::new(key.0 as f32, key.1 as f32, key.2 as f32) * cell);
                        (vertices.len() - 1) as u32
                    });
                }
                for split in &SPLIT[(i + j + l) % 2] {
                    let mut tet = split.map(|k| ids[k]);
                    if tet_volume(tet.map(|v| vertices[v as usize])) < 0.0 {
                        tet.swap(2, 3);
                    }
                    tets.push(tet);
                }
            }
        }
    }
    if tets.is_empty() {
        return Err(ClothError::BadParam { what: "cell" });
    }
    let volumes: Vec<f32> = tets
        .iter()
        .map(|t| tet_volume(t.map(|v| vertices[v as usize])))
        .collect();
    let max = volumes.iter().copied().fold(0.0f32, f32::max);
    let min = volumes.iter().copied().fold(f32::MAX, f32::min);
    let mut outside = 0;
    let mut surface_binding = Vec::with_capacity(positions.len());
    for &p in positions {
        let mut best = (f32::MIN, 0usize, [0.25f32; 4]);
        for (ti, t) in tets.iter().enumerate() {
            let w = barycentric(t.map(|v| vertices[v as usize]), p);
            let smallest = w.iter().copied().fold(f32::MAX, f32::min);
            if smallest > best.0 {
                best = (smallest, ti, w);
            }
        }
        if best.0 < -1e-5 {
            outside += 1;
        }
        surface_binding.push(TetBinding {
            tet: best.1 as u32,
            weights: best.2,
        });
    }
    let report = TetGenReport {
        tets: tets.len(),
        vertices: vertices.len(),
        min_volume_ratio: if max > 0.0 { min / max } else { 0.0 },
        outside_vertices: outside,
    };
    Ok((
        TetMeshInput {
            positions: vertices,
            tets,
            surface_binding,
        },
        report,
    ))
}

#[cfg(test)]
pub(crate) mod tests_support {
    use regia_core::math::Vec3;

    /// A closed, outward-wound axis-aligned box from `lo` to `hi` (8 vertices, 12 triangles).
    pub(crate) fn box_surface(lo: Vec3, hi: Vec3) -> (Vec<Vec3>, Vec<[u32; 3]>) {
        let positions = (0..8)
            .map(|k| {
                Vec3::new(
                    if k & 1 == 0 { lo.x } else { hi.x },
                    if k & 2 == 0 { lo.y } else { hi.y },
                    if k & 4 == 0 { lo.z } else { hi.z },
                )
            })
            .collect();
        let triangles = vec![
            [0, 2, 1],
            [1, 2, 3],
            [4, 5, 6],
            [5, 7, 6],
            [0, 1, 4],
            [1, 5, 4],
            [2, 6, 3],
            [3, 6, 7],
            [0, 4, 2],
            [2, 4, 6],
            [1, 3, 5],
            [3, 7, 5],
        ];
        (positions, triangles)
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::box_surface;
    use super::*;

    #[test]
    fn a_box_fills_with_conforming_positive_tets_and_binds_its_corners() {
        let (positions, triangles) = box_surface(Vec3::ZERO, Vec3::new(0.4, 0.4, 0.4));
        let (mesh, report) = generate_tets(&positions, &triangles, 0.1).expect("generates");
        assert_eq!(report.tets, 4 * 4 * 4 * 5);
        assert_eq!(report.vertices, 5 * 5 * 5);
        assert!((report.min_volume_ratio - 0.5).abs() < 1e-3);
        let total: f32 = mesh
            .tets
            .iter()
            .map(|t| tet_volume(t.map(|v| mesh.positions[v as usize])))
            .sum();
        assert!((total - 0.064).abs() < 1e-5, "volume {total}");
        assert!(
            mesh.tets
                .iter()
                .all(|t| tet_volume(t.map(|v| mesh.positions[v as usize])) > 0.0)
        );
        // Faces conform: every interior triangle face is shared by exactly two tets.
        let mut faces: BTreeMap<[u32; 3], u32> = BTreeMap::new();
        for t in &mesh.tets {
            for skip in 0..4 {
                let mut f: Vec<u32> = (0..4).filter(|&k| k != skip).map(|k| t[k]).collect();
                f.sort_unstable();
                *faces.entry([f[0], f[1], f[2]]).or_default() += 1;
            }
        }
        assert!(faces.values().all(|&n| n <= 2));
        assert_eq!(report.outside_vertices, 0);
        for (b, &p) in mesh.surface_binding.iter().zip(&positions) {
            let t = mesh.tets[b.tet as usize];
            let back: Vec3 = (0..4).map(|k| mesh.positions[t[k] as usize] * b.weights[k]).sum();
            assert!(back.distance(p) < 1e-5);
        }
        let again = generate_tets(&positions, &triangles, 0.1).expect("generates");
        assert_eq!(again.0, mesh, "deterministic");
    }

    #[test]
    fn open_meshes_and_bad_cells_are_refused() {
        let (positions, mut triangles) = box_surface(Vec3::ZERO, Vec3::ONE);
        assert_eq!(
            generate_tets(&positions, &triangles, 0.0)
                .expect_err("cell")
                .code(),
            "cloth_bad_param"
        );
        assert_eq!(
            generate_tets(&positions, &triangles, 1e-4)
                .expect_err("fine")
                .code(),
            "cloth_bad_param"
        );
        triangles.pop();
        assert_eq!(
            generate_tets(&positions, &triangles, 0.1)
                .expect_err("open")
                .code(),
            "soft_not_closed"
        );
    }
}
