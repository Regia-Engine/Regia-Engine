// SPDX-License-Identifier: MPL-2.0
//! Input validation and the welded, edge-adjacent working mesh of the atlas generator.
//!
//! Vertices closer than `WELD_RELATIVE` times the bounding-box diagonal are welded (each onto the
//! lowest-index earlier vertex in reach, found through a spatial hash of that cell size) so a
//! normal or UV seam in the source, or an exporter's rounding at a seam or pole, does not cut the
//! surface apart (xatlas welds within an epsilon too); adjacency pairs each directed edge `a → b` with
//! the first unpaired edge `b → a` (in face order, so non-manifold fans resolve the same way every
//! run). Faces with zero area, repeated welded corners or non-finite normals are skipped with
//! [`AtlasWarning::DegenerateTriangle`].

use std::collections::HashMap;

use regia_core::glam::Vec3;

use super::{AtlasError, AtlasMesh, AtlasWarning};

/// No opposite edge / no chart.
pub(super) const NONE: u32 = u32::MAX;

/// Normals closer than this (per component) are the same normal (xatlas `kNormalEpsilon`).
const NORMAL_EPSILON: f32 = 0.001;
/// Weld distance as a share of the bounding-box diagonal.
pub(super) const WELD_RELATIVE: f32 = 1e-6;

/// The mesh the chart and parametrization stages work on. Faces are the non-degenerate source
/// faces, renumbered densely; half-edge `f * 3 + i` runs from corner `i` to corner `(i + 1) % 3`.
pub(super) struct Prepared {
    /// Source positions.
    pub positions: Vec<Vec3>,
    /// Corners of each working face (source vertex indices).
    pub faces: Vec<[u32; 3]>,
    /// Source face index of each working face.
    pub face_source: Vec<u32>,
    /// Source faces that were skipped, ascending.
    pub skipped: Vec<u32>,
    /// Welded (canonical) vertex of each source vertex.
    pub weld: Vec<u32>,
    /// Opposite half-edge of each half-edge, or [`NONE`] on a boundary.
    pub opposite: Vec<u32>,
    /// Unit face normals.
    pub face_normals: Vec<Vec3>,
    /// Face areas.
    pub face_areas: Vec<f32>,
    /// Half-edge lengths.
    pub edge_lengths: Vec<f32>,
    /// Source vertex normals, when given.
    pub normals: Option<Vec<Vec3>>,
    /// Source texture coordinates, when given.
    pub uvs: Option<Vec<[f32; 2]>>,
}

/// Welded (canonical) vertex of every vertex: the lowest-index earlier canonical vertex within
/// `WELD_RELATIVE` of the bounding-box diagonal, else itself.
fn weld_vertices(positions: &[Vec3]) -> Vec<u32> {
    let (lo, hi) = positions
        .iter()
        .fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(lo, hi), p| {
            (lo.min(*p), hi.max(*p))
        });
    let diagonal = (hi - lo).length();
    let eps = if diagonal.is_finite() && diagonal > 0.0 {
        (diagonal * WELD_RELATIVE).max(f32::MIN_POSITIVE)
    } else {
        1.0
    };
    let cell_of = |p: Vec3| -> [i64; 3] {
        let c = ((p - lo) / eps).floor();
        [c.x as i64, c.y as i64, c.z as i64]
    };
    let mut cells: HashMap<[i64; 3], Vec<u32>> = HashMap::with_capacity(positions.len());
    let mut weld = Vec::with_capacity(positions.len());
    for (v, p) in positions.iter().enumerate() {
        let [cx, cy, cz] = cell_of(*p);
        let mut found: Option<u32> = None;
        for dz in -1..=1 {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let Some(list) = cells.get(&[cx + dx, cy + dy, cz + dz]) else {
                        continue;
                    };
                    for &c in list {
                        if (positions[c as usize] - *p).length() <= eps && found.is_none_or(|f| c < f) {
                            found = Some(c);
                        }
                    }
                }
            }
        }
        match found {
            Some(c) => weld.push(c),
            None => {
                weld.push(v as u32);
                cells.entry([cx, cy, cz]).or_default().push(v as u32);
            }
        }
    }
    weld
}

impl Prepared {
    /// Validates `mesh` and builds the working mesh.
    pub fn new(mesh: &AtlasMesh<'_>, warnings: &mut Vec<AtlasWarning>) -> Result<Self, AtlasError> {
        let vertex_count = mesh.positions.len();
        if !mesh.indices.len().is_multiple_of(3) {
            return Err(AtlasError::IndexCount {
                count: mesh.indices.len(),
            });
        }
        if let Some(normals) = mesh.normals
            && normals.len() != vertex_count
        {
            return Err(AtlasError::AttributeLength {
                attribute: "normals",
                expected: vertex_count,
                actual: normals.len(),
            });
        }
        if let Some(uvs) = mesh.uvs
            && uvs.len() != vertex_count
        {
            return Err(AtlasError::AttributeLength {
                attribute: "uvs",
                expected: vertex_count,
                actual: uvs.len(),
            });
        }
        for (v, p) in mesh.positions.iter().enumerate() {
            if !p.iter().all(|c| c.is_finite()) {
                return Err(AtlasError::NonFinitePosition { vertex: v as u32 });
            }
        }
        let (corners, _) = mesh.indices.as_chunks::<3>();
        for (f, tri) in corners.iter().enumerate() {
            for &index in tri {
                if index as usize >= vertex_count {
                    return Err(AtlasError::IndexOutOfRange {
                        face: f as u32,
                        index,
                        vertex_count,
                    });
                }
            }
        }

        let positions: Vec<Vec3> = mesh.positions.iter().map(|p| Vec3::from_array(*p)).collect();
        let weld = weld_vertices(&positions);

        let mut faces = Vec::with_capacity(corners.len());
        let mut face_source = Vec::with_capacity(corners.len());
        let mut face_normals = Vec::with_capacity(corners.len());
        let mut face_areas = Vec::with_capacity(corners.len());
        let mut skipped = Vec::new();
        for (f, tri) in corners.iter().enumerate() {
            let [a, b, c] = *tri;
            let (wa, wb, wc) = (weld[a as usize], weld[b as usize], weld[c as usize]);
            let degenerate_corners = wa == wb || wb == wc || wa == wc;
            let (pa, pb, pc) = (
                positions[a as usize],
                positions[b as usize],
                positions[c as usize],
            );
            let cross = (pb - pa).cross(pc - pa);
            let len = cross.length();
            let longest = (pb - pa)
                .length_squared()
                .max((pc - pb).length_squared())
                .max((pa - pc).length_squared());
            // Zero area, or a sliver whose normal is pure rounding noise.
            let degenerate_area = !(len.is_finite() && len > f32::EPSILON * longest && len > 0.0);
            if degenerate_corners || degenerate_area {
                skipped.push(f as u32);
                warnings.push(AtlasWarning::DegenerateTriangle { face: f as u32 });
                continue;
            }
            faces.push([a, b, c]);
            face_source.push(f as u32);
            face_normals.push(cross / len);
            face_areas.push(0.5 * len);
        }
        if faces.is_empty() {
            return Err(AtlasError::NoTriangles);
        }

        let mut edge_lengths = Vec::with_capacity(faces.len() * 3);
        for tri in &faces {
            for i in 0..3 {
                let p0 = positions[tri[i] as usize];
                let p1 = positions[tri[(i + 1) % 3] as usize];
                edge_lengths.push((p1 - p0).length());
            }
        }

        // Pair `a → b` with the first unpaired `b → a`, in half-edge order.
        let mut opposite = vec![NONE; faces.len() * 3];
        let mut directed: HashMap<(u32, u32), Vec<u32>> = HashMap::with_capacity(faces.len() * 3);
        for (f, tri) in faces.iter().enumerate() {
            for i in 0..3 {
                let a = weld[tri[i] as usize];
                let b = weld[tri[(i + 1) % 3] as usize];
                directed.entry((a, b)).or_default().push((f * 3 + i) as u32);
            }
        }
        for (f, tri) in faces.iter().enumerate() {
            for i in 0..3 {
                let e = f * 3 + i;
                if opposite[e] != NONE {
                    continue;
                }
                let a = weld[tri[i] as usize];
                let b = weld[tri[(i + 1) % 3] as usize];
                if let Some(candidates) = directed.get(&(b, a))
                    && let Some(&o) = candidates
                        .iter()
                        .find(|&&o| opposite[o as usize] == NONE && o as usize / 3 != f)
                {
                    opposite[e] = o;
                    opposite[o as usize] = e as u32;
                }
            }
        }

        Ok(Self {
            positions,
            faces,
            face_source,
            skipped,
            weld,
            opposite,
            face_normals,
            face_areas,
            edge_lengths,
            normals: mesh
                .normals
                .map(|n| n.iter().map(|v| Vec3::from_array(*v)).collect()),
            uvs: mesh.uvs.map(<[[f32; 2]]>::to_vec),
        })
    }

    /// Number of working faces.
    pub fn face_count(&self) -> usize {
        self.faces.len()
    }

    /// Source vertex at half-edge `e`'s start.
    pub fn edge_v0(&self, e: u32) -> u32 {
        self.faces[e as usize / 3][e as usize % 3]
    }

    /// Source vertex at half-edge `e`'s end.
    pub fn edge_v1(&self, e: u32) -> u32 {
        self.faces[e as usize / 3][(e as usize % 3 + 1) % 3]
    }

    /// Whether the faces on both sides of `e` reach it through different source vertices (the edge
    /// exists only because positions were welded).
    pub fn is_seam(&self, e: u32) -> bool {
        let o = self.opposite[e as usize];
        o != NONE && (self.edge_v0(e) != self.edge_v1(o) || self.edge_v1(e) != self.edge_v0(o))
    }

    /// Whether the source texture coordinates differ across seam `e`.
    pub fn is_texture_seam(&self, e: u32) -> bool {
        let Some(uvs) = &self.uvs else {
            return false;
        };
        if !self.is_seam(e) {
            return false;
        }
        let o = self.opposite[e as usize];
        uvs[self.edge_v0(e) as usize] != uvs[self.edge_v1(o) as usize]
            || uvs[self.edge_v1(e) as usize] != uvs[self.edge_v0(o) as usize]
    }

    /// Whether the normals differ across `e` (vertex normals when given, else face normals).
    pub fn is_normal_seam(&self, e: u32, same_region: bool) -> bool {
        let o = self.opposite[e as usize];
        if o == NONE {
            return false;
        }
        if let Some(normals) = &self.normals {
            if !self.is_seam(e) {
                return false;
            }
            let n0 = normals[self.edge_v0(e) as usize];
            let n1 = normals[self.edge_v1(e) as usize];
            let on0 = normals[self.edge_v0(o) as usize];
            let on1 = normals[self.edge_v1(o) as usize];
            return !n0.abs_diff_eq(on1, NORMAL_EPSILON) || !n1.abs_diff_eq(on0, NORMAL_EPSILON);
        }
        if same_region {
            return false;
        }
        let f0 = e as usize / 3;
        let f1 = o as usize / 3;
        !self.face_normals[f0].abs_diff_eq(self.face_normals[f1], NORMAL_EPSILON)
    }

    /// Centroid of face `f`.
    pub fn face_center(&self, f: u32) -> Vec3 {
        let [a, b, c] = self.faces[f as usize];
        (self.positions[a as usize] + self.positions[b as usize] + self.positions[c as usize]) / 3.0
    }
}
