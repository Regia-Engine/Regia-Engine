// SPDX-License-Identifier: MPL-2.0
//! Cook-time zipper from a mouth ring to the authored boundary (spec 26 § 2a.4 step 3, ADR-8291).
//!
//! W-10's collar is the cut lattice triangle on each ring edge. An f32 step inside that edge can
//! land in a cut cell the collar does not cover, and support then answers a floor under the
//! ground. This module builds the strip between the ring and the authored boundary when that
//! boundary lies within [`MAX_STITCH_GAP_M`]. A loop whose centroid falls through is a hole:
//! the cook fans it from the millimetre centroid. A side lip, whose centroid sits on a floor,
//! is not written. A stitch or cap triangle whose interior fights an authored floor is dropped
//! (ADR-8334). The ring edge stays one canonical edge of the filtered collar. The render skirt
//! is not cooked.

use std::collections::BTreeMap;

use regia_detsim::float::{det_clamp_f64, det_max_f64, det_min_f64};
use regia_detsim::math::sqrt_f64;

use crate::cook::WorldTriangleMm;
use crate::cooked::{TRI_FLAG_NO_SUPPORT, TriKind};
use crate::mm_to_m;

/// A mouth whose bridging boundary is farther than this keeps W-10's collar.
///
/// The side lip sits about 2 m out (`LIP_W_M` is 3 m). On these fixtures the entrance the
/// walker crosses is a boundary about 10 m from the ring, so 8 m only reached the side lip.
/// 16 m still rejects a loop at the other end of the tunnel.
pub(crate) const MAX_STITCH_GAP_M: f64 = 16.0;

#[derive(Clone, Copy)]
struct Pt {
    mm: [i32; 3],
    p: [f64; 3],
}

/// One band per mouth. `None` means that mouth keeps the collar.
pub(crate) fn zip_layer(
    mesh: &[WorldTriangleMm],
    mouths: &[Vec<Vec<[i32; 3]>>],
) -> Vec<Option<Vec<WorldTriangleMm>>> {
    let loops = boundary_loops(mesh);
    let mut pairs: Vec<(f64, usize, usize, usize)> = Vec::new();
    for (mi, rings) in mouths.iter().enumerate() {
        for (ri, ring) in rings.iter().enumerate() {
            if ring.len() < 3 {
                continue;
            }
            let ring_y = ring_height(ring);
            for (li, loop_pts) in loops.iter().enumerate() {
                // The lip sits on the ring. A loop that only touches the ring and otherwise
                // runs along a lower floor is the tunnel, not the lip; stitching it replaces
                // the collar with a chord that does not contain the ring.
                let near = loop_pts.iter().filter(|p| (p.p[1] - ring_y).abs() <= 1.0).count();
                if near * 2 < loop_pts.len() {
                    continue;
                }
                pairs.push((loop_gap(loop_pts, ring), mi, ri, li));
            }
        }
    }
    pairs.sort_by(|a, b| {
        a.0.total_cmp(&b.0)
            .then(a.1.cmp(&b.1))
            .then(a.2.cmp(&b.2))
            .then(a.3.cmp(&b.3))
    });
    let mut owner: Vec<Vec<Option<usize>>> = mouths.iter().map(|rings| vec![None; rings.len()]).collect();
    let mut hole: Vec<Vec<bool>> = mouths.iter().map(|rings| vec![false; rings.len()]).collect();
    let mut used = vec![false; loops.len()];
    for (gap, mi, ri, li) in &pairs {
        if *gap > MAX_STITCH_GAP_M {
            break;
        }
        if used[*li] || owner[*mi][*ri].is_some() {
            continue;
        }
        // A hole loop has its centroid over the tunnel. A side lip's centroid sits on the
        // ramp, and stitching that lip does not cover the entrance.
        if !centroid_falls(mesh, &loops[*li], &mouths[*mi][*ri]) {
            continue;
        }
        used[*li] = true;
        owner[*mi][*ri] = Some(*li);
        hole[*mi][*ri] = true;
    }
    // Mouths with no hole keep the nearest lip inside 8 m. That is the island case, and
    // the square unit test.
    for (gap, mi, ri, li) in &pairs {
        if *gap > 8.0 {
            break;
        }
        if used[*li] || owner[*mi][*ri].is_some() {
            continue;
        }
        used[*li] = true;
        owner[*mi][*ri] = Some(*li);
    }
    mouths
        .iter()
        .enumerate()
        .map(|(mi, rings)| {
            if rings.is_empty() || owner[mi].iter().any(Option::is_none) {
                return None;
            }
            let mut band = Vec::new();
            for (ri, ring) in rings.iter().enumerate() {
                let li = owner[mi][ri]?;
                let strip = if hole[mi][ri] {
                    cap_loop(&loops[li])
                } else {
                    stitch_ring(&loops[li], ring)?
                };
                if strip.is_empty() {
                    return None;
                }
                band.extend(strip);
            }
            Some(band)
        })
        .collect()
}

fn ring_height(ring: &[[i32; 3]]) -> f64 {
    let n = if ring.is_empty() { 1.0 } else { ring.len() as f64 };
    ring.iter().fold(0.0, |s, p| s + f64::from(mm_to_m(0, p[1]))) / n
}

fn mm_point(mm: [i32; 3]) -> Pt {
    Pt {
        mm,
        p: [
            f64::from(mm_to_m(0, mm[0])),
            f64::from(mm_to_m(0, mm[1])),
            f64::from(mm_to_m(0, mm[2])),
        ],
    }
}

fn boundary_loops(mesh: &[WorldTriangleMm]) -> Vec<Vec<Pt>> {
    let mut count: BTreeMap<([i32; 3], [i32; 3]), u32> = BTreeMap::new();
    // Floor edges only. A mouth cross-section also has wall and ceiling edges; stitching those
    // to the ground ring builds a ramp up the wall.
    for tri in mesh.iter().filter(|tri| tri.kind == TriKind::Floor) {
        for e in 0..3 {
            let (a, b) = (tri.v[e], tri.v[(e + 1) % 3]);
            let key = if a <= b { (a, b) } else { (b, a) };
            *count.entry(key).or_default() += 1;
        }
    }
    let mut next: BTreeMap<[i32; 3], [i32; 3]> = BTreeMap::new();
    for tri in mesh.iter().filter(|tri| tri.kind == TriKind::Floor) {
        for e in 0..3 {
            let (a, b) = (tri.v[e], tri.v[(e + 1) % 3]);
            let key = if a <= b { (a, b) } else { (b, a) };
            if count.get(&key).copied().unwrap_or(0) == 1 {
                next.entry(a).or_insert(b);
            }
        }
    }
    let mut seen: BTreeMap<[i32; 3], ()> = BTreeMap::new();
    let starts: Vec<[i32; 3]> = next.keys().copied().collect();
    let mut loops = Vec::new();
    for start in starts {
        if seen.contains_key(&start) {
            continue;
        }
        let mut cur = start;
        let mut loop_mm = Vec::new();
        loop {
            if seen.insert(cur, ()).is_some() {
                break;
            }
            loop_mm.push(cur);
            match next.get(&cur).copied() {
                Some(n) if n != start => cur = n,
                _ => break,
            }
            if loop_mm.len() > next.len() {
                break;
            }
        }
        if loop_mm.len() >= 3 {
            loops.push(loop_mm.into_iter().map(mm_point).collect());
        }
    }
    loops
}

fn loop_gap(loop_pts: &[Pt], ring: &[[i32; 3]]) -> f64 {
    let ring_p: Vec<Pt> = ring.iter().copied().map(mm_point).collect();
    let mut worst = 0.0;
    for p in loop_pts {
        let mut best = f64::MAX;
        for e in 0..ring_p.len() {
            best = det_min_f64(
                best,
                dist_to_seg(p.p, ring_p[e].p, ring_p[(e + 1) % ring_p.len()].p),
            );
        }
        worst = det_max_f64(worst, best);
    }
    worst
}

fn dist_to_seg(p: [f64; 3], a: [f64; 3], b: [f64; 3]) -> f64 {
    let (abx, abz) = (b[0] - a[0], b[2] - a[2]);
    let (apx, apz) = (p[0] - a[0], p[2] - a[2]);
    let denom = abx * abx + abz * abz;
    let t = if denom <= 1.0e-20 {
        0.0
    } else {
        det_clamp_f64((apx * abx + apz * abz) / denom, 0.0, 1.0)
    };
    let (dx, dz) = (apx - abx * t, apz - abz * t);
    sqrt_f64(dx * dx + dz * dz)
}

/// `true` when the loop's centroid has no floor in the pin band.
fn centroid_falls(mesh: &[WorldTriangleMm], loop_pts: &[Pt], ring: &[[i32; 3]]) -> bool {
    let ring_y = ring_height(ring);
    let (mut sx, mut sz) = (0.0, 0.0);
    for p in loop_pts {
        sx += p.p[0];
        sz += p.p[2];
    }
    let n = if loop_pts.is_empty() {
        1.0
    } else {
        loop_pts.len() as f64
    };
    falls_through(mesh, sx / n, sz / n, ring_y)
}

/// No floor contains `(x, z)` at or above 8 m below `ring_y`.
fn falls_through(mesh: &[WorldTriangleMm], x: f64, z: f64, ring_y: f64) -> bool {
    for tri in mesh {
        if tri.kind != TriKind::Floor || tri.flags & TRI_FLAG_NO_SUPPORT != 0 {
            continue;
        }
        let Some(h) = plane_y_m(tri, x, z) else {
            continue;
        };
        if h >= ring_y - 8.0 {
            return false;
        }
    }
    true
}

fn plane_y_m(tri: &WorldTriangleMm, x: f64, z: f64) -> Option<f64> {
    let p = tri.v.map(|q| {
        [
            f64::from(mm_to_m(0, q[0])),
            f64::from(mm_to_m(0, q[1])),
            f64::from(mm_to_m(0, q[2])),
        ]
    });
    let (ax, az) = (p[0][0], p[0][2]);
    let (bx, bz) = (p[1][0], p[1][2]);
    let (cx, cz) = (p[2][0], p[2][2]);
    let det = (bx - ax) * (cz - az) - (cx - ax) * (bz - az);
    if det.abs() <= 1.0e-12 {
        return None;
    }
    let l1 = ((x - ax) * (cz - az) - (cx - ax) * (z - az)) / det;
    let l2 = ((bx - ax) * (z - az) - (x - ax) * (bz - az)) / det;
    let l0 = 1.0 - l1 - l2;
    (l0 >= 0.0 && l1 >= 0.0 && l2 >= 0.0).then(|| l0 * p[0][1] + l1 * p[1][1] + l2 * p[2][1])
}

/// Fan the loop from its millimetre centroid so the hole's interior is a floor.
fn cap_loop(loop_pts: &[Pt]) -> Vec<WorldTriangleMm> {
    if loop_pts.len() < 3 {
        return Vec::new();
    }
    let mut pts = loop_pts.to_vec();
    orient_ccw(&mut pts);
    let pts = subdivide_loop(&pts, 4.0);
    let (mut x, mut y, mut z) = (0.0, 0.0, 0.0);
    for p in &pts {
        x += f64::from(p.mm[0]);
        y += f64::from(p.mm[1]);
        z += f64::from(p.mm[2]);
    }
    let n = pts.len() as f64;
    let center = mm_point([
        (x / n).round_ties_even() as i32,
        (y / n).round_ties_even() as i32,
        (z / n).round_ties_even() as i32,
    ]);
    let mut out = Vec::new();
    for i in 0..pts.len() {
        let a = pts[i];
        let b = pts[(i + 1) % pts.len()];
        if a.mm == b.mm || a.mm == center.mm || b.mm == center.mm {
            continue;
        }
        let mut tri = [a, b, center];
        if cross_y(&tri) < 0.0 {
            tri.swap(1, 2);
        }
        if tri_span(&tri) <= 1.0e-8 {
            continue;
        }
        out.push(WorldTriangleMm {
            v: [tri[0].mm, tri[1].mm, tri[2].mm],
            kind: TriKind::Floor,
            flags: 0,
        });
    }
    out
}

/// Insert points so no edge is longer than `max_m` in XZ.
fn subdivide_loop(loop_pts: &[Pt], max_m: f64) -> Vec<Pt> {
    let mut out = Vec::with_capacity(loop_pts.len());
    for i in 0..loop_pts.len() {
        let a = &loop_pts[i];
        let b = &loop_pts[(i + 1) % loop_pts.len()];
        out.push(*a);
        let d = xz_dist(a.p, b.p);
        if d <= max_m {
            continue;
        }
        let segments = det_max_f64((d / max_m).ceil(), 2.0) as usize;
        for s in 1..segments {
            let t = s as f64 / segments as f64;
            let mm = [0, 1, 2].map(|k| {
                let v = f64::from(a.mm[k]) + (f64::from(b.mm[k]) - f64::from(a.mm[k])) * t;
                v.round_ties_even() as i32
            });
            out.push(mm_point(mm));
        }
    }
    out
}

fn xz_dist(a: [f64; 3], b: [f64; 3]) -> f64 {
    let (dx, dz) = (a[0] - b[0], a[2] - b[2]);
    sqrt_f64(dx * dx + dz * dz)
}

fn orient_ccw(loop_pts: &mut [Pt]) {
    let mut area = 0.0;
    for i in 0..loop_pts.len() {
        let p = loop_pts[i].p;
        let q = loop_pts[(i + 1) % loop_pts.len()].p;
        area += p[0] * q[2] - q[0] * p[2];
    }
    if area < 0.0 {
        loop_pts.reverse();
    }
}

fn rotate_to_closest(authored: &mut [Pt], ring: &mut [Pt]) {
    let mut best = (f64::MAX, 0usize, 0usize);
    for (i, a) in authored.iter().enumerate() {
        for (j, b) in ring.iter().enumerate() {
            let d = xz_dist(a.p, b.p);
            if d < best.0 {
                best = (d, i, j);
            }
        }
    }
    authored.rotate_left(best.1);
    ring.rotate_left(best.2);
}

fn stitch_ring(authored_loop: &[Pt], ring_mm: &[[i32; 3]]) -> Option<Vec<WorldTriangleMm>> {
    let mut ring: Vec<Pt> = ring_mm.iter().copied().map(mm_point).collect();
    if authored_loop.len() < 3 || ring.len() < 3 {
        return None;
    }
    // A 40 m entrance edge becomes triangles the 16 m span filter would drop, which reopens
    // the hole. Split the authored boundary so each stitch edge is at most 4 m.
    let mut authored = subdivide_loop(authored_loop, 4.0);
    orient_ccw(&mut authored);
    orient_ccw(&mut ring);
    rotate_to_closest(&mut authored, &mut ring);
    let mut strip = stitch(&authored, &ring);
    // A sliver or a long chord shares a ring edge without holding a floor there, and its
    // bounds fill the query index. Drop it; the collar keeps that edge.
    strip.retain(|tri| {
        tri_span(tri) > 1.0e-8 && max_axis_span_m(tri) <= 16.0 && !covers_foreign_ring_xz(tri, &ring)
    });
    if strip.is_empty() {
        return None;
    }
    // Each remaining triangle faces up on its own. A quantized lip is not one winding.
    const EPS: f64 = 1.0e-8;
    for tri in &mut strip {
        if tri_span(tri) > EPS && cross_y(tri) < -EPS {
            tri.swap(1, 2);
        }
    }
    let mut out = Vec::with_capacity(strip.len());
    for tri in strip {
        out.push(WorldTriangleMm {
            v: [tri[0].mm, tri[1].mm, tri[2].mm],
            kind: TriKind::Floor,
            flags: 0,
        });
    }
    Some(out)
}

fn stitch(authored: &[Pt], ring: &[Pt]) -> Vec<[Pt; 3]> {
    let arc_a = arc_params(authored);
    let arc_b = arc_params(ring);
    let (na, nb) = (authored.len(), ring.len());
    let mut ia = 0usize;
    let mut ib = 0usize;
    let mut tris = Vec::with_capacity(na + nb);
    while ia < na || ib < nb {
        let (can_a, can_b) = (ia < na, ib < nb);
        let tri_a = [authored[ia % na], ring[ib % nb], authored[(ia + 1) % na]];
        let tri_b = [ring[ib % nb], ring[(ib + 1) % nb], authored[ia % na]];
        // A triangle that covers some other ring vertex shortcuts the ring. The plane then sits
        // above the pin at the ring samples that triangle swallows, and support picks it.
        let advance_a = if can_a && can_b {
            match (
                covers_foreign_ring_xz(&tri_a, ring),
                covers_foreign_ring_xz(&tri_b, ring),
            ) {
                (false, true) => true,
                (true, false) => false,
                _ => match (faces_down_or_flat(&tri_a), faces_down_or_flat(&tri_b)) {
                    (true, false) => true,
                    (false, true) => false,
                    _ => {
                        let da = xz_dist(authored[(ia + 1) % na].p, ring[ib % nb].p);
                        let db = xz_dist(ring[(ib + 1) % nb].p, authored[ia % na].p);
                        if (da - db).abs() <= 1.0e-5 {
                            arc_a[ia + 1] <= arc_b[ib + 1]
                        } else {
                            da < db
                        }
                    }
                },
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

fn arc_params(loop_pts: &[Pt]) -> Vec<f64> {
    let n = loop_pts.len();
    let mut cum = vec![0.0; n + 1];
    for i in 0..n {
        cum[i + 1] = cum[i] + xz_dist(loop_pts[i].p, loop_pts[(i + 1) % n].p);
    }
    let total = det_max_f64(cum[n], 1.0e-12);
    for c in &mut cum {
        *c /= total;
    }
    cum
}

fn faces_down_or_flat(tri: &[Pt; 3]) -> bool {
    tri_span(tri) <= 1.0e-8 || cross_y(tri) <= 1.0e-8
}

fn cross_y(tri: &[Pt; 3]) -> f64 {
    let (u0, u2) = (tri[1].p[0] - tri[0].p[0], tri[1].p[2] - tri[0].p[2]);
    let (v0, v2) = (tri[2].p[0] - tri[0].p[0], tri[2].p[2] - tri[0].p[2]);
    u2 * v0 - u0 * v2
}

fn cross(tri: &[Pt; 3]) -> [f64; 3] {
    let u = [
        tri[1].p[0] - tri[0].p[0],
        tri[1].p[1] - tri[0].p[1],
        tri[1].p[2] - tri[0].p[2],
    ];
    let v = [
        tri[2].p[0] - tri[0].p[0],
        tri[2].p[1] - tri[0].p[1],
        tri[2].p[2] - tri[0].p[2],
    ];
    [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ]
}

fn tri_span(tri: &[Pt; 3]) -> f64 {
    let n = cross(tri);
    n[0] * n[0] + n[1] * n[1] + n[2] * n[2]
}

/// `true` when `tri` covers the XZ of a ring vertex that is not one of its own vertices.
///
/// The stitch sometimes joins a long authored edge to one ring vertex. That triangle swallows
/// other ring vertices, and its plane is above the pin there. Those triangles are dropped; the
/// collar keeps any ring edge the strip no longer carries.
fn max_axis_span_m(tri: &[Pt; 3]) -> f64 {
    let (mut min_x, mut max_x) = (tri[0].p[0], tri[0].p[0]);
    let (mut min_z, mut max_z) = (tri[0].p[2], tri[0].p[2]);
    for p in &tri[1..] {
        min_x = det_min_f64(min_x, p.p[0]);
        max_x = det_max_f64(max_x, p.p[0]);
        min_z = det_min_f64(min_z, p.p[2]);
        max_z = det_max_f64(max_z, p.p[2]);
    }
    det_max_f64(max_x - min_x, max_z - min_z)
}

fn covers_foreign_ring_xz(tri: &[Pt; 3], ring: &[Pt]) -> bool {
    let verts = [tri[0].mm, tri[1].mm, tri[2].mm];
    ring.iter()
        .any(|p| !verts.contains(&p.mm) && xz_in_closed_triangle(p.mm, verts))
}

fn xz_in_closed_triangle(p: [i32; 3], tri: [[i32; 3]; 3]) -> bool {
    let orient = |a: [i32; 3], b: [i32; 3], c: [i32; 3]| -> i64 {
        (i64::from(b[0]) - i64::from(a[0])) * (i64::from(c[2]) - i64::from(a[2]))
            - (i64::from(c[0]) - i64::from(a[0])) * (i64::from(b[2]) - i64::from(a[2]))
    };
    let area = orient(tri[0], tri[1], tri[2]);
    if area == 0 {
        return false;
    }
    let c0 = orient(tri[1], tri[2], p);
    let c1 = orient(tri[2], tri[0], p);
    let c2 = orient(tri[0], tri[1], p);
    if area > 0 {
        c0 >= 0 && c1 >= 0 && c2 >= 0
    } else {
        c0 <= 0 && c1 <= 0 && c2 <= 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_square_covers_each_ring_edge_once() {
        let inner = [[1000, 0, 1000], [3000, 0, 1000], [3000, 0, 3000], [1000, 0, 3000]];
        let mesh = vec![
            WorldTriangleMm {
                v: [inner[0], inner[1], inner[2]],
                kind: TriKind::Floor,
                flags: 0,
            },
            WorldTriangleMm {
                v: [inner[0], inner[2], inner[3]],
                kind: TriKind::Floor,
                flags: 0,
            },
        ];
        let ring = vec![[0, 0, 0], [4000, 0, 0], [4000, 0, 4000], [0, 0, 4000]];
        let Some(Some(band)) = zip_layer(&mesh, &[vec![ring.clone()]]).pop() else {
            panic!("the inner square is inside 8 m");
        };
        let mut edges: BTreeMap<([i32; 3], [i32; 3]), u32> = BTreeMap::new();
        for tri in &band {
            for e in 0..3 {
                let (a, b) = (tri.v[e], tri.v[(e + 1) % 3]);
                let key = if a <= b { (a, b) } else { (b, a) };
                *edges.entry(key).or_default() += 1;
            }
        }
        for i in 0..ring.len() {
            let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
            let key = if a <= b { (a, b) } else { (b, a) };
            assert_eq!(edges.get(&key), Some(&1), "ring edge {a:?}-{b:?}");
        }
    }
}
