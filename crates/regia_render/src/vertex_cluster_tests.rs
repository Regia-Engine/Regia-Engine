// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/world/decimate.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (lines 102-127: `a_dense_plane_clusters_down_and_keeps_its_extent`, now on the shared-grid call).

//! Tests for the vertex-cluster far LOD (ADR-8464).

use super::*;

/// A hull: an ellipsoid with radii `r`, `seg` segments around and `rings` rings, split into
/// `tiles_u × tiles_v` pieces that each own their vertices (seam vertices are duplicated across
/// pieces by position, like a model split per UV tile).
/// One hull piece: positions, normals and a triangle list.
type HullPiece = (Vec<[f32; 3]>, Vec<[f32; 3]>, Vec<u32>);

struct Hull {
    pieces: Vec<HullPiece>,
}

impl Hull {
    fn new(r: [f32; 3], seg: u32, rings: u32, tiles_u: u32, tiles_v: u32) -> Self {
        let point = |i: u32, j: u32| -> ([f32; 3], [f32; 3]) {
            let theta = std::f32::consts::TAU * i as f32 / seg as f32;
            let phi = std::f32::consts::PI * j as f32 / rings as f32;
            let unit = [phi.sin() * theta.cos(), phi.cos(), phi.sin() * theta.sin()];
            let p = [unit[0] * r[0], unit[1] * r[1], unit[2] * r[2]];
            let n = Vec3::new(unit[0] / r[0], unit[1] / r[1], unit[2] / r[2]).normalize_or_zero();
            (p, n.to_array())
        };
        let mut pieces = Vec::new();
        let (su, sv) = (seg / tiles_u, rings / tiles_v);
        for tv in 0..tiles_v {
            for tu in 0..tiles_u {
                let mut positions = Vec::new();
                let mut normals = Vec::new();
                for j in 0..=sv {
                    for i in 0..=su {
                        let (p, n) = point(tu * su + i, tv * sv + j);
                        positions.push(p);
                        normals.push(n);
                    }
                }
                let mut indices = Vec::new();
                for j in 0..sv {
                    for i in 0..su {
                        let a = j * (su + 1) + i;
                        let b = a + su + 1;
                        // Outward winding (the pole rows make degenerate triangles, as a real
                        // UV sphere does; they are dropped like any other).
                        indices.extend_from_slice(&[a, a + 1, b, a + 1, b + 1, b]);
                    }
                }
                pieces.push((positions, normals, indices));
            }
        }
        Self { pieces }
    }

    fn as_pieces(&self) -> Vec<ClusterPiece<'_>> {
        self.pieces
            .iter()
            .map(|(positions, normals, indices)| ClusterPiece {
                positions,
                normals,
                indices,
            })
            .collect()
    }

    fn triangles(&self) -> usize {
        self.pieces.iter().map(|p| p.2.len() / 3).sum()
    }
}

/// Orthographic coverage mask of triangles projected along `axis` onto an `n × n` grid over
/// `[lo, lo + size]²` in the two other axes. Pixel centres are tested, both windings count.
fn coverage(tris: &[[[f32; 3]; 3]], axis: usize, lo: [f32; 2], size: f32, n: usize) -> Vec<bool> {
    let (u, v) = match axis {
        0 => (1, 2),
        1 => (0, 2),
        _ => (0, 1),
    };
    let px = size / n as f32;
    let mut mask = vec![false; n * n];
    for tri in tris {
        let q = tri.map(|p| [(p[u] - lo[0]) / px, (p[v] - lo[1]) / px]);
        let edge = |a: [f32; 2], b: [f32; 2], c: [f32; 2]| {
            (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
        };
        let area = edge(q[0], q[1], q[2]);
        if area == 0.0 {
            continue;
        }
        let min_x = q.iter().map(|p| p[0]).fold(f32::MAX, f32::min).floor().max(0.0) as usize;
        let max_x = (q.iter().map(|p| p[0]).fold(f32::MIN, f32::max).ceil() as usize).min(n);
        let min_y = q.iter().map(|p| p[1]).fold(f32::MAX, f32::min).floor().max(0.0) as usize;
        let max_y = (q.iter().map(|p| p[1]).fold(f32::MIN, f32::max).ceil() as usize).min(n);
        for y in min_y..max_y {
            for x in min_x..max_x {
                let c = [x as f32 + 0.5, y as f32 + 0.5];
                let w0 = edge(q[1], q[2], c) * area.signum();
                let w1 = edge(q[2], q[0], c) * area.signum();
                let w2 = edge(q[0], q[1], c) * area.signum();
                if w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0 {
                    mask[y * n + x] = true;
                }
            }
        }
    }
    mask
}

/// The largest distance, in pixels, from a pixel set in `a` but not in `b` to the nearest pixel
/// set in `b` (a one-sided silhouette Hausdorff distance on the grid). Searches up to `limit`.
fn one_sided_gap(a: &[bool], b: &[bool], n: usize, limit: usize) -> f32 {
    let mut worst = 0.0f32;
    for y in 0..n {
        for x in 0..n {
            if !a[y * n + x] || b[y * n + x] {
                continue;
            }
            let mut best = f32::MAX;
            for yy in y.saturating_sub(limit)..(y + limit + 1).min(n) {
                for xx in x.saturating_sub(limit)..(x + limit + 1).min(n) {
                    if b[yy * n + xx] {
                        let d = ((xx as f32 - x as f32).powi(2) + (yy as f32 - y as f32).powi(2)).sqrt();
                        best = best.min(d);
                    }
                }
            }
            worst = worst.max(best);
        }
    }
    worst
}

fn source_tris(pieces: &[ClusterPiece<'_>]) -> Vec<[[f32; 3]; 3]> {
    pieces
        .iter()
        .flat_map(|p| {
            p.indices
                .as_chunks::<3>()
                .0
                .iter()
                .map(|t| t.map(|i| p.positions[i as usize]))
        })
        .collect()
}

fn lod_tris(lod: &ClusterLod) -> Vec<[[f32; 3]; 3]> {
    lod.pieces
        .iter()
        .flat_map(|p| {
            p.indices
                .as_chunks::<3>()
                .0
                .iter()
                .map(|t| t.map(|i| p.positions[i as usize]))
        })
        .collect()
}

/// P-19 acceptance: a 26-piece hull of ~60k triangles clusters to the far budget, keeps every
/// piece, and its silhouette from each axis stays within the reported error of the source's.
#[test]
fn cluster_lod_keeps_silhouette_within_bound() -> Result<(), VertexClusterError> {
    // 13 × 2 tiles = 26 pieces (the source's hero case), 208 × 144 quads.
    let hull = Hull::new([4.0, 1.5, 1.2], 208, 144, 13, 2);
    let pieces = hull.as_pieces();
    let source = hull.triangles();
    assert!(source > 55_000, "{source}");
    let lod = cluster_pieces(&pieces, &VertexClusterSettings::with_target(2_000))?;

    assert_eq!(lod.pieces.len(), 26);
    assert_eq!(lod.source_triangles, source);
    let kept = lod.triangles();
    assert!(kept <= 2_000, "{kept} triangles over the 2000 budget");
    assert!(kept >= 500, "{kept} triangles: collapsed too far");
    for (i, piece) in lod.pieces.iter().enumerate() {
        assert!(piece.triangles() > 0, "piece {i} dropped (a floating-plate hole)");
    }
    // The bound is real: every vertex moved at most one cell diagonal, and the level is coarse
    // enough to matter but finer than a tenth of the model.
    assert!(lod.max_error > 0.0);
    assert!(
        lod.max_error <= lod.cell_size * 3.0f32.sqrt() + 1.0e-4,
        "error {} over the cell diagonal {}",
        lod.max_error,
        lod.cell_size * 3.0f32.sqrt()
    );
    assert!(
        lod.max_error < 0.8,
        "error {} over a tenth of the 8 m hull",
        lod.max_error
    );

    let (a, b) = (source_tris(&pieces), lod_tris(&lod));
    let n = 160;
    let size = 8.4;
    for axis in 0..3 {
        let lo = [-4.2, -4.2];
        let ma = coverage(&a, axis, lo, size, n);
        let mb = coverage(&b, axis, lo, size, n);
        let px = size / n as f32;
        // One pixel diagonal of slack for pixel-centre sampling on each side.
        let allowed = lod.max_error / px + 2.0f32.sqrt() + 0.01;
        let limit = allowed.ceil() as usize + 2;
        let lost = one_sided_gap(&ma, &mb, n, limit);
        let grown = one_sided_gap(&mb, &ma, n, limit);
        assert!(
            lost <= allowed && grown <= allowed,
            "axis {axis}: silhouette moved {lost:.2} px in / {grown:.2} px out, bound {allowed:.2} px"
        );
        assert!(mb.iter().filter(|&&s| s).count() * 10 > ma.iter().filter(|&&s| s).count() * 9);
    }
    Ok(())
}

/// Seam vertices two pieces share by position land on the same point: the shared grid keeps the
/// seams closed. Clustering each piece alone (the source's per-piece call) opens them.
#[test]
fn cluster_pieces_keep_shared_seams_closed() -> Result<(), VertexClusterError> {
    let hull = Hull::new([4.0, 1.5, 1.2], 96, 64, 6, 2);
    let pieces = hull.as_pieces();
    let lod = cluster_pieces(&pieces, &VertexClusterSettings::with_target(800))?;
    let mut by_position: HashMap<[u32; 3], Vec<[f32; 3]>> = HashMap::new();
    for (piece, out) in pieces.iter().zip(&lod.pieces) {
        for (p, &k) in piece.positions.iter().zip(&out.remap) {
            by_position
                .entry(p.map(|c| (c + 0.0).to_bits()))
                .or_default()
                .push(out.positions[k as usize]);
        }
    }
    let shared = by_position.values().filter(|v| v.len() > 1).count();
    assert!(shared > 100, "{shared} seam positions");
    for reps in by_position.values() {
        assert!(reps.iter().all(|r| r == &reps[0]), "seam opened: {reps:?}");
    }

    // The same pieces clustered one at a time: their borders snap apart.
    let budget = 800 / pieces.len();
    let mut opened = 0;
    let mut alone: HashMap<[u32; 3], Vec<[f32; 3]>> = HashMap::new();
    for piece in &pieces {
        let one = cluster_pieces(
            std::slice::from_ref(piece),
            &VertexClusterSettings::with_target(budget),
        )?;
        for (p, &k) in piece.positions.iter().zip(&one.pieces[0].remap) {
            alone
                .entry(p.map(|c| (c + 0.0).to_bits()))
                .or_default()
                .push(one.pieces[0].positions[k as usize]);
        }
    }
    for reps in alone.values() {
        if reps.iter().any(|r| r != &reps[0]) {
            opened += 1;
        }
    }
    assert!(
        opened > 0,
        "per-piece clustering kept every seam: the test shape is too easy"
    );
    Ok(())
}

/// The source's own case: a dense plane clusters into budget and keeps its extent.
#[test]
fn a_dense_plane_clusters_down_and_keeps_its_extent() {
    let n = 200u32;
    let mut vertices = Vec::new();
    for z in 0..=n {
        for x in 0..=n {
            vertices.push(LabVertex::from_pos_normal_uv(
                [x as f32 * 0.05, 0.0, z as f32 * 0.05],
                [0.0, 1.0, 0.0],
                [x as f32 / n as f32, z as f32 / n as f32],
            ));
        }
    }
    let mut indices = Vec::new();
    for z in 0..n {
        for x in 0..n {
            let i = z * (n + 1) + x;
            indices.extend_from_slice(&[i, i + n + 1, i + 1, i + 1, i + n + 1, i + n + 2]);
        }
    }
    let (v, i) = cluster_lab_mesh(&vertices, &indices, 2_000);
    assert!(i.len() / 3 <= 2_000, "{}", i.len() / 3);
    assert!(i.len() / 3 > 200, "{}", i.len() / 3);
    assert!(i.iter().all(|&k| (k as usize) < v.len()));
    let max_x = v.iter().map(|v| v.position[0]).fold(0.0f32, f32::max);
    assert!(max_x > 9.0, "{max_x}");
    // Attributes other than position and normal come from a source vertex of the cell.
    assert!(
        v.iter()
            .all(|v| v.normal == [0.0, 1.0, 0.0] && v.color == [1.0; 4])
    );
    assert!(v.iter().all(|v| (0.0..=1.0).contains(&v.uv[0])));
}

/// A thin plate (two sides back to back) keeps both faces with their own normals.
#[test]
fn thin_plate_keeps_both_faces() -> Result<(), VertexClusterError> {
    let n = 60u32;
    let mut positions = Vec::new();
    let mut normals = Vec::new();
    let mut indices = Vec::new();
    for (side, (y, ny)) in [(0.0f32, 1.0f32), (-0.001, -1.0)].into_iter().enumerate() {
        let base = positions.len() as u32;
        for z in 0..=n {
            for x in 0..=n {
                positions.push([x as f32 * 0.1, y, z as f32 * 0.1]);
                normals.push([0.0, ny, 0.0]);
            }
        }
        for z in 0..n {
            for x in 0..n {
                let i = base + z * (n + 1) + x;
                if side == 0 {
                    indices.extend_from_slice(&[i, i + n + 1, i + 1, i + 1, i + n + 1, i + n + 2]);
                } else {
                    indices.extend_from_slice(&[i, i + 1, i + n + 1, i + 1, i + n + 2, i + n + 1]);
                }
            }
        }
    }
    let piece = ClusterPiece {
        positions: &positions,
        normals: &normals,
        indices: &indices,
    };
    let lod = cluster_pieces(&[piece], &VertexClusterSettings::with_target(400))?;
    let out = &lod.pieces[0];
    assert!(
        lod.triangles() <= 400 && lod.triangles() > 40,
        "{}",
        lod.triangles()
    );
    let mut up = 0;
    let mut down = 0;
    for tri in out.indices.as_chunks::<3>().0 {
        let ns = tri.map(|k| out.normals[k as usize][1]);
        if ns.iter().all(|&y| y == 1.0) {
            up += 1;
        } else if ns.iter().all(|&y| y == -1.0) {
            down += 1;
        } else {
            panic!("a triangle mixes the two faces: {ns:?}");
        }
    }
    assert!(up > 20 && down > 20, "{up} up, {down} down");

    // Without the facing split the two sides share vertices and their normals cancel.
    let merged = cluster_pieces(
        &[piece],
        &VertexClusterSettings {
            split_by_facing: false,
            ..VertexClusterSettings::with_target(400)
        },
    )?;
    let p = &merged.pieces[0];
    assert!(p.normals.iter().any(|n| n[1].abs() != 1.0) || p.positions.len() < out.positions.len());
    Ok(())
}

/// After the snap, a cell pair can receive the same triangle twice: only one copy is kept.
#[test]
fn repeated_triangles_after_the_snap_are_dropped() -> Result<(), VertexClusterError> {
    // A dense strip folded on itself: many source triangles map to the same cell triple.
    let hull = Hull::new([1.0, 1.0, 1.0], 128, 64, 1, 1);
    let pieces = hull.as_pieces();
    let lod = cluster_pieces(&pieces, &VertexClusterSettings::with_target(300))?;
    let mut seen = HashSet::new();
    for tri in lod.pieces[0].indices.as_chunks::<3>().0 {
        let [a, b, c] = *tri;
        assert!(a != b && b != c && a != c);
        let key = if a < b && a < c {
            [a, b, c]
        } else if b < c {
            [b, c, a]
        } else {
            [c, a, b]
        };
        assert!(seen.insert(key), "triangle {tri:?} kept twice");
    }
    Ok(())
}

/// Small inputs come back unchanged with zero error; bad input is refused with a reason.
#[test]
fn small_or_bad_input() {
    let positions = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
    let indices = [0, 1, 2];
    let piece = ClusterPiece {
        positions: &positions,
        normals: &[],
        indices: &indices,
    };
    let lod = cluster_pieces(&[piece], &VertexClusterSettings::with_target(1));
    assert!(matches!(&lod, Ok(l) if l.passes == 0 && l.max_error == 0.0 && l.pieces[0].indices == indices));

    let bad_index = ClusterPiece {
        indices: &[0, 1, 3],
        ..piece
    };
    assert!(matches!(
        cluster_pieces(&[bad_index], &VertexClusterSettings::default()),
        Err(VertexClusterError::IndexOutOfRange {
            piece: 0,
            index: 3,
            ..
        })
    ));
    let nan = [[0.0, 0.0, 0.0], [f32::NAN, 0.0, 0.0], [0.0, 1.0, 0.0]];
    assert!(matches!(
        cluster_pieces(
            &[ClusterPiece {
                positions: &nan,
                ..piece
            }],
            &VertexClusterSettings::default()
        ),
        Err(VertexClusterError::NonFinitePosition { piece: 0, vertex: 1 })
    ));
    assert!(matches!(
        cluster_pieces(
            &[ClusterPiece {
                normals: &[[0.0, 0.0, 1.0]],
                ..piece
            }],
            &VertexClusterSettings::default()
        ),
        Err(VertexClusterError::NormalCount { .. })
    ));
    assert!(matches!(
        cluster_pieces(
            &[piece],
            &VertexClusterSettings {
                shrink: 1.0,
                ..VertexClusterSettings::default()
            }
        ),
        Err(VertexClusterError::Settings(_))
    ));
    // The LabVertex call never fails a draw: bad input comes back as given.
    let vertices: Vec<LabVertex> = nan
        .iter()
        .map(|&p| LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], [0.0, 0.0]))
        .collect();
    let (v, i) = cluster_lab_mesh(&vertices, &indices, 0);
    assert_eq!((v.len(), i), (3, indices.to_vec()));
}

/// The CPU-mesh adapter copies attributes from source vertices and yields valid meshes.
#[test]
fn cpu_meshes_cluster_to_valid_meshes() -> Result<(), VertexClusterError> {
    let hull = Hull::new([2.0, 1.0, 1.0], 64, 32, 2, 1);
    let meshes: Vec<regia_asset::CpuMesh> = hull
        .pieces
        .iter()
        .map(|(positions, normals, indices)| regia_asset::CpuMesh {
            positions: positions.clone(),
            normals: normals.clone(),
            uvs: positions.iter().map(|p| [p[0], p[1]]).collect(),
            colors: positions.iter().map(|p| [p[2], 0.0, 0.0, 1.0]).collect(),
            indices: indices.clone(),
            ..Default::default()
        })
        .collect();
    let refs: Vec<&regia_asset::CpuMesh> = meshes.iter().collect();
    let (out, lod) = cluster_cpu_meshes(&refs, &VertexClusterSettings::with_target(300))?;
    assert_eq!(out.len(), 2);
    assert!(lod.triangles() <= 300);
    for (mesh, (source, piece)) in out.iter().zip(meshes.iter().zip(&lod.pieces)) {
        assert!(mesh.validate().is_ok(), "{:?}", mesh.validate());
        assert!(mesh.tangents.is_empty() && mesh.skin.is_none());
        for (k, &s) in piece.source_vertices.iter().enumerate() {
            assert_eq!(mesh.uvs[k], source.uvs[s as usize]);
            assert_eq!(mesh.colors[k], source.colors[s as usize]);
        }
    }
    Ok(())
}

/// Vertices no triangle uses (another level's, in a shared buffer) take no part: they neither
/// pull a cell's mean off the surface nor widen the grid.
#[test]
fn unreferenced_vertices_take_no_part() -> Result<(), VertexClusterError> {
    let hull = Hull::new([2.0, 1.0, 1.0], 64, 32, 1, 1);
    let (positions, normals, indices) = &hull.pieces[0];
    let clean = cluster_pieces(
        &[ClusterPiece {
            positions,
            normals,
            indices,
        }],
        &VertexClusterSettings::with_target(300),
    )?;
    // The same buffer with stray vertices far outside and at the centre, plus a NaN.
    let mut stray_positions = positions.clone();
    let mut stray_normals = normals.clone();
    for p in [[50.0, 50.0, 50.0], [0.0, 0.0, 0.0], [f32::NAN, 0.0, 0.0]] {
        stray_positions.push(p);
        stray_normals.push([0.0, 1.0, 0.0]);
    }
    let stray = cluster_pieces(
        &[ClusterPiece {
            positions: &stray_positions,
            normals: &stray_normals,
            indices,
        }],
        &VertexClusterSettings::with_target(300),
    )?;
    assert_eq!(stray.pieces[0].indices, clean.pieces[0].indices);
    assert_eq!(stray.pieces[0].positions, clean.pieces[0].positions);
    assert_eq!(stray.max_error, clean.max_error);
    assert!(
        stray.pieces[0].remap[positions.len()..]
            .iter()
            .all(|&k| k == u32::MAX)
    );
    Ok(())
}
