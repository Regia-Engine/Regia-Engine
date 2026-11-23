// SPDX-License-Identifier: MPL-2.0
//! Boolean-lite: clipping a mesh by a plane (with a cap over the cut) and intersecting it with a
//! convex solid given as planes.
//!
//! Method: Sutherland–Hodgman polygon clipping per triangle (I. Sutherland and G. Hodgman,
//! "Reentrant Polygon Clipping", CACM 17(1), 1974). Each cut point is computed from its edge's
//! two positions in a canonical order, so the triangles on both sides of an edge get the same bits
//! and the result stays watertight. The cap chains the cut's open edges into loops, sorts them into
//! outer rings and holes by signed area, and ear-clips them ([`crate::triangulate`]).

use std::collections::{BTreeMap, HashMap};

use crate::build::{Builder, corners, cross, dot, face_normal, normalize};
use crate::triangulate::{signed_area2, triangulate};
use crate::{CpuMesh, GenError, check_vertices, finite, weld_key};

/// A plane `normal · p = offset`. A clip keeps the side `normal · p ≤ offset` (behind the plane).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Plane {
    /// The plane normal; any non-zero length (it is normalized).
    pub normal: [f32; 3],
    /// Signed distance of the plane from the origin along the normalized normal.
    pub offset: f32,
}

#[derive(Clone, Copy)]
struct Corner {
    p: [f32; 3],
    n: [f32; 3],
    uv: [f32; 2],
    /// Source vertex (`u32::MAX` for a cut point).
    src: u32,
    /// The cut's edge key for a cut point.
    edge: (u32, u32),
}

/// Keeps the part of `mesh` behind `plane` (`normal · p ≤ offset`). With `cap`, the cut is closed
/// with flat triangles facing along the plane normal (UVs: the cut point's coordinates in the
/// plane), so a watertight input stays watertight. Errors: a zero or non-finite normal; with
/// `cap`, a cut that does not close into loops ([`GenError::CapNotClosed`]).
pub fn clip(mesh: &CpuMesh, plane: Plane, cap: bool) -> Result<CpuMesh, GenError> {
    let _span = tracing::trace_span!("regia_gen::clip").entered();
    finite(
        "plane",
        &[plane.normal[0], plane.normal[1], plane.normal[2], plane.offset],
    )?;
    let normal = normalize(plane.normal);
    if normal == [0.0; 3] {
        return Err(GenError::InvalidInput("the plane normal is zero".into()));
    }
    let dist = |p: [f32; 3]| dot(normal, p) - plane.offset;
    let mut b = Builder::default();
    let mut kept: HashMap<u32, u32> = HashMap::new();
    let mut cuts: HashMap<(u32, u32), u32> = HashMap::new();
    let attr = |i: u32| Corner {
        p: mesh.positions[i as usize],
        n: mesh.normals.get(i as usize).copied().unwrap_or([0.0; 3]),
        uv: mesh.uvs.get(i as usize).copied().unwrap_or([0.0; 2]),
        src: i,
        edge: (0, 0),
    };
    for t in mesh.indices.as_chunks::<3>().0 {
        if corners(&mesh.positions, t).is_none() {
            return Err(GenError::InvalidInput("an index is out of range".into()));
        }
        let c = t.map(attr);
        let d = c.map(|c| dist(c.p));
        let inside = d.map(|d| d <= 0.0);
        if inside.iter().all(|&i| !i) {
            continue;
        }
        // A face lying in the plane bounds the kept side only when it faces along the normal; one
        // facing back has the solid on the removed side (clipping a cube exactly at its bottom face).
        if d == [0.0; 3] && dot(face_normal(c[0].p, c[1].p, c[2].p), normal) < 0.0 {
            continue;
        }
        let mut poly: Vec<Corner> = Vec::with_capacity(4);
        for k in 0..3 {
            let k1 = (k + 1) % 3;
            if inside[k] {
                poly.push(c[k]);
            }
            if inside[k] != inside[k1] {
                poly.push(cut_point(c[k], c[k1], &dist));
            }
        }
        let ids: Vec<u32> = poly
            .iter()
            .map(|v| {
                if v.src != u32::MAX {
                    *kept.entry(v.src).or_insert_with(|| b.vertex(v.p, v.n, v.uv))
                } else {
                    *cuts.entry(v.edge).or_insert_with(|| b.vertex(v.p, v.n, v.uv))
                }
            })
            .collect();
        for k in 1..ids.len() - 1 {
            b.tri(ids[0], ids[k], ids[k + 1]);
        }
    }
    let mut out = b.finish();
    check_vertices(out.positions.len())?;
    if cap {
        add_cap(&mut out, normal, plane.offset)?;
    }
    Ok(out)
}

/// The point where edge `a b` crosses the plane, with attributes interpolated. Computed from the
/// endpoints in position-key order so both triangles sharing the edge get the same bits.
fn cut_point(a: Corner, b: Corner, dist: &impl Fn([f32; 3]) -> f32) -> Corner {
    let (a, b) = if weld_key(a.p) <= weld_key(b.p) {
        (a, b)
    } else {
        (b, a)
    };
    let (da, db) = (dist(a.p), dist(b.p));
    let t = da / (da - db);
    let lerp = |x: f32, y: f32| x + (y - x) * t;
    let p = std::array::from_fn(|k| lerp(a.p[k], b.p[k]));
    let n = normalize(std::array::from_fn(|k| lerp(a.n[k], b.n[k])));
    let uv = std::array::from_fn(|k| lerp(a.uv[k], b.uv[k]));
    let edge = (a.src.min(b.src), a.src.max(b.src));
    Corner {
        p,
        n,
        uv,
        src: u32::MAX,
        edge,
    }
}

/// Closes the open edges lying in the plane with flat triangles facing `normal`.
fn add_cap(mesh: &mut CpuMesh, normal: [f32; 3], offset: f32) -> Result<(), GenError> {
    // Weld positions in first-seen order.
    let mut ids: HashMap<[u32; 3], u32> = HashMap::new();
    let mut points: Vec<[f32; 3]> = Vec::new();
    let welded: Vec<u32> = mesh
        .positions
        .iter()
        .map(|p| {
            *ids.entry(weld_key(*p)).or_insert_with(|| {
                points.push(*p);
                (points.len() - 1) as u32
            })
        })
        .collect();
    let mut directed: BTreeMap<(u32, u32), i32> = BTreeMap::new();
    for t in mesh.indices.as_chunks::<3>().0 {
        for k in 0..3 {
            let (a, b) = (welded[t[k] as usize], welded[t[(k + 1) % 3] as usize]);
            *directed.entry((a, b)).or_insert(0) += 1;
        }
    }
    // Tolerance for "on the plane": cut points are within rounding of it.
    let scale = points
        .iter()
        .flatten()
        .fold(offset.abs(), |m, v| m.max(v.abs()))
        .max(1.0);
    let eps = scale * 1e-5;
    let on_plane = |i: u32| (dot(normal, points[i as usize]) - offset).abs() <= eps;
    // Open edges of the kept part in the plane, as cap edges (reversed: the cap runs the other way).
    let mut out_edges: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (&(a, b), &count) in &directed {
        let back = directed.get(&(b, a)).copied().unwrap_or(0);
        if count > back && on_plane(a) && on_plane(b) {
            for _ in 0..(count - back) {
                out_edges.entry(b).or_default().push(a);
            }
        }
    }
    for nexts in out_edges.values_mut() {
        nexts.sort_unstable();
        nexts.reverse();
    }
    // Chain loops, smallest start vertex first, smallest next vertex at forks.
    let mut loops: Vec<Vec<u32>> = Vec::new();
    while let Some((&start, _)) = out_edges.iter().find(|(_, v)| !v.is_empty()) {
        let mut ring = vec![start];
        let mut cur = start;
        let mut closed = false;
        while let Some(next) = out_edges.get_mut(&cur).and_then(Vec::pop) {
            if next == start {
                closed = true;
                break;
            }
            ring.push(next);
            cur = next;
        }
        if !closed {
            let left: usize = out_edges.values().map(Vec::len).sum();
            return Err(GenError::CapNotClosed {
                open_edges: left + ring.len() - 1,
            });
        }
        if ring.len() >= 3 {
            loops.push(ring);
        }
    }
    if loops.is_empty() {
        return Ok(());
    }
    // Plane basis with u × v = normal, so counter-clockwise in (u, v) faces along the normal.
    let helper = if normal[0].abs() < 0.9 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    let u = normalize(cross(helper, normal));
    let v = cross(normal, u);
    let flat: Vec<Vec<[f32; 2]>> = loops
        .iter()
        .map(|l| {
            l.iter()
                .map(|&i| {
                    let p = points[i as usize];
                    [dot(p, u), dot(p, v)]
                })
                .collect()
        })
        .collect();
    let areas: Vec<f64> = flat.iter().map(|l| signed_area2(l)).collect();
    // Each hole (negative area) goes to the smallest outer ring containing its first point.
    let mut holes_of: Vec<Vec<usize>> = vec![Vec::new(); loops.len()];
    for h in (0..loops.len()).filter(|&h| areas[h] < 0.0) {
        let q = flat[h][0];
        let owner = (0..loops.len())
            .filter(|&o| areas[o] > 0.0 && contains(&flat[o], q))
            .min_by(|&a, &b| areas[a].total_cmp(&areas[b]).then(a.cmp(&b)));
        match owner {
            Some(o) => holes_of[o].push(h),
            None => {
                return Err(GenError::CapNotClosed {
                    open_edges: loops[h].len(),
                });
            }
        }
    }
    let mut b = Builder {
        mesh: std::mem::take(mesh),
    };
    for o in (0..loops.len()).filter(|&o| areas[o] > 0.0) {
        let holes: Vec<Vec<[f32; 2]>> = holes_of[o].iter().map(|&h| flat[h].clone()).collect();
        let tris = triangulate(&flat[o], &holes)?;
        let ring_ids: Vec<u32> = std::iter::once(o)
            .chain(holes_of[o].iter().copied())
            .flat_map(|l| loops[l].iter().copied())
            .collect();
        let base = b.mesh.positions.len() as u32;
        for &i in &ring_ids {
            let p = points[i as usize];
            b.vertex(p, normal, [dot(p, u), dot(p, v)]);
        }
        for [i0, i1, i2] in tris {
            b.tri(base + i0, base + i1, base + i2);
        }
    }
    *mesh = b.finish();
    check_vertices(mesh.positions.len())
}

/// Even-odd point-in-polygon test (W. R. Franklin's crossing test).
fn contains(ring: &[[f32; 2]], q: [f32; 2]) -> bool {
    let mut inside = false;
    let n = ring.len();
    for k in 0..n {
        let (a, b) = (ring[k], ring[(k + 1) % n]);
        if (a[1] > q[1]) != (b[1] > q[1]) {
            let x = a[0] + (q[1] - a[1]) * (b[0] - a[0]) / (b[1] - a[1]);
            if q[0] < x {
                inside = !inside;
            }
        }
    }
    inside
}

/// Keeps the part of `mesh` inside every plane (the convex solid `⋂ normal·p ≤ offset`), capping
/// each cut. This is the intersection of `mesh` with that convex solid.
pub fn intersect_convex(mesh: &CpuMesh, planes: &[Plane]) -> Result<CpuMesh, GenError> {
    let mut out = mesh.clone();
    for plane in planes {
        out = clip(&out, *plane, true)?;
    }
    Ok(out)
}
