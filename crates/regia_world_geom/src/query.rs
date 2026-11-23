// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-content/src/surface.rs and ir-sim/src/tunnels.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 122-270, mark: adapt (Stage B).
// Edits: typed hits, settings, 16-entry buffers, overflow reporting, canonical ownership and lattice ground.
//! Native, allocation-free column queries (spec 121 §5, ADR-8246).

use crate::cooked::{TRI_FLAG_NO_SUPPORT, TriKind};
/// The mutable sim state every query takes (defined in [`crate::edit`]; re-exported at its old path).
pub use crate::edit::GeomState;
use crate::ground::{GroundLattice, GroundTri, vertex_m};
use crate::query_column::{CellList, NearTri};
use crate::query_index::{Plane, QueryIndex, canonical_mm_key, reported_normal, unit_up, xz_outside};
use crate::{TriRef, WorldGeom};
use regia_detsim::float::{det_clamp, det_max, det_min};

/// The kind of a queried surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceKind {
    /// Uncut lattice ground.
    Ground,
    /// A layer floor.
    Floor,
    /// A layer roof.
    Roof,
    /// A layer wall.
    Wall,
    /// A blocker plug.
    Blocker,
}
impl From<TriKind> for SurfaceKind {
    fn from(kind: TriKind) -> Self {
        match kind {
            TriKind::Floor => Self::Floor,
            TriKind::Roof => Self::Roof,
            TriKind::Wall => Self::Wall,
            TriKind::Blocker => Self::Blocker,
        }
    }
}
/// Dense cooked section identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub struct SectionId(pub u16);
/// A surface crossing a vertical query line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceHit {
    /// Height in meters.
    pub y: f32,
    /// Unit normal, facing upward for a floor or roof.
    pub normal: [f32; 3],
    /// Surface class.
    pub kind: SurfaceKind,
    /// Canonical reference; absent for ground.
    pub tri: Option<TriRef>,
    /// Owning section; absent outdoors.
    pub section: Option<SectionId>,
    /// Footstep material slot.
    pub material: u16,
}
const EMPTY_HIT: SurfaceHit = SurfaceHit {
    y: 0.0,
    normal: [0.0, 1.0, 0.0],
    kind: SurfaceKind::Ground,
    tri: None,
    section: None,
    material: 0,
};
/// A free vertical interval, floor and optional lid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
    /// Supporting surface.
    pub floor: SurfaceHit,
    /// Lowest lid more than the configured gap above the floor.
    pub ceiling: Option<SurfaceHit>,
}
/// The interval containing a query point, with T3's nearest-rock-face rule.
pub type Support = Span;
const EMPTY_SPAN: Span = Span {
    floor: EMPTY_HIT,
    ceiling: None,
};
/// Fixed-capacity column. Overflow is reported, never silently discarded.
#[derive(Clone, Debug)]
pub struct SpanBuf {
    spans: [Span; 16],
    len: usize,
    /// More than 16 distinct floors or lids crossed this column.
    pub overflowed: bool,
}
impl Default for SpanBuf {
    fn default() -> Self {
        Self {
            spans: [EMPTY_SPAN; 16],
            len: 0,
            overflowed: false,
        }
    }
}
impl SpanBuf {
    /// Spans in ascending floor height, stable on equal heights.
    #[must_use]
    pub fn spans(&self) -> &[Span] {
        &self.spans[..self.len]
    }
    /// T3's span selection, or `None` in a completely empty cut column.
    #[must_use]
    pub fn span_at(&self, y: f32) -> Option<Span> {
        let spans = self.spans();
        let mut best: Option<usize> = None;
        let mut above: Option<usize> = None;
        for (i, span) in spans.iter().enumerate() {
            let fy = span.floor.y;
            if fy <= y {
                if best.is_none_or(|b| fy > spans[b].floor.y || span.floor.kind == SurfaceKind::Ground) {
                    best = Some(i);
                }
            } else if above.is_none_or(|a| fy == spans[a].floor.y && span.floor.kind == SurfaceKind::Ground) {
                above = Some(i);
            }
        }
        let i = match (best, above) {
            (Some(b), Some(a)) => match spans[b].ceiling {
                Some(lid) if y >= lid.y && y - lid.y > spans[a].floor.y - y => a,
                _ => b,
            },
            (Some(b), None) => b,
            (None, Some(a)) => a,
            (None, None) => return None,
        };
        Some(spans[i])
    }
}
/// One roof a ceiling stop may meet: its XZ bounds as `xz_outside` reads them
/// (`[min x, max x, min z, max z]`, metres) and its plane.
struct RoofCandidate {
    bounds: [f32; 4],
    plane: Plane,
}

/// The roofs of one column-grid cell that reach an edit's footprint.
struct RoofCell<'a> {
    key: [i32; 2],
    /// Every triangle the cell lists: the edge-ownership test of a crossing reads them all.
    ids: &'a [u32],
    roofs: Vec<RoofCandidate>,
}

/// The D1 ceiling stop over one edit's footprint (spec 121 § 6, ADR-8818): the answer of
/// `WorldGeom::highest_roof_at_or_below` at the footprint's lattice vertices, row by row.
///
/// The per-point form finds its column cell, then decodes and builds the plane of every roof
/// whose box holds the point, for every vertex. Here a cell is found once per run of columns, a
/// roof's plane is built once per edit, a roof whose box misses the footprint is dropped once, and
/// a roof whose z range misses the row is dropped once per row. Each drop is the per-point form's
/// own box test (`xz_outside`) on `f32` bounds, so the same roofs reach the same
/// `Plane::crossing` with the same cell list, in the same order.
pub(crate) struct RoofStops<'a> {
    geom: &'a WorldGeom,
    /// The footprint's column and row positions, metres, ascending.
    xs: &'a [f32],
    zs: &'a [f32],
    /// The column grid's cell x index of every footprint column.
    kx: [i32; crate::edit::MAX_EDIT_SIDE as usize],
    cells: Vec<RoofCell<'a>>,
    /// The current row's z and cell row.
    z: f32,
    kz: i32,
    /// The cell under the last query on this row (`usize::MAX` before the first), and its roofs
    /// whose z range holds the row.
    cell: usize,
    active: Vec<u32>,
}

impl<'a> RoofStops<'a> {
    /// The stops of a footprint whose columns are at `xs` and rows at `zs` (finite, ascending, at
    /// most `MAX_EDIT_SIDE` each). Reads nothing until the first query.
    pub(crate) fn new(geom: &'a WorldGeom, xs: &'a [f32], zs: &'a [f32]) -> Self {
        let cell = geom.query_index().columns.cell;
        let mut kx = [0i32; crate::edit::MAX_EDIT_SIDE as usize];
        for (k, x) in kx.iter_mut().zip(xs) {
            *k = (x / cell).floor() as i32;
        }
        Self {
            geom,
            xs,
            zs,
            kx,
            cells: Vec::new(),
            z: 0.0,
            kz: 0,
            cell: usize::MAX,
            active: Vec::new(),
        }
    }

    /// Moves to footprint row `row` (an index into `zs`).
    pub(crate) fn begin_row(&mut self, row: usize) {
        self.z = self.zs.get(row).copied().unwrap_or(f32::NAN);
        self.kz = (self.z / self.geom.query_index().columns.cell).floor() as i32;
        self.cell = usize::MAX;
    }

    /// The highest roof at footprint column `col` of the current row with its height at or below
    /// `y`, metres.
    #[inline]
    pub(crate) fn highest_at_or_below(&mut self, col: usize, y: f32) -> Option<f32> {
        let (&x, &kx) = (self.xs.get(col)?, self.kx.get(col)?);
        let key = [kx, self.kz];
        if self.cells.get(self.cell).is_none_or(|c| c.key != key) {
            self.enter(key);
        }
        let cell = self.cells.get(self.cell)?;
        let mut best: Option<f32> = None;
        for &k in &self.active {
            let roof = &cell.roofs[k as usize];
            if x < roof.bounds[0] || x > roof.bounds[1] {
                continue;
            }
            if let Some(ry) = roof
                .plane
                .crossing(x, self.z, cell.ids, |other| self.geom.plane_of(other))
                && ry <= y
                && best.is_none_or(|b| ry > b)
            {
                best = Some(ry);
            }
        }
        best
    }

    /// Makes `key` the current cell (building its roof list on first use) and lists its roofs
    /// whose z range holds the current row.
    fn enter(&mut self, key: [i32; 2]) {
        self.cell = match self.cells.iter().position(|c| c.key == key) {
            Some(at) => at,
            None => {
                let cell = self.build(key);
                self.cells.push(cell);
                self.cells.len() - 1
            }
        };
        self.active.clear();
        self.active.reserve(self.cells[self.cell].roofs.len());
        let z = self.z;
        for (k, roof) in self.cells[self.cell].roofs.iter().enumerate() {
            if !(z < roof.bounds[2] || z > roof.bounds[3]) {
                self.active.push(k as u32);
            }
        }
    }

    /// The roofs of cell `key` whose box reaches the footprint's box, in the cell's own order.
    fn build(&self, key: [i32; 2]) -> RoofCell<'a> {
        let ids = self.geom.query_index().columns.cell_at(key);
        let (Some(&x_lo), Some(&x_hi), Some(&z_lo), Some(&z_hi)) =
            (self.xs.first(), self.xs.last(), self.zs.first(), self.zs.last())
        else {
            return RoofCell {
                key,
                ids,
                roofs: Vec::new(),
            };
        };
        // A roof's box as `xz_outside` compares a point with it, in that function's arithmetic,
        // when it reaches the footprint. Every footprint vertex is inside
        // [x_lo, x_hi] x [z_lo, z_hi]: a box wholly beyond one side is outside at each of them.
        let reaching = |id: u32| -> Option<[f32; 4]> {
            let tile = &self.geom.cooked().tiles[(id >> 16) as usize];
            let tri = &tile.triangles[(id & 0xffff) as usize];
            if tri.kind != TriKind::Roof {
                return None;
            }
            let (mut min_x, mut max_x) = (tri.v[0][0], tri.v[0][0]);
            let (mut min_z, mut max_z) = (tri.v[0][2], tri.v[0][2]);
            for p in &tri.v[1..] {
                min_x = min_x.min(p[0]);
                max_x = max_x.max(p[0]);
                min_z = min_z.min(p[2]);
                max_z = max_z.max(p[2]);
            }
            let bounds = [
                crate::mm_to_m(tile.origin_mm[0], min_x),
                crate::mm_to_m(tile.origin_mm[0], max_x),
                crate::mm_to_m(tile.origin_mm[1], min_z),
                crate::mm_to_m(tile.origin_mm[1], max_z),
            ];
            let beyond = x_hi < bounds[0] || x_lo > bounds[1] || z_hi < bounds[2] || z_lo > bounds[3];
            (!beyond).then_some(bounds)
        };
        // Counted first, so the list is one heap request.
        let count = ids.iter().filter(|&&id| reaching(id).is_some()).count();
        let mut roofs = Vec::with_capacity(count);
        for &id in ids {
            if let Some(bounds) = reaching(id) {
                roofs.push(RoofCandidate {
                    bounds,
                    plane: self.geom.plane_of(id),
                });
            }
        }
        RoofCell { key, ids, roofs }
    }
}

/// Raw presentation surfaces near a height.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SurfaceAt {
    /// Highest supporting surface no higher than the requested height plus tolerance.
    pub under: Option<SurfaceHit>,
    /// Lowest roof above the requested height.
    pub over: Option<SurfaceHit>,
}
/// Vertical wall-contact interval, meters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Band {
    /// Bottom height.
    pub lo: f32,
    /// Top height.
    pub hi: f32,
}
/// Result of a sequential wall push.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Push {
    /// Final horizontal center.
    pub xz: [f32; 2],
    /// Number of wall contacts that moved the center.
    pub contacts: u16,
    /// Last wall that moved the center.
    pub last: Option<TriRef>,
}
/// Ceiling confinement without any host velocity or contact-state policy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CeilingClamp {
    /// Clamped origin height.
    pub y: f32,
    /// Lid that moved the origin; `None` if it did not move.
    pub contact: Option<SurfaceHit>,
}
/// Query work counters, enabled only in tests or with `stats`.
#[cfg(any(test, feature = "stats"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryStats {
    /// Canonical triangles whose projected edge functions were evaluated.
    pub triangle_tests: u32,
}

impl WorldGeom {
    pub(crate) fn query_index(&self) -> &QueryIndex {
        &self.query_index
    }
    fn plane_of(&self, id: u32) -> Plane {
        let tri = self.tri(id);
        Plane::new(&tri)
    }
    fn mm_key(&self, id: u32) -> [[i32; 3]; 3] {
        canonical_mm_key(self.tri(id).v)
    }
    pub(crate) fn hit(&self, id: u32, y: f32) -> SurfaceHit {
        let tri = self.tri(id);
        SurfaceHit {
            y,
            normal: reported_normal(tri.v),
            kind: tri.kind.into(),
            tri: Some(tri.tri),
            section: tri.section.map(SectionId),
            material: tri.material,
        }
    }
    /// [`WorldGeom::hit`] for a column crossing: with the plane normal the fast path already
    /// built, the triangle is not converted again (ADR-8817). Same hit, bit for bit:
    /// `reported_normal` is `unit_up` of the same `cross_n`.
    #[inline]
    fn hit_of(&self, c: Crossing) -> SurfaceHit {
        if c.n[1] == 0.0 {
            return self.hit(c.id, c.y);
        }
        let tri = &self.cooked().tiles[(c.id >> 16) as usize].triangles[(c.id & 0xffff) as usize];
        SurfaceHit {
            y: c.y,
            normal: unit_up(c.n),
            kind: tri.kind.into(),
            tri: Some(TriRef {
                layer: tri.layer,
                triangle: tri.index,
            }),
            section: tri.section.map(SectionId),
            material: tri.material,
        }
    }
    /// Kind, support flag, and the integer-millimetre XZ box.
    ///
    /// A finite point rejected here would also fail the plane bounds test. The plane, including
    /// its edges, is built only for a candidate that can contain the point (ADR-8246 R7).
    #[inline]
    fn column_skip(&self, id: u32, x: f32, z: f32, kind: TriKind, support_only: bool) -> bool {
        let tile = &self.cooked().tiles[(id >> 16) as usize];
        let tri = &tile.triangles[(id & 0xffff) as usize];
        tri.kind != kind
            || (support_only && tri.flags & TRI_FLAG_NO_SUPPORT != 0)
            || xz_outside(tile.origin_mm, tri.v, x, z)
    }
    #[inline]
    fn ground_hit(&self, state: &GeomState, x: f32, z: f32, ignore_cut: bool) -> Option<SurfaceHit> {
        let tri = match self
            .ground_coordinates
            .triangle(self.cooked(), &state.ground, x, z)
        {
            GroundTri::Tri(tri) => tri,
            _ if ignore_cut => {
                // Built only on this path: its constructor divides (ADR-8817).
                let lattice = GroundLattice::new(self.cooked()).with_edits(&state.ground);
                let c = lattice.locate(x, z)?;
                // The fallback is used only for invalid/off-map or malformed empty-cut queries.
                let y = lattice.height_in_tile(lattice.tile_of_cell(c.i, c.j), x, z)?;
                return Some(SurfaceHit { y, ..EMPTY_HIT });
            }
            _ => return None,
        };
        let (y, normal) = Plane::ground_crossing(tri, x, z)?;
        Some(SurfaceHit {
            y,
            normal,
            ..EMPTY_HIT
        })
    }
    fn fallback(&self, state: &GeomState, x: f32, z: f32) -> SurfaceHit {
        let l = &self.cooked().lattice;
        let edge = |v: f32| {
            det_clamp(
                if v.is_nan() { 0.0 } else { v },
                vertex_m(l, 0),
                vertex_m(l, l.cells()),
            )
        };
        self.ground_hit(state, edge(x), edge(z), true)
            .unwrap_or(EMPTY_HIT)
    }
    /// All floors and their lids. The output buffer is reset on each call.
    pub fn floors_at(&self, state: &GeomState, x: f32, z: f32, out: &mut SpanBuf) {
        self.column(state, x, z, out, &mut 0);
    }
    /// Column query with an explicit work counter; does not use atomics or shared mutable state.
    #[cfg(any(test, feature = "stats"))]
    pub fn floors_at_with_stats(
        &self,
        state: &GeomState,
        x: f32,
        z: f32,
        out: &mut SpanBuf,
        stats: &mut QueryStats,
    ) {
        self.column(state, x, z, out, &mut stats.triangle_tests);
    }
    /// [`WorldGeom::floors_at_with_stats`] through the per-call form of the column: every candidate
    /// of the column grid's cell, converted and boxed per call (ADR-8246). The fast path (ADR-8817)
    /// must return these spans, this overflow flag and this count, bit for bit; the tests compare
    /// the two.
    #[cfg(any(test, feature = "stats"))]
    pub fn floors_at_reference(
        &self,
        state: &GeomState,
        x: f32,
        z: f32,
        out: &mut SpanBuf,
        stats: &mut QueryStats,
    ) {
        let column = self.crossings_reference(state, x, z, &mut stats.triangle_tests);
        self.fill(&column, out);
    }
    /// [`WorldGeom::support`] through the per-call form of the column (see
    /// [`WorldGeom::floors_at_reference`]).
    #[cfg(any(test, feature = "stats"))]
    #[must_use]
    pub fn support_reference(&self, state: &GeomState, x: f32, z: f32, y: f32) -> Support {
        if !y.is_finite() {
            return Span {
                floor: self.fallback(state, x, z),
                ceiling: None,
            };
        }
        let column = self.crossings_reference(state, x, z, &mut 0);
        column.select(self, y).unwrap_or_else(|| Span {
            floor: self.fallback(state, x, z),
            ceiling: None,
        })
    }
    fn column(&self, state: &GeomState, x: f32, z: f32, out: &mut SpanBuf, tests: &mut u32) {
        let column = self.crossings(state, x, z, tests);
        self.fill(&column, out);
    }
    fn fill(&self, column: &Column, out: &mut SpanBuf) {
        out.len = column.len;
        out.overflowed = column.overflowed;
        for i in 0..column.len {
            out.spans[i] = column.span(self, i);
        }
    }
    /// The ids of the floors and roofs filed at `(x, z)` whose XZ box holds the point, in `TriRef`
    /// order, written to `near`: the fine directory's cell and its box table (ADR-8817), or, on a
    /// map without a directory, the column grid's cell boxed per candidate. `None` when more than
    /// [`NEAR`] hold it. `x` and `z` finite.
    #[inline]
    fn column_near(&self, x: f32, z: f32, near: &mut [u32; NEAR]) -> Option<usize> {
        let index = self.query_index();
        let mut n = 0usize;
        if index.fine.enabled() {
            let fine = &index.fine;
            let mut take = |position: u32| -> bool {
                let boxed = fine.boxed(position);
                if boxed.holds(x, z) {
                    if n == NEAR {
                        return false;
                    }
                    near[n] = boxed.id;
                    n += 1;
                }
                true
            };
            let fits = match fine.at(x, z) {
                CellList::Narrow(list) => list.iter().all(|&p| take(u32::from(p))),
                CellList::Wide(list) => list.iter().all(|&p| take(p)),
            };
            return fits.then_some(n);
        }
        let tiles = &self.cooked().tiles;
        for &id in index.columns.at(x, z) {
            let tile = &tiles[(id >> 16) as usize];
            if xz_outside(tile.origin_mm, tile.triangles[(id & 0xffff) as usize].v, x, z) {
                continue;
            }
            if n == NEAR {
                return None;
            }
            near[n] = id;
            n += 1;
        }
        Some(n)
    }
    /// `f(height, id)` for every layer roof crossing the vertical line at `(x, z)`, in `TriRef`
    /// order. Nothing for a non-finite point.
    #[inline]
    fn roof_crossings(&self, x: f32, z: f32, mut f: impl FnMut(f32, u32)) {
        if !x.is_finite() || !z.is_finite() {
            return;
        }
        let mut near = [0u32; NEAR];
        if let Some(n) = self.column_near(x, z, &mut near) {
            if n == 0 {
                return;
            }
            // The roofs among the box hits, each evaluated once; an edge's owner is a roof.
            let tiles = &self.cooked().tiles;
            let mut hits = [NearTri::EMPTY; NEAR];
            let mut roofs = 0usize;
            for k in 0..n {
                let id = near[k];
                let tile = &tiles[(id >> 16) as usize];
                let tri = &tile.triangles[(id & 0xffff) as usize];
                if tri.kind == TriKind::Roof {
                    near[roofs] = id;
                    hits[roofs] = NearTri::at(tile.origin_mm, tri, x, z);
                    roofs += 1;
                }
            }
            let hits = &hits[..roofs];
            for (hit, &id) in hits.iter().zip(&near[..roofs]) {
                if let Some(y) = hit.crossing(x, z, hits) {
                    f(y, id);
                }
            }
            return;
        }
        let ids = self.query_index().columns.at(x, z);
        for &id in ids {
            if self.column_skip(id, x, z, TriKind::Roof, false) {
                continue;
            }
            let plane = self.plane_of(id);
            if let Some(y) = plane.crossing(x, z, ids, |other| self.plane_of(other)) {
                f(y, id);
            }
        }
    }
    #[inline]
    fn crossings(&self, state: &GeomState, x: f32, z: f32, tests: &mut u32) -> Column {
        if !x.is_finite() || !z.is_finite() {
            return Column::default();
        }
        let mut near = [0u32; NEAR];
        let mut out = Column::default();
        let fast = match self.column_near(x, z, &mut near) {
            // The common column: a floor, a roof and the triangles around them.
            Some(n) if n <= NEAR_SMALL => {
                self.crossings_fast::<NEAR_SMALL>(state, x, z, &near[..n], &mut out)
            }
            Some(n) if n <= NEAR_MID => self.crossings_fast::<NEAR_MID>(state, x, z, &near[..n], &mut out),
            Some(n) => self.crossings_fast::<NEAR>(state, x, z, &near[..n], &mut out),
            None => None,
        };
        match fast {
            Some(tested) => {
                *tests += tested;
                out
            }
            None => self.crossings_reference(state, x, z, tests),
        }
    }
    /// The column through fixed small buffers: the box hits `near` (at most `N`) and at most
    /// [`FAST_FLOORS`] floor crossings (ADR-8817). One pass over the box hits takes roofs and
    /// floors in `TriRef` order each, which is the order [`WorldGeom::crossings_reference`]
    /// collects them in; an edge's owner is looked for among the box hits, which hold every
    /// triangle that can own it. `None` for a column with more floors; the caller then runs the
    /// reference form (which starts from its own column). Fills `out`, a fresh column, and returns
    /// the number of plane tests.
    #[inline]
    fn crossings_fast<const N: usize>(
        &self,
        state: &GeomState,
        x: f32,
        z: f32,
        near: &[u32],
        out: &mut Column,
    ) -> Option<u32> {
        let mut tests = 0u32;
        let mut raw = [Crossing::default(); FAST_FLOORS];
        let mut raw_n = 0usize;
        if !near.is_empty() {
            // Each box hit is evaluated once per call. A point on a shared edge or vertex asks every
            // box hit whether it owns that edge, for each triangle around the point; the per-call
            // form rebuilt the asked plane each time.
            let tiles = &self.cooked().tiles;
            // Each slot is written once: a hit, or the empty slot past the hits.
            let hits: [NearTri; N] = std::array::from_fn(|k| match near.get(k) {
                Some(&id) => {
                    let tile = &tiles[(id >> 16) as usize];
                    NearTri::at(tile.origin_mm, &tile.triangles[(id & 0xffff) as usize], x, z)
                }
                None => NearTri::EMPTY,
            });
            let hits = &hits[..near.len().min(N)];
            for (hit, &id) in hits.iter().zip(near) {
                let roof = match hit.kind {
                    TriKind::Roof => true,
                    TriKind::Floor if hit.flags & TRI_FLAG_NO_SUPPORT == 0 => false,
                    _ => continue,
                };
                tests += 1;
                let Some(y) = hit.crossing(x, z, hits) else {
                    continue;
                };
                let crossing = Crossing {
                    y,
                    id,
                    n: hit.plane_n(),
                };
                if !roof {
                    if raw_n == FAST_FLOORS {
                        return None;
                    }
                    raw[raw_n] = crossing;
                    raw_n += 1;
                } else if out.lid_len == 16 {
                    out.overflowed = true;
                } else {
                    out.lids[out.lid_len] = crossing;
                    out.lid_len += 1;
                }
            }
        }
        // Active blocker plugs, as in the reference form: lids after the roofs, floors after the
        // layer floors.
        if state.any_blocker_active() {
            let mut full = false;
            self.blocker_crossings(state, x, z, &mut tests, |c, up| {
                if up {
                    if raw_n == FAST_FLOORS {
                        full = true;
                    } else {
                        raw[raw_n] = c;
                        raw_n += 1;
                    }
                } else if out.lid_len == 16 {
                    out.overflowed = true;
                } else {
                    out.lids[out.lid_len] = c;
                    out.lid_len += 1;
                }
            });
            if full {
                return None;
            }
        }
        match raw_n {
            0 => {}
            1 => out.insert(raw[0]),
            _ => self.cluster_floors(&raw[..raw_n], out),
        }
        if let Some(ground) = self.ground_hit(state, x, z, false) {
            out.ground = ground;
            out.insert(Crossing {
                y: ground.y,
                id: u32::MAX,
                n: [0.0; 3],
            });
        }
        Some(tests)
    }
    /// Near-equal floors collapse by canonical millimetre key, never by visit order (ADR-8246 R4):
    /// the reference form's clustering for at most [`FAST_FLOORS`] crossings. A key is built only
    /// when two floors share a cluster (ADR-8817); a lone floor needs none.
    fn cluster_floors(&self, raw: &[Crossing], out: &mut Column) {
        let n = raw.len().min(FAST_FLOORS);
        let mut parent = [0u8; FAST_FLOORS];
        for (i, slot) in parent.iter_mut().enumerate() {
            *slot = i as u8;
        }
        let same = self.settings().same_height_m;
        for i in 0..n {
            for j in i + 1..n {
                if (raw[i].y - raw[j].y).abs() <= same {
                    let ra = find_root(&mut parent, i);
                    let rb = find_root(&mut parent, j);
                    if ra != rb {
                        parent[ra.max(rb)] = ra.min(rb) as u8;
                    }
                }
            }
        }
        let mut emitted = [false; FAST_FLOORS];
        for i in 0..n {
            let root = find_root(&mut parent, i);
            if emitted[root] {
                continue;
            }
            emitted[root] = true;
            let mut best = i;
            let mut best_key = None;
            for j in 0..n {
                if j != best && find_root(&mut parent, j) == root {
                    let key = self.mm_key(raw[j].id);
                    if key < *best_key.get_or_insert_with(|| self.mm_key(raw[best].id)) {
                        best = j;
                        best_key = Some(key);
                    }
                }
            }
            out.insert(raw[best]);
        }
    }
    /// The column in its per-call form (ADR-8246): every candidate of the 16 m column cell, 64 raw
    /// floors with their keys. The fast path's fallback, and what the tests compare it with.
    fn crossings_reference(&self, state: &GeomState, x: f32, z: f32, tests: &mut u32) -> Column {
        let mut out = Column::default();
        if !x.is_finite() || !z.is_finite() {
            return out;
        }
        let ids = self.query_index().columns.at(x, z);
        for &id in ids {
            if self.column_skip(id, x, z, TriKind::Roof, false) {
                continue;
            }
            let plane = self.plane_of(id);
            *tests += 1;
            if let Some(y) = plane.crossing(x, z, ids, |other| self.plane_of(other)) {
                if out.lid_len == 16 {
                    out.overflowed = true;
                } else {
                    out.lids[out.lid_len] = Crossing { y, id, n: [0.0; 3] };
                    out.lid_len += 1;
                }
            }
        }
        // Active blocker plugs (spec 121 § 5.3 rule 6): a down-facing plug face is a lid, an
        // up-facing one a floor. One bit test per plug candidate; none at all while every plug is
        // clear, so that state answers exactly as a map without plugs.
        let mut blocker_floors = [Crossing::default(); 16];
        let mut blocker_floor_n = 0usize;
        if state.any_blocker_active() {
            self.blocker_crossings(state, x, z, tests, |c, up| {
                if up {
                    if blocker_floor_n == 16 {
                        out.overflowed = true;
                    } else {
                        blocker_floors[blocker_floor_n] = c;
                        blocker_floor_n += 1;
                    }
                } else if out.lid_len == 16 {
                    out.overflowed = true;
                } else {
                    out.lids[out.lid_len] = c;
                    out.lid_len += 1;
                }
            });
        }
        // Near-equal floors collapse by canonical millimetre key, never by visit order (ADR-8246 R4).
        const RAW: usize = 64;
        let mut raw = [Crossing::default(); RAW];
        let mut keys = [[[0i32; 3]; 3]; RAW];
        let mut raw_n = 0usize;
        for &id in ids {
            if self.column_skip(id, x, z, TriKind::Floor, true) {
                continue;
            }
            let plane = self.plane_of(id);
            *tests += 1;
            let Some(y) = plane.crossing(x, z, ids, |other| self.plane_of(other)) else {
                continue;
            };
            push_raw(
                &mut raw,
                &mut keys,
                &mut raw_n,
                &mut out.overflowed,
                Crossing { y, id, n: [0.0; 3] },
                self.mm_key(id),
            );
        }
        for &c in &blocker_floors[..blocker_floor_n] {
            push_raw(
                &mut raw,
                &mut keys,
                &mut raw_n,
                &mut out.overflowed,
                c,
                self.mm_key(c.id),
            );
        }
        let mut parent = [0u8; RAW];
        for (i, slot) in parent.iter_mut().enumerate().take(raw_n) {
            *slot = i as u8;
        }
        let same = self.settings().same_height_m;
        for i in 0..raw_n {
            for j in i + 1..raw_n {
                if (raw[i].y - raw[j].y).abs() <= same {
                    let ra = find_root(&mut parent, i);
                    let rb = find_root(&mut parent, j);
                    if ra != rb {
                        parent[ra.max(rb)] = ra.min(rb) as u8;
                    }
                }
            }
        }
        let mut emitted = [false; RAW];
        for i in 0..raw_n {
            let root = find_root(&mut parent, i);
            if emitted[root] {
                continue;
            }
            emitted[root] = true;
            let mut best = i;
            for j in 0..raw_n {
                if find_root(&mut parent, j) == root && keys[j] < keys[best] {
                    best = j;
                }
            }
            out.insert(raw[best]);
        }
        if let Some(ground) = self.ground_hit(state, x, z, false) {
            out.ground = ground;
            out.insert(Crossing {
                y: ground.y,
                id: u32::MAX,
                n: [0.0; 3],
            });
        }
        out
    }
    /// T3 support selection. The caller supplies its own waist/reach height.
    #[must_use]
    pub fn support(&self, state: &GeomState, x: f32, z: f32, y: f32) -> Support {
        if !y.is_finite() {
            return Span {
                floor: self.fallback(state, x, z),
                ceiling: None,
            };
        }
        let column = self.crossings(state, x, z, &mut 0);
        column.select(self, y).unwrap_or_else(|| Span {
            floor: self.fallback(state, x, z),
            ceiling: None,
        })
    }

    /// Highest floor below `near_y + tolerance_m`, and lowest lid above `near_y`.
    #[must_use]
    pub fn surface_at(&self, state: &GeomState, x: f32, z: f32, near_y: f32, tolerance_m: f32) -> SurfaceAt {
        if ![x, z, near_y, tolerance_m].iter().all(|v| v.is_finite()) {
            return SurfaceAt::default();
        }
        let mut spans = SpanBuf::default();
        self.floors_at(state, x, z, &mut spans);
        let mut under = None;
        for span in spans.spans() {
            if span.floor.y <= near_y + tolerance_m {
                under = Some(span.floor);
            }
        }
        SurfaceAt {
            under,
            over: self.lowest_roof(state, x, z, near_y),
        }
    }
    /// Lowest roof more than `lid_gap_m` above a floor, in canonical tie order.
    #[must_use]
    pub fn ceiling_above(&self, state: &GeomState, x: f32, z: f32, floor_y: f32) -> Option<SurfaceHit> {
        if ![x, z, floor_y].iter().all(|v| v.is_finite()) {
            return None;
        }
        self.lowest_roof(state, x, z, floor_y + self.settings().lid_gap_m)
    }
    /// The highest layer roof crossing the vertical line at `(x, z)` with its height at or below
    /// `y`, metres: the D1 ceiling stop (spec 121 § 6, the § 5.3 plane height). Blocker plugs are
    /// not roofs. One point at a time; an edit asks through [`RoofStops`], and this stays as the
    /// reference walk's form (ADR-8818).
    #[cfg(any(test, feature = "stats"))]
    pub(crate) fn highest_roof_at_or_below(&self, x: f32, z: f32, y: f32) -> Option<f32> {
        let mut best: Option<f32> = None;
        self.roof_crossings(x, z, |ry, _| {
            if ry <= y && best.is_none_or(|b| ry > b) {
                best = Some(ry);
            }
        });
        best
    }
    fn lowest_roof(&self, state: &GeomState, x: f32, z: f32, threshold: f32) -> Option<SurfaceHit> {
        let mut best: Option<(f32, u32)> = None;
        let mut consider = |y: f32, id: u32| {
            if y > threshold
                && best.is_none_or(|(by, bid)| y < by || (y == by && self.mm_key(id) < self.mm_key(bid)))
            {
                best = Some((y, id));
            }
        };
        self.roof_crossings(x, z, &mut consider);
        if state.any_blocker_active() {
            self.blocker_crossings(state, x, z, &mut 0, |c, up| {
                if !up {
                    consider(c.y, c.id);
                }
            });
        }
        best.map(|(y, id)| self.hit(id, y))
    }
    /// Every active plug triangle crossing the vertical line at `(x, z)`, with whether it faces up
    /// (spec 121 § 5.3 rule 6). Plug triangles are filed in the wall grid (padded `wall_pad_m`,
    /// wider than the column pad); shared plug edges are owned among that cell's plug triangles.
    fn blocker_crossings(
        &self,
        state: &GeomState,
        x: f32,
        z: f32,
        tests: &mut u32,
        mut f: impl FnMut(Crossing, bool),
    ) {
        let ids = self.query_index().walls.at(x, z);
        for &id in ids {
            let tile = &self.cooked().tiles[(id >> 16) as usize];
            let tri = &tile.triangles[(id & 0xffff) as usize];
            if tri.kind != TriKind::Blocker
                || !self.plug_on(state, tri.section)
                || xz_outside(tile.origin_mm, tri.v, x, z)
            {
                continue;
            }
            let plane = self.plane_of(id);
            *tests += 1;
            if let Some(y) = plane.crossing(x, z, ids, |other| self.plane_of(other)) {
                f(Crossing { y, id, n: [0.0; 3] }, plane.n[1] > 0.0);
            }
        }
    }
    /// T3 confinement: clamp only a body at or below its own span's lid.
    #[must_use]
    pub fn clamp_under_ceiling(
        &self,
        state: &GeomState,
        x: f32,
        y: f32,
        z: f32,
        floor_y: f32,
        half_h: f32,
    ) -> f32 {
        self.clamp_under_ceiling_contact(state, x, y, z, floor_y, half_h)
            .y
    }
    /// Same clamp with the lid that moved the origin, for a host's upward-speed cancellation.
    #[must_use]
    pub fn clamp_under_ceiling_contact(
        &self,
        state: &GeomState,
        x: f32,
        y: f32,
        z: f32,
        floor_y: f32,
        half_h: f32,
    ) -> CeilingClamp {
        let mut result = CeilingClamp { y, contact: None };
        if ![x, y, z, floor_y, half_h].iter().all(|v| v.is_finite()) {
            return result;
        }
        // The clamp moves the origin only for a lid at or above `y` and less than `half_h` above
        // it, or when `y` is under `floor_y`. With `y` above `floor_y` and no lid able to cross
        // this line from `y` to `y + half_h` (the segment grid's roof ranges; lane W-11 001,
        // ADR-8815), the column query below could only return `y` unchanged: it is skipped.
        if y > floor_y && self.clear_of_lids(state, x, z, y, det_max(y, y + half_h)) {
            return result;
        }
        if let Some(lid) = self.support(state, x, z, y).ceiling
            && y <= lid.y
        {
            result.y = det_max(det_min(y, lid.y - half_h), floor_y);
            if result.y != y {
                result.contact = Some(lid);
            }
        }
        result
    }
    /// Whether the selected span is covered by a lid above the point.
    #[must_use]
    pub fn covered_at(&self, state: &GeomState, x: f32, z: f32, y: f32) -> bool {
        [x, z, y].iter().all(|v| v.is_finite())
            && self.support(state, x, z, y).ceiling.is_some_and(|lid| lid.y > y)
    }
    /// Highest layer floor with enough clearance or open sky; otherwise the ground.
    #[must_use]
    pub fn support_for_placement(&self, state: &GeomState, x: f32, z: f32) -> SurfaceHit {
        let mut spans = SpanBuf::default();
        self.floors_at(state, x, z, &mut spans);
        spans
            .spans()
            .iter()
            .rev()
            .find(|s| {
                s.floor.kind == SurfaceKind::Floor
                    && s.ceiling
                        .is_none_or(|c| c.y - s.floor.y >= self.settings().hull_fit_m)
            })
            .map_or_else(|| self.fallback(state, x, z), |s| s.floor)
    }
    /// Host-friendly alias for [`WorldGeom::support_for_placement`].
    #[must_use]
    pub fn place_floor(&self, state: &GeomState, x: f32, z: f32) -> SurfaceHit {
        self.support_for_placement(state, x, z)
    }
}

/// Adds a floor crossing to the raw list; past 64 the largest canonical key gives way (ADR-8246 R4).
#[inline]
fn push_raw(
    raw: &mut [Crossing; 64],
    keys: &mut [[[i32; 3]; 3]; 64],
    raw_n: &mut usize,
    overflowed: &mut bool,
    c: Crossing,
    key: [[i32; 3]; 3],
) {
    if *raw_n < raw.len() {
        raw[*raw_n] = c;
        keys[*raw_n] = key;
        *raw_n += 1;
    } else {
        *overflowed = true;
        let worst = (1..raw.len()).fold(0, |w, i| if keys[i] > keys[w] { i } else { w });
        if key < keys[worst] {
            raw[worst] = c;
            keys[worst] = key;
        }
    }
}

fn find_root(parent: &mut [u8], mut i: usize) -> usize {
    while parent[i] as usize != i {
        parent[i] = parent[parent[i] as usize];
        i = parent[i] as usize;
    }
    i
}

/// Box hits the fast column path holds; a column with more runs the per-call form (ADR-8817).
const NEAR: usize = 48;
/// Box hits of the common column: its evaluations fit a buffer a sixth the size.
const NEAR_SMALL: usize = 8;
/// Box hits of a column on a mesh vertex (the floors and roofs around it).
const NEAR_MID: usize = 16;
/// Floor crossings the fast column path clusters; a column with more runs the per-call form.
const FAST_FLOORS: usize = 16;

// Compact, initialized fixed buffers keep the hot support path from copying full metadata for
// every span. Rich hits are materialized only for the selected floor and lid; floors_at expands all.
#[derive(Clone, Copy, Default)]
struct Crossing {
    y: f32,
    id: u32,
    /// The triangle's unnormalized plane normal when the fast path evaluated it (ADR-8817); zero
    /// when unknown (a crossing plane's `n[1]` is never zero), and the hit then rebuilds it.
    n: [f32; 3],
}
struct Column {
    floors: [Crossing; 16],
    lids: [Crossing; 16],
    len: usize,
    lid_len: usize,
    ground: SurfaceHit,
    overflowed: bool,
}
impl Default for Column {
    fn default() -> Self {
        Self {
            floors: [Crossing::default(); 16],
            lids: [Crossing::default(); 16],
            len: 0,
            lid_len: 0,
            ground: EMPTY_HIT,
            overflowed: false,
        }
    }
}
impl Column {
    fn insert(&mut self, crossing: Crossing) {
        if self.len == 16 {
            self.overflowed = true;
            return;
        }
        let mut at = self.len;
        while at > 0 && self.floors[at - 1].y > crossing.y {
            self.floors[at] = self.floors[at - 1];
            at -= 1;
        }
        self.floors[at] = crossing;
        self.len += 1;
    }
    fn lid(&self, geom: &WorldGeom, y: f32) -> Option<Crossing> {
        let mut best: Option<Crossing> = None;
        for &lid in &self.lids[..self.lid_len] {
            if lid.y <= y + geom.settings().lid_gap_m {
                continue;
            }
            let replace = match best {
                None => true,
                Some(b) if lid.y < b.y => true,
                Some(b) if lid.y == b.y => geom.mm_key(lid.id) < geom.mm_key(b.id),
                Some(_) => false,
            };
            if replace {
                best = Some(lid);
            }
        }
        best
    }
    fn span(&self, geom: &WorldGeom, i: usize) -> Span {
        let floor = self.floors[i];
        Span {
            floor: if floor.id == u32::MAX {
                self.ground
            } else {
                geom.hit_of(floor)
            },
            ceiling: self.lid(geom, floor.y).map(|l| geom.hit_of(l)),
        }
    }
    #[inline]
    fn select(&self, geom: &WorldGeom, y: f32) -> Option<Span> {
        let mut best: Option<usize> = None;
        let mut above: Option<usize> = None;
        for (i, floor) in self.floors[..self.len].iter().enumerate() {
            if floor.y <= y {
                if best.is_none_or(|b| floor.y > self.floors[b].y || floor.id == u32::MAX) {
                    best = Some(i);
                }
            } else if above.is_none_or(|a| floor.y == self.floors[a].y && floor.id == u32::MAX) {
                above = Some(i);
            }
        }
        let i = match (best, above) {
            (Some(b), Some(a)) => match self.lid(geom, self.floors[b].y) {
                Some(lid) if y >= lid.y && y - lid.y > self.floors[a].y - y => a,
                _ => b,
            },
            (Some(b), None) => b,
            (None, Some(a)) => a,
            (None, None) => return None,
        };
        Some(self.span(geom, i))
    }
}
