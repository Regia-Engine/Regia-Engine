// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-content/src/tunnels.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 1008-1033, mark: as-is arithmetic (Stage B).
// Edits: unnormalized plane on the candidate; the unit normal is built only for a returned hit.
// Canonical directed edges replace inclusive barycentrics.
//! Load-time query acceleration and canonical edge ownership (ADR-8246).

use crate::GeomSettings;
use crate::cooked::{GeomLoadError, TriKind};
use crate::geom::GeomTriangle;
use crate::units::mm_to_m;
use regia_detsim::float::{det_max, det_min};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub(crate) struct Plane {
    pub(crate) n: [f32; 3],
    bounds: [f32; 4],
    valid: bool,
    edges: [Edge; 3],
    anchor: [f32; 3],
    pub(crate) kind: TriKind,
}

pub(crate) fn cross_n(v: [[f32; 3]; 3]) -> [f32; 3] {
    let [a, b, c] = v;
    let [ux, uy, uz] = sub(b, a);
    let [vx, vy, vz] = sub(c, a);
    [uy * vz - uz * vy, uz * vx - ux * vz, ux * vy - uy * vx]
}

fn len2_of(n: [f32; 3]) -> f32 {
    n[0] * n[0] + n[1] * n[1] + n[2] * n[2]
}

/// The reported unit normal. Same bits as the old in-plane cache (spec 121 § 15.3).
pub(crate) fn reported_normal(v: [[f32; 3]; 3]) -> [f32; 3] {
    unit_up(cross_n(v))
}

/// The reported unit normal of a plane whose unnormalized normal is `n` (`cross_n` of its
/// triangle): [`reported_normal`]'s second half, for a caller that already holds `n` (ADR-8817).
#[expect(
    clippy::disallowed_methods,
    reason = "ADR-8246: spec 121 §15.3 requires IEEE sqrt for normals, never libm on the native query path"
)]
pub(crate) fn unit_up(n: [f32; 3]) -> [f32; 3] {
    // det-scan: allow(std_float_fn): spec 121 §15.3 requires IEEE sqrt for the reported normal only.
    let len = len2_of(n).sqrt();
    let inv = if len >= 1e-8 { 1.0 / len } else { 0.0 };
    let normal = [n[0] * inv, n[1] * inv, n[2] * inv];
    if normal[1] < 0.0 {
        normal.map(|c| -c)
    } else {
        normal
    }
}

/// `true` when `sqrt(n·n) >= 1e-8` and `|ny| >= 1e-6`.
///
/// A squared length of at least 1 has an IEEE square root of at least 1, so that arm does not
/// take the square root. Shorter vectors keep it, and the 1e-8 cut does not move (ADR-8246 R7).
#[expect(
    clippy::disallowed_methods,
    reason = "ADR-8246: spec 121 §15.3 requires IEEE sqrt for normals, never libm on the native query path"
)]
pub(crate) fn plane_valid(n: [f32; 3]) -> bool {
    let len2 = len2_of(n);
    // det-scan: allow(std_float_fn): spec 121 §15.3 pins IEEE sqrt for the plane length cut (ADR-8246 R7).
    let long_enough = if len2 >= 1.0 { true } else { len2.sqrt() >= 1e-8 };
    long_enough && n[1].abs() >= 1e-6
}

impl Plane {
    fn equation(v: [[f32; 3]; 3]) -> ([f32; 3], [f32; 3], bool) {
        let n = cross_n(v);
        (n, reported_normal(v), plane_valid(n))
    }
    pub(crate) fn ground_crossing(v: [[f32; 3]; 3], x: f32, z: f32) -> Option<(f32, [f32; 3])> {
        let (n, normal, valid) = Self::equation(v);
        if !valid {
            return None;
        }
        let a = v[0];
        Some((a[1] - (n[0] * (x - a[0]) + n[2] * (z - a[2])) / n[1], normal))
    }
    pub(crate) fn new(tri: &GeomTriangle) -> Self {
        let v = tri.v;
        let [a, b, c] = v;
        let n = cross_n(v);
        Self {
            anchor: v[0],
            kind: tri.kind,
            edges: std::array::from_fn(|i| Edge::new(v[i], v[(i + 1) % 3], v[(i + 2) % 3])),
            n,
            valid: plane_valid(n),
            bounds: [
                det_min(a[0], det_min(b[0], c[0])),
                det_max(a[0], det_max(b[0], c[0])),
                det_min(a[2], det_min(b[2], c[2])),
                det_max(a[2], det_max(b[2], c[2])),
            ],
        }
    }
    #[inline]
    pub(crate) fn height(&self, a: [f32; 3], x: f32, z: f32) -> Option<f32> {
        if !self.valid {
            return None;
        }
        Some(a[1] - (self.n[0] * (x - a[0]) + self.n[2] * (z - a[2])) / self.n[1])
    }
    #[inline]
    pub(crate) fn bounds_contain(&self, x: f32, z: f32) -> bool {
        x >= self.bounds[0] && x <= self.bounds[1] && z >= self.bounds[2] && z <= self.bounds[3]
    }
    /// Closed triangle, including every boundary, without asking a neighbor.
    #[inline]
    fn contains_closed(&self, x: f32, z: f32) -> bool {
        self.valid
            && self.bounds_contain(x, z)
            && self
                .edges
                .iter()
                .all(|e| !matches!(e.relation(x, z), EdgeRel::Outside))
    }
    #[inline]
    pub(crate) fn crossing(
        &self,
        x: f32,
        z: f32,
        ids: &[u32],
        mut neighbor: impl FnMut(u32) -> Plane,
    ) -> Option<f32> {
        if !self.valid || !self.bounds_contain(x, z) {
            return None;
        }
        for edge in &self.edges {
            match edge.relation(x, z) {
                EdgeRel::Inside | EdgeRel::OwnedBoundary => {}
                EdgeRel::Outside => return None,
                EdgeRel::UnownedBoundary => {
                    // A clockwise triangle of the same kind owns the segment only when it also
                    // contains the point. A wall that shares the XZ edge must not open a gap in
                    // the floor (ADR-8246 R1, spec 121 §5.3 rule 2).
                    let claimed = ids.iter().any(|&other| {
                        let plane = neighbor(other);
                        plane.kind == self.kind
                            && plane.contains_closed(x, z)
                            && plane.edges.iter().any(|e| e.claims_directed(edge))
                    });
                    if claimed {
                        return None;
                    }
                }
            }
        }
        self.height(self.anchor, x, z)
    }
}
/// Where a query point sits relative to one canonical edge.
#[derive(Clone, Copy)]
enum EdgeRel {
    /// Strictly on the triangle's side of the line.
    Inside,
    /// Strictly on the other side, or off the segment in a way the half-plane rejects.
    Outside,
    /// On the segment, and this triangle walks the edge in its canonical direction.
    OwnedBoundary,
    /// On the segment, and the clockwise neighbor owns the edge when that neighbor exists.
    UnownedBoundary,
}
/// Evaluate each undirected edge from its lexicographically smaller XZ endpoint. The interior
/// sign comes from the third vertex, so roof winding and authored floor winding use the same rule.
/// At zero, the clockwise XZ triangle that walks the edge in its canonical direction owns it.
/// This matches the lattice's west/south/diagonal decisions exactly.
#[derive(Clone, Copy, Debug)]
struct Edge {
    lo: [f32; 2],
    delta: [f32; 2],
    side: f32,
}
impl Edge {
    fn new(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> Self {
        let (lo, hi) = if a[0] < b[0] || (a[0] == b[0] && a[2] < b[2]) {
            (a, b)
        } else {
            (b, a)
        };
        let (dx, dz) = (hi[0] - lo[0], hi[2] - lo[2]);
        Self {
            lo: [lo[0], lo[2]],
            delta: [dx, dz],
            side: dx * (c[2] - lo[2]) - dz * (c[0] - lo[0]),
        }
    }
    #[inline]
    fn edge_fn(&self, x: f32, z: f32) -> f32 {
        self.delta[0] * (z - self.lo[1]) - self.delta[1] * (x - self.lo[0])
    }
    /// The point lies on the closed segment (exact `e == 0`, parameter in `0..=1`).
    #[inline]
    fn on_segment(&self, x: f32, z: f32) -> bool {
        let len2 = self.delta[0] * self.delta[0] + self.delta[1] * self.delta[1];
        if len2 == 0.0 || self.edge_fn(x, z) != 0.0 {
            return false;
        }
        let t = ((x - self.lo[0]) * self.delta[0] + (z - self.lo[1]) * self.delta[1]) / len2;
        (0.0..=1.0).contains(&t)
    }
    #[inline]
    fn relation(&self, x: f32, z: f32) -> EdgeRel {
        if self.side == 0.0 {
            return EdgeRel::Outside;
        }
        let e = self.edge_fn(x, z);
        if e == 0.0 && self.on_segment(x, z) {
            return if self.side < 0.0 {
                EdgeRel::OwnedBoundary
            } else {
                EdgeRel::UnownedBoundary
            };
        }
        // Half-plane, matching the previous predicate: side < 0 owns e <= 0, side > 0 owns e > 0.
        let inside = if self.side < 0.0 { e <= 0.0 } else { e > 0.0 };
        if inside { EdgeRel::Inside } else { EdgeRel::Outside }
    }
    /// This edge is the clockwise owner of `other` (same canonical segment, opposite side).
    #[inline]
    fn claims_directed(&self, other: &Edge) -> bool {
        self.side < 0.0
            && self.lo[0].to_bits() == other.lo[0].to_bits()
            && self.lo[1].to_bits() == other.lo[1].to_bits()
            && self.delta[0].to_bits() == other.delta[0].to_bits()
            && self.delta[1].to_bits() == other.delta[1].to_bits()
    }
}
/// Outside the triangle's XZ bounds, using millimetre min and max.
///
/// The millimetre-to-metre cast is monotone, so this matches the plane's f32 bounds for a finite
/// point. NaN does not match; callers already reject non-finite queries (ADR-8246 R7).
#[inline]
#[must_use]
pub(crate) fn xz_outside(origin: [i32; 2], v: [[i32; 3]; 3], x: f32, z: f32) -> bool {
    let (mut min_x, mut max_x) = (v[0][0], v[0][0]);
    let (mut min_z, mut max_z) = (v[0][2], v[0][2]);
    for p in &v[1..] {
        min_x = min_x.min(p[0]);
        max_x = max_x.max(p[0]);
        min_z = min_z.min(p[2]);
        max_z = max_z.max(p[2]);
    }
    x < mm_to_m(origin[0], min_x)
        || x > mm_to_m(origin[0], max_x)
        || z < mm_to_m(origin[1], min_z)
        || z > mm_to_m(origin[1], max_z)
}

/// Lexicographic key of a triangle's vertices in canonical integer millimetres (ADR-8246 R4).
#[must_use]
pub(crate) fn canonical_mm_key(v: [[f32; 3]; 3]) -> [[i32; 3]; 3] {
    let mut key = v.map(|p| [crate::m_to_mm(p[0]), crate::m_to_mm(p[1]), crate::m_to_mm(p[2])]);
    key.sort_unstable();
    key
}
pub(crate) fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
pub(crate) fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

#[derive(Clone, Debug)]
pub(crate) struct Grid {
    pub(crate) cell: f32,
    cells: Vec<([i32; 2], std::ops::Range<usize>)>,
    ids: Vec<u32>,
}
impl Grid {
    fn build(
        tris: &[GeomTriangle],
        ids: &[u32],
        settings: &GeomSettings,
        walls: bool,
    ) -> Result<Self, GeomLoadError> {
        let cell = settings.index_cell_m;
        let mut map: BTreeMap<[i32; 2], Vec<u32>> = BTreeMap::new();
        let mut entries = 0u64;
        for (slot, tri) in tris.iter().enumerate() {
            let wall = tri.kind == TriKind::Wall || tri.kind == TriKind::Blocker;
            if wall != walls {
                continue;
            }
            let pad = if wall { settings.wall_pad_m } else { 0.25 };
            let b = Plane::new(tri).bounds;
            let lo = [((b[0] - pad) / cell).floor(), ((b[2] - pad) / cell).floor()];
            let hi = [((b[1] + pad) / cell).floor(), ((b[3] + pad) / cell).floor()];
            if lo
                .iter()
                .chain(hi.iter())
                .any(|v| !v.is_finite() || v.abs() > 1_000_000.0)
            {
                return Err(GeomLoadError::Corrupt(
                    "query index exceeds its cell coordinate bound".into(),
                ));
            }
            let lo = lo.map(|v| v as i32);
            let hi = hi.map(|v| v as i32);
            entries += (i64::from(hi[0]) - i64::from(lo[0]) + 1) as u64
                * (i64::from(hi[1]) - i64::from(lo[1]) + 1) as u64;
            if entries > 16_000_000 {
                return Err(GeomLoadError::Corrupt(
                    "query index exceeds 16 million references".into(),
                ));
            }
            for z in lo[1]..=hi[1] {
                for x in lo[0]..=hi[0] {
                    map.entry([x, z]).or_default().push(ids[slot]);
                }
            }
        }
        let mut cells = Vec::with_capacity(map.len());
        let mut ids = Vec::with_capacity(entries as usize);
        for (key, list) in map {
            let start = ids.len();
            ids.extend(list);
            cells.push((key, start..ids.len()));
        }
        // No dense directory. A full-pitch map's directory is about 11 KB, which puts the resident
        // set over the 34_000_000 byte budget of the time (ADR-8246 R7; 35_000_000 since ADR-8396). `cells` is already in key order, so
        // `cell_at` binary-searches it. Answers do not depend on this accelerator.
        Ok(Self { cell, cells, ids })
    }

    /// The inclusive cell bounds `[[min x, min z], [max x, max z]]` of the non-empty cells, for the
    /// reference segment walk (`segment_without_pyramid`); `segment` walks the segment grid.
    #[cfg(any(test, feature = "stats"))]
    pub(crate) fn bounds(&self) -> Option<[[i32; 2]; 2]> {
        let mut bounds: Option<[[i32; 2]; 2]> = None;
        for (key, _) in &self.cells {
            match &mut bounds {
                Some(b) => {
                    for axis in 0..2 {
                        b[0][axis] = b[0][axis].min(key[axis]);
                        b[1][axis] = b[1][axis].max(key[axis]);
                    }
                }
                None => bounds = Some([*key, *key]),
            }
        }
        bounds
    }

    pub(crate) fn at(&self, x: f32, z: f32) -> &[u32] {
        if !x.is_finite() || !z.is_finite() {
            return &[];
        }
        self.cell_at([(x / self.cell).floor() as i32, (z / self.cell).floor() as i32])
    }
    pub(crate) fn cell_at(&self, key: [i32; 2]) -> &[u32] {
        self.cells
            .binary_search_by_key(&key, |c| c.0)
            .map_or(&[], |i| &self.ids[self.cells[i].1.clone()])
    }

    fn resident_bytes(&self) -> usize {
        self.cells.capacity() * std::mem::size_of::<([i32; 2], std::ops::Range<usize>)>()
            + self.ids.capacity() * 4
    }
}
#[derive(Clone, Debug)]
pub(crate) struct QueryIndex {
    pub(crate) columns: Grid,
    pub(crate) walls: Grid,
    /// The fine directory of the floors and roofs for the column queries (ADR-8817).
    pub(crate) fine: crate::query_column::ColumnIndex,
    /// `rank[tile][local]`: the triangle's place in canonical `(mm key, layer, index)` order, the
    /// order `push_disc` applies walls in (ADR-8246 R3). The keys are unique, so sorting by rank is
    /// sorting by key without decoding a triangle per comparison (ADR-8662).
    rank: Vec<Vec<u32>>,
}
impl QueryIndex {
    /// Builds column and wall grids. `ids[i]` is the packed id of `tris[i]` (`tile << 16 | local`).
    /// Planes are rebuilt per candidate so the runtime does not keep a second copy (ADR-8246 R7).
    ///
    /// # Errors
    ///
    /// `GeomLoadError::Corrupt` when the id list length disagrees or a cell coordinate overflows.
    pub(crate) fn build(
        tris: &[GeomTriangle],
        ids: &[u32],
        settings: &GeomSettings,
    ) -> Result<Self, GeomLoadError> {
        if tris.len() != ids.len() {
            return Err(GeomLoadError::Corrupt(
                "query index id count does not match the triangles".into(),
            ));
        }
        let columns = Grid::build(tris, ids, settings, false)?;
        let walls = Grid::build(tris, ids, settings, true)?;
        // The keys once, not once per comparison (load time, ADR-8816). They are unique (the layer
        // and the triangle index are), so the order is the one the keyed sort gave.
        let keys: Vec<_> = tris
            .iter()
            .map(|t| (canonical_mm_key(t.v), t.tri.layer, t.tri.triangle))
            .collect();
        let mut order: Vec<usize> = (0..tris.len()).collect();
        order.sort_unstable_by(|&a, &b| keys[a].cmp(&keys[b]));
        let tiles = ids.iter().map(|id| (id >> 16) as usize + 1).max().unwrap_or(0);
        let mut rank: Vec<Vec<u32>> = vec![Vec::new(); tiles];
        for (r, &i) in order.iter().enumerate() {
            let (tile, local) = ((ids[i] >> 16) as usize, (ids[i] & 0xffff) as usize);
            let row = &mut rank[tile];
            if row.len() <= local {
                row.resize(local + 1, u32::MAX);
            }
            row[local] = r as u32;
        }
        for row in &mut rank {
            row.shrink_to_fit();
        }
        let fine = crate::query_column::ColumnIndex::build(tris, ids);
        Ok(Self {
            columns,
            walls,
            fine,
            rank,
        })
    }
    /// The canonical-order rank of triangle `id` (`u32::MAX` for an id the index never saw).
    #[inline]
    pub(crate) fn rank(&self, id: u32) -> u32 {
        self.rank
            .get((id >> 16) as usize)
            .and_then(|row| row.get((id & 0xffff) as usize))
            .copied()
            .unwrap_or(u32::MAX)
    }
    pub(crate) fn resident_bytes(&self) -> usize {
        self.columns.resident_bytes()
            + self.walls.resident_bytes()
            + self.fine.resident_bytes()
            + self.rank.iter().map(|r| r.capacity() * 4).sum::<usize>()
            + self.rank.capacity() * std::mem::size_of::<Vec<u32>>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TriRef;

    /// The fast length cut agrees with `sqrt(len2) >= 1e-8` across positive, negative, and special values.
    #[expect(
        clippy::disallowed_methods,
        reason = "ADR-8246: this test compares the fast cut against IEEE sqrt, which is the contract"
    )]
    #[test]
    fn plane_length_cut_matches_ieee_sqrt() {
        let mut bits = 0u32;
        loop {
            let len2 = f32::from_bits(bits);
            let old = len2.sqrt() >= 1e-8;
            let new = if len2 >= 1.0 { true } else { len2.sqrt() >= 1e-8 };
            assert_eq!(old, new, "len2 bits {bits:#x}");
            let Some(next) = bits.checked_add(4096) else {
                break;
            };
            bits = next;
        }
        for special in [0.0f32, -0.0, 1.0, f32::INFINITY, f32::NEG_INFINITY, f32::NAN] {
            let len2 = special;
            let old = len2.sqrt() >= 1e-8;
            let new = if len2 >= 1.0 { true } else { len2.sqrt() >= 1e-8 };
            assert_eq!(old, new);
        }
    }

    #[test]
    fn integer_xz_bounds_match_the_plane_for_finite_points() {
        let origins = [[0, 0], [1_024_000, -2_048_000], [-3_000, 4_000]];
        let locals = [
            [[0, 0, 0], [2_000, 0, 0], [0, 500, 3_000]],
            [[-50, 10, 20], [10, -5, -20], [4_000, 0, 4_000]],
            [[100, 0, 100], [100, 0, 100], [500, 1_000, 100]],
        ];
        for origin in origins {
            for local in locals {
                let v = local.map(|p| {
                    [
                        mm_to_m(origin[0], p[0]),
                        mm_to_m(0, p[1]),
                        mm_to_m(origin[1], p[2]),
                    ]
                });
                let tri = GeomTriangle {
                    v,
                    tri: TriRef {
                        layer: 0,
                        triangle: 0,
                    },
                    kind: TriKind::Floor,
                    flags: 0,
                    section: None,
                    material: 0,
                };
                let plane = Plane::new(&tri);
                let min_x = mm_to_m(origin[0], local[0][0].min(local[1][0]).min(local[2][0]));
                let max_x = mm_to_m(origin[0], local[0][0].max(local[1][0]).max(local[2][0]));
                let min_z = mm_to_m(origin[1], local[0][2].min(local[1][2]).min(local[2][2]));
                let max_z = mm_to_m(origin[1], local[0][2].max(local[1][2]).max(local[2][2]));
                let mut xs = [min_x, max_x, 0.0, 1.5];
                let mut zs = [min_z, max_z, 0.0, -2.0];
                xs[2] = f32::from_bits(min_x.to_bits().wrapping_sub(1));
                xs[3] = f32::from_bits(max_x.to_bits().wrapping_add(1));
                zs[2] = f32::from_bits(min_z.to_bits().wrapping_sub(1));
                zs[3] = f32::from_bits(max_z.to_bits().wrapping_add(1));
                for x in xs {
                    for z in zs {
                        if !x.is_finite() || !z.is_finite() {
                            continue;
                        }
                        assert_eq!(
                            xz_outside(origin, local, x, z),
                            !plane.bounds_contain(x, z),
                            "origin {origin:?} x={x} z={z}"
                        );
                    }
                }
            }
        }
    }
}
