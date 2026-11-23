// SPDX-License-Identifier: MPL-2.0
//! The solid a convex gem cut's facet planes bound: a box clipped by each half-space in turn
//! (polygon clipping per face, Sutherland and Hodgman 1974, applied to a closed polyhedron), then
//! its volume (divergence theorem over the faces) and bounding box. The part-kit check uses it to
//! prove a cut marked `convex` closes (no face of the start box survives), has no redundant facet
//! (every plane keeps a face of positive area) and matches its published size and proportions.

/// One clipping plane: `normal . x <= offset`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HalfSpace {
    /// Outward unit normal.
    pub normal: [f64; 3],
    /// Plane offset along the normal.
    pub offset: f64,
}

/// A face of the clipped solid: its polygon and the facet (index) that made it, `None` for a face
/// of the start box.
#[derive(Debug, Clone)]
pub struct Face {
    /// Which facet the face lies on; `None` while it is still a face of the start box.
    pub facet: Option<usize>,
    /// The polygon, counter-clockwise seen from outside.
    pub points: Vec<[f64; 3]>,
}

/// The clipped solid.
#[derive(Debug, Clone)]
pub struct Solid {
    /// The faces; empty when the half-spaces leave nothing of the box.
    pub faces: Vec<Face>,
}

/// Tolerance in millimetres for "on the plane".
const EPS: f64 = 1e-9;

impl Solid {
    /// An axis-aligned box from `-half` to `+half` on every axis.
    pub fn cube(half: f64) -> Self {
        let h = half;
        let quad = |pts: [[f64; 3]; 4]| Face {
            facet: None,
            points: pts.to_vec(),
        };
        Self {
            faces: vec![
                quad([[h, -h, -h], [h, h, -h], [h, h, h], [h, -h, h]]),
                quad([[-h, -h, -h], [-h, -h, h], [-h, h, h], [-h, h, -h]]),
                quad([[-h, h, -h], [-h, h, h], [h, h, h], [h, h, -h]]),
                quad([[-h, -h, -h], [h, -h, -h], [h, -h, h], [-h, -h, h]]),
                quad([[-h, -h, h], [h, -h, h], [h, h, h], [-h, h, h]]),
                quad([[-h, -h, -h], [-h, h, -h], [h, h, -h], [h, -h, -h]]),
            ],
        }
    }

    /// Clips the solid by `plane`, tagging the new cap face with `facet`.
    ///
    /// A plane that leaves every vertex inside or on it does not cut: an exterior plane is a no-op,
    /// and a plane that lies on a face the solid already has must not add a second copy of that face
    /// (the volume would count it twice, and a repeated facet would look like it had area).
    pub fn clip(&mut self, plane: HalfSpace, facet: usize) {
        let outside = |p: [f64; 3]| dot(plane.normal, p) - plane.offset > EPS;
        if !self.faces.iter().any(|f| f.points.iter().any(|p| outside(*p))) {
            return;
        }
        let mut faces = Vec::with_capacity(self.faces.len() + 1);
        let mut cut_points: Vec<[f64; 3]> = Vec::new();
        for face in &self.faces {
            let clipped = clip_polygon(&face.points, plane, &mut cut_points);
            if clipped.len() >= 3 {
                faces.push(Face {
                    facet: face.facet,
                    points: clipped,
                });
            }
        }
        if let Some(cap) = cap_polygon(&cut_points, plane.normal) {
            faces.push(Face {
                facet: Some(facet),
                points: cap,
            });
        }
        self.faces = faces;
    }

    /// The enclosed volume (cubic millimetres).
    pub fn volume(&self) -> f64 {
        let mut v = 0.0;
        for face in &self.faces {
            let p0 = face.points[0];
            for w in face.points[1..].windows(2) {
                v += dot(p0, cross(w[0], w[1]));
            }
        }
        v / 6.0
    }

    /// The area of every face made by `facet`.
    pub fn facet_area(&self, facet: usize) -> f64 {
        self.faces
            .iter()
            .filter(|f| f.facet == Some(facet))
            .map(|f| polygon_area(&f.points))
            .sum()
    }

    /// `true` while a face of the start box is left: the planes do not close the solid.
    pub fn touches_box(&self) -> bool {
        self.faces
            .iter()
            .any(|f| f.facet.is_none() && polygon_area(&f.points) > EPS)
    }

    /// The bounding box of every vertex: `(min, max)`, `None` when the solid is empty.
    pub fn bounds(&self) -> Option<([f64; 3], [f64; 3])> {
        let mut pts = self.faces.iter().flat_map(|f| f.points.iter());
        let first = *pts.next()?;
        Some(pts.fold((first, first), |(lo, hi), p| {
            (
                [lo[0].min(p[0]), lo[1].min(p[1]), lo[2].min(p[2])],
                [hi[0].max(p[0]), hi[1].max(p[1]), hi[2].max(p[2])],
            )
        }))
    }

    /// The z range of the faces made by facets for which `pick` is true.
    pub fn z_range_of(&self, pick: impl Fn(usize) -> bool) -> Option<(f64, f64)> {
        let zs = self
            .faces
            .iter()
            .filter(|f| f.facet.is_some_and(&pick))
            .flat_map(|f| f.points.iter().map(|p| p[2]));
        zs.fold(None, |acc, z| match acc {
            None => Some((z, z)),
            Some((lo, hi)) => Some((lo.min(z), hi.max(z))),
        })
    }
}

/// Keeps the part of `poly` with `normal . x <= offset`; points where an edge crosses the plane
/// are added to `cut`.
fn clip_polygon(poly: &[[f64; 3]], plane: HalfSpace, cut: &mut Vec<[f64; 3]>) -> Vec<[f64; 3]> {
    let side = |p: [f64; 3]| dot(plane.normal, p) - plane.offset;
    let mut out = Vec::with_capacity(poly.len() + 1);
    for i in 0..poly.len() {
        let a = poly[i];
        let b = poly[(i + 1) % poly.len()];
        let (sa, sb) = (side(a), side(b));
        let a_in = sa <= EPS;
        let b_in = sb <= EPS;
        if a_in {
            out.push(a);
            if sa.abs() <= EPS {
                cut.push(a);
            }
        }
        if a_in != b_in && (sa.abs() > EPS && sb.abs() > EPS) {
            let t = sa / (sa - sb);
            let p = [
                a[0] + t * (b[0] - a[0]),
                a[1] + t * (b[1] - a[1]),
                a[2] + t * (b[2] - a[2]),
            ];
            out.push(p);
            cut.push(p);
        }
    }
    out
}

/// The cap polygon on the clipping plane from the crossing points: unique points ordered
/// counter-clockwise around `normal` (outward), `None` when fewer than three remain.
fn cap_polygon(points: &[[f64; 3]], normal: [f64; 3]) -> Option<Vec<[f64; 3]>> {
    let mut unique: Vec<[f64; 3]> = Vec::new();
    for p in points {
        if !unique.iter().any(|q| dist2(*p, *q) < 1e-14) {
            unique.push(*p);
        }
    }
    if unique.len() < 3 {
        return None;
    }
    let n = unique.len() as f64;
    let c = unique.iter().fold([0.0; 3], |s, p| {
        [s[0] + p[0] / n, s[1] + p[1] / n, s[2] + p[2] / n]
    });
    let helper = if normal[0].abs() < 0.9 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    let u = normalize(cross(normal, helper));
    let v = cross(normal, u);
    let angle = |p: &[f64; 3]| {
        let d = [p[0] - c[0], p[1] - c[1], p[2] - c[2]];
        dot(d, v).atan2(dot(d, u))
    };
    unique.sort_by(|a, b| angle(a).total_cmp(&angle(b)));
    Some(unique)
}

fn polygon_area(points: &[[f64; 3]]) -> f64 {
    if points.len() < 3 {
        return 0.0;
    }
    let p0 = points[0];
    let mut s = [0.0; 3];
    for w in points[1..].windows(2) {
        let a = [w[0][0] - p0[0], w[0][1] - p0[1], w[0][2] - p0[2]];
        let b = [w[1][0] - p0[0], w[1][1] - p0[1], w[1][2] - p0[2]];
        let c = cross(a, b);
        s = [s[0] + c[0], s[1] + c[1], s[2] + c[2]];
    }
    0.5 * dot(s, s).sqrt()
}

/// Dot product.
pub fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize(a: [f64; 3]) -> [f64; 3] {
    let l = dot(a, a).sqrt();
    if l > 0.0 {
        [a[0] / l, a[1] / l, a[2] / l]
    } else {
        a
    }
}

fn dist2(a: [f64; 3], b: [f64; 3]) -> f64 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    dot(d, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane(n: [f64; 3], d: f64) -> HalfSpace {
        HalfSpace { normal: n, offset: d }
    }

    #[test]
    fn a_unit_cube_by_six_planes() {
        let mut s = Solid::cube(10.0);
        let planes = [
            plane([1.0, 0.0, 0.0], 1.0),
            plane([-1.0, 0.0, 0.0], 1.0),
            plane([0.0, 1.0, 0.0], 1.0),
            plane([0.0, -1.0, 0.0], 1.0),
            plane([0.0, 0.0, 1.0], 1.0),
            plane([0.0, 0.0, -1.0], 1.0),
        ];
        for (i, p) in planes.iter().enumerate() {
            s.clip(*p, i);
        }
        assert!(!s.touches_box());
        assert!((s.volume() - 8.0).abs() < 1e-9, "{}", s.volume());
        for i in 0..6 {
            assert!((s.facet_area(i) - 4.0).abs() < 1e-9);
        }
        let (lo, hi) = s.bounds().expect("bounds");
        assert_eq!((lo, hi), ([-1.0; 3], [1.0; 3]));
    }

    #[test]
    fn a_plane_on_an_existing_face_adds_no_second_face() {
        let mut s = Solid::cube(1.0);
        // The cube already ends at x = 1, so this plane supports that face.
        s.clip(plane([1.0, 0.0, 0.0], 1.0), 0);
        assert_eq!(s.facet_area(0), 0.0, "a supporting plane is not a new facet");
        assert!((s.volume() - 8.0).abs() < 1e-9, "volume {}", s.volume());
    }

    #[test]
    fn an_open_set_touches_the_box_and_a_redundant_plane_has_no_area() {
        let mut s = Solid::cube(10.0);
        s.clip(plane([0.0, 0.0, 1.0], 1.0), 0);
        s.clip(plane([0.0, 0.0, -1.0], 1.0), 1);
        assert!(s.touches_box(), "a slab is unbounded");
        s.clip(plane([0.0, 0.0, 1.0], 5.0), 2);
        assert_eq!(s.facet_area(2), 0.0);
    }

    #[test]
    fn a_tetrahedron_volume() {
        // x, y, z >= 0 and x + y + z <= 1: volume 1/6.
        let mut s = Solid::cube(4.0);
        let k = 1.0 / 3.0_f64.sqrt();
        s.clip(plane([-1.0, 0.0, 0.0], 0.0), 0);
        s.clip(plane([0.0, -1.0, 0.0], 0.0), 1);
        s.clip(plane([0.0, 0.0, -1.0], 0.0), 2);
        s.clip(plane([k, k, k], k), 3);
        assert!(!s.touches_box());
        assert!((s.volume() - 1.0 / 6.0).abs() < 1e-9, "{}", s.volume());
    }
}
