// SPDX-License-Identifier: MPL-2.0
//! Volume-sourced mesh layers (spec 26 § 2a.9 `Volume { sdf, iso, cell_m }`, ADR-10071, cites
//! ADR-7974).
//!
//! A `Volume` layer is a signed-distance field (negative inside the solid rock, the
//! [`crate::volume_mesh`] convention) whose surface is the layer: a cave is a hollow, an
//! overhang under the ground a pocket. [`mesh_volume_layer`] meshes it with the crate's dual
//! contouring ([`mesh_chunk`]) on one global cell grid, in blocks of `block_cells` cells, and
//! welds the blocks by exact position (the module's stitch contract: the union is the mesh one
//! big chunk gives). [`clip_below_ground`] then keeps only what lies under the LOD-0 ground: each
//! triangle that crosses the ground is split where its vertices' height over the ground changes
//! sign (a deterministic linear split of each crossing edge, the new vertex placed on the
//! ground), and the kept part's new edges chain into closed **clip loops** on the ground. A clip
//! loop that a layer fills is the mouth: its XZ polygon is the named hole's footprint, so the
//! lattice cut opens the ground inside it and the cook's zipper joins the loop (the layer's
//! open boundary) to the ring. A loop the layer does not fill stays under whole ground.
//!
//! The canonical (query) mesh is this LOD-0 mesh; the render chunks and their LODs are cut from
//! it like any other layer's canonical triangles (§ 2a.5).

use std::collections::BTreeMap;

use crate::volume_mesh::{MeshError, MeshParams, NeighborLods, SdfField, mesh_chunk};

/// A triangle, world metres.
pub type Triangle = [[f32; 3]; 3];

/// A clip segment on the ground, world metres.
pub type Segment = [[f32; 3]; 2];

/// What [`mesh_volume_layer`] meshes.
pub struct VolumeLayerInputs<'a> {
    /// The field (negative inside the solid).
    pub field: &'a dyn SdfField,
    /// The iso level: the surface is `field = iso`.
    pub iso: f32,
    /// The cell size, metres (a power of two keeps the block origins exact, see
    /// [`crate::volume_mesh`]'s known limitation).
    pub cell_m: f32,
    /// The world box meshed, metres (`[min, max]`); the surface is cropped at it.
    pub bounds: [[f32; 3]; 2],
    /// Cells per meshing block along x and z (the cook passes its chunk side over `cell_m`). The
    /// welded result does not depend on it.
    pub block_cells: u32,
    /// The LOD-0 ground height at world `(x, z)`, metres.
    pub ground: &'a dyn Fn(f64, f64) -> f64,
}

/// A meshed, clipped volume layer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VolumeLayerMesh {
    /// The triangles under the ground, world metres, in a deterministic order (block order, then
    /// the mesher's order; split parts follow their source triangle).
    pub triangles: Vec<[[f32; 3]; 3]>,
    /// The closed clip loops on the ground, each starting at its smallest vertex (by `(z, x)`
    /// bits order), sorted by that vertex.
    pub loops: Vec<Vec<[f32; 3]>>,
    /// Clip-edge chains that did not close (the surface left the meshed bounds on the ground).
    pub open_chains: u32,
}

/// Why a volume layer did not mesh. Codes are part of the agent protocol.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum VolumeLayerError {
    /// (`meshlayers_volume_mesh`) the dual-contouring mesher refused the field or the grid.
    #[error("volume mesh: {0:?}")]
    Mesh(MeshError),
    /// (`meshlayers_volume_grid`) the cell size, the bounds or the block size cannot form a grid.
    #[error("volume grid: {0}")]
    Grid(String),
}

impl VolumeLayerError {
    /// The agent error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Mesh(_) => "meshlayers_volume_mesh",
            Self::Grid(_) => "meshlayers_volume_grid",
        }
    }
}

/// The field with its iso level subtracted, so the mesher's zero is the layer's surface.
struct IsoField<'a> {
    field: &'a dyn SdfField,
    iso: f32,
}

impl SdfField for IsoField<'_> {
    fn sample(&self, p: [f32; 3]) -> f32 {
        self.field.sample(p) - self.iso
    }

    fn gradient(&self, p: [f32; 3]) -> [f32; 3] {
        self.field.gradient(p)
    }
}

/// Meshes and clips one volume layer (module docs).
///
/// # Errors
///
/// [`VolumeLayerError`].
pub fn mesh_volume_layer(inputs: &VolumeLayerInputs<'_>) -> Result<VolumeLayerMesh, VolumeLayerError> {
    let _span = tracing::info_span!(target: "regia_terrain", "terrain.mesh_volume_layer").entered();
    let cell = inputs.cell_m;
    if !cell.is_finite() || cell <= 0.0 {
        return Err(VolumeLayerError::Grid(format!(
            "cell_m {cell} is not a positive size"
        )));
    }
    if !inputs.iso.is_finite() {
        return Err(VolumeLayerError::Grid("iso is not finite".into()));
    }
    let [lo, hi] = inputs.bounds;
    if lo.iter().chain(&hi).any(|v| !v.is_finite()) || (0..3).any(|k| hi[k] <= lo[k]) {
        return Err(VolumeLayerError::Grid(format!("bounds {lo:?}..{hi:?} are empty")));
    }
    if inputs.block_cells == 0 {
        return Err(VolumeLayerError::Grid("block_cells is 0".into()));
    }
    // The global cell grid: integer cell coordinates from the origin, so every block's origin is
    // `origin + k * cell` (exact for a power-of-two cell and moderate coordinates).
    let first = lo.map(|v| (f64::from(v) / f64::from(cell)).floor() as i64);
    let last = hi.map(|v| (f64::from(v) / f64::from(cell)).ceil() as i64);
    let cells: [i64; 3] = [0, 1, 2].map(|k| (last[k] - first[k]).max(1));
    let block = i64::from(inputs.block_cells);
    let field = IsoField {
        field: inputs.field,
        iso: inputs.iso,
    };
    let params = MeshParams::default();
    let mut soup: Vec<[[f32; 3]; 3]> = Vec::new();
    let blocks = |n: i64| (n.unsigned_abs()).div_ceil(block.unsigned_abs()) as i64;
    for bz in 0..blocks(cells[2]) {
        for bx in 0..blocks(cells[0]) {
            let c0 = [first[0] + bx * block, first[1], first[2] + bz * block];
            let n = [
                block.min(cells[0] - bx * block),
                cells[1],
                block.min(cells[2] - bz * block),
            ];
            let origin = c0.map(|c| (c as f64 * f64::from(cell)) as f32);
            let size = n.map(|v| u32::try_from(v).unwrap_or(u32::MAX));
            let mesh = mesh_chunk(&field, origin, cell, size, NeighborLods::same(), &params)
                .map_err(VolumeLayerError::Mesh)?;
            for t in mesh.indices.as_chunks::<3>().0 {
                soup.push(t.map(|i| mesh.positions[i as usize]));
            }
        }
    }
    let (triangles, segments) = clip_below_ground(&soup, inputs.ground);
    let (loops, open_chains) = chain_loops(&segments);
    Ok(VolumeLayerMesh {
        triangles,
        loops,
        open_chains,
    })
}

/// The bits of a position, for exact keys.
fn key(p: [f32; 3]) -> [u32; 3] {
    p.map(f32::to_bits)
}

/// The point where edge `a`-`b` crosses the ground, on the ground. Computed from the edge's
/// endpoints in a fixed order (smaller key first), so both triangles of a shared edge get the
/// same bits.
fn crossing(a: [f32; 3], da: f64, b: [f32; 3], db: f64, ground: &dyn Fn(f64, f64) -> f64) -> [f32; 3] {
    let ((a, da), (b, db)) = if key(a) <= key(b) {
        ((a, da), (b, db))
    } else {
        ((b, db), (a, da))
    };
    if da == 0.0 {
        return a;
    }
    if db == 0.0 {
        return b;
    }
    let t = da / (da - db);
    let x = f64::from(a[0]) + t * (f64::from(b[0]) - f64::from(a[0]));
    let z = f64::from(a[2]) + t * (f64::from(b[2]) - f64::from(a[2]));
    [x as f32, ground(x, z) as f32, z as f32]
}

/// Keeps the part of each triangle under the ground (spec 26 § 2a.9, ADR-10071): a vertex whose
/// height over `ground` is positive is above; an edge from an above vertex to one that is not is
/// split at the zero crossing (linear in the height difference), and the new vertex is placed on
/// the ground. A triangle wholly above is dropped, one wholly under is kept as it is, and a
/// crossing one keeps one triangle (two vertices above) or two (one above), in its own winding.
/// Returns the kept triangles and the clip segments (the new edges on the ground), each once.
#[must_use]
pub fn clip_below_ground(
    triangles: &[Triangle],
    ground: &dyn Fn(f64, f64) -> f64,
) -> (Vec<Triangle>, Vec<Segment>) {
    let mut kept = Vec::with_capacity(triangles.len());
    let mut segments = Vec::new();
    fn push(out: &mut Vec<[[f32; 3]; 3]>, t: [[f32; 3]; 3]) {
        if key(t[0]) != key(t[1]) && key(t[1]) != key(t[2]) && key(t[0]) != key(t[2]) {
            out.push(t);
        }
    }
    for t in triangles {
        let d = t.map(|p| f64::from(p[1]) - ground(f64::from(p[0]), f64::from(p[2])));
        let above = d.map(|v| v > 0.0);
        let count = above.iter().filter(|a| **a).count();
        match count {
            0 => kept.push(*t),
            3 => {}
            _ => {
                // Rotate so that vertex 0 is the odd one out (the lone above vertex, or the lone
                // vertex under two above ones); the winding is kept.
                let lone = (0..3).find(|&k| above[k] == (count == 1)).unwrap_or_default();
                let (a, b, c) = (lone, (lone + 1) % 3, (lone + 2) % 3);
                let pab = crossing(t[a], d[a], t[b], d[b], ground);
                let pca = crossing(t[c], d[c], t[a], d[a], ground);
                if count == 1 {
                    // a above: keep the quad b, c, pca, pab.
                    push(&mut kept, [t[b], t[c], pca]);
                    push(&mut kept, [t[b], pca, pab]);
                } else {
                    // a under, b and c above: keep a, pab, pca.
                    push(&mut kept, [t[a], pab, pca]);
                }
                if key(pab) != key(pca) {
                    segments.push([pab, pca]);
                }
            }
        }
    }
    (kept, segments)
}

/// Chains clip segments into closed loops by exact endpoint bits. A vertex with other than two
/// segments ends a chain (counted open). Each loop starts at its smallest vertex in `(z, x, y)`
/// bit order and the loops are sorted by it.
#[must_use]
pub fn chain_loops(segments: &[Segment]) -> (Vec<Vec<[f32; 3]>>, u32) {
    let order = |p: [f32; 3]| (p[2].to_bits(), p[0].to_bits(), p[1].to_bits());
    let mut adj: BTreeMap<[u32; 3], Vec<([u32; 3], usize)>> = BTreeMap::new();
    let mut pos: BTreeMap<[u32; 3], [f32; 3]> = BTreeMap::new();
    for (s, [a, b]) in segments.iter().enumerate() {
        adj.entry(key(*a)).or_default().push((key(*b), s));
        adj.entry(key(*b)).or_default().push((key(*a), s));
        pos.insert(key(*a), *a);
        pos.insert(key(*b), *b);
    }
    let mut used = vec![false; segments.len()];
    let mut loops = Vec::new();
    let mut open = 0u32;
    let mut starts: Vec<[f32; 3]> = pos.values().copied().collect();
    starts.sort_by_key(|p| order(*p));
    for start in starts {
        let sk = key(start);
        while let Some(&(mut next, seg)) = adj[&sk].iter().find(|(_, s)| !used[*s]) {
            used[seg] = true;
            let mut chain = vec![start];
            let mut closed = false;
            let mut prev_seg = seg;
            loop {
                if next == sk {
                    closed = true;
                    break;
                }
                chain.push(pos[&next]);
                let links = &adj[&next];
                if links.len() != 2 {
                    break;
                }
                let Some(&(n2, s2)) = links.iter().find(|(_, s)| *s != prev_seg && !used[*s]) else {
                    break;
                };
                used[s2] = true;
                prev_seg = s2;
                next = n2;
            }
            if closed && adj[&sk].len() == 2 && chain.len() >= 3 {
                loops.push(chain);
            } else {
                open += 1;
            }
        }
    }
    loops.sort_by_key(|l| order(l[0]));
    (loops, open)
}

/// The XZ polygon of a clip loop, world millimetres (`m_to_mm` per coordinate), as a fan of
/// triangles from the loop's mean point, each written as a quad with its last corner repeated
/// (the shape of a `regia_world_geom::cut::MouthFootprint` quad). `None` when the loop is not
/// star-shaped from that point (some fan triangle winds the other way or has no area): such a
/// mouth needs an authored footprint (ADR-10071).
#[must_use]
pub fn loop_footprint_quads(points: &[[f32; 3]]) -> Option<Vec<[[i64; 2]; 4]>> {
    if points.len() < 3 {
        return None;
    }
    let mm: Vec<[i64; 2]> = points
        .iter()
        .map(|p| {
            [
                i64::from(regia_world_geom::units::m_to_mm(p[0])),
                i64::from(regia_world_geom::units::m_to_mm(p[2])),
            ]
        })
        .collect();
    let n = mm.len() as i64;
    let c = [
        mm.iter().map(|p| p[0]).sum::<i64>().div_euclid(n),
        mm.iter().map(|p| p[1]).sum::<i64>().div_euclid(n),
    ];
    let cross = |a: [i64; 2], b: [i64; 2]| {
        i128::from(a[0] - c[0]) * i128::from(b[1] - c[1]) - i128::from(a[1] - c[1]) * i128::from(b[0] - c[0])
    };
    let mut sign = 0i128;
    let mut quads = Vec::with_capacity(mm.len());
    for (k, &a) in mm.iter().enumerate() {
        let b = mm[(k + 1) % mm.len()];
        let s = cross(a, b).signum();
        if s == 0 || (sign != 0 && s != sign) {
            return None;
        }
        sign = s;
        quads.push([c, a, b, b]);
    }
    Some(quads)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(h: f64) -> impl Fn(f64, f64) -> f64 {
        move |_, _| h
    }

    #[test]
    fn a_crossing_triangle_keeps_its_part_under_the_ground_in_its_winding() {
        let g = flat(0.0);
        // One vertex above.
        let t = [[0.0, 1.0, 0.0], [1.0, -1.0, 0.0], [0.0, -1.0, 1.0]];
        let (kept, segs) = clip_below_ground(&[t], &g);
        assert_eq!(kept.len(), 2);
        assert_eq!(segs.len(), 1);
        for k in &kept {
            assert!(k.iter().all(|p| p[1] <= 0.0), "{k:?}");
        }
        // Two vertices above.
        let t2 = [[0.0, -1.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 1.0]];
        let (kept2, segs2) = clip_below_ground(&[t2], &g);
        assert_eq!(kept2.len(), 1);
        assert_eq!(segs2[0][0], [0.5, 0.0, 0.0]);
        assert_eq!(segs2[0][1], [0.0, 0.0, 0.5]);
        // The winding (normal direction) is the source's.
        let n = |t: [[f32; 3]; 3]| {
            let u = [t[1][0] - t[0][0], t[1][1] - t[0][1], t[1][2] - t[0][2]];
            let v = [t[2][0] - t[0][0], t[2][1] - t[0][1], t[2][2] - t[0][2]];
            [
                u[1] * v[2] - u[2] * v[1],
                u[2] * v[0] - u[0] * v[2],
                u[0] * v[1] - u[1] * v[0],
            ]
        };
        let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        assert!(dot(n(kept2[0]), n(t2)) > 0.0);
        for k in &kept {
            assert!(dot(n(*k), n(t)) > 0.0);
        }
        // Wholly above: dropped; wholly under: kept as is.
        let up = [[0.0, 1.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 1.0]];
        let down = up.map(|p| [p[0], -1.0, p[2]]);
        let (k3, s3) = clip_below_ground(&[up, down], &g);
        assert_eq!(k3, vec![down]);
        assert!(s3.is_empty());
    }

    #[test]
    fn a_hollow_sphere_through_flat_ground_clips_to_one_closed_loop_on_the_ground() {
        let field = |p: [f32; 3]| {
            let d = [p[0] - 8.0, p[1] + 1.0, p[2] - 8.0];
            3.0 - (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
        };
        let ground = flat(0.0);
        let run = |block_cells| {
            mesh_volume_layer(&VolumeLayerInputs {
                field: &field,
                iso: 0.0,
                cell_m: 0.5,
                bounds: [[0.0, -8.0, 0.0], [16.0, 4.0, 16.0]],
                block_cells,
                ground: &ground,
            })
            .expect("the sphere meshes")
        };
        let one = run(8);
        assert!(!one.triangles.is_empty());
        assert_eq!(one.loops.len(), 1, "one mouth loop");
        assert_eq!(one.open_chains, 0);
        for p in one.triangles.iter().flatten() {
            assert!(p[1] <= 0.0, "a vertex above the ground: {p:?}");
        }
        for p in &one.loops[0] {
            assert_eq!(p[1], 0.0, "loop vertices are on the ground");
            let r = ((p[0] - 8.0).powi(2) + (p[2] - 8.0).powi(2)).sqrt();
            // The sphere meets y = 0 on a circle of radius sqrt(9 - 1).
            assert!((r - 8f32.sqrt()).abs() < 0.25, "loop radius {r}");
        }
        assert!(loop_footprint_quads(&one.loops[0]).is_some());
        // The welded set does not depend on the meshing blocks.
        let other = run(5);
        let set = |m: &VolumeLayerMesh| {
            let mut v: Vec<[[u32; 3]; 3]> = m.triangles.iter().map(|t| t.map(key)).collect();
            v.sort_unstable();
            v
        };
        assert_eq!(set(&one), set(&other));
        assert_eq!(one.loops.len(), other.loops.len());
    }

    #[test]
    fn a_loop_that_is_not_star_shaped_has_no_fan_footprint() {
        // A U shape seen from its mean point.
        let u = [
            [0.0, 0.0, 0.0],
            [3.0, 0.0, 0.0],
            [3.0, 0.0, 3.0],
            [2.0, 0.0, 3.0],
            [2.0, 0.0, 0.5],
            [1.0, 0.0, 0.5],
            [1.0, 0.0, 3.0],
            [0.0, 0.0, 3.0],
        ];
        assert!(loop_footprint_quads(&u).is_none());
        let square = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 0.0, 1.0], [0.0, 0.0, 1.0]];
        assert_eq!(loop_footprint_quads(&square).map(|q| q.len()), Some(4));
    }
}
