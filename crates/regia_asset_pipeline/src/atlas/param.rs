// SPDX-License-Identifier: MIT
// Portions ported from xatlas (https://github.com/jpcy/xatlas, commit f700c77), source/xatlas/xatlas.cpp
// (param::Chart::parameterize's orthogonal-first flow and its stretch thresholds, param::Quality's
// flipped-face and stretch metrics, param::findApproximateDiameterVertices). Copyright (c) 2018-2020
// Jonathan Young. xatlas is derived from thekla_atlas (https://github.com/Thekla/thekla_atlas),
// Copyright (c) 2013 Thekla, Inc and Copyright NVIDIA Corporation 2006 -- Ignacio Castano.
// MIT licence (see docs/ported-sources.toml):
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

//! Chart parametrization (stage 2 of `generate`).
//!
//! Per chart: project onto the chart plane and keep that when it is valid and nearly isometric
//! (stretch metrics of Sander et al. 2001, "Texture Mapping Progressive Meshes", within xatlas's
//! 1.1 / 1.25 limits); else solve a least squares conformal map. The LSCM rows are written here
//! from Lévy, Petitjean, Ray & Maillot 2002 ("Least Squares Conformal Maps for Automatic Texture
//! Atlas Generation"): per triangle, in its own orthonormal frame, `Σ_j E_j U_j = 0` with `E_j` the
//! edge opposite corner `j` as a complex number and `U_j = u_j + i v_j`, weighted by
//! `1 / sqrt(2 · area)` so the energy is the area-integrated Cauchy-Riemann residual. Two boundary
//! vertices far apart are pinned at their projected positions; the normal equations are solved by
//! Jacobi-preconditioned conjugate gradients (Shewchuk 1994, "An Introduction to the Conjugate
//! Gradient Method Without the Agonizing Pain", § B3), warm-started from the projection. A chart
//! whose map still folds (mixed triangle orientation, zero-area triangles, or a self-crossing
//! boundary) is split by normal-cone growth into pieces that project cleanly (`ChartKind::Piecewise`),
//! down to single triangles.

use std::collections::{HashMap, HashSet};

use regia_core::glam::{DVec2, DVec3};

use super::mesh::{NONE, Prepared};
use super::segment::SegChart;
use super::{ChartKind, parallel_map};

/// Largest RMS stretch for keeping the orthogonal projection (xatlas).
const ORTHO_MAX_STRETCH: f64 = 1.1;
/// Largest per-triangle stretch for keeping the orthogonal projection (xatlas).
const ORTHO_MAX_PEAK_STRETCH: f64 = 1.25;
/// Parametric / surface area ratio under which a triangle counts as collapsed.
const ZERO_AREA_RATIO: f64 = 1e-6;
/// Relative residual at which conjugate gradients stop.
const CG_TOLERANCE: f64 = 1e-9;
/// Iteration cap of conjugate gradients (also bounded by 5 × unknowns, as upstream).
const CG_MAX_ITERATIONS: usize = 20_000;
/// Normal-cone cosines of the piecewise fallback, loosest first.
const PIECE_CONES: [f64; 3] = [0.5, 0.866, 0.966];

/// A flattened chart: local vertices are the chart's welded vertices.
pub(super) struct FlatChart {
    /// Working faces.
    pub faces: Vec<u32>,
    /// Local vertex indices of each face's corners.
    pub local_faces: Vec<[u32; 3]>,
    /// Parametric coordinates of each local vertex.
    pub uvs: Vec<DVec2>,
    /// How the chart was flattened.
    pub kind: ChartKind,
    /// Surface area of the chart.
    pub surface_area: f64,
}

/// A chart's faces as a small indexed mesh over welded vertices.
struct ChartMesh {
    faces: Vec<u32>,
    local_faces: Vec<[u32; 3]>,
    positions: Vec<DVec3>,
    /// Local `(start, end)` of every half-edge without a neighbour inside the chart.
    boundary: Vec<(u32, u32)>,
    normal: DVec3,
    surface_area: f64,
}

impl ChartMesh {
    fn new(m: &Prepared, faces: &[u32], normal: Option<DVec3>) -> Self {
        let members: HashSet<u32> = faces.iter().copied().collect();
        let mut local_of: HashMap<u32, u32> = HashMap::with_capacity(faces.len() * 2);
        let mut positions = Vec::new();
        let mut local_faces = Vec::with_capacity(faces.len());
        let mut boundary = Vec::new();
        let mut normal_sum = DVec3::ZERO;
        let mut surface_area = 0.0;
        for &f in faces {
            let mut tri = [0u32; 3];
            for (corner, slot) in tri.iter_mut().enumerate() {
                let welded = m.weld[m.faces[f as usize][corner] as usize];
                *slot = *local_of.entry(welded).or_insert_with(|| {
                    positions.push(m.positions[welded as usize].as_dvec3());
                    (positions.len() - 1) as u32
                });
            }
            for i in 0..3 {
                let o = m.opposite[f as usize * 3 + i];
                if o == NONE || !members.contains(&(o / 3)) {
                    boundary.push((tri[i], tri[(i + 1) % 3]));
                }
            }
            let area = f64::from(m.face_areas[f as usize]);
            normal_sum += m.face_normals[f as usize].as_dvec3() * area;
            surface_area += area;
            local_faces.push(tri);
        }
        let first_normal = m.face_normals[faces[0] as usize].as_dvec3();
        let normal = normal.unwrap_or_else(|| normal_sum.try_normalize().unwrap_or(first_normal));
        Self {
            faces: faces.to_vec(),
            local_faces,
            positions,
            boundary,
            normal,
            surface_area,
        }
    }

    /// Projection onto the plane of `self.normal` (a right-handed frame, so front faces keep a
    /// positive parametric area).
    fn project(&self) -> Vec<DVec2> {
        let n = self.normal;
        let axis = if n.x.abs() < 0.9 { DVec3::X } else { DVec3::Y };
        let tangent = n.cross(axis).normalize();
        let bitangent = n.cross(tangent);
        self.positions
            .iter()
            .map(|p| DVec2::new(p.dot(tangent), p.dot(bitangent)))
            .collect()
    }

    fn into_flat(self, uvs: Vec<DVec2>, kind: ChartKind) -> FlatChart {
        FlatChart {
            faces: self.faces,
            local_faces: self.local_faces,
            uvs,
            kind,
            surface_area: self.surface_area,
        }
    }

    fn surface_area_of(&self, face: usize) -> f64 {
        let [a, b, c] = self.local_faces[face];
        let (pa, pb, pc) = (
            self.positions[a as usize],
            self.positions[b as usize],
            self.positions[c as usize],
        );
        0.5 * (pb - pa).cross(pc - pa).length()
    }
}

fn signed_area(a: DVec2, b: DVec2, c: DVec2) -> f64 {
    0.5 * (b - a).perp_dot(c - a)
}

/// Checks a parametrization: every triangle non-collapsed and of one orientation (mirrored to
/// counter-clockwise when all are clockwise), and a boundary that does not cross itself.
fn validate(mesh: &ChartMesh, uvs: &mut [DVec2]) -> bool {
    let mut flipped = 0usize;
    for (face, tri) in mesh.local_faces.iter().enumerate() {
        let area = signed_area(uvs[tri[0] as usize], uvs[tri[1] as usize], uvs[tri[2] as usize]);
        let surface = mesh.surface_area_of(face);
        if !area.is_finite() || area.abs() <= ZERO_AREA_RATIO * surface {
            return false;
        }
        if area < 0.0 {
            flipped += 1;
        }
    }
    if flipped == mesh.local_faces.len() {
        for uv in uvs.iter_mut() {
            uv.x = -uv.x;
        }
    } else if flipped != 0 {
        return false;
    }
    !boundary_crosses(uvs, &mesh.boundary)
}

/// Sander et al. 2001 stretch of a valid parametrization: (RMS stretch, peak stretch), both
/// normalised so a uniform scale reads 1.
fn stretch(mesh: &ChartMesh, uvs: &[DVec2]) -> (f64, f64) {
    let (mut l2, mut peak, mut surface_total, mut param_total) = (0.0, 0.0f64, 0.0, 0.0);
    for (face, tri) in mesh.local_faces.iter().enumerate() {
        let [a, b, c] = *tri;
        let (t1, t2, t3) = (uvs[a as usize], uvs[b as usize], uvs[c as usize]);
        let param = signed_area(t1, t2, t3).abs();
        if param <= 0.0 {
            continue;
        }
        let (q1, q2, q3) = (
            mesh.positions[a as usize],
            mesh.positions[b as usize],
            mesh.positions[c as usize],
        );
        let surface = mesh.surface_area_of(face);
        // Partial derivatives of the 3D position over the parametric coordinates.
        let ss = (q1 * (t2.y - t3.y) + q2 * (t3.y - t1.y) + q3 * (t1.y - t2.y)) / (2.0 * param);
        let st = (q1 * (t3.x - t2.x) + q2 * (t1.x - t3.x) + q3 * (t2.x - t1.x)) / (2.0 * param);
        let (e, f, g) = (ss.dot(ss), ss.dot(st), st.dot(st));
        let root = ((e - g) * (e - g) + 4.0 * f * f).sqrt();
        let sigma_max = (0.5 * (e + g + root)).max(0.0).sqrt();
        l2 += 0.5 * (e + g) * surface;
        peak = peak.max(sigma_max);
        surface_total += surface;
        param_total += param;
    }
    if surface_total <= 0.0 {
        return (f64::INFINITY, f64::INFINITY);
    }
    let norm = (param_total / surface_total).sqrt();
    ((l2 / surface_total).sqrt() * norm, peak * norm)
}

/// Whether two boundary edges that share no vertex cross (uniform grid over the edges).
fn boundary_crosses(uvs: &[DVec2], edges: &[(u32, u32)]) -> bool {
    if edges.len() < 4 {
        return false;
    }
    let (mut lo, mut hi) = (DVec2::splat(f64::MAX), DVec2::splat(f64::MIN));
    for &(a, b) in edges {
        for v in [a, b] {
            lo = lo.min(uvs[v as usize]);
            hi = hi.max(uvs[v as usize]);
        }
    }
    let extent = (hi - lo).max(DVec2::splat(1e-30));
    let side = ((edges.len() as f64).sqrt().ceil() as usize).clamp(1, 512);
    let cell = extent / side as f64;
    let to_cell = |p: DVec2| -> (usize, usize) {
        let c = (p - lo) / cell;
        (
            (c.x.max(0.0) as usize).min(side - 1),
            (c.y.max(0.0) as usize).min(side - 1),
        )
    };
    let mut buckets: Vec<Vec<u32>> = vec![Vec::new(); side * side];
    for (i, &(a, b)) in edges.iter().enumerate() {
        let (pa, pb) = (uvs[a as usize], uvs[b as usize]);
        let (x0, y0) = to_cell(pa.min(pb));
        let (x1, y1) = to_cell(pa.max(pb));
        for y in y0..=y1 {
            for x in x0..=x1 {
                buckets[y * side + x].push(i as u32);
            }
        }
    }
    let eps = 1e-12 * extent.length_squared();
    let orient = |a: DVec2, b: DVec2, c: DVec2| (b - a).perp_dot(c - a);
    for bucket in &buckets {
        for (k, &i) in bucket.iter().enumerate() {
            let (a, b) = edges[i as usize];
            for &j in &bucket[k + 1..] {
                let (c, d) = edges[j as usize];
                if a == c || a == d || b == c || b == d {
                    continue;
                }
                let (pa, pb, pc, pd) = (uvs[a as usize], uvs[b as usize], uvs[c as usize], uvs[d as usize]);
                let (o1, o2) = (orient(pa, pb, pc), orient(pa, pb, pd));
                let (o3, o4) = (orient(pc, pd, pa), orient(pc, pd, pb));
                let straddles = |p: f64, q: f64| (p > eps && q < -eps) || (p < -eps && q > eps);
                if straddles(o1, o2) && straddles(o3, o4) {
                    return true;
                }
            }
        }
    }
    false
}

/// Two boundary vertices far apart: the extremes along the axis of largest boundary extent.
fn diameter_pins(mesh: &ChartMesh) -> Option<(u32, u32)> {
    let mut on_boundary = vec![false; mesh.positions.len()];
    for &(a, b) in &mesh.boundary {
        on_boundary[a as usize] = true;
        on_boundary[b as usize] = true;
    }
    let first = on_boundary.iter().position(|&b| b)? as u32;
    let mut min_v = [first; 3];
    let mut max_v = [first; 3];
    for (v, p) in mesh.positions.iter().enumerate() {
        if !on_boundary[v] {
            continue;
        }
        for axis in 0..3 {
            if p[axis] < mesh.positions[min_v[axis] as usize][axis] {
                min_v[axis] = v as u32;
            }
            if p[axis] > mesh.positions[max_v[axis] as usize][axis] {
                max_v[axis] = v as u32;
            }
        }
    }
    let len =
        |axis: usize| (mesh.positions[min_v[axis] as usize] - mesh.positions[max_v[axis] as usize]).length();
    let (l0, l1, l2) = (len(0), len(1), len(2));
    let axis = if l0 > l1 && l0 > l2 {
        0
    } else if l1 > l2 {
        1
    } else {
        2
    };
    (min_v[axis] != max_v[axis]).then_some((min_v[axis], max_v[axis]))
}

/// A symmetric sparse matrix in compressed rows.
struct Csr {
    row_start: Vec<usize>,
    cols: Vec<u32>,
    values: Vec<f64>,
}

impl Csr {
    /// From `(row, col, value)` triplets; duplicates are summed in insertion order.
    fn from_triplets(n: usize, mut triplets: Vec<(u32, u32, f64)>) -> Self {
        triplets.sort_by_key(|&(r, c, _)| (r, c));
        let mut row_start = vec![0usize; n + 1];
        let mut cols = Vec::with_capacity(triplets.len());
        let mut values: Vec<f64> = Vec::with_capacity(triplets.len());
        let mut last: Option<(u32, u32)> = None;
        for (r, c, v) in triplets {
            if last == Some((r, c)) {
                if let Some(slot) = values.last_mut() {
                    *slot += v;
                }
                continue;
            }
            last = Some((r, c));
            cols.push(c);
            values.push(v);
            row_start[r as usize + 1] += 1;
        }
        for i in 0..n {
            row_start[i + 1] += row_start[i];
        }
        Self {
            row_start,
            cols,
            values,
        }
    }

    fn mul(&self, x: &[f64], y: &mut [f64]) {
        for (row, out) in y.iter_mut().enumerate() {
            let range = self.row_start[row]..self.row_start[row + 1];
            *out = self.cols[range.clone()]
                .iter()
                .zip(&self.values[range])
                .map(|(&c, &v)| v * x[c as usize])
                .sum();
        }
    }

    fn diagonal(&self) -> Vec<f64> {
        (0..self.row_start.len() - 1)
            .map(|row| {
                let range = self.row_start[row]..self.row_start[row + 1];
                self.cols[range.clone()]
                    .iter()
                    .zip(&self.values[range])
                    .find(|&(&c, _)| c as usize == row)
                    .map_or(0.0, |(_, &v)| v)
            })
            .collect()
    }
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Jacobi-preconditioned conjugate gradients on `a x = b`, starting from `x`. Returns false when
/// the iteration broke down (non-finite values or a non-positive curvature).
fn conjugate_gradient(a: &Csr, b: &[f64], x: &mut [f64]) -> bool {
    let n = b.len();
    let inv_diag: Vec<f64> = a
        .diagonal()
        .iter()
        .map(|&d| if d.abs() > 0.0 { 1.0 / d } else { 1.0 })
        .collect();
    let mut r = vec![0.0; n];
    a.mul(x, &mut r);
    for (ri, bi) in r.iter_mut().zip(b) {
        *ri = bi - *ri;
    }
    let mut d: Vec<f64> = r.iter().zip(&inv_diag).map(|(ri, m)| ri * m).collect();
    let mut delta = dot(&r, &d);
    let target = CG_TOLERANCE * CG_TOLERANCE * dot(b, b);
    let mut q = vec![0.0; n];
    let mut s = vec![0.0; n];
    let max_iterations = (5 * n).clamp(1, CG_MAX_ITERATIONS);
    for _ in 0..max_iterations {
        if dot(&r, &r) <= target {
            break;
        }
        a.mul(&d, &mut q);
        let curvature = dot(&d, &q);
        if curvature.is_nan() || curvature <= 0.0 {
            return curvature == 0.0 && x.iter().all(|v| v.is_finite());
        }
        let alpha = delta / curvature;
        for i in 0..n {
            x[i] += alpha * d[i];
            r[i] -= alpha * q[i];
            s[i] = r[i] * inv_diag[i];
        }
        let delta_old = delta;
        delta = dot(&r, &s);
        let beta = delta / delta_old;
        for (di, si) in d.iter_mut().zip(&s) {
            *di = si + beta * *di;
        }
    }
    x.iter().all(|v| v.is_finite())
}

/// Least squares conformal map warm-started (and pinned) from `init`.
fn lscm(mesh: &ChartMesh, init: &[DVec2]) -> Option<Vec<DVec2>> {
    let (pin0, pin1) = diameter_pins(mesh)?;
    let n = mesh.positions.len();
    // Unknown index of each vertex's u (v is the next one), or NONE for the pins.
    let mut unknown = vec![NONE; n];
    let mut count = 0u32;
    for (v, slot) in unknown.iter_mut().enumerate() {
        if v as u32 != pin0 && v as u32 != pin1 {
            *slot = count;
            count += 2;
        }
    }
    let size = count as usize;
    if size == 0 {
        return Some(init.to_vec());
    }
    let mut triplets: Vec<(u32, u32, f64)> = Vec::with_capacity(mesh.local_faces.len() * 72);
    let mut rhs = vec![0.0; size];
    let mut free: Vec<(u32, f64)> = Vec::with_capacity(6);
    for tri in &mesh.local_faces {
        let p = tri.map(|v| mesh.positions[v as usize]);
        let x_axis = (p[1] - p[0]).normalize_or_zero();
        let z_axis = x_axis.cross(p[2] - p[0]).normalize_or_zero();
        let y_axis = z_axis.cross(x_axis);
        let z = [
            DVec2::ZERO,
            DVec2::new((p[1] - p[0]).length(), 0.0),
            DVec2::new((p[2] - p[0]).dot(x_axis), (p[2] - p[0]).dot(y_axis)),
        ];
        let area2 = z[1].x * z[2].y;
        if !area2.is_finite() || area2 <= 0.0 {
            continue;
        }
        let w = 1.0 / area2.sqrt();
        let e = [z[2] - z[1], z[0] - z[2], z[1] - z[0]];
        // Real row: Σ a_j u_j - b_j v_j; imaginary row: Σ b_j u_j + a_j v_j.
        for row in 0..2 {
            free.clear();
            let mut locked = 0.0;
            for j in 0..3 {
                let (cu, cv) = if row == 0 {
                    (e[j].x * w, -e[j].y * w)
                } else {
                    (e[j].y * w, e[j].x * w)
                };
                let v = tri[j] as usize;
                if unknown[v] == NONE {
                    locked += cu * init[v].x + cv * init[v].y;
                } else {
                    free.push((unknown[v], cu));
                    free.push((unknown[v] + 1, cv));
                }
            }
            for &(i, ci) in &free {
                for &(k, ck) in &free {
                    triplets.push((i, k, ci * ck));
                }
                rhs[i as usize] -= ci * locked;
            }
        }
    }
    let matrix = Csr::from_triplets(size, triplets);
    let mut x = vec![0.0; size];
    for (v, &u) in unknown.iter().enumerate() {
        if u != NONE {
            x[u as usize] = init[v].x;
            x[u as usize + 1] = init[v].y;
        }
    }
    if !conjugate_gradient(&matrix, &rhs, &mut x) {
        return None;
    }
    Some(
        unknown
            .iter()
            .enumerate()
            .map(|(v, &u)| {
                if u == NONE {
                    init[v]
                } else {
                    DVec2::new(x[u as usize], x[u as usize + 1])
                }
            })
            .collect(),
    )
}

/// Flattens one segmented chart (one or more output charts when it has to be split).
fn parameterize_chart(m: &Prepared, seg: &SegChart) -> Vec<FlatChart> {
    let mesh = ChartMesh::new(m, &seg.faces, None);
    let mut ortho = mesh.project();
    if seg.planar {
        // Coplanar and edge-connected with consistent winding: the projection is exact.
        return vec![mesh.into_flat(ortho, ChartKind::Planar)];
    }
    if validate(&mesh, &mut ortho) {
        let (rms, peak) = stretch(&mesh, &ortho);
        if rms <= ORTHO_MAX_STRETCH && peak <= ORTHO_MAX_PEAK_STRETCH {
            return vec![mesh.into_flat(ortho, ChartKind::Ortho)];
        }
    }
    let init = mesh.project();
    if let Some(mut uvs) = lscm(&mesh, &init)
        && validate(&mesh, &mut uvs)
    {
        return vec![mesh.into_flat(uvs, ChartKind::Lscm)];
    }
    let mut out = Vec::new();
    split(m, &seg.faces, 0, &mut out);
    out
}

/// Piecewise fallback: grows pieces from seeds in face order while faces stay within the normal
/// cone of `PIECE_CONES[level]` around the seed, projects each piece onto its seed's plane and
/// splits again (tighter cone) when the projection is invalid; single triangles always are valid.
fn split(m: &Prepared, faces: &[u32], level: usize, out: &mut Vec<FlatChart>) {
    let Some(&cone) = PIECE_CONES.get(level) else {
        for &f in faces {
            let mesh = ChartMesh::new(m, &[f], None);
            let uvs = mesh.project();
            out.push(mesh.into_flat(uvs, ChartKind::Piecewise));
        }
        return;
    };
    let members: HashSet<u32> = faces.iter().copied().collect();
    let mut assigned: HashSet<u32> = HashSet::with_capacity(faces.len());
    let mut stack = Vec::new();
    for &seed in faces {
        if assigned.contains(&seed) {
            continue;
        }
        let seed_normal = m.face_normals[seed as usize].as_dvec3();
        let mut piece = vec![seed];
        assigned.insert(seed);
        stack.clear();
        stack.push(seed);
        while let Some(f) = stack.pop() {
            for i in 0..3 {
                let o = m.opposite[f as usize * 3 + i];
                if o == NONE {
                    continue;
                }
                let of = o / 3;
                if !members.contains(&of) || assigned.contains(&of) {
                    continue;
                }
                if m.face_normals[of as usize].as_dvec3().dot(seed_normal) <= cone {
                    continue;
                }
                assigned.insert(of);
                piece.push(of);
                stack.push(of);
            }
        }
        let mesh = ChartMesh::new(m, &piece, Some(seed_normal));
        let mut uvs = mesh.project();
        if validate(&mesh, &mut uvs) {
            out.push(mesh.into_flat(uvs, ChartKind::Piecewise));
        } else {
            split(m, &piece, level + 1, out);
        }
    }
}

/// Flattens every chart, in chart order, on up to `threads` workers.
pub(super) fn parameterize_all(m: &Prepared, charts: &[SegChart], threads: usize) -> Vec<FlatChart> {
    let _span = tracing::debug_span!("atlas.parameterize", charts = charts.len()).entered();
    parallel_map(charts.len(), threads, |i| parameterize_chart(m, &charts[i]))
        .into_iter()
        .flatten()
        .collect()
}
