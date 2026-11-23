// SPDX-License-Identifier: MPL-2.0
//! Mesh envelopes (spec 68 § 4, ADR-2731): the point-in-mesh test behind a `plant.grow` or
//! `plant.grow_envelope` whose `envelope` pin holds a Mesh.
//!
//! The rule is ray parity: a point is inside when a ray along +X crosses the surface an odd number of
//! times, and a point on the surface counts as inside. The crossing test uses add, subtract, multiply,
//! divide and compare on f64 only, so it is the same on every platform (spec 68 § 9). A ray through a
//! shared edge or vertex is decided per edge with a half-open rule evaluated on the edge's canonical
//! endpoint order: both triangles of an edge compute the same bits, so the ray crosses exactly one of
//! them and a closed mesh has no leaks. Original code.

use std::collections::HashMap;

use crate::data::MeshData;

/// The error of a mesh that cannot be an envelope: a stable code (documented in
/// `book/agents/procgen.md`) and a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvelopeError {
    /// `envelope_empty`, `envelope_not_manifold` or `envelope_invalid`.
    pub code: &'static str,
    /// Which mesh and why.
    pub message: String,
}

/// A closed mesh prepared for point-in-mesh tests.
#[derive(Clone, Debug)]
pub struct MeshEnvelope {
    triangles: Vec<[[f64; 3]; 3]>,
    lo: [f64; 3],
    hi: [f64; 3],
    /// Triangle lists of a uniform grid over the (y, z) rectangle, row-major.
    cells: Vec<Vec<u32>>,
    side: usize,
}

/// The weld key of a position: its bits, with `-0.0` read as `0.0`.
fn weld_key(p: [f32; 3]) -> [u32; 3] {
    p.map(|x| if x == 0.0 { 0 } else { x.to_bits() })
}

impl MeshEnvelope {
    /// Prepares `mesh`. Vertices at the same position count as one (a glTF splits them at UV seams).
    ///
    /// # Errors
    ///
    /// `envelope_empty` for a mesh with no triangles, `envelope_invalid` for a position that is not
    /// finite or an index out of range, `envelope_not_manifold` when an edge is not shared by exactly
    /// two triangles or a triangle has a repeated vertex: such a mesh has no inside.
    pub fn new(mesh: &MeshData) -> Result<Self, EnvelopeError> {
        let error = |code, what: String| EnvelopeError {
            code,
            message: format!("envelope `{}`: {what}", mesh.asset),
        };
        let tris = mesh.indices.as_chunks::<3>().0;
        if tris.is_empty() {
            return Err(error("envelope_empty", "the mesh has no triangles".to_string()));
        }
        if let Some(i) = mesh
            .positions
            .iter()
            .position(|p| p.iter().any(|c| !c.is_finite()))
        {
            return Err(error("envelope_invalid", format!("vertex {i} is not finite")));
        }
        let mut welded: HashMap<[u32; 3], u32> = HashMap::with_capacity(mesh.positions.len());
        let ids: Vec<u32> = mesh
            .positions
            .iter()
            .map(|p| {
                let next = welded.len() as u32;
                *welded.entry(weld_key(*p)).or_insert(next)
            })
            .collect();
        let mut edges: HashMap<(u32, u32), u32> = HashMap::with_capacity(tris.len() * 2);
        let edge = |a: u32, b: u32| (a.min(b), a.max(b));
        let mut triangles = Vec::with_capacity(tris.len());
        for (t, tri) in tris.iter().enumerate() {
            let mut corner = [0u32; 3];
            let mut position = [[0.0f64; 3]; 3];
            for (k, &i) in tri.iter().enumerate() {
                let (Some(&id), Some(p)) = (ids.get(i as usize), mesh.positions.get(i as usize)) else {
                    return Err(error("envelope_invalid", format!("triangle {t} has index {i}")));
                };
                corner[k] = id;
                position[k] = p.map(f64::from);
            }
            if corner[0] == corner[1] || corner[1] == corner[2] || corner[0] == corner[2] {
                return Err(error(
                    "envelope_not_manifold",
                    format!("triangle {t} repeats a vertex"),
                ));
            }
            for (a, b) in [(0, 1), (1, 2), (2, 0)] {
                *edges.entry(edge(corner[a], corner[b])).or_insert(0) += 1;
            }
            triangles.push(position);
        }
        // Reported in triangle order, so the message does not depend on the map.
        for (t, tri) in tris.iter().enumerate() {
            for (a, b) in [(0, 1), (1, 2), (2, 0)] {
                let (a, b) = (ids[tri[a] as usize], ids[tri[b] as usize]);
                let uses = edges.get(&edge(a, b)).copied().unwrap_or(0);
                if uses != 2 {
                    return Err(error(
                        "envelope_not_manifold",
                        format!("an edge of triangle {t} is used by {uses} triangles, not 2"),
                    ));
                }
            }
        }
        let (mut lo, mut hi) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
        for p in triangles.iter().flatten() {
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
        let mut out = Self {
            lo,
            hi,
            cells: Vec::new(),
            side: (triangles.len().isqrt()).clamp(1, 256),
            triangles,
        };
        // Long thin triangles across the bounds would each fill many cells; coarsen the grid until
        // the cell lists stay within a budget linear in the triangle count (one cell always fits).
        let budget = Self::grid_budget(out.triangles.len());
        while out.side > 1 && out.grid_entries() > budget {
            out.side /= 2;
        }
        out.cells = vec![Vec::new(); out.side * out.side];
        for t in 0..out.triangles.len() {
            let ((y0, y1), (z0, z1)) = out.cell_span(t);
            for z in z0..=z1 {
                for y in y0..=y1 {
                    let c = z * out.side + y;
                    out.cells[c].push(t as u32);
                }
            }
        }
        Ok(out)
    }

    /// The most grid entries a mesh of `triangles` may use.
    fn grid_budget(triangles: usize) -> u64 {
        16 * triangles as u64 + 65_536
    }

    /// The cell rows and columns triangle `t`'s (y, z) bounds cover.
    fn cell_span(&self, t: usize) -> ((usize, usize), (usize, usize)) {
        let tri = &self.triangles[t];
        let min = |k: usize| tri[0][k].min(tri[1][k]).min(tri[2][k]);
        let max = |k: usize| tri[0][k].max(tri[1][k]).max(tri[2][k]);
        (
            (self.cell_of(1, min(1)), self.cell_of(1, max(1))),
            (self.cell_of(2, min(2)), self.cell_of(2, max(2))),
        )
    }

    /// The grid entries the triangles would use at the current `side`.
    fn grid_entries(&self) -> u64 {
        (0..self.triangles.len())
            .map(|t| {
                let ((y0, y1), (z0, z1)) = self.cell_span(t);
                ((y1 - y0 + 1) * (z1 - z0 + 1)) as u64
            })
            .sum()
    }

    /// The grid row or column of coordinate `v` on `axis` (1 = y, 2 = z).
    fn cell_of(&self, axis: usize, v: f64) -> usize {
        let span = self.hi[axis] - self.lo[axis];
        if span <= 0.0 {
            return 0;
        }
        (((v - self.lo[axis]) / span * self.side as f64) as usize).min(self.side - 1)
    }

    /// The mesh's bounds `(min, max)`.
    pub fn bounds(&self) -> ([f64; 3], [f64; 3]) {
        (self.lo, self.hi)
    }

    /// Whether `p` is inside the mesh or on its surface.
    pub fn contains(&self, p: [f64; 3]) -> bool {
        if (0..3).any(|k| p[k] < self.lo[k] || p[k] > self.hi[k]) {
            return false;
        }
        let c = self.cell_of(2, p[2]) * self.side + self.cell_of(1, p[1]);
        let mut inside = false;
        for &t in &self.cells[c] {
            let tri = &self.triangles[t as usize];
            // Faces parallel to the +X ray have no YZ area, so the parity test never sees them.
            if point_on_triangle(tri, p) {
                return true;
            }
            if !projection_contains(tri, p) {
                continue;
            }
            let x = crossing_x(tri, p);
            if x == p[0] {
                return true;
            }
            if x > p[0] {
                inside = !inside;
            }
        }
        inside
    }
}

/// Whether `p` lies in the triangle's plane and inside it, boundary included. Exact equality, so a
/// near miss stays with the parity test. A degenerate triangle is not a surface.
fn point_on_triangle(tri: &[[f64; 3]; 3], p: [f64; 3]) -> bool {
    let sub = |a: [f64; 3], b: [f64; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    let dot = |a: [f64; 3], b: [f64; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let cross = |a: [f64; 3], b: [f64; 3]| {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    };
    let e1 = sub(tri[1], tri[0]);
    let e2 = sub(tri[2], tri[0]);
    let n = cross(e1, e2);
    let n2 = dot(n, n);
    if n2 == 0.0 {
        return false;
    }
    let w = sub(p, tri[0]);
    if dot(n, w) != 0.0 {
        return false;
    }
    let u = dot(cross(w, e2), n) / n2;
    let v = dot(cross(e1, w), n) / n2;
    u >= 0.0 && v >= 0.0 && u + v <= 1.0
}

/// Whether `(p.y, p.z)` is inside the triangle's projection along X: the crossing number of a ray
/// along +Z, half-open in y. Each edge is evaluated on its canonical endpoint order.
fn projection_contains(tri: &[[f64; 3]; 3], p: [f64; 3]) -> bool {
    let mut inside = false;
    for (a, b) in [(0, 1), (1, 2), (2, 0)] {
        let (u, v) = if (tri[a][1], tri[a][2], tri[a][0]) <= (tri[b][1], tri[b][2], tri[b][0]) {
            (&tri[a], &tri[b])
        } else {
            (&tri[b], &tri[a])
        };
        if (u[1] > p[1]) != (v[1] > p[1]) {
            let z = u[2] + (p[1] - u[1]) * (v[2] - u[2]) / (v[1] - u[1]);
            if p[2] < z {
                inside = !inside;
            }
        }
    }
    inside
}

/// The x where the triangle's plane meets the line through `p` along X. A triangle parallel to X has
/// no such point (its projection has no area, so only rounding gets here): its centroid's x.
fn crossing_x(tri: &[[f64; 3]; 3], p: [f64; 3]) -> f64 {
    let e1 = [
        tri[1][0] - tri[0][0],
        tri[1][1] - tri[0][1],
        tri[1][2] - tri[0][2],
    ];
    let e2 = [
        tri[2][0] - tri[0][0],
        tri[2][1] - tri[0][1],
        tri[2][2] - tri[0][2],
    ];
    let n = [
        e1[1] * e2[2] - e1[2] * e2[1],
        e1[2] * e2[0] - e1[0] * e2[2],
        e1[0] * e2[1] - e1[1] * e2[0],
    ];
    if n[0] == 0.0 {
        return (tri[0][0] + tri[1][0] + tri[2][0]) / 3.0;
    }
    tri[0][0] - (n[1] * (p[1] - tri[0][1]) + n[2] * (p[2] - tri[0][2])) / n[0]
}

/// A closed torus around the Y axis as a [`MeshData`], for tests and fixtures: `major` and `minor`
/// radii, `rings` around the axis and `sides` around the tube (vertices on exact cosines).
pub fn torus_mesh(major: f32, minor: f32, rings: u32, sides: u32) -> MeshData {
    let circle = |k: u32, n: u32| {
        let (s, c) = crate::detmath::sin_cos(std::f64::consts::TAU * f64::from(k) / f64::from(n));
        (s as f32, c as f32)
    };
    let mut positions = Vec::with_capacity((rings * sides) as usize);
    for r in 0..rings {
        let (sr, cr) = circle(r, rings);
        for t in 0..sides {
            let (st, ct) = circle(t, sides);
            let radial = major + minor * ct;
            positions.push([radial * cr, minor * st, radial * sr]);
        }
    }
    let at = |r: u32, t: u32| (r % rings) * sides + (t % sides);
    let mut indices = Vec::with_capacity((rings * sides * 6) as usize);
    for r in 0..rings {
        for t in 0..sides {
            let (a, b, c, d) = (at(r, t), at(r + 1, t), at(r + 1, t + 1), at(r, t + 1));
            indices.extend_from_slice(&[a, b, c, a, c, d]);
        }
    }
    MeshData {
        asset: "torus".to_string(),
        positions,
        normals: Vec::new(),
        indices,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cube() -> MeshData {
        let positions = (0..8)
            .map(|i| {
                [
                    if i & 1 == 0 { -1.0 } else { 1.0 },
                    if i & 2 == 0 { -1.0 } else { 1.0 },
                    if i & 4 == 0 { -1.0 } else { 1.0 },
                ]
            })
            .collect();
        MeshData {
            asset: "cube".to_string(),
            positions,
            normals: Vec::new(),
            #[rustfmt::skip]
            indices: vec![
                0, 2, 1, 1, 2, 3, 4, 5, 6, 5, 7, 6,
                0, 1, 4, 1, 5, 4, 2, 6, 3, 3, 6, 7,
                0, 4, 2, 2, 4, 6, 1, 3, 5, 3, 7, 5,
            ],
        }
    }

    /// The generalized winding number (a sum of solid angles, van Oosterom and Strackee 1983): an
    /// independent rule to check parity against. Uses `atan2`, so test code only.
    fn winding(mesh: &MeshData, p: [f64; 3]) -> f64 {
        let sub = |a: [f64; 3], b: [f64; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
        let dot = |a: [f64; 3], b: [f64; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let cross = |a: [f64; 3], b: [f64; 3]| {
            [
                a[1] * b[2] - a[2] * b[1],
                a[2] * b[0] - a[0] * b[2],
                a[0] * b[1] - a[1] * b[0],
            ]
        };
        let mut omega = 0.0;
        for tri in mesh.indices.as_chunks::<3>().0 {
            let v = tri.map(|i| sub(mesh.positions[i as usize].map(f64::from), p));
            let l = v.map(|x| dot(x, x).sqrt());
            let num = dot(v[0], cross(v[1], v[2]));
            let den =
                l[0] * l[1] * l[2] + dot(v[0], v[1]) * l[2] + dot(v[1], v[2]) * l[0] + dot(v[2], v[0]) * l[1];
            omega += 2.0 * num.atan2(den);
        }
        omega / (4.0 * std::f64::consts::PI)
    }

    #[test]
    fn a_cube_contains_what_its_faces_say() {
        let e = MeshEnvelope::new(&cube()).expect("closed");
        assert!(e.contains([0.0, 0.0, 0.0]));
        assert!(e.contains([0.99, -0.99, 0.5]));
        assert!(!e.contains([1.01, 0.0, 0.0]));
        assert!(!e.contains([0.0, 2.0, 0.0]));
        // Through a face diagonal, a shared edge and a corner of the ray's way out.
        assert!(
            e.contains([0.5, 0.0, 0.0]),
            "the +X face's diagonal runs through (y, z) = 0"
        );
        assert!(!e.contains([-1.5, 1.0, 1.0]), "outside, level with an edge");
        // On the surface counts as inside, including faces parallel to the test ray
        // and the shared edge between the two top triangles.
        assert!(e.contains([1.0, 0.25, 0.25]));
        assert!(e.contains([-0.5, 1.0, -0.5]), "+Y face");
        assert!(e.contains([-0.5, -0.5, 1.0]), "+Z face");
        assert!(e.contains([0.0, 1.0, 0.0]), "top edge");
        assert!(e.contains([0.0, -1.0, 0.0]), "-Y face");
        assert!(e.contains([0.0, 0.0, -1.0]), "-Z face");
    }

    /// A point in a parity disagreement with the winding number would mean a leak. Coordinates on the
    /// mesh's own vertex rows and columns aim the ray at edges and vertices on purpose.
    #[test]
    fn parity_agrees_with_the_winding_number_including_rays_through_edges_and_vertices() {
        let mesh = torus_mesh(2.0, 0.75, 24, 12);
        let e = MeshEnvelope::new(&mesh).expect("closed");
        let mut rng = 0x9e37_79b9u32;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            f64::from(rng) / f64::from(u32::MAX)
        };
        let mut checked = (0, 0);
        for k in 0..4000 {
            let mut p = [next() * 6.0 - 3.0, next() * 2.0 - 1.0, next() * 6.0 - 3.0];
            if k % 4 == 0 {
                // y and z exactly those of a vertex: this ray meets edges and corners.
                let v = mesh.positions[k % mesh.positions.len()];
                p[1] = f64::from(v[1]);
                p[2] = f64::from(v[2]);
            }
            let w = winding(&mesh, p);
            if (w.abs() - 0.5).abs() < 0.4 {
                continue; // On or very near the surface: the winding number is not decisive.
            }
            let inside = w.abs() > 0.5;
            assert_eq!(e.contains(p), inside, "{p:?} winding {w}");
            if inside {
                checked.0 += 1;
            } else {
                checked.1 += 1;
            }
        }
        assert!(checked.0 > 300 && checked.1 > 300, "{checked:?}");
    }

    #[test]
    fn bad_meshes_are_refused_with_their_codes() {
        let code = |m: &MeshData| MeshEnvelope::new(m).map(|_| ()).map_err(|e| e.code);
        let mut m = cube();
        m.indices.clear();
        assert_eq!(code(&m), Err("envelope_empty"));
        let mut m = cube();
        m.indices.truncate(m.indices.len() - 3);
        assert_eq!(code(&m), Err("envelope_not_manifold"), "an open mesh");
        let mut m = cube();
        m.indices.extend_from_slice(&[0, 2, 1]);
        assert_eq!(
            code(&m),
            Err("envelope_not_manifold"),
            "an edge on three triangles"
        );
        let mut m = cube();
        m.indices[1] = m.indices[0];
        assert_eq!(code(&m), Err("envelope_not_manifold"), "a repeated vertex");
        let mut m = cube();
        m.positions[3][1] = f32::NAN;
        assert_eq!(code(&m), Err("envelope_invalid"));
        let mut m = cube();
        m.indices[4] = 99;
        assert_eq!(code(&m), Err("envelope_invalid"));
    }

    /// A closed bicone of long slivers along the (y, z) diagonal: every triangle's bounds cover a
    /// quarter of the grid, so the grid coarsens to stay within its budget, and the answers stay right.
    #[test]
    fn long_slivers_keep_the_grid_within_its_budget() {
        let k = 4096u32;
        let (r, s2) = (0.01f64, std::f64::consts::FRAC_1_SQRT_2);
        let mut positions = vec![[0.0f32, 0.0, 0.0], [0.0, 1.0, 1.0]];
        for i in 0..k {
            let (sn, cs) = crate::detmath::sin_cos(std::f64::consts::TAU * f64::from(i) / f64::from(k));
            // The ring around the midpoint, in the plane spanned by x and (0, 1, -1) / sqrt 2.
            positions.push([
                (r * cs) as f32,
                (0.5 + r * sn * s2) as f32,
                (0.5 - r * sn * s2) as f32,
            ]);
        }
        let mut indices = Vec::new();
        for i in 0..k {
            let (a, b) = (2 + i, 2 + (i + 1) % k);
            indices.extend([0, a, b, 1, b, a]);
        }
        let mesh = MeshData {
            asset: "bicone".to_string(),
            positions,
            normals: Vec::new(),
            indices,
        };
        let env = MeshEnvelope::new(&mesh).expect("closed");
        let entries: u64 = env.cells.iter().map(|c| c.len() as u64).sum();
        assert!(
            entries <= MeshEnvelope::grid_budget(env.triangles.len()),
            "{entries}"
        );
        assert!(
            env.side < (2 * k as usize).isqrt(),
            "the grid coarsened: {}",
            env.side
        );
        assert!(env.contains([0.0, 0.5, 0.5]));
        assert!(env.contains([0.0, 0.25, 0.25]));
        assert!(!env.contains([0.2, 0.5, 0.5]));
        assert!(!env.contains([0.0, 0.25, 0.75]));
    }

    #[test]
    fn vertices_split_at_a_seam_weld_into_a_closed_mesh() {
        let mut m = cube();
        // Duplicate vertex 0 and use the copy on one triangle, as a UV seam would.
        m.positions.push(m.positions[0]);
        m.indices[0] = 8;
        assert!(MeshEnvelope::new(&m).is_ok());
    }
}
