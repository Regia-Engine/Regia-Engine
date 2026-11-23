// SPDX-License-Identifier: MPL-2.0
// Source: C. B. Barber, D. P. Dobkin, H. Huhdanpaa, "The Quickhull Algorithm for Convex Hulls",
// ACM Transactions on Mathematical Software 22(4), 1996.

//! A 3D quickhull in `f64`: initial simplex from the axis extremes, outside sets per face, the
//! globally farthest outside point added next, visible faces found by a breadth-first walk from the
//! face that sees it, and a cone of new faces over the horizon.
//!
//! Adding the globally farthest point first makes an early stop (the vertex cap) an inner
//! approximation that keeps the largest features. Ties resolve by face and point order, and the
//! edge map is only looked up, never iterated, so the output depends on the input order alone.

use std::collections::HashMap;

use glam::DVec3;

/// A closed convex hull: vertices and outward counter-clockwise triangles.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Hull {
    /// Hull vertices (each used by at least one triangle).
    pub(crate) vertices: Vec<DVec3>,
    /// Index triples into `vertices`, counter-clockwise seen from outside.
    pub(crate) triangles: Vec<[u32; 3]>,
}

impl Hull {
    /// The enclosed volume (divergence theorem about the first vertex).
    pub(crate) fn volume(&self) -> f64 {
        let Some(&origin) = self.vertices.first() else {
            return 0.0;
        };
        let six: f64 = self
            .triangles
            .iter()
            .map(|&[a, b, c]| {
                let pa = self.vertices[a as usize] - origin;
                let pb = self.vertices[b as usize] - origin;
                let pc = self.vertices[c as usize] - origin;
                pa.dot(pb.cross(pc))
            })
            .sum();
        six / 6.0
    }
}

/// Distance tolerance relative to the points' scale (about one `f32` rounding step).
const EPS_RELATIVE: f64 = 1e-7;

struct Face {
    v: [usize; 3],
    normal: DVec3,
    offset: f64,
    outside: Vec<usize>,
    farthest: Option<(usize, f64)>,
    alive: bool,
    mark: u32,
}

impl Face {
    fn new(points: &[DVec3], v: [usize; 3]) -> Self {
        let [a, b, c] = v.map(|i| points[i]);
        let n = (b - a).cross(c - a);
        let len = n.length();
        let normal = if len > 0.0 { n / len } else { DVec3::ZERO };
        Self {
            v,
            normal,
            offset: normal.dot(a),
            outside: Vec::new(),
            farthest: None,
            alive: true,
            mark: 0,
        }
    }

    fn distance(&self, p: DVec3) -> f64 {
        self.normal.dot(p) - self.offset
    }

    fn push_outside(&mut self, index: usize, distance: f64) {
        self.outside.push(index);
        if self.farthest.is_none_or(|(_, best)| distance > best) {
            self.farthest = Some((index, distance));
        }
    }

    fn edges(&self) -> [(usize, usize); 3] {
        let [a, b, c] = self.v;
        [(a, b), (b, c), (c, a)]
    }
}

/// The convex hull of `points`, or `None` when they are fewer than four, not finite, or all
/// coplanar, collinear or coincident (within `EPS_RELATIVE` times the sum of their largest
/// absolute coordinates).
///
/// `max_vertices` caps the hull's vertex count (0 = no cap, otherwise at least 4): the hull then
/// holds the farthest-first subset of the points and may leave some outside.
pub(crate) fn quickhull(points: &[DVec3], max_vertices: usize) -> Option<Hull> {
    if points.len() < 4 || points.iter().any(|p| !p.is_finite()) {
        return None;
    }
    let (lo, hi) = points
        .iter()
        .fold((points[0], points[0]), |(lo, hi), &p| (lo.min(p), hi.max(p)));
    let scale = lo.abs().max(hi.abs()).element_sum();
    // Single precision: the hulls leave as `f32`, so points closer than its rounding to a face
    // plane are inside (two vertices that round to one `f32` would leave sliver faces).
    let eps = (scale * EPS_RELATIVE).max(f64::MIN_POSITIVE);

    // The initial simplex: the farthest pair among the axis extremes, then the point farthest
    // from their line, then the point farthest from that plane.
    let mut extremes = [0usize; 6];
    for (i, p) in points.iter().enumerate() {
        for axis in 0..3 {
            if p[axis] < points[extremes[2 * axis]][axis] {
                extremes[2 * axis] = i;
            }
            if p[axis] > points[extremes[2 * axis + 1]][axis] {
                extremes[2 * axis + 1] = i;
            }
        }
    }
    let (mut i0, mut i1, mut best) = (0, 0, -1.0);
    for (k, &ea) in extremes.iter().enumerate() {
        for &eb in &extremes[k + 1..] {
            let d = points[ea].distance_squared(points[eb]);
            if d > best {
                (i0, i1, best) = (ea, eb, d);
            }
        }
    }
    if best.sqrt() <= eps {
        return None;
    }
    let p0 = points[i0];
    let dir = (points[i1] - p0).normalize();
    let (i2, line_distance) = farthest(points, |p| (p - p0).cross(dir).length());
    if line_distance <= eps {
        return None;
    }
    let n = (points[i1] - p0).cross(points[i2] - p0).normalize();
    let (i3, plane_distance) = farthest(points, |p| n.dot(p - p0).abs());
    if plane_distance <= eps {
        return None;
    }

    let centroid = (p0 + points[i1] + points[i2] + points[i3]) * 0.25;
    let mut faces: Vec<Face> = Vec::new();
    let mut edges: HashMap<(usize, usize), usize> = HashMap::new();
    for tri in [[i0, i1, i2], [i0, i3, i1], [i1, i3, i2], [i0, i2, i3]] {
        let mut face = Face::new(points, tri);
        if face.distance(centroid) > 0.0 {
            face = Face::new(points, [tri[0], tri[2], tri[1]]);
        }
        for edge in face.edges() {
            edges.insert(edge, faces.len());
        }
        faces.push(face);
    }
    for (i, &p) in points.iter().enumerate() {
        if [i0, i1, i2, i3].contains(&i) {
            continue;
        }
        for face in &mut faces {
            let d = face.distance(p);
            if d > eps {
                face.push_outside(i, d);
                break;
            }
        }
    }

    let mut vertex_count = 4;
    let mut mark = 0u32;
    loop {
        if max_vertices != 0 && vertex_count >= max_vertices {
            break;
        }
        let mut pick: Option<(usize, usize, f64)> = None;
        for (fi, face) in faces.iter().enumerate() {
            if face.alive
                && let Some((pi, d)) = face.farthest
                && pick.is_none_or(|(_, _, best)| d > best)
            {
                pick = Some((fi, pi, d));
            }
        }
        let Some((start, eye_index, _)) = pick else {
            break;
        };
        let eye = points[eye_index];

        // Faces the eye sees, connected to the start face.
        mark += 1;
        faces[start].mark = mark;
        let mut visible = vec![start];
        let mut cursor = 0;
        while cursor < visible.len() {
            let fi = visible[cursor];
            cursor += 1;
            for (a, b) in faces[fi].edges() {
                if let Some(&nb) = edges.get(&(b, a))
                    && faces[nb].alive
                    && faces[nb].mark != mark
                    && faces[nb].distance(eye) > eps
                {
                    faces[nb].mark = mark;
                    visible.push(nb);
                }
            }
        }

        // Horizon: edges of visible faces whose twin is not visible, in the visible faces'
        // winding so the new faces keep facing outward.
        let mut horizon = Vec::new();
        for &fi in &visible {
            for (a, b) in faces[fi].edges() {
                let twin_visible = edges
                    .get(&(b, a))
                    .is_some_and(|&nb| faces[nb].alive && faces[nb].mark == mark);
                if !twin_visible {
                    horizon.push((a, b));
                }
            }
        }

        // The horizon must be one simple loop (every vertex starts and ends one edge). If rounding
        // hid a nearly coplanar face inside the visible region, it is not, and the cone would
        // corrupt the hull: drop the point instead (it lies within rounding of a face plane).
        let mut starts: Vec<usize> = horizon.iter().map(|&(a, _)| a).collect();
        let mut ends: Vec<usize> = horizon.iter().map(|&(_, b)| b).collect();
        starts.sort_unstable();
        ends.sort_unstable();
        if horizon.len() < 3 || starts.windows(2).any(|w| w[0] == w[1]) || starts != ends {
            let face = &mut faces[start];
            face.outside.retain(|&i| i != eye_index);
            face.farthest = None;
            for &i in &face.outside {
                let d = face.normal.dot(points[i]) - face.offset;
                if face.farthest.is_none_or(|(_, best)| d > best) {
                    face.farthest = Some((i, d));
                }
            }
            continue;
        }

        let mut orphans = Vec::new();
        for &fi in &visible {
            let face = &mut faces[fi];
            face.alive = false;
            face.farthest = None;
            orphans.append(&mut face.outside);
            for edge in face.edges() {
                if edges.get(&edge) == Some(&fi) {
                    edges.remove(&edge);
                }
            }
        }

        let first_new = faces.len();
        for &(a, b) in &horizon {
            let face = Face::new(points, [a, b, eye_index]);
            for edge in face.edges() {
                edges.insert(edge, faces.len());
            }
            faces.push(face);
        }
        vertex_count += 1;

        for &pi in &orphans {
            if pi == eye_index {
                continue;
            }
            let p = points[pi];
            for face in &mut faces[first_new..] {
                let d = face.distance(p);
                if d > eps {
                    face.push_outside(pi, d);
                    break;
                }
            }
        }
    }

    let mut remap = vec![u32::MAX; points.len()];
    let mut vertices = Vec::new();
    let mut triangles = Vec::new();
    for face in faces.iter().filter(|f| f.alive) {
        let mut tri = [0u32; 3];
        for (slot, &vi) in tri.iter_mut().zip(&face.v) {
            if remap[vi] == u32::MAX {
                remap[vi] = vertices.len() as u32;
                vertices.push(points[vi]);
            }
            *slot = remap[vi];
        }
        triangles.push(tri);
    }
    Some(Hull { vertices, triangles })
}

/// The first index with the largest `metric`, and that value.
fn farthest(points: &[DVec3], metric: impl Fn(DVec3) -> f64) -> (usize, f64) {
    let mut best = (0, -1.0);
    for (i, &p) in points.iter().enumerate() {
        let d = metric(p);
        if d > best.1 {
            best = (i, d);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cube_corners(size: f64) -> Vec<DVec3> {
        let mut out = Vec::new();
        for z in [0.0, size] {
            for y in [0.0, size] {
                for x in [0.0, size] {
                    out.push(DVec3::new(x, y, z));
                }
            }
        }
        out
    }

    /// Every vertex lies on or behind every face plane.
    fn assert_convex(hull: &Hull, points: &[DVec3], tolerance: f64) {
        for &[a, b, c] in &hull.triangles {
            let (pa, pb, pc) = (
                hull.vertices[a as usize],
                hull.vertices[b as usize],
                hull.vertices[c as usize],
            );
            let n = (pb - pa).cross(pc - pa).normalize();
            for &p in points {
                assert!(
                    n.dot(p - pa) <= tolerance,
                    "point {p} is outside face {a} {b} {c}"
                );
            }
        }
    }

    #[test]
    fn cube_with_interior_and_duplicate_points() {
        let mut points = cube_corners(2.0);
        // Interior points, face centers and duplicates must not become vertices.
        for i in 0..50 {
            let t = f64::from(i) / 50.0;
            points.push(DVec3::new(0.2 + 1.6 * t, 1.0, 0.3 + 1.4 * (1.0 - t)));
        }
        points.push(DVec3::new(1.0, 1.0, 0.0));
        points.extend(cube_corners(2.0));
        let hull = quickhull(&points, 0).expect("hull");
        assert_eq!(hull.vertices.len(), 8);
        assert!((hull.volume() - 8.0).abs() < 1e-12, "volume {}", hull.volume());
        assert_convex(&hull, &points, 1e-9);
        // Closed: every directed edge has its twin.
        let mut directed = std::collections::HashSet::new();
        for &[a, b, c] in &hull.triangles {
            for e in [(a, b), (b, c), (c, a)] {
                assert!(directed.insert(e), "edge {e:?} used twice");
            }
        }
        for &(a, b) in &directed {
            assert!(directed.contains(&(b, a)), "edge ({a}, {b}) has no twin");
        }
    }

    #[test]
    fn points_on_a_sphere_all_become_vertices() {
        let mut points = Vec::new();
        for i in 0..12 {
            for j in 1..8 {
                let theta = std::f64::consts::TAU * f64::from(i) / 12.0;
                let phi = std::f64::consts::PI * f64::from(j) / 8.0;
                points.push(DVec3::new(
                    phi.sin() * theta.cos(),
                    phi.cos(),
                    phi.sin() * theta.sin(),
                ));
            }
        }
        points.push(DVec3::Y);
        points.push(-DVec3::Y);
        let hull = quickhull(&points, 0).expect("hull");
        assert_eq!(hull.vertices.len(), points.len());
        assert_eq!(hull.triangles.len(), 2 * points.len() - 4);
        assert_convex(&hull, &points, 1e-9);
        assert!(hull.volume() > 0.0 && hull.volume() < 4.0 / 3.0 * std::f64::consts::PI);
    }

    #[test]
    fn degenerate_inputs_have_no_hull() {
        assert_eq!(quickhull(&[], 0), None);
        assert_eq!(quickhull(&cube_corners(1.0)[..3], 0), None);
        let same = vec![DVec3::splat(3.0); 10];
        assert_eq!(quickhull(&same, 0), None);
        let line: Vec<DVec3> = (0..10).map(|i| DVec3::splat(f64::from(i))).collect();
        assert_eq!(quickhull(&line, 0), None);
        let plane: Vec<DVec3> = (0..20)
            .map(|i| DVec3::new(f64::from(i % 5), f64::from(i / 5), 0.0))
            .collect();
        assert_eq!(quickhull(&plane, 0), None);
        let mut bad = cube_corners(1.0);
        bad.push(DVec3::new(f64::NAN, 0.0, 0.0));
        assert_eq!(quickhull(&bad, 0), None);
    }

    #[test]
    fn vertex_cap_keeps_the_farthest_points() {
        let mut points = Vec::new();
        for i in 0..40 {
            let theta = std::f64::consts::TAU * f64::from(i) / 40.0;
            points.push(DVec3::new(theta.cos(), -1.0, theta.sin()));
            points.push(DVec3::new(theta.cos(), 1.0, theta.sin()));
        }
        let full = quickhull(&points, 0).expect("hull");
        assert_eq!(full.vertices.len(), 80);
        let capped = quickhull(&points, 12).expect("hull");
        assert_eq!(capped.vertices.len(), 12);
        assert!(capped.volume() < full.volume());
        assert!(capped.volume() > 0.5 * full.volume());
        // The capped hull is still closed and convex over its own vertices.
        assert_convex(&capped, &capped.vertices, 1e-9);
    }

    #[test]
    fn same_input_same_hull() {
        let points: Vec<DVec3> = (0..200)
            .map(|i| {
                let f = f64::from(i);
                DVec3::new(
                    (f * 0.73).sin(),
                    (f * 1.31).cos(),
                    (f * 0.37).sin() * (f * 0.11).cos(),
                )
            })
            .collect();
        let a = quickhull(&points, 0).expect("hull");
        let b = quickhull(&points, 0).expect("hull");
        assert_eq!(a, b);
        assert_convex(&a, &points, 1e-9);
    }
}
