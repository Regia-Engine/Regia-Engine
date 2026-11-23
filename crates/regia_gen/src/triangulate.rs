// SPDX-License-Identifier: MPL-2.0
//! Ear-clipping triangulation of a simple polygon with holes.
//!
//! Method (public source: D. Eberly, "Triangulation by Ear Clipping", Geometric Tools, 2002;
//! the two-ears theorem of G. Meisters, 1975; the in-cone test of J. O'Rourke, "Computational
//! Geometry in C", 2nd ed., § 1.6): each hole is joined to the outer ring by a bridge from its
//! rightmost vertex to a mutually visible ring vertex, then ears are clipped from the single ring.
//! Decisions use exact-enough `f64` products of the `f32` inputs; the output is indices only, so
//! the positions a caller builds from them are the input bits.

use crate::{GenError, MAX_RING_POINTS};

type P = [f64; 2];

fn cross(o: P, a: P, b: P) -> f64 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

/// Twice the signed area of a ring (positive = counter-clockwise).
pub(crate) fn signed_area2(pts: &[[f32; 2]]) -> f64 {
    let n = pts.len();
    (0..n)
        .map(|i| {
            let a = pts[i].map(f64::from);
            let b = pts[(i + 1) % n].map(f64::from);
            a[0] * b[1] - b[0] * a[1]
        })
        .sum()
}

fn in_triangle(p: P, a: P, b: P, c: P) -> bool {
    cross(a, b, p) >= 0.0 && cross(b, c, p) >= 0.0 && cross(c, a, p) >= 0.0
}

/// Triangulates the polygon `outer` with `holes`, returning counter-clockwise triangles as indices
/// into the concatenation `outer ++ holes[0] ++ holes[1] ++ …`.
///
/// Either winding is accepted for every ring (rings are reoriented internally). Holes must lie
/// inside the outer ring and not touch each other; a degenerate input (collinear runs, touching
/// rings) still yields a triangulation that uses every boundary edge exactly once, possibly with
/// zero-area triangles. Errors: a ring with fewer than 3 points, more than
/// [`MAX_RING_POINTS`](crate::MAX_RING_POINTS) points in all rings together, a non-finite point,
/// or zero area.
pub fn triangulate(outer: &[[f32; 2]], holes: &[Vec<[f32; 2]>]) -> Result<Vec<[u32; 3]>, GenError> {
    // Holes merge into one ring, so the limit is on all points together.
    let total = outer.len() + holes.iter().map(Vec::len).sum::<usize>();
    if total > MAX_RING_POINTS {
        return Err(GenError::TooLarge {
            what: "profile points",
            value: total,
            limit: MAX_RING_POINTS,
        });
    }
    let mut pts: Vec<P> = Vec::new();
    let mut rings: Vec<Vec<u32>> = Vec::new();
    for (r, ring) in std::iter::once(outer)
        .chain(holes.iter().map(Vec::as_slice))
        .enumerate()
    {
        if ring.len() < 3 {
            return Err(GenError::InvalidInput(format!(
                "ring {r} has fewer than 3 points"
            )));
        }
        crate::finite("profile points", ring.as_flattened())?;
        let area = signed_area2(ring);
        if area == 0.0 {
            return Err(GenError::InvalidInput(format!("ring {r} has zero area")));
        }
        let base = pts.len() as u32;
        pts.extend(ring.iter().map(|p| p.map(f64::from)));
        let mut idx: Vec<u32> = (0..ring.len() as u32).map(|i| base + i).collect();
        // Outer counter-clockwise, holes clockwise.
        if (r == 0) != (area > 0.0) {
            idx.reverse();
        }
        rings.push(idx);
    }
    let mut ring = rings.remove(0);
    // Holes by rightmost x, descending (ties: lower ring index first), so each bridge sees only
    // holes already merged to its right.
    let mut order: Vec<(usize, u32)> = rings
        .iter()
        .enumerate()
        .map(|(h, r)| (h, rightmost(&pts, r)))
        .collect();
    order.sort_by(|a, b| {
        pts[b.1 as usize][0]
            .total_cmp(&pts[a.1 as usize][0])
            .then(a.0.cmp(&b.0))
    });
    for (h, m) in order {
        bridge(&pts, &mut ring, &rings[h], m);
    }
    Ok(clip_ears(&pts, ring))
}

fn rightmost(pts: &[P], ring: &[u32]) -> u32 {
    let mut best = ring[0];
    for &i in ring {
        let (p, b) = (pts[i as usize], pts[best as usize]);
        if p[0] > b[0] || (p[0] == b[0] && p[1] < b[1]) {
            best = i;
        }
    }
    best
}

/// O'Rourke's in-cone test: is the direction from ring vertex `cur` to `q` inside the interior
/// angle at `cur` (neighbors `prev`, `next`, counter-clockwise ring)?
fn in_cone(prev: P, cur: P, next: P, q: P) -> bool {
    if cross(cur, next, prev) >= 0.0 {
        // Convex vertex.
        cross(cur, q, prev) > 0.0 && cross(q, cur, next) > 0.0
    } else {
        !(cross(cur, q, next) >= 0.0 && cross(q, cur, prev) >= 0.0)
    }
}

/// Joins `hole` (clockwise, rightmost vertex `m`) into `ring` with a two-way bridge.
fn bridge(pts: &[P], ring: &mut Vec<u32>, hole: &[u32], m: u32) {
    let mp = pts[m as usize];
    let n = ring.len();
    // Nearest edge hit by the ray from M toward +x. A counter-clockwise ring's edges seen from
    // inside going right point upward.
    let mut best: Option<(f64, usize)> = None;
    for k in 0..n {
        let a = pts[ring[k] as usize];
        let b = pts[ring[(k + 1) % n] as usize];
        if !(a[1] <= mp[1] && mp[1] <= b[1] && a[1] < b[1]) {
            continue;
        }
        let x = a[0] + (mp[1] - a[1]) * (b[0] - a[0]) / (b[1] - a[1]);
        if x >= mp[0] && best.is_none_or(|(bx, _)| x < bx) {
            best = Some((x, k));
        }
    }
    let target = match best {
        None => 0,
        Some((x, k)) => {
            let ka = k;
            let kb = (k + 1) % n;
            let (a, b) = (pts[ring[ka] as usize], pts[ring[kb] as usize]);
            let i = [x, mp[1]];
            let mut pk = if a[0] > b[0] { ka } else { kb };
            if i == a {
                pk = ka;
            } else if i == b {
                pk = kb;
            } else {
                // A reflex vertex inside triangle (M, I, P) blocks P: take the one closest in angle
                // to the ray (ties: the nearer).
                let p = pts[ring[pk] as usize];
                let (t0, t1, t2) = if cross(mp, i, p) >= 0.0 {
                    (mp, i, p)
                } else {
                    (mp, p, i)
                };
                let mut best_r: Option<(f64, f64, usize)> = None;
                for r in 0..n {
                    if r == pk {
                        continue;
                    }
                    let q = pts[ring[r] as usize];
                    let prev = pts[ring[(r + n - 1) % n] as usize];
                    let next = pts[ring[(r + 1) % n] as usize];
                    let reflex = cross(prev, q, next) <= 0.0;
                    if !reflex || !in_triangle(q, t0, t1, t2) || q == mp {
                        continue;
                    }
                    let d = [q[0] - mp[0], q[1] - mp[1]];
                    let len = (d[0] * d[0] + d[1] * d[1]).sqrt();
                    let cos = d[0] / len;
                    if best_r.is_none_or(|(bc, bl, _)| cos > bc || (cos == bc && len < bl)) {
                        best_r = Some((cos, len, r));
                    }
                }
                if let Some((_, _, r)) = best_r {
                    pk = r;
                }
            }
            pk
        }
    };
    // A vertex index may appear twice after earlier bridges: use the copy whose cone sees M.
    let index = ring[target];
    let mut pk = target;
    for (r, &v) in ring.iter().enumerate() {
        if v == index {
            let prev = pts[ring[(r + n - 1) % n] as usize];
            let next = pts[ring[(r + 1) % n] as usize];
            if in_cone(prev, pts[v as usize], next, mp) {
                pk = r;
                break;
            }
        }
    }
    let start = hole.iter().position(|&v| v == m).unwrap_or(0);
    let mut insert: Vec<u32> = Vec::with_capacity(hole.len() + 2);
    insert.extend(hole[start..].iter().chain(&hole[..start]));
    insert.push(m);
    insert.push(ring[pk]);
    ring.splice(pk + 1..pk + 1, insert);
}

/// Clips ears from a counter-clockwise ring (indices may repeat at bridges).
fn clip_ears(pts: &[P], ring: Vec<u32>) -> Vec<[u32; 3]> {
    let n = ring.len();
    let mut out = Vec::with_capacity(n.saturating_sub(2));
    let mut prev: Vec<usize> = (0..n).map(|i| (i + n - 1) % n).collect();
    let mut next: Vec<usize> = (0..n).map(|i| (i + 1) % n).collect();
    let mut live = n;
    let mut cur = 0usize;
    let mut misses = 0usize;
    let p = |k: usize| pts[ring[k] as usize];
    while live > 3 {
        let (a, c) = (prev[cur], next[cur]);
        let ear = misses < live && is_ear(pts, &ring, &next, a, cur, c);
        let forced = misses >= live && (misses >= 2 * live || cross(p(a), p(cur), p(c)) >= 0.0);
        if ear || forced {
            out.push([ring[a], ring[cur], ring[c]]);
            next[a] = c;
            prev[c] = a;
            live -= 1;
            misses = 0;
            cur = c;
        } else {
            misses += 1;
            cur = c;
        }
    }
    let (a, c) = (prev[cur], next[cur]);
    out.push([ring[a], ring[cur], ring[c]]);
    out
}

fn is_ear(pts: &[P], ring: &[u32], next: &[usize], a: usize, b: usize, c: usize) -> bool {
    let (pa, pb, pc) = (
        pts[ring[a] as usize],
        pts[ring[b] as usize],
        pts[ring[c] as usize],
    );
    if cross(pa, pb, pc) <= 0.0 {
        return false;
    }
    let skip = [ring[a], ring[b], ring[c]];
    let mut k = next[c];
    while k != a {
        let v = ring[k];
        let q = pts[v as usize];
        if !skip.contains(&v) && q != pa && q != pb && q != pc && in_triangle(q, pa, pb, pc) {
            return false;
        }
        k = next[k];
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(pts: &[[f32; 2]], tris: &[[u32; 3]]) -> f64 {
        tris.iter()
            .map(|t| {
                let [a, b, c] = t.map(|i| pts[i as usize].map(f64::from));
                cross(a, b, c) / 2.0
            })
            .sum()
    }

    #[test]
    fn a_square_is_two_counter_clockwise_triangles_either_winding() {
        let sq = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let t = triangulate(&sq, &[]).expect("square");
        assert_eq!(t.len(), 2);
        assert_eq!(area(&sq, &t), 1.0);
        let mut cw = sq;
        cw.reverse();
        let t = triangulate(&cw, &[]).expect("clockwise square");
        assert_eq!(area(&cw, &t), 1.0);
    }

    #[test]
    fn a_concave_l_and_a_square_with_a_hole_cover_their_area() {
        let l = [
            [0.0, 0.0],
            [2.0, 0.0],
            [2.0, 1.0],
            [1.0, 1.0],
            [1.0, 2.0],
            [0.0, 2.0],
        ];
        let t = triangulate(&l, &[]).expect("L");
        assert_eq!(t.len(), 4);
        assert_eq!(area(&l, &t), 3.0);
        let outer = vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0]];
        let hole = vec![[1.0, 1.0], [3.0, 1.0], [3.0, 3.0], [1.0, 3.0]];
        let t = triangulate(&outer, std::slice::from_ref(&hole)).expect("frame");
        // n + 2h - 2 triangles for n total vertices and h holes.
        assert_eq!(t.len(), 8 + 2 - 2);
        let all: Vec<[f32; 2]> = outer.iter().chain(&hole).copied().collect();
        assert_eq!(area(&all, &t), 12.0);
        assert!(t.iter().all(|t| {
            let [a, b, c] = t.map(|i| all[i as usize].map(f64::from));
            cross(a, b, c) > 0.0
        }));
    }

    #[test]
    fn two_holes_side_by_side_bridge_through_each_other() {
        let outer = vec![[0.0, 0.0], [10.0, 0.0], [10.0, 4.0], [0.0, 4.0]];
        let h1 = vec![[1.0, 1.0], [3.0, 1.0], [3.0, 3.0], [1.0, 3.0]];
        let h2 = vec![[5.0, 1.0], [7.0, 1.5], [7.0, 3.0], [5.0, 3.0]];
        let t = triangulate(&outer, &[h1.clone(), h2.clone()]).expect("two holes");
        let all: Vec<[f32; 2]> = outer.iter().chain(&h1).chain(&h2).copied().collect();
        assert_eq!(t.len(), 12 + 4 - 2);
        let expect = 40.0 - 4.0 - (2.0 * 2.0 - 0.5 * 2.0 * 0.5);
        assert!((area(&all, &t) - expect).abs() < 1e-9, "{}", area(&all, &t));
    }

    #[test]
    fn the_point_limit_counts_every_ring_together() {
        let ngon = |n: usize, r: f32| -> Vec<[f32; 2]> {
            (0..n)
                .map(|i| {
                    let (s, c) = regia_core::math::det::sin_cos(std::f32::consts::TAU * i as f32 / n as f32);
                    [r * c, r * s]
                })
                .collect()
        };
        let outer = ngon(2048, 10.0);
        let hole = ngon(2049, 1.0);
        assert!(triangulate(&outer, &[]).is_ok());
        let err = triangulate(&outer, &[hole]).expect_err("2048 + 2049 points");
        assert!(matches!(err, GenError::TooLarge { value: 4097, .. }), "{err:?}");
    }

    #[test]
    fn bad_rings_are_errors() {
        assert!(triangulate(&[[0.0, 0.0], [1.0, 0.0]], &[]).is_err());
        assert!(triangulate(&[[0.0, 0.0], [1.0, 0.0], [2.0, 0.0]], &[]).is_err());
        assert!(triangulate(&[[0.0, 0.0], [f32::NAN, 0.0], [1.0, 1.0]], &[]).is_err());
    }
}
