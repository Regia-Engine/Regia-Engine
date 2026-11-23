// SPDX-License-Identifier: MPL-2.0
//! The seam zipper (spec 26 § 2a.4 step 3, ADR-8291).
//!
//! Two closed polylines, the authored mouth loop and the lattice ring, are joined by the shorter
//! new edge, with arc length breaking a tie (Turk and Levoy, "Zippered Polygon Meshes from Range
//! Images", SIGGRAPH 1994). Authored vertex
//! positions are copied and never written. A vertical skirt hangs from the ring. Triangles of the
//! source mesh that sit in the mouth and less than `throat_m` under the ground move into the band;
//! the source mesh itself is not changed.

use std::collections::BTreeMap;

use regia_world_geom::TriRef;

use super::{BandMesh, BandVertex, CopiedTri, SeamCookError, ZipperParams};

/// One zipper: the ring, the authored indexed mesh, and the ground tests.
pub struct ZipperInput<'a> {
    /// Hole id, copied into errors.
    pub hole: &'a str,
    /// Lattice ring, one closed loop, the first vertex not repeated.
    pub ring: &'a [[f32; 3]],
    /// Authored vertex positions. The zipper only reads them.
    pub positions: &'a [[f32; 3]],
    /// Triangle indices into `positions`.
    pub indices: &'a [u32],
    /// Stitch gap, skirt, throat and the above-ground slack.
    pub params: ZipperParams,
    /// LOD-0 ground height at world XZ.
    pub ground_y: &'a dyn Fn(f32, f32) -> f32,
    /// `true` when the XZ point lies in the lattice cut (the hole footprint).
    pub inside_cut: &'a dyn Fn(f32, f32) -> bool,
    /// One entry per source triangle, or empty when the mesh has no canonical ids.
    pub source_tris: &'a [Option<TriRef>],
}

/// The band the zipper built, plus the source triangles split into throat and body.
#[derive(Clone, Debug)]
pub struct ZipperOutput {
    /// Strip, ring and skirt.
    pub band: BandMesh,
    /// Source triangles that moved into the band.
    pub throat: Vec<CopiedTri>,
    /// Source triangles that stay in the body.
    pub body: Vec<CopiedTri>,
}

/// Joins `input.ring` to the authored boundary loop nearest it.
///
/// # Errors
///
/// [`SeamCookError::LoopMissing`] when that loop is farther than `max_stitch_gap_m`.
/// [`SeamCookError::AboveGround`] when an authored loop vertex outside the cut rises more than
/// the slack above the LOD-0 ground. [`SeamCookError::StripFolds`] when the strip's normals
/// disagree or adjacent strip triangles face each other.
pub fn zipper(input: &ZipperInput<'_>) -> Result<ZipperOutput, SeamCookError> {
    let _span = tracing::info_span!("terrain_seam_zipper", hole = input.hole).entered();
    if input.ring.len() < 3 || input.indices.is_empty() || !input.indices.len().is_multiple_of(3) {
        return Err(SeamCookError::LoopMissing {
            hole: input.hole.to_owned(),
            gap_m: f32::MAX,
        });
    }
    let mut loops = boundary_loops(input.indices);
    if loops.is_empty() {
        return Err(SeamCookError::LoopMissing {
            hole: input.hole.to_owned(),
            gap_m: f32::MAX,
        });
    }
    for loop_ix in &mut loops {
        let mut pts: Vec<[f32; 3]> = loop_ix.iter().map(|&i| input.positions[i as usize]).collect();
        orient_ccw_xz(&mut pts);
        *loop_ix = pts.iter().map(|p| nearest_index(input.positions, p)).collect();
    }
    let ring = {
        let mut r = input.ring.to_vec();
        orient_ccw_xz(&mut r);
        r
    };
    let mut best: Option<(f32, usize)> = None;
    for (i, loop_ix) in loops.iter().enumerate() {
        let gap = loop_gap(input.positions, loop_ix, &ring);
        if best.is_none_or(|(g, _)| gap < g) {
            best = Some((gap, i));
        }
    }
    let (gap, chosen) = best.unwrap_or((f32::MAX, 0));
    if !gap.is_finite() || gap > input.params.max_stitch_gap_m {
        return Err(SeamCookError::LoopMissing {
            hole: input.hole.to_owned(),
            gap_m: gap,
        });
    }
    let mut authored: Vec<[f32; 3]> = loops[chosen]
        .iter()
        .map(|&i| input.positions[i as usize])
        .collect();
    let mut ring = ring;
    rotate_to_closest(&mut authored, &mut ring);
    if let Some(at) = authored.iter().find(|p| {
        !(input.inside_cut)(p[0], p[2])
            && p[1] > (input.ground_y)(p[0], p[2]) + input.params.above_ground_slack_m
    }) {
        return Err(SeamCookError::AboveGround {
            hole: input.hole.to_owned(),
            at: *at,
        });
    }
    let mut strip = stitch(&authored, &ring);
    fix_strip_normals(&mut strip).map_err(|at| SeamCookError::StripFolds {
        hole: input.hole.to_owned(),
        at,
    })?;
    let mut band = band_from_strip(&authored, &ring, &strip);
    add_skirt(&mut band, &ring, input.params.skirt_m);
    let (throat, body) = split_source(input);
    Ok(ZipperOutput { band, throat, body })
}

fn nearest_index(positions: &[[f32; 3]], p: &[f32; 3]) -> u32 {
    positions
        .iter()
        .position(|q| {
            q[0].to_bits() == p[0].to_bits()
                && q[1].to_bits() == p[1].to_bits()
                && q[2].to_bits() == p[2].to_bits()
        })
        .unwrap_or(0) as u32
}

fn boundary_loops(indices: &[u32]) -> Vec<Vec<u32>> {
    let mut count: BTreeMap<(u32, u32), u32> = BTreeMap::new();
    let mut dir: BTreeMap<(u32, u32), (u32, u32)> = BTreeMap::new();
    for tri in indices.as_chunks::<3>().0 {
        let [a, b, c] = *tri;
        for (u, v) in [(a, b), (b, c), (c, a)] {
            let key = if u < v { (u, v) } else { (v, u) };
            *count.entry(key).or_insert(0) += 1;
            dir.entry(key).or_insert((u, v));
        }
    }
    let mut next: BTreeMap<u32, u32> = BTreeMap::new();
    for (key, n) in &count {
        if *n == 1 {
            let (u, v) = dir[key];
            next.insert(u, v);
        }
    }
    let mut loops = Vec::new();
    while let Some((&start, _)) = next.iter().next() {
        let mut loop_ix = Vec::new();
        let mut at = start;
        while let Some(to) = next.remove(&at) {
            loop_ix.push(at);
            at = to;
            if at == start {
                break;
            }
        }
        if loop_ix.len() >= 3 {
            loops.push(loop_ix);
        }
    }
    loops
}

fn orient_ccw_xz(loop_pts: &mut [[f32; 3]]) {
    if signed_area_xz(loop_pts) < 0.0 {
        loop_pts.reverse();
    }
}

fn signed_area_xz(loop_pts: &[[f32; 3]]) -> f32 {
    let n = loop_pts.len();
    let mut a = 0.0f32;
    for i in 0..n {
        let p = loop_pts[i];
        let q = loop_pts[(i + 1) % n];
        a += p[0] * q[2] - q[0] * p[2];
    }
    a
}

fn xz_dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[2] - b[2];
    (dx * dx + dz * dz).sqrt()
}

fn loop_gap(positions: &[[f32; 3]], loop_ix: &[u32], ring: &[[f32; 3]]) -> f32 {
    let mut max_d = 0.0f32;
    for &i in loop_ix {
        let p = positions[i as usize];
        let mut min_d = f32::MAX;
        for e in 0..ring.len() {
            min_d = min_d.min(dist_to_seg(p, ring[e], ring[(e + 1) % ring.len()]));
        }
        max_d = max_d.max(min_d);
    }
    max_d
}

fn dist_to_seg(p: [f32; 3], a: [f32; 3], b: [f32; 3]) -> f32 {
    let abx = b[0] - a[0];
    let abz = b[2] - a[2];
    let apx = p[0] - a[0];
    let apz = p[2] - a[2];
    let denom = abx * abx + abz * abz;
    let t = if denom <= 1.0e-20 {
        0.0
    } else {
        ((apx * abx + apz * abz) / denom).clamp(0.0, 1.0)
    };
    let dx = apx - abx * t;
    let dz = apz - abz * t;
    (dx * dx + dz * dz).sqrt()
}

fn rotate_to_closest(authored: &mut [[f32; 3]], ring: &mut [[f32; 3]]) {
    let mut best = (f32::MAX, 0usize, 0usize);
    for (i, a) in authored.iter().enumerate() {
        for (j, b) in ring.iter().enumerate() {
            let d = xz_dist(*a, *b);
            if d < best.0 {
                best = (d, i, j);
            }
        }
    }
    authored.rotate_left(best.1);
    ring.rotate_left(best.2);
}

fn arc_params(loop_pts: &[[f32; 3]]) -> Vec<f32> {
    let n = loop_pts.len();
    let mut cum = vec![0.0f32; n + 1];
    for i in 0..n {
        cum[i + 1] = cum[i] + xz_dist(loop_pts[i], loop_pts[(i + 1) % n]);
    }
    let total = cum[n].max(1.0e-12);
    for c in &mut cum {
        *c /= total;
    }
    cum
}

/// Zipper. Both loops are CCW in XZ with the authored loop inside the ring, so a new triangle
/// faces down (`cross_y` < 0) until [`fix_strip_normals`] flips the strip. A candidate that faces
/// up has crossed; the other loop advances instead. When both face the same way, the shorter new
/// edge wins, and equal edges use arc length so two regular polygons alternate instead of fanning.
fn stitch(authored: &[[f32; 3]], ring: &[[f32; 3]]) -> Vec<[[f32; 3]; 3]> {
    let arc_a = arc_params(authored);
    let arc_b = arc_params(ring);
    let na = authored.len();
    let nb = ring.len();
    let mut ia = 0usize;
    let mut ib = 0usize;
    let mut tris = Vec::with_capacity(na + nb);
    while ia < na || ib < nb {
        let can_a = ia < na;
        let can_b = ib < nb;
        let tri_a = [authored[ia % na], ring[ib % nb], authored[(ia + 1) % na]];
        let tri_b = [ring[ib % nb], ring[(ib + 1) % nb], authored[ia % na]];
        let advance_a = if can_a && can_b {
            match (plan_down_or_flat(&tri_a), plan_down_or_flat(&tri_b)) {
                (true, false) => true,
                (false, true) => false,
                _ => {
                    let da = xz_dist(authored[(ia + 1) % na], ring[ib % nb]);
                    let db = xz_dist(ring[(ib + 1) % nb], authored[ia % na]);
                    if (da - db).abs() <= 1.0e-5 {
                        arc_a[ia + 1] <= arc_b[ib + 1]
                    } else {
                        da < db
                    }
                }
            }
        } else {
            can_a
        };
        if advance_a {
            tris.push(tri_a);
            ia += 1;
        } else if can_b {
            tris.push(tri_b);
            ib += 1;
        } else {
            break;
        }
    }
    tris
}

/// `true` when the triangle is degenerate or faces down in XZ (the unflipped strip).
fn plan_down_or_flat(tri: &[[f32; 3]; 3]) -> bool {
    tri_span(tri) <= 1.0e-8 || cross_y(tri) <= 1.0e-8
}

fn cross_y(tri: &[[f32; 3]; 3]) -> f32 {
    let u = [
        tri[1][0] - tri[0][0],
        tri[1][1] - tri[0][1],
        tri[1][2] - tri[0][2],
    ];
    let v = [
        tri[2][0] - tri[0][0],
        tri[2][1] - tri[0][1],
        tri[2][2] - tri[0][2],
    ];
    u[2] * v[0] - u[0] * v[2]
}

fn cross(tri: &[[f32; 3]; 3]) -> [f32; 3] {
    let u = [
        tri[1][0] - tri[0][0],
        tri[1][1] - tri[0][1],
        tri[1][2] - tri[0][2],
    ];
    let v = [
        tri[2][0] - tri[0][0],
        tri[2][1] - tri[0][1],
        tri[2][2] - tri[0][2],
    ];
    [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ]
}

fn tri_span(tri: &[[f32; 3]; 3]) -> f32 {
    let n = cross(tri);
    n[0] * n[0] + n[1] * n[1] + n[2] * n[2]
}

fn fix_strip_normals(tris: &mut [[[f32; 3]; 3]]) -> Result<(), [f32; 3]> {
    const EPS: f32 = 1.0e-8;
    let mut up = 0u32;
    let mut down = 0u32;
    for tri in tris.iter() {
        if tri_span(tri) <= EPS {
            continue;
        }
        let ny = cross_y(tri);
        if ny > EPS {
            up += 1;
        } else if ny < -EPS {
            down += 1;
        }
    }
    if down > up {
        for tri in tris.iter_mut() {
            tri.swap(1, 2);
        }
    }
    up = 0;
    down = 0;
    let mut offender = None;
    for tri in tris.iter() {
        if tri_span(tri) <= EPS {
            continue;
        }
        let ny = cross_y(tri);
        if ny > EPS {
            up += 1;
        } else if ny < -EPS {
            down += 1;
            offender = Some(centroid(tri));
        }
    }
    if up > 0 && down > 0 {
        return Err(offender.unwrap_or(centroid(&tris[0])));
    }
    // Adjacent strip triangles that share an edge in XZ must not face opposite ways.
    let mut edges: BTreeMap<(u64, u64), [f32; 3]> = BTreeMap::new();
    for tri in tris.iter() {
        if tri_span(tri) <= EPS {
            continue;
        }
        let n = cross(tri);
        for e in 0..3 {
            let a = key_xz(tri[e]);
            let b = key_xz(tri[(e + 1) % 3]);
            let key = if a < b { (a, b) } else { (b, a) };
            if let Some(prev) = edges.get(&key) {
                let dot = prev[0] * n[0] + prev[1] * n[1] + prev[2] * n[2];
                if dot < 0.0 {
                    return Err(centroid(tri));
                }
            } else {
                edges.insert(key, n);
            }
        }
    }
    Ok(())
}

fn centroid(tri: &[[f32; 3]; 3]) -> [f32; 3] {
    [
        (tri[0][0] + tri[1][0] + tri[2][0]) / 3.0,
        (tri[0][1] + tri[1][1] + tri[2][1]) / 3.0,
        (tri[0][2] + tri[1][2] + tri[2][2]) / 3.0,
    ]
}

fn key_xz(p: [f32; 3]) -> u64 {
    (u64::from(p[0].to_bits()) << 32) | u64::from(p[2].to_bits())
}

fn band_from_strip(authored: &[[f32; 3]], ring: &[[f32; 3]], strip: &[[[f32; 3]; 3]]) -> BandMesh {
    let mut vertices = Vec::new();
    let mut index_of: BTreeMap<(u32, u32, u32), u32> = BTreeMap::new();
    let mut push = |p: [f32; 3]| -> u32 {
        let key = (p[0].to_bits(), p[1].to_bits(), p[2].to_bits());
        if let Some(&i) = index_of.get(&key) {
            return i;
        }
        let i = vertices.len() as u32;
        let ring_match = ring.iter().any(|r| {
            r[0].to_bits() == p[0].to_bits()
                && r[1].to_bits() == p[1].to_bits()
                && r[2].to_bits() == p[2].to_bits()
        });
        vertices.push(BandVertex {
            position: p,
            lod0: p,
            ring: ring_match,
        });
        index_of.insert(key, i);
        i
    };
    // Keep every authored and ring position even when a degenerate strip drops one.
    for p in authored.iter().chain(ring.iter()) {
        push(*p);
    }
    let mut triangles = Vec::with_capacity(strip.len());
    for tri in strip {
        if tri_span(tri) <= 1.0e-16 {
            continue;
        }
        triangles.push([push(tri[0]), push(tri[1]), push(tri[2])]);
    }
    BandMesh { vertices, triangles }
}

/// Vertical skirt under each ring edge, facing out of the hole (right of a CCW ring).
fn add_skirt(band: &mut BandMesh, ring: &[[f32; 3]], skirt_m: f32) {
    let drop = skirt_m.abs();
    let n = ring.len();
    let mut bottom = Vec::with_capacity(n);
    for p in ring {
        let q = [p[0], p[1] - drop, p[2]];
        let i = band.vertices.len() as u32;
        band.vertices.push(BandVertex {
            position: q,
            lod0: q,
            ring: false,
        });
        bottom.push(i);
    }
    let ring_index = |p: [f32; 3]| -> u32 {
        band.vertices
            .iter()
            .position(|v| {
                v.position[0].to_bits() == p[0].to_bits()
                    && v.position[1].to_bits() == p[1].to_bits()
                    && v.position[2].to_bits() == p[2].to_bits()
            })
            .unwrap_or(0) as u32
    };
    for j in 0..n {
        let b0 = ring_index(ring[j]);
        let b1 = ring_index(ring[(j + 1) % n]);
        let d1 = bottom[(j + 1) % n];
        let d0 = bottom[j];
        band.triangles.push([b0, b1, d1]);
        band.triangles.push([b0, d1, d0]);
    }
}

fn split_source(input: &ZipperInput<'_>) -> (Vec<CopiedTri>, Vec<CopiedTri>) {
    let mut throat = Vec::new();
    let mut body = Vec::new();
    for (t, tri) in input.indices.as_chunks::<3>().0.iter().enumerate() {
        let v = [
            input.positions[tri[0] as usize],
            input.positions[tri[1] as usize],
            input.positions[tri[2] as usize],
        ];
        let c = centroid(&v);
        let ground = (input.ground_y)(c[0], c[2]);
        let depth = ground - c[1];
        let under = c[1] <= ground + 1.0e-4;
        let in_throat =
            (input.inside_cut)(c[0], c[2]) && under && depth < input.params.throat_m && depth >= -1.0e-3;
        let copied = CopiedTri {
            v,
            source: input.source_tris.get(t).copied().flatten(),
        };
        if in_throat {
            throat.push(copied);
        } else {
            body.push(copied);
        }
    }
    (throat, body)
}
