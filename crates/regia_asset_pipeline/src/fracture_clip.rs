// SPDX-License-Identifier: MPL-2.0
//! Half-space clipping and planar capping of closed triangle meshes (private to the fracture tool,
//! `fracture.rs`).
//!
//! Clipping is Sutherland-Hodgman per triangle (Sutherland and Hodgman, "Reentrant polygon
//! clipping", CACM 17(1), 1974). Intersection points on an edge are computed from the edge's
//! endpoints in a canonical order, so the two triangles that share the edge produce bit-identical
//! points and the clipped surface stays watertight. The cut is closed with the directed-edge
//! boundary of the clipped triangle set (the edges whose reverse is missing, Botsch et al.,
//! *Polygon Mesh Processing*, 2010, § 1.3), traced into loops and triangulated by ear clipping
//! (Meisters, "Polygons have ears", Amer. Math. Monthly 82, 1975), with holes joined to the outer
//! loop by a bridge edge (D. Eberly, "Triangulation by Ear Clipping", Geometric Tools, 2002).
//! All arithmetic is `f64`.

use std::collections::HashMap;

/// A point or vector.
pub(crate) type P = [f64; 3];

/// Tag of a triangle that belongs to the source surface (not to a cap).
pub(crate) const SURFACE: u32 = u32::MAX;

pub(crate) fn sub(a: P, b: P) -> P {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub(crate) fn add(a: P, b: P) -> P {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

pub(crate) fn scale(a: P, s: f64) -> P {
    [a[0] * s, a[1] * s, a[2] * s]
}

pub(crate) fn dot(a: P, b: P) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub(crate) fn cross(a: P, b: P) -> P {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

pub(crate) fn length(a: P) -> f64 {
    dot(a, a).sqrt()
}

/// One triangle: counter-clockwise seen from outside, plus its tag (`SURFACE` or the site index
/// whose bisector plane cut it) and, for a surface piece, the source triangle it came from (so its
/// corners can read the source's normals and UVs; 0 on caps).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tri {
    pub v: [P; 3],
    pub tag: u32,
    pub src: u32,
}

impl Tri {
    pub fn area(&self) -> f64 {
        0.5 * length(cross(sub(self.v[1], self.v[0]), sub(self.v[2], self.v[0])))
    }

    /// Signed volume of the tetrahedron against the origin.
    pub fn volume6(&self) -> f64 {
        dot(self.v[0], cross(self.v[1], self.v[2]))
    }
}

/// The half-space `n . x <= c` is kept.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Plane {
    pub n: P,
    pub c: f64,
}

impl Plane {
    fn dist(&self, p: P) -> f64 {
        dot(self.n, p) - self.c
    }
}

/// Bit-exact key of a point (`-0.0` and `0.0` are the same point).
pub(crate) type Key = [u64; 3];

pub(crate) fn key(p: P) -> Key {
    let f = |x: f64| if x == 0.0 { 0.0f64.to_bits() } else { x.to_bits() };
    [f(p[0]), f(p[1]), f(p[2])]
}

fn lex_less(a: P, b: P) -> bool {
    for (x, y) in a.iter().zip(b.iter()) {
        match x.total_cmp(y) {
            std::cmp::Ordering::Less => return true,
            std::cmp::Ordering::Greater => return false,
            std::cmp::Ordering::Equal => {}
        }
    }
    false
}

/// Where the segment `a`-`b` crosses the plane, computed identically from either direction.
fn intersect(plane: &Plane, a: P, b: P) -> P {
    let (lo, hi) = if lex_less(b, a) { (b, a) } else { (a, b) };
    let dl = plane.dist(lo);
    let dh = plane.dist(hi);
    let t = dl / (dl - dh);
    add(lo, scale(sub(hi, lo), t))
}

/// Keeps the part of `tris` on the `n . x <= c` side. `eps` is the on-plane tolerance (a vertex
/// closer than that counts as on the plane).
pub(crate) fn clip(tris: &[Tri], plane: &Plane, eps: f64) -> Vec<Tri> {
    let mut out = Vec::with_capacity(tris.len());
    for tri in tris {
        let raw = [plane.dist(tri.v[0]), plane.dist(tri.v[1]), plane.dist(tri.v[2])];
        let d = raw.map(|x| if x.abs() <= eps { 0.0 } else { x });
        let max = d[0].max(d[1]).max(d[2]);
        let min = d[0].min(d[1]).min(d[2]);
        if max <= 0.0 {
            // A triangle wholly on the plane bounds the kept side only when it faces out of it
            // (along `n`); facing the other way, the solid behind it is across the plane. Keeping
            // it on both sides doubled a coplanar source wall and capped the copy again.
            if min >= 0.0 && dot(cross(sub(tri.v[1], tri.v[0]), sub(tri.v[2], tri.v[0])), plane.n) <= 0.0 {
                continue;
            }
            out.push(*tri);
            continue;
        }
        if min >= 0.0 {
            continue;
        }
        let mut poly: [P; 4] = [[0.0; 3]; 4];
        let mut n = 0;
        for i in 0..3 {
            let j = (i + 1) % 3;
            if d[i] <= 0.0 {
                poly[n] = tri.v[i];
                n += 1;
            }
            if (d[i] < 0.0 && d[j] > 0.0) || (d[i] > 0.0 && d[j] < 0.0) {
                poly[n] = intersect(plane, tri.v[i], tri.v[j]);
                n += 1;
            }
        }
        for i in 1..n.saturating_sub(1) {
            out.push(Tri {
                v: [poly[0], poly[i], poly[i + 1]],
                tag: tri.tag,
                src: tri.src,
            });
        }
    }
    out
}

/// Why a cap could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CapError {
    /// The boundary edges do not form closed loops: the mesh was not closed.
    OpenBoundary,
    /// A hole loop lies inside no outer loop.
    OrphanHole,
}

/// Closes the opening a [`clip`] left with cap triangles facing `plane.n`, tagged `tag`.
pub(crate) fn cap(tris: &mut Vec<Tri>, plane: &Plane, tag: u32) -> Result<(), CapError> {
    let loops = boundary_loops(tris)?;
    if loops.is_empty() {
        return Ok(());
    }
    let (u, v) = plane_basis(plane.n);
    let mut outers: Vec<Poly> = Vec::new();
    let mut holes: Vec<Poly> = Vec::new();
    for pts in loops {
        let flat: Vec<[f64; 2]> = pts.iter().map(|p| [dot(*p, u), dot(*p, v)]).collect();
        let area = signed_area(&flat);
        let poly = Poly { pts, flat };
        if area < 0.0 {
            holes.push(poly);
        } else {
            outers.push(poly);
        }
    }
    // Each hole joins the smallest outer loop that contains it.
    let mut owned: Vec<Vec<Poly>> = vec![Vec::new(); outers.len()];
    for hole in holes {
        let mut best: Option<(usize, f64)> = None;
        for (i, outer) in outers.iter().enumerate() {
            if hole
                .flat
                .iter()
                .any(|p| point_in_polygon(*p, &outer.flat) && !outer.flat.contains(p))
            {
                let a = signed_area(&outer.flat);
                if best.is_none_or(|(_, ba)| a < ba) {
                    best = Some((i, a));
                }
            }
        }
        let Some((i, _)) = best else {
            return Err(CapError::OrphanHole);
        };
        owned[i].push(hole);
    }
    for (outer, holes) in outers.into_iter().zip(owned) {
        let merged = merge_holes(outer, holes);
        for [a, b, c] in ear_clip(&merged.flat) {
            tris.push(Tri {
                v: [merged.pts[a], merged.pts[b], merged.pts[c]],
                tag,
                src: 0,
            });
        }
    }
    Ok(())
}

#[derive(Clone)]
struct Poly {
    pts: Vec<P>,
    flat: Vec<[f64; 2]>,
}

pub(crate) fn plane_basis(n: P) -> (P, P) {
    let helper = if n[0].abs() < 0.6 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    let u = cross(helper, n);
    let u = scale(u, 1.0 / length(u));
    // u x v = n
    let v = cross(n, u);
    (u, v)
}

fn signed_area(p: &[[f64; 2]]) -> f64 {
    let mut a = 0.0;
    for i in 0..p.len() {
        let q = p[(i + 1) % p.len()];
        a += p[i][0] * q[1] - q[0] * p[i][1];
    }
    0.5 * a
}

fn point_in_polygon(p: [f64; 2], poly: &[[f64; 2]]) -> bool {
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[j]);
        if (a[1] > p[1]) != (b[1] > p[1]) && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0] {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// The closed loops (in the traversal direction of the reversed unmatched edges) of a triangle
/// set. Empty when the set is closed.
fn boundary_loops(tris: &[Tri]) -> Result<Vec<Vec<P>>, CapError> {
    let mut count: HashMap<(Key, Key), i32> = HashMap::new();
    let mut order: Vec<(Key, Key)> = Vec::new();
    let mut points: HashMap<Key, P> = HashMap::new();
    for t in tris {
        for i in 0..3 {
            let (a, b) = (t.v[i], t.v[(i + 1) % 3]);
            let (ka, kb) = (key(a), key(b));
            points.entry(ka).or_insert(a);
            points.entry(kb).or_insert(b);
            let c = count.entry((ka, kb)).or_insert(0);
            if *c == 0 {
                order.push((ka, kb));
            }
            *c += 1;
        }
    }
    // Reversed boundary edges, grouped by start vertex.
    let mut edges: Vec<(Key, Key)> = Vec::new();
    for &(ka, kb) in &order {
        let fwd = count.get(&(ka, kb)).copied().unwrap_or(0);
        let back = count.get(&(kb, ka)).copied().unwrap_or(0);
        for _ in 0..(fwd - back).max(0) {
            edges.push((kb, ka));
        }
    }
    if edges.is_empty() {
        return Ok(Vec::new());
    }
    let mut out_of: HashMap<Key, Vec<usize>> = HashMap::new();
    for (i, e) in edges.iter().enumerate() {
        out_of.entry(e.0).or_default().push(i);
    }
    let mut used = vec![false; edges.len()];
    let mut loops: Vec<Vec<P>> = Vec::new();
    for start in 0..edges.len() {
        if used[start] {
            continue;
        }
        used[start] = true;
        let mut path: Vec<Key> = vec![edges[start].0, edges[start].1];
        let mut at: HashMap<Key, usize> = HashMap::new();
        at.insert(path[0], 0);
        // The first step already went to path[1].
        if path[1] == path[0] {
            return Err(CapError::OpenBoundary);
        }
        at.insert(path[1], 1);
        loop {
            let Some(&cur) = path.last() else {
                return Err(CapError::OpenBoundary);
            };
            let next = out_of
                .get(&cur)
                .and_then(|list| list.iter().copied().find(|&e| !used[e]));
            let Some(e) = next else { break };
            used[e] = true;
            let v = edges[e].1;
            if let Some(&i) = at.get(&v) {
                let cycle: Vec<Key> = path[i..].to_vec();
                loops.push(cycle.iter().map(|k| points[k]).collect());
                for k in &path[i + 1..] {
                    at.remove(k);
                }
                path.truncate(i + 1);
            } else {
                at.insert(v, path.len());
                path.push(v);
            }
        }
        if path.len() != 1 {
            return Err(CapError::OpenBoundary);
        }
    }
    Ok(loops)
}

fn seg_cross(a: [f64; 2], b: [f64; 2], c: [f64; 2], d: [f64; 2]) -> bool {
    let o =
        |p: [f64; 2], q: [f64; 2], r: [f64; 2]| (q[0] - p[0]) * (r[1] - p[1]) - (q[1] - p[1]) * (r[0] - p[0]);
    let (d1, d2, d3, d4) = (o(a, b, c), o(a, b, d), o(c, d, a), o(c, d, b));
    ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0)) && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
}

/// Joins each hole (clockwise) to the outer loop (counter-clockwise) with a bridge, the hole with
/// the largest `x` first.
fn merge_holes(mut outer: Poly, mut holes: Vec<Poly>) -> Poly {
    holes.sort_by(|a, b| {
        let ma = a.flat.iter().map(|p| p[0]).fold(f64::NEG_INFINITY, f64::max);
        let mb = b.flat.iter().map(|p| p[0]).fold(f64::NEG_INFINITY, f64::max);
        mb.total_cmp(&ma)
    });
    while !holes.is_empty() {
        let hole = holes.remove(0);
        // The hole vertex with the largest x.
        let hi = (0..hole.flat.len())
            .max_by(|&a, &b| hole.flat[a][0].total_cmp(&hole.flat[b][0]))
            .unwrap_or(0);
        let h = hole.flat[hi];
        let mut cands: Vec<usize> = (0..outer.flat.len()).collect();
        cands.sort_by(|&a, &b| {
            let da = (outer.flat[a][0] - h[0]).powi(2) + (outer.flat[a][1] - h[1]).powi(2);
            let db = (outer.flat[b][0] - h[0]).powi(2) + (outer.flat[b][1] - h[1]).powi(2);
            da.total_cmp(&db).then(a.cmp(&b))
        });
        let visible = |ci: usize| -> bool {
            let c = outer.flat[ci];
            let blocked = |poly: &[[f64; 2]]| {
                (0..poly.len()).any(|i| {
                    let (p, q) = (poly[i], poly[(i + 1) % poly.len()]);
                    p != h && q != h && p != c && q != c && seg_cross(h, c, p, q)
                })
            };
            !blocked(&outer.flat) && !blocked(&hole.flat) && !holes.iter().any(|o| blocked(&o.flat))
        };
        let ci = cands
            .iter()
            .copied()
            .find(|&ci| visible(ci))
            .or_else(|| cands.first().copied())
            .unwrap_or(0);
        let mut pts = Vec::with_capacity(outer.pts.len() + hole.pts.len() + 2);
        let mut flat = Vec::with_capacity(pts.capacity());
        pts.extend_from_slice(&outer.pts[..=ci]);
        flat.extend_from_slice(&outer.flat[..=ci]);
        for k in 0..=hole.pts.len() {
            let idx = (hi + k) % hole.pts.len();
            pts.push(hole.pts[idx]);
            flat.push(hole.flat[idx]);
        }
        pts.extend_from_slice(&outer.pts[ci..]);
        flat.extend_from_slice(&outer.flat[ci..]);
        outer = Poly { pts, flat };
    }
    outer
}

fn cross2(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

/// Polygons up to this many vertices search every ear for the best one.
const EAR_SEARCH_LIMIT: usize = 48;

/// Triangulates a counter-clockwise polygon (indices into `p`). Always terminates; degenerate
/// (collinear) polygons produce zero-area triangles so the edge set still matches the surface.
fn ear_clip(p: &[[f64; 2]]) -> Vec<[usize; 3]> {
    let mut idx: Vec<usize> = (0..p.len()).collect();
    let mut out = Vec::with_capacity(p.len().saturating_sub(2));
    while idx.len() > 3 {
        let n = idx.len();
        let mut chosen: Option<(usize, f64)> = None;
        let mut flattest: (usize, f64) = (0, f64::INFINITY);
        for i in 0..n {
            let (a, b, c) = (idx[(i + n - 1) % n], idx[i], idx[(i + 1) % n]);
            let area = cross2(p[a], p[b], p[c]);
            if area.abs() < flattest.1 {
                flattest = (i, area.abs());
            }
            if area <= 0.0 {
                continue;
            }
            let blocked = idx.iter().any(|&k| {
                if p[k] == p[a] || p[k] == p[b] || p[k] == p[c] {
                    return false;
                }
                cross2(p[a], p[b], p[k]) >= 0.0
                    && cross2(p[b], p[c], p[k]) >= 0.0
                    && cross2(p[c], p[a], p[k]) >= 0.0
            });
            if blocked {
                continue;
            }
            // Prefer the fattest ear (area over the sum of squared edge lengths) on small polygons,
            // so caps are not fans of slivers; big polygons take the first ear.
            let sq = |u: [f64; 2], v: [f64; 2]| (u[0] - v[0]).powi(2) + (u[1] - v[1]).powi(2);
            let quality = area / (sq(p[a], p[b]) + sq(p[b], p[c]) + sq(p[c], p[a])).max(1e-300);
            if chosen.is_none_or(|(_, q)| quality > q) {
                chosen = Some((i, quality));
            }
            if n > EAR_SEARCH_LIMIT {
                break;
            }
        }
        let i = chosen.map_or(flattest.0, |c| c.0);
        let n = idx.len();
        out.push([idx[(i + n - 1) % n], idx[i], idx[(i + 1) % n]]);
        idx.remove(i);
    }
    if idx.len() == 3 {
        out.push([idx[0], idx[1], idx[2]]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_cube() -> Vec<Tri> {
        let v = |x: f64, y: f64, z: f64| [x, y, z];
        let quads: [[P; 4]; 6] = [
            [v(0., 0., 0.), v(0., 1., 0.), v(1., 1., 0.), v(1., 0., 0.)], // -z
            [v(0., 0., 1.), v(1., 0., 1.), v(1., 1., 1.), v(0., 1., 1.)], // +z
            [v(0., 0., 0.), v(1., 0., 0.), v(1., 0., 1.), v(0., 0., 1.)], // -y
            [v(0., 1., 0.), v(0., 1., 1.), v(1., 1., 1.), v(1., 1., 0.)], // +y
            [v(0., 0., 0.), v(0., 0., 1.), v(0., 1., 1.), v(0., 1., 0.)], // -x
            [v(1., 0., 0.), v(1., 1., 0.), v(1., 1., 1.), v(1., 0., 1.)], // +x
        ];
        quads
            .iter()
            .flat_map(|q| {
                [
                    Tri {
                        v: [q[0], q[1], q[2]],
                        tag: SURFACE,
                        src: 0,
                    },
                    Tri {
                        v: [q[0], q[2], q[3]],
                        tag: SURFACE,
                        src: 0,
                    },
                ]
            })
            .collect()
    }

    fn volume(tris: &[Tri]) -> f64 {
        tris.iter().map(Tri::volume6).sum::<f64>() / 6.0
    }

    #[test]
    fn cube_has_unit_volume() {
        assert!((volume(&unit_cube()) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn clip_and_cap_a_cube_in_half_conserves_the_half() {
        let plane = Plane {
            n: [1.0, 0.0, 0.0],
            c: 0.5,
        };
        let mut t = clip(&unit_cube(), &plane, 1e-12);
        assert_eq!(cap(&mut t, &plane, 7), Ok(()));
        assert!((volume(&t) - 0.5).abs() < 1e-12);
        let cap_area: f64 = t.iter().filter(|x| x.tag == 7).map(Tri::area).sum();
        assert!((cap_area - 1.0).abs() < 1e-12);
        assert!(boundary_loops(&t).is_ok_and(|l| l.is_empty()));
    }

    #[test]
    fn a_diagonal_cut_through_vertices_stays_closed() {
        // x + y <= 1 passes through two cube corners and the diagonal of two faces.
        let s = std::f64::consts::FRAC_1_SQRT_2;
        let plane = Plane { n: [s, s, 0.0], c: s };
        let mut t = clip(&unit_cube(), &plane, 1e-12);
        assert_eq!(cap(&mut t, &plane, 1), Ok(()));
        assert!((volume(&t) - 0.5).abs() < 1e-12);
        assert!(boundary_loops(&t).is_ok_and(|l| l.is_empty()));
    }

    #[test]
    fn a_plane_outside_keeps_everything_and_a_plane_behind_removes_everything() {
        let keep = Plane {
            n: [1.0, 0.0, 0.0],
            c: 5.0,
        };
        assert_eq!(clip(&unit_cube(), &keep, 1e-12).len(), 12);
        let gone = Plane {
            n: [1.0, 0.0, 0.0],
            c: -5.0,
        };
        assert!(clip(&unit_cube(), &gone, 1e-12).is_empty());
    }

    #[test]
    fn a_plane_on_a_face_keeps_the_cube() {
        let plane = Plane {
            n: [1.0, 0.0, 0.0],
            c: 1.0,
        };
        let mut t = clip(&unit_cube(), &plane, 1e-12);
        assert_eq!(t.len(), 12);
        assert_eq!(cap(&mut t, &plane, 1), Ok(()));
        assert_eq!(t.len(), 12);
    }

    #[test]
    fn a_face_on_the_plane_belongs_only_to_the_side_it_faces() {
        // Keeping x >= 1 touches the cube only along its +x face, which faces out of x <= 1:
        // the solid behind it is across the plane, so nothing is kept (before the fix both
        // sides kept that face, doubling a coplanar wall).
        let across = Plane {
            n: [-1.0, 0.0, 0.0],
            c: -1.0,
        };
        assert!(clip(&unit_cube(), &across, 1e-12).is_empty());
        // Wholly behind the face's own side, the face stays with the rest of the cube.
        let behind = Plane {
            n: [1.0, 0.0, 0.0],
            c: 1.0,
        };
        assert_eq!(clip(&unit_cube(), &behind, 1e-12).len(), 12);
    }

    #[test]
    fn a_surface_with_a_hole_is_capped_around_the_hole_too() {
        // The boundary of any triangle set is a union of closed loops, so a hole in the input is
        // not a cap error; `fracture_mesh` rejects open input before it clips.
        let mut t = unit_cube();
        t.remove(0);
        let plane = Plane {
            n: [1.0, 0.0, 0.0],
            c: 0.5,
        };
        let mut t = clip(&t, &plane, 1e-12);
        assert_eq!(cap(&mut t, &plane, 1), Ok(()));
    }

    #[test]
    fn the_cut_is_the_same_from_both_sides() {
        let plane = Plane {
            n: [0.6, 0.0, 0.8],
            c: 0.45,
        };
        let back = Plane {
            n: [-0.6, 0.0, -0.8],
            c: -0.45,
        };
        let mut a = clip(&unit_cube(), &plane, 1e-12);
        let mut b = clip(&unit_cube(), &back, 1e-12);
        cap(&mut a, &plane, 1).ok();
        cap(&mut b, &back, 2).ok();
        assert!((volume(&a) + volume(&b) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn ear_clipping_a_concave_polygon_covers_its_area() {
        // An L shape, area 3.
        let l = [
            [0.0, 0.0],
            [2.0, 0.0],
            [2.0, 1.0],
            [1.0, 1.0],
            [1.0, 2.0],
            [0.0, 2.0],
        ];
        let tris = ear_clip(&l);
        assert_eq!(tris.len(), 4);
        let area: f64 = tris.iter().map(|t| 0.5 * cross2(l[t[0]], l[t[1]], l[t[2]])).sum();
        assert!((area - 3.0).abs() < 1e-12);
    }

    #[test]
    fn ear_clipping_collinear_points_terminates() {
        let line = [[0.0, 0.0], [1.0, 0.0], [2.0, 0.0], [3.0, 0.0]];
        assert_eq!(ear_clip(&line).len(), 2);
    }

    #[test]
    fn a_square_tube_cut_across_has_an_annular_cap() {
        // A 3x3x1 slab with a 1x1 hole through z (a closed genus-1 mesh), cut at z = 0.5.
        let mut tris = Vec::new();
        let quad = |tris: &mut Vec<Tri>, q: [P; 4]| {
            tris.push(Tri {
                v: [q[0], q[1], q[2]],
                tag: SURFACE,
                src: 0,
            });
            tris.push(Tri {
                v: [q[0], q[2], q[3]],
                tag: SURFACE,
                src: 0,
            });
        };
        // Top and bottom rings: 8 cells of the 3x3 grid minus the centre.
        for ix in 0..3 {
            for iy in 0..3 {
                if ix == 1 && iy == 1 {
                    continue;
                }
                let (x, y) = (f64::from(ix), f64::from(iy));
                quad(
                    &mut tris,
                    [
                        [x, y, 1.0],
                        [x + 1.0, y, 1.0],
                        [x + 1.0, y + 1.0, 1.0],
                        [x, y + 1.0, 1.0],
                    ],
                );
                quad(
                    &mut tris,
                    [
                        [x, y, 0.0],
                        [x, y + 1.0, 0.0],
                        [x + 1.0, y + 1.0, 0.0],
                        [x + 1.0, y, 0.0],
                    ],
                );
            }
        }
        // Outer walls.
        for i in 0..3 {
            let t = f64::from(i);
            quad(
                &mut tris,
                [
                    [t, 0.0, 0.0],
                    [t + 1.0, 0.0, 0.0],
                    [t + 1.0, 0.0, 1.0],
                    [t, 0.0, 1.0],
                ],
            );
            quad(
                &mut tris,
                [
                    [t + 1.0, 3.0, 0.0],
                    [t, 3.0, 0.0],
                    [t, 3.0, 1.0],
                    [t + 1.0, 3.0, 1.0],
                ],
            );
            quad(
                &mut tris,
                [
                    [0.0, t + 1.0, 0.0],
                    [0.0, t, 0.0],
                    [0.0, t, 1.0],
                    [0.0, t + 1.0, 1.0],
                ],
            );
            quad(
                &mut tris,
                [
                    [3.0, t, 0.0],
                    [3.0, t + 1.0, 0.0],
                    [3.0, t + 1.0, 1.0],
                    [3.0, t, 1.0],
                ],
            );
        }
        // Hole walls (facing the hole axis).
        quad(
            &mut tris,
            [[1.0, 1.0, 0.0], [1.0, 1.0, 1.0], [2.0, 1.0, 1.0], [2.0, 1.0, 0.0]],
        );
        quad(
            &mut tris,
            [[2.0, 1.0, 0.0], [2.0, 1.0, 1.0], [2.0, 2.0, 1.0], [2.0, 2.0, 0.0]],
        );
        quad(
            &mut tris,
            [[2.0, 2.0, 0.0], [2.0, 2.0, 1.0], [1.0, 2.0, 1.0], [1.0, 2.0, 0.0]],
        );
        quad(
            &mut tris,
            [[1.0, 2.0, 0.0], [1.0, 2.0, 1.0], [1.0, 1.0, 1.0], [1.0, 1.0, 0.0]],
        );
        assert!(boundary_loops(&tris).is_ok_and(|l| l.is_empty()));
        assert!((volume(&tris) - 8.0).abs() < 1e-12);
        let plane = Plane {
            n: [0.0, 0.0, 1.0],
            c: 0.5,
        };
        let mut t = clip(&tris, &plane, 1e-12);
        assert_eq!(cap(&mut t, &plane, 3), Ok(()));
        assert!((volume(&t) - 4.0).abs() < 1e-12);
        let cap_area: f64 = t.iter().filter(|x| x.tag == 3).map(Tri::area).sum();
        assert!((cap_area - 8.0).abs() < 1e-9, "annulus area {cap_area}");
        assert!(boundary_loops(&t).is_ok_and(|l| l.is_empty()));
    }
}
