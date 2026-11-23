// SPDX-License-Identifier: MPL-2.0
//! Allocation-free 2D DDA segments (Amanatides and Woo 1987), with two-sided triangle
//! intersections (Woop, Benthin and Wald 2013, C2). Original implementation from spec 121 §5.5.
//!
//! Early out (plan row W-31, ADR-8662): the walks visit cells front to back and stop at the first
//! cell, or skip the first pyramid node, that the segment enters beyond the closest hit so far plus
//! `PRUNE_MARGIN_M` (5 cm). The closest hit is a total order (`t`, then the lower `TriRef`, ground
//! first), so the answer does not depend on which cells are visited once every cell that could hold
//! a hit at `t` or less is: a cell entered past `best.t + margin` cannot.
//!
//! Layout (plan row W-11, ADR-8815): the walk reads the segment tables of `query_seg_index`: one
//! triangle grid for every kind, a 12-byte quantized box per candidate before its triangle is decoded, the
//! shear constants of the segment computed once, and on a plain ground tile the four counts of a
//! cell tested against the segment's own height before a ground triangle is built. A sight line
//! stops at its first hit of any kind. None of it changes an answer: every skipped test is one
//! that cannot hit, and the walk of every cell (`segment_without_pyramid`) stays the oracle.
//!
//! One ground path (ADR-8884): the pyramid's tiles do not start cell walks of their own. A tile's
//! walk starts at the cell where the single walk over the whole lattice is when it enters the tile
//! (`GroundPath::entry`), so it visits the same cells, in the same order, as that walk does inside
//! the tile, also on a segment along a lattice line or through a vertex.

use crate::edit::GeomState;
use crate::ground::{GroundLattice, vertex_m};
use crate::query::SurfaceKind;
use crate::query_index::sub;
use crate::query_seg_index::{misses, plain_count};
use crate::{TriRef, WorldGeom};
use regia_detsim::float::{det_clamp, det_max, det_min};

/// Slack of the early out, metres along the segment: a cell or pyramid node is skipped only when
/// the segment enters it more than this past the closest hit so far. It covers the rounding gap
/// between a cell boundary's `t` and a triangle's intersection `t` (both a few ulps of world
/// coordinates up to 1e4 m; ADR-8662).
const PRUNE_MARGIN_M: f32 = 0.05;

/// Height slack of the per-cell ground skip, metres (ADR-8815): a ground cell is skipped only when
/// the segment, over its crossing of the cell widened by [`PRUNE_MARGIN_M`] of horizontal travel
/// each way, stays more than this above the cell's highest corner or below its lowest.
const GROUND_SKIP_MARGIN_M: f32 = 0.01;

/// The `t` past which no visited cell can improve `best`: `best.t` plus [`PRUNE_MARGIN_M`] over
/// the segment's largest axis extent (a lower bound of its length, so the slack in metres is at
/// least the margin), or infinity without a hit or with pruning off.
fn prune_limit(best: &Option<SegmentHit>, d: [f32; 3], prune: bool) -> f32 {
    match best {
        Some(hit) if prune => {
            let extent = det_max(det_max(d[0].abs(), d[1].abs()), d[2].abs());
            if extent > 0.0 {
                hit.t + PRUNE_MARGIN_M / extent
            } else {
                f32::INFINITY
            }
        }
        _ => f32::INFINITY,
    }
}

/// Which kinds of solid surface a segment tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaceMask(pub u8);
impl FaceMask {
    /// Lattice ground.
    pub const GROUND: Self = Self(1);
    /// Supporting layer floors.
    pub const FLOOR: Self = Self(2);
    /// Roofs.
    pub const ROOF: Self = Self(4);
    /// Walls.
    pub const WALL: Self = Self(8);
    /// Blocker plugs.
    pub const BLOCKER: Self = Self(16);
    /// Every solid face.
    pub const SOLID: Self = Self(31);
    /// Walls, roofs and blockers.
    pub const NO_FLOORS: Self = Self(28);
    fn contains(self, kind: SurfaceKind) -> bool {
        let bit = match kind {
            SurfaceKind::Ground => 1,
            SurfaceKind::Floor => 2,
            SurfaceKind::Roof => 4,
            SurfaceKind::Wall => 8,
            SurfaceKind::Blocker => 16,
        };
        self.0 & bit != 0
    }
}
/// First surface intersected by a finite segment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SegmentHit {
    /// Fraction from start to end, inclusive.
    pub t: f32,
    /// World position in meters.
    pub point: [f32; 3],
    /// Surface class.
    pub kind: SurfaceKind,
    /// Canonical reference, absent for ground.
    pub tri: Option<TriRef>,
}
impl WorldGeom {
    /// First solid intersection. Equal fractions prefer ground, then the lowest TriRef.
    #[must_use]
    pub fn segment(
        &self,
        state: &GeomState,
        from: [f32; 3],
        to: [f32; 3],
        mask: FaceMask,
    ) -> Option<SegmentHit> {
        self.segment_inner(state, from, to, mask, Walk::FAST)
    }
    /// Every wall id the DDA walk visits, and the closest wall from a full triangle scan.
    ///
    /// ADR-8246 R6, on the segment grid since ADR-8815: a wall the ray intersects is filed in a cell
    /// the walk visits. The full scan is the oracle. Callers compare the two; this helper exists
    /// for that property test.
    #[cfg(any(test, feature = "stats"))]
    #[must_use]
    pub fn wall_visit_and_scan(&self, from: [f32; 3], to: [f32; 3]) -> (Vec<u32>, Option<SegmentHit>) {
        let mut visited = Vec::new();
        let mut closest = None;
        if !from.iter().chain(to.iter()).all(|v| v.is_finite()) {
            return (visited, closest);
        }
        let d = sub(to, from);
        let seg = self.seg_index();
        grid_walk(seg.cell, seg.bounds, from, d, |cell| {
            for &entry in seg.walls_at(cell) {
                let id = seg.id(entry.slot());
                if !entry.is_blocker() && !visited.contains(&id) {
                    visited.push(id);
                }
            }
            f32::INFINITY
        });
        for tri in self.triangles() {
            if tri.kind != crate::cooked::TriKind::Wall {
                continue;
            }
            if let Some(t) = intersect(tri.v, from, d) {
                keep(&mut closest, t, from, d, SurfaceKind::Wall, Some(tri.tri));
            }
        }
        (visited, closest)
    }
    /// Reference traversal: every cell the segment crosses in the column grid, the wall grid and
    /// the ground lattice, with no skip, no early out and none of the segment tables, for
    /// bit-equivalence checks.
    #[cfg(any(test, feature = "stats"))]
    #[must_use]
    pub fn segment_without_pyramid(
        &self,
        state: &GeomState,
        from: [f32; 3],
        to: [f32; 3],
        mask: FaceMask,
    ) -> Option<SegmentHit> {
        if !from.iter().chain(to.iter()).all(|v| v.is_finite()) {
            return None;
        }
        let d = sub(to, from);
        if !d.iter().all(|v| v.is_finite()) {
            return None;
        }
        let ray = Ray::new(from, d)?;
        let mut best = None;
        for grid in [&self.query_index().columns, &self.query_index().walls] {
            grid_walk(grid.cell, grid.bounds(), from, d, |cell| {
                for &id in grid.cell_at(cell) {
                    let tri = self.tri(id);
                    let kind = tri.kind.into();
                    if !mask.contains(kind)
                        || (kind == SurfaceKind::Blocker
                            && !(state.any_blocker_active() && self.plug_on(state, tri.section)))
                    {
                        continue;
                    }
                    if let Some(t) = ray.hit(tri.v) {
                        keep(&mut best, t, from, d, kind, Some(tri.tri));
                    }
                }
                f32::INFINITY
            });
        }
        if mask.contains(SurfaceKind::Ground) {
            let ground = GroundLattice::new(self.cooked()).with_edits(&state.ground);
            ground_cells(
                &ground,
                [[0, 0], [ground.lattice().cells() as i32 - 1; 2]],
                &ray,
                &mut best,
                Walk::FULL,
            );
        }
        best
    }
    fn segment_inner(
        &self,
        state: &GeomState,
        from: [f32; 3],
        to: [f32; 3],
        mask: FaceMask,
        walk: Walk,
    ) -> Option<SegmentHit> {
        // The cooked pyramid does not bound edited ground (spec 121 § 6; ADR-8313). On an edited
        // state the descent reads a node's range from the state's own ranges when it has them
        // (`GroundEdits::edited_range`, ADR-8818), otherwise it trusts a cooked range only at a tile
        // no edit touches; a touched tile is read cell by cell with the edits (ADR-8815).
        let edits = (!state.ground.is_empty()).then_some(&state.ground);
        if !from.iter().chain(to.iter()).all(|v| v.is_finite()) {
            return None;
        }
        let d = sub(to, from);
        if !d.iter().all(|v| v.is_finite()) {
            return None;
        }
        // A zero-length segment intersects nothing (the shear has no axis).
        let ray = Ray::new(from, d)?;
        let seg = self.seg_index();
        let blockers = state.any_blocker_active();
        let mut best = None;
        if mask.0 & !FaceMask::GROUND.0 != 0 {
            // The masks of the three parts of a cell: walls and plug faces, floors, roofs.
            let quant = seg.quant();
            let part_masks = [
                FaceMask::WALL.0 | FaceMask::BLOCKER.0,
                FaceMask::FLOOR.0,
                FaceMask::ROOF.0,
            ];
            grid_walk_spans(seg.cell, seg.bounds, from, d, |cell, t_in, t_out| {
                let Some(parts) = seg.cell_parts(cell) else {
                    return prune_limit(&best, d, true);
                };
                // The box of the segment's part inside the cell, grown by the margin: a part of
                // the cell whose triangles all lie above or below it is skipped, and so is a
                // triangle whose own box misses it. Any hit such a triangle makes lies in the cell
                // of its own point, which lists it too and passes both tests (ADR-8662). The part
                // of the segment is the walk's own crossing of the cell, widened by the slack
                // (ADR-8815).
                let w = segment_window(&ray, t_in, t_out);
                // In the quanta of the box table: its boxes contain their triangles, and the
                // window's quanta contain the window, so a miss here is a miss in metres.
                let w = quant.window(w[0], w[1]);
                for (part, (entries, heights)) in parts.into_iter().enumerate() {
                    if mask.0 & part_masks[part] == 0
                        || w[1][1] < i32::from(heights[0])
                        || w[0][1] > i32::from(heights[1])
                    {
                        continue;
                    }
                    for &entry in entries {
                        if misses(seg.tri_box(entry.slot()), &w) {
                            continue;
                        }
                        let kind = match part {
                            0 if entry.is_blocker() => SurfaceKind::Blocker,
                            0 => SurfaceKind::Wall,
                            1 => SurfaceKind::Floor,
                            _ => SurfaceKind::Roof,
                        };
                        if !mask.contains(kind) {
                            continue;
                        }
                        let tri = self.tri(seg.id(entry.slot()));
                        debug_assert_eq!(SurfaceKind::from(tri.kind), kind);
                        // A plug face is a solid only while its blocker is active (§ 5.3 rule 6).
                        if kind == SurfaceKind::Blocker && !(blockers && self.plug_on(state, tri.section)) {
                            continue;
                        }
                        if let Some(t) = ray.hit(tri.v) {
                            if walk.rock && kind != SurfaceKind::Roof {
                                let p = [from[0] + d[0] * t, from[1] + d[1] * t, from[2] + d[2] * t];
                                if !self.covered_at(state, p[0], p[2], p[1]) {
                                    continue;
                                }
                            }
                            keep(&mut best, t, from, d, kind, Some(tri.tri));
                            if walk.any {
                                return f32::NEG_INFINITY;
                            }
                        }
                    }
                }
                prune_limit(&best, d, true)
            });
            if walk.any && best.is_some() {
                return best;
            }
        }
        if mask.contains(SurfaceKind::Ground) {
            // Built at the first tile the descent reaches: a sight line the pyramid skips whole
            // never needs it.
            let path = std::cell::OnceCell::new();
            self.ground_node(
                (self.height_pyramid.levels.len() - 1, [0, 0]),
                [0.0, 1.0],
                (&ray, &path),
                edits,
                &mut best,
                walk,
            );
        }
        best
    }
    /// Whether every solid surface leaves this segment clear.
    #[must_use]
    pub fn line_of_sight(&self, state: &GeomState, from: [f32; 3], to: [f32; 3]) -> bool {
        // Any hit blocks the line, so the walk stops at the first one it finds (ADR-8815): the
        // same answer as `segment(..).is_none()`, which looks for the closest.
        self.segment_inner(
            state,
            from,
            to,
            FaceMask::SOLID,
            Walk {
                any: true,
                ..Walk::FAST
            },
        )
        .is_none()
    }
    /// Fraction of the first roof, or covered wall, along this segment.
    #[must_use]
    pub fn rock_between(&self, state: &GeomState, from: [f32; 3], to: [f32; 3]) -> Option<f32> {
        self.segment_inner(
            state,
            from,
            to,
            FaceMask::NO_FLOORS,
            Walk {
                rock: true,
                ..Walk::FAST
            },
        )
        .map(|hit| hit.t)
    }
    /// The height range that bounds the ground of node `index` of pyramid `level`: the cooked one on
    /// an unedited state or at an untouched tile (`cooked_ground`), the edited state's own at any
    /// node when it holds them (ADR-8818), and none otherwise (the node is then descended without
    /// the height tests). ADR-10082 merges ADR-8815's untouched-tile rule with ADR-8818's ranges.
    fn node_range(
        &self,
        edits: Option<&crate::edit::GroundEdits>,
        level: usize,
        index: u32,
        cooked_ground: bool,
    ) -> Option<[f32; 2]> {
        let cooked_range = self.height_pyramid.levels[level].ranges[index as usize];
        match edits {
            None => Some(cooked_range),
            Some(e) if e.pyramid_bounds() => {
                Some(e.edited_range(level, index as usize).unwrap_or(cooked_range))
            }
            Some(_) => cooked_ground.then_some(cooked_range),
        }
    }
    /// One node of the height pyramid. `span` is the part of the segment (`t` from, to) that can
    /// still hold a ground hit of this node: the parents' crossings and height slabs (ADR-8815).
    /// `path` is the single walk's cell path, built at the first tile reached, where each tile's walk
    /// starts (ADR-8884).
    fn ground_node(
        &self,
        (level, node): (usize, [u32; 2]),
        span: [f32; 2],
        (ray, path): (&Ray, &std::cell::OnceCell<Option<GroundPath>>),
        edits: Option<&crate::edit::GroundEdits>,
        best: &mut Option<SegmentHit>,
        walk: Walk,
    ) {
        let data = &self.height_pyramid.levels[level];
        if node[0] >= data.side || node[1] >= data.side {
            return;
        }
        let (from, d) = (ray.from, ray.d);
        let l = &self.cooked().lattice;
        let seg = self.seg_index();
        let size = u32::from(l.ground_tile_cells) << level;
        let lo = node.map(|v| v * size);
        let hi = node.map(|v| ((v + 1) * size).min(l.cells()));
        // `vertex_m` from the axis table: the same bits (ADR-8815).
        let at = |v: u32| seg.axis_m(v).unwrap_or_else(|| vertex_m(l, v));
        let lo_m = lo.map(at);
        let hi_m = hi.map(at);
        let Some((enter, exit)) = clip(lo_m, hi_m, from, d) else {
            return;
        };
        if enter > prune_limit(best, d, true) {
            return;
        }
        // A tile (level 0) that no edit touches reads the cooked ground: not promoted, and no
        // override owned by the tile or by the east, north and north-east tiles that own its border
        // vertices (ADR-8815). Only such a tile takes the plain-cell walk.
        let tile = node[1] * data.side + node[0];
        let cooked_ground = match edits {
            None => true,
            Some(e) => {
                level == 0 && {
                    let (east, north) = (node[0] + 1 < data.side, node[1] + 1 < data.side);
                    !(e.is_promoted(tile)
                        || e.owns_overrides(tile)
                        || (east && e.owns_overrides(tile + 1))
                        || (north && e.owns_overrides(tile + data.side))
                        || (east && north && e.owns_overrides(tile + data.side + 1)))
                }
            }
        };
        let range = self.node_range(edits, level, tile, cooked_ground);
        // The span is cut to the node's crossing, widened by the slack (ADR-8815): a ground hit of
        // this node has its point inside the node.
        let mut span = [
            det_max(span[0], enter - ray.slack),
            det_min(span[1], exit + ray.slack),
        ];
        if let Some(range) = range {
            let (y_enter, y_exit) = (from[1] + d[1] * enter, from[1] + d[1] * exit);
            if det_min(y_enter, y_exit) > range[1] + 0.001 {
                return;
            }
            // Wholly under the node's lowest ground (a sight line from a tube): no ground triangle
            // of the node reaches it (ADR-8662).
            if det_max(y_enter, y_exit) < range[0] - 0.001 {
                return;
            }
            // And to the slab of `t` where the segment's height is within the node's range plus
            // `GROUND_SKIP_MARGIN_M`: the hit lies on one of the node's triangles, so its height
            // is inside the range. The children and the cell walk look no further.
            if d[1] != 0.0 {
                let a = (range[0] - GROUND_SKIP_MARGIN_M - from[1]) / d[1];
                let b = (range[1] + GROUND_SKIP_MARGIN_M - from[1]) / d[1];
                span = [det_max(span[0], det_min(a, b)), det_min(span[1], det_max(a, b))];
            }
        }
        if span[0] > span[1] {
            return;
        }
        if level == 0 {
            let bounds = [lo.map(|v| v as i32), hi.map(|v| v as i32 - 1)];
            // The single walk's first cell in the tile that it leaves no earlier than the span's
            // start, and the `t` it enters that cell at; the span is widened by the slack each way
            // here, so a hit within rounding of a cell border is tested in both cells as the
            // single walk tests it (ADR-8884).
            let Some(path) = path.get_or_init(|| GroundPath::new(self, ray)) else {
                // The single walk visits no cell.
                return;
            };
            let Some((start, t_in)) = path.entry(self, bounds, span[0] - ray.slack) else {
                return;
            };
            let exit = det_min(path.exit, span[1] + ray.slack);
            if cooked_ground && seg.tile_is_plain(tile) {
                self.ground_cells_plain(bounds, start, [t_in, exit], ray, best, walk);
            } else {
                // Any tile level, overrides, mouth cuts, and the D1 edits of an edited state.
                let ground = GroundLattice::new(self.cooked());
                let ground = match edits {
                    Some(e) => ground.with_edits(e),
                    None => ground,
                };
                ground_cells_from(&ground, bounds, start, exit, ray, best, walk);
            }
        } else {
            // Children nearer the segment's start first (by the sign of each axis), so the early
            // out skips the far ones once a hit is known.
            let order = |neg: bool| if neg { [1u32, 0] } else { [0u32, 1] };
            // A child the span's own XZ box (plus the margin) does not reach holds none of the
            // span: it is skipped without its clip (ADR-8815).
            let ends = [span[0], span[1]].map(|t| [from[0] + d[0] * t, from[2] + d[2] * t]);
            let reach = |axis: usize, child: u32| {
                let half = size / 2;
                let lo = at((node[axis] * size + child * half).min(l.cells()));
                let hi = at((node[axis] * size + (child + 1) * half).min(l.cells()));
                det_max(ends[0][axis], ends[1][axis]) >= lo - PRUNE_MARGIN_M
                    && det_min(ends[0][axis], ends[1][axis]) <= hi + PRUNE_MARGIN_M
            };
            for dz in order(d[2] < 0.0) {
                if !reach(1, dz) {
                    continue;
                }
                for dx in order(d[0] < 0.0) {
                    if !reach(0, dx) {
                        continue;
                    }
                    self.ground_node(
                        (level - 1, [node[0] * 2 + dx, node[1] * 2 + dz]),
                        span,
                        (ray, path),
                        edits,
                        best,
                        walk,
                    );
                    if walk.any && best.is_some() {
                        return;
                    }
                }
            }
        }
    }
    /// `ground_cells` on a plain ground tile (`SegIndex::tile_is_plain`, no D1 edit): the cells
    /// the single walk visits in the tile from `start` (entered at `span[0]`) until it steps past
    /// `span[1]` (see [`WorldGeom::ground_node`]), in the same order, but
    /// a cell's two triangles are built and intersected only when
    /// the segment, over its crossing of the cell, comes within the margins of the range of the
    /// cell's four corner heights. A triangle of the cell lies inside that range, so a skipped
    /// cell holds no hit (ADR-8815). The triangles are built from the axis table and the counts:
    /// the bits `GroundLattice::cell_triangles` returns for such a tile.
    fn ground_cells_plain(
        &self,
        bounds: [[i32; 2]; 2],
        start: [i32; 2],
        span: [f32; 2],
        ray: &Ray,
        best: &mut Option<SegmentHit>,
        walk: Walk,
    ) {
        let (from, d) = (ray.from, ray.d);
        let c = self.cooked();
        let l = &c.lattice;
        let axis = self.seg_index().axis();
        if corner(axis, bounds[0], 0).is_none() || corner(axis, bounds[1], 1).is_none() {
            return ground_cells_from(&GroundLattice::new(c), bounds, start, span[1], ray, best, walk);
        }
        let (enter, exit) = (span[0], span[1]);
        let mut cell = start;
        let shift = u32::from(l.ground_tile_cells).trailing_zeros();
        let per = l.tiles_per_side();
        let (offset, step_m) = (l.offset_m(), l.step_m());
        let step = [if d[0] < 0.0 { -1 } else { 1 }, if d[2] < 0.0 { -1 } else { 1 }];
        // The crossing of a cell is widened by the slack each way.
        let slack = ray.slack;
        // The tile's own block: a cell away from its east and north borders reads its four
        // counts from it directly (`plain_count` of each, the same values).
        let (tile_a, tile_b) = ((bounds[0][0] as u32) >> shift, (bounds[0][1] as u32) >> shift);
        let (x0, x1) = l.tile_cells(tile_a.min(per - 1));
        let z0 = tile_b.min(per - 1) << shift;
        let cols = if tile_a + 1 >= per { x1 - x0 + 1 } else { x1 - x0 };
        let block: &[u16] = c
            .ground_counts
            .get((tile_b.min(per - 1) * per + tile_a.min(per - 1)) as usize)
            .map_or(&[], Vec::as_slice);
        let own = |i: u32, j: u32| -> Option<[u16; 4]> {
            let (di, dj) = (i.checked_sub(x0)?, j.checked_sub(z0)?);
            if di + 1 >= cols {
                return None;
            }
            let k = (dj * cols + di) as usize;
            let low = block.get(k..k + 2)?;
            let high = block.get(k + cols as usize..k + cols as usize + 2)?;
            Some([low[0], high[0], low[1], high[1]])
        };
        let mut t_in = enter;
        loop {
            let mut next = [f32::INFINITY; 2];
            for (a, world) in [0, 2].into_iter().enumerate() {
                if d[world] != 0.0 {
                    let boundary = (cell[a] + i32::from(step[a] > 0)) as usize;
                    next[a] = (axis[boundary] - from[world]) / d[world];
                }
            }
            let a = usize::from(next[0] > next[1]);
            let t_out = det_min(next[a], exit);
            let (i, j) = (cell[0] as u32, cell[1] as u32);
            // `[c00, c01, c10, c11]`: the counts of `(i, j)`, `(i, j + 1)`, `(i + 1, j)`, `(i + 1, j + 1)`.
            let counts = own(i, j).or_else(|| {
                Some([
                    plain_count(c, shift, per, i, j)?,
                    plain_count(c, shift, per, i, j + 1)?,
                    plain_count(c, shift, per, i + 1, j)?,
                    plain_count(c, shift, per, i + 1, j + 1)?,
                ])
            });
            if let Some([c00, c01, c10, c11]) = counts {
                let t0 = det_max(det_min(t_in, t_out) - slack, 0.0);
                let t1 = det_min(det_max(t_in, t_out) + slack, 1.0);
                let (ya, yb) = (from[1] + d[1] * t0, from[1] + d[1] * t1);
                // `offset + count * step` does not decrease with the count (the step is positive).
                let lowest = c00.min(c01).min(c10).min(c11);
                let highest = c00.max(c01).max(c10).max(c11);
                let low = offset + f32::from(lowest) * step_m;
                let high = offset + f32::from(highest) * step_m;
                if det_min(ya, yb) <= high + GROUND_SKIP_MARGIN_M
                    && det_max(ya, yb) >= low - GROUND_SKIP_MARGIN_M
                {
                    let (x0, x1) = (axis[i as usize], axis[i as usize + 1]);
                    let (z0, z1) = (axis[j as usize], axis[j as usize + 1]);
                    let h = |count: u16| offset + f32::from(count) * step_m;
                    let (v00, v01) = ([x0, h(c00), z0], [x0, h(c01), z1]);
                    let (v10, v11) = ([x1, h(c10), z0], [x1, h(c11), z1]);
                    let tris = [[v00, v01, v10], [v10, v01, v11]];
                    debug_assert_eq!(
                        GroundLattice::new(c)
                            .cell_triangles(i, j)
                            .map(|t| t.map(|(v, cut)| (v.map(|p| p.map(f32::to_bits)), cut))),
                        Some(tris.map(|v| (v.map(|p| p.map(f32::to_bits)), false))),
                        "plain ground cell ({i}, {j})"
                    );
                    for tri in tris {
                        if let Some(t) = ray.hit(tri) {
                            keep(best, t, from, d, SurfaceKind::Ground, None);
                        }
                    }
                }
            } else if let Some(tris) = GroundLattice::new(c).cell_triangles(i, j) {
                // Never taken on a plain tile; the general read keeps the answer if it were.
                for (tri, cut) in tris {
                    if !cut && let Some(t) = ray.hit(tri) {
                        keep(best, t, from, d, SurfaceKind::Ground, None);
                    }
                }
            }
            if walk.any && best.is_some() {
                return;
            }
            let limit = prune_limit(best, d, true);
            if next[a] > exit || !next[a].is_finite() || next[a] > limit {
                break;
            }
            cell[a] += step[a];
            if cell[a] < bounds[0][a] || cell[a] > bounds[1][a] {
                break;
            }
            t_in = next[a];
        }
    }
}
/// The metre position of a cell range's low (`side` 0) or high (`side` 1) corner from the axis
/// table: `vertex_m` of the cell index, plus one on the high side.
fn corner(axis: &[f32], cell: [i32; 2], side: i32) -> Option<[f32; 2]> {
    let at = |v: i32| axis.get(usize::try_from(v + side).ok()?).copied();
    Some([at(cell[0])?, at(cell[1])?])
}
/// How `segment_inner` walks: `rock` keeps only roofs and covered walls (`rock_between`), `any`
/// stops at the first hit found instead of the closest (`line_of_sight`, ADR-8815), `prune` is
/// the early out (ADR-8662; off only in the reference walk).
#[derive(Clone, Copy)]
struct Walk {
    rock: bool,
    any: bool,
    prune: bool,
}
impl Walk {
    const FAST: Self = Self {
        rock: false,
        any: false,
        prune: true,
    };
    #[cfg(any(test, feature = "stats"))]
    const FULL: Self = Self {
        rock: false,
        any: false,
        prune: false,
    };
}
fn keep(
    best: &mut Option<SegmentHit>,
    t: f32,
    from: [f32; 3],
    d: [f32; 3],
    kind: SurfaceKind,
    tri: Option<TriRef>,
) {
    if best.is_none_or(|old| t < old.t || (t == old.t && tri < old.tri)) {
        *best = Some(SegmentHit {
            t,
            point: [from[0] + d[0] * t, from[1] + d[1] * t, from[2] + d[2] * t],
            kind,
            tri,
        });
    }
}
/// One segment's shear (Woop et al. 2013), computed once: the dominant axis `kz`, the other two
/// in winding order, and the three shear constants. The same expressions, in the same order, as
/// computing them per triangle, so the same bits (ADR-8815).
#[derive(Clone, Copy, Debug)]
struct Ray {
    from: [f32; 3],
    d: [f32; 3],
    k: [usize; 3],
    s: [f32; 3],
    /// [`PRUNE_MARGIN_M`] of horizontal travel as a span of `t`; infinite for a segment with no
    /// horizontal travel (it stays in one cell: all of it).
    slack: f32,
}
impl Ray {
    /// `None` for a zero-length segment, which intersects nothing.
    fn new(from: [f32; 3], d: [f32; 3]) -> Option<Self> {
        let mut kz = 0;
        for axis in 1..3 {
            if d[axis].abs() > d[kz].abs() {
                kz = axis;
            }
        }
        if d[kz] == 0.0 {
            return None;
        }
        let mut kx = (kz + 1) % 3;
        let mut ky = (kx + 1) % 3;
        if d[kz] < 0.0 {
            std::mem::swap(&mut kx, &mut ky);
        }
        let horizontal = det_max(d[0].abs(), d[2].abs());
        Some(Self {
            from,
            d,
            k: [kx, ky, kz],
            s: [d[kx] / d[kz], d[ky] / d[kz], 1.0 / d[kz]],
            slack: if horizontal > 0.0 {
                PRUNE_MARGIN_M / horizontal
            } else {
                f32::INFINITY
            },
        })
    }
    // C2: ray-dependent shear followed by three antisymmetric edge tests (Woop et al. 2013).
    // All neighboring triangles transform a shared vertex identically. No triangle-dependent u+v
    // test, epsilon expansion, FMA, or double-precision fallback is used on the lockstep path.
    #[inline]
    fn hit(&self, v: [[f32; 3]; 3]) -> Option<f32> {
        let [kx, ky, kz] = self.k;
        let [sx, sy, sz] = self.s;
        let p = v.map(|v| {
            let a = sub(v, self.from);
            [a[kx] - sx * a[kz], a[ky] - sy * a[kz], a[kz] * sz]
        });
        let edge = |a: [f32; 3], b: [f32; 3]| a[0] * b[1] - a[1] * b[0];
        let u = edge(p[2], p[1]);
        let v = edge(p[0], p[2]);
        let w = edge(p[1], p[0]);
        if (u < 0.0 || v < 0.0 || w < 0.0) && (u > 0.0 || v > 0.0 || w > 0.0) {
            return None;
        }
        let det = (u + v) + w;
        if det == 0.0 {
            return None;
        }
        let t = ((u * p[0][2] + v * p[1][2]) + w * p[2][2]) / det;
        (0.0..=1.0).contains(&t).then_some(t)
    }
}
#[cfg(any(test, feature = "stats"))]
fn intersect(v: [[f32; 3]; 3], from: [f32; 3], d: [f32; 3]) -> Option<f32> {
    Ray::new(from, d)?.hit(v)
}

fn clip(lo: [f32; 2], hi: [f32; 2], from: [f32; 3], d: [f32; 3]) -> Option<(f32, f32)> {
    let mut enter = 0.0;
    let mut exit = 1.0;
    for (axis, world) in [0, 2].into_iter().enumerate() {
        if d[world] == 0.0 {
            if from[world] < lo[axis] || from[world] > hi[axis] {
                return None;
            }
        } else {
            let a = (lo[axis] - from[world]) / d[world];
            let b = (hi[axis] - from[world]) / d[world];
            enter = det_max(enter, det_min(a, b));
            exit = det_min(exit, det_max(a, b));
            if enter > exit {
                return None;
            }
        }
    }
    Some((enter, exit))
}
/// The box of the segment between `t_in` and `t_out` (a cell crossing of the walk), each widened
/// by the slack and kept inside the segment, grown by `PRUNE_MARGIN_M` and one millimetre: metres
/// `[min, max]` per axis.
fn segment_window(ray: &Ray, t_in: f32, t_out: f32) -> [[f32; 3]; 2] {
    let t0 = det_max(det_min(t_in, t_out) - ray.slack, 0.0);
    let t1 = det_min(det_max(t_in, t_out) + ray.slack, 1.0);
    let a = [0, 1, 2].map(|k| ray.from[k] + ray.d[k] * t0);
    let b = [0, 1, 2].map(|k| ray.from[k] + ray.d[k] * t1);
    let margin = PRUNE_MARGIN_M + 0.001;
    [
        [0, 1, 2].map(|k| det_min(a[k], b[k]) - margin),
        [0, 1, 2].map(|k| det_max(a[k], b[k]) + margin),
    ]
}

/// [`grid_walk`] that also hands `visit` the `t` at which the walk enters the cell and the `t` at
/// which it leaves it (the segment's end of the grid for the last cell).
fn grid_walk_spans(
    size: f32,
    bounds: Option<[[i32; 2]; 2]>,
    from: [f32; 3],
    d: [f32; 3],
    mut visit: impl FnMut([i32; 2], f32, f32) -> f32,
) {
    let Some(bounds) = bounds else {
        return;
    };
    let lo = bounds[0].map(|v| v as f32 * size);
    let hi = bounds[1].map(|v| (v + 1) as f32 * size);
    let Some((enter, exit)) = clip(lo, hi, from, d) else {
        return;
    };
    let mut cell = [
        ((from[0] + d[0] * enter) / size).floor() as i32,
        ((from[2] + d[2] * enter) / size).floor() as i32,
    ];
    for axis in 0..2 {
        cell[axis] = cell[axis].clamp(bounds[0][axis], bounds[1][axis]);
    }
    let step = [if d[0] < 0.0 { -1 } else { 1 }, if d[2] < 0.0 { -1 } else { 1 }];
    let mut t_in = enter;
    loop {
        let mut next = [f32::INFINITY; 2];
        for (axis, world) in [0, 2].into_iter().enumerate() {
            if d[world] != 0.0 {
                let boundary = cell[axis] + i32::from(step[axis] > 0);
                next[axis] = (boundary as f32 * size - from[world]) / d[world];
            }
        }
        let axis = usize::from(next[0] > next[1]);
        let limit = visit(cell, t_in, det_min(next[axis], exit));
        if next[axis] > exit || !next[axis].is_finite() || next[axis] > limit {
            break;
        }
        cell[axis] += step[axis];
        if cell[axis] < bounds[0][axis] || cell[axis] > bounds[1][axis] {
            break;
        }
        t_in = next[axis];
    }
}

/// Visits the cells of size `size` within `bounds` that the segment crosses, front to back.
/// `visit` returns the `t` past which the walk may stop ([`prune_limit`]). The reference walks' own
/// traversal; `segment` uses [`grid_walk_spans`].
#[cfg(any(test, feature = "stats"))]
fn grid_walk(
    size: f32,
    bounds: Option<[[i32; 2]; 2]>,
    from: [f32; 3],
    d: [f32; 3],
    visit: impl FnMut([i32; 2]) -> f32,
) {
    let Some(bounds) = bounds else {
        return;
    };
    let lo = bounds[0].map(|v| v as f32 * size);
    let hi = bounds[1].map(|v| (v + 1) as f32 * size);
    let Some((enter, exit)) = clip(lo, hi, from, d) else {
        return;
    };
    let mut cell = [
        ((from[0] + d[0] * enter) / size).floor() as i32,
        ((from[2] + d[2] * enter) / size).floor() as i32,
    ];
    for axis in 0..2 {
        cell[axis] = cell[axis].clamp(bounds[0][axis], bounds[1][axis]);
    }
    walk_cells(
        &mut cell,
        from,
        d,
        exit,
        bounds,
        |_, boundary| boundary as f32 * size,
        visit,
    );
}
fn walk_cells(
    cell: &mut [i32; 2],
    from: [f32; 3],
    d: [f32; 3],
    exit: f32,
    bounds: [[i32; 2]; 2],
    coordinate: impl Fn(usize, i32) -> f32,
    mut visit: impl FnMut([i32; 2]) -> f32,
) {
    let step = [if d[0] < 0.0 { -1 } else { 1 }, if d[2] < 0.0 { -1 } else { 1 }];
    loop {
        let limit = visit(*cell);
        let mut next = [f32::INFINITY; 2];
        for (axis, world) in [0, 2].into_iter().enumerate() {
            if d[world] != 0.0 {
                let boundary = cell[axis] + i32::from(step[axis] > 0);
                next[axis] = (coordinate(axis, boundary) - from[world]) / d[world];
            }
        }
        let axis = usize::from(next[0] > next[1]);
        if next[axis] > exit || !next[axis].is_finite() || next[axis] > limit {
            break;
        }
        cell[axis] += step[axis];
        if cell[axis] < bounds[0][axis] || cell[axis] > bounds[1][axis] {
            break;
        }
    }
}

/// The ground cells of `bounds` the segment crosses, front to back, each cell's two lattice
/// triangles read through `GroundLattice::cell_triangles` (any tile level, overrides, D1 edits,
/// mouth cuts). The reference walk's; the pyramid's tiles walk the same cells through
/// `ground_cells_from` (ADR-8884).
#[cfg(any(test, feature = "stats"))]
fn ground_cells(
    ground: &GroundLattice<'_>,
    bounds: [[i32; 2]; 2],
    ray: &Ray,
    best: &mut Option<SegmentHit>,
    walk: Walk,
) {
    let (from, d) = (ray.from, ray.d);
    let l = ground.lattice();
    let lo = bounds[0].map(|v| vertex_m(l, v as u32));
    let hi = bounds[1].map(|v| vertex_m(l, v as u32 + 1));
    let Some((enter, exit)) = clip(lo, hi, from, d) else {
        return;
    };
    let x = det_clamp(from[0] + d[0] * enter, lo[0], hi[0]);
    let z = det_clamp(from[2] + d[2] * enter, lo[1], hi[1]);
    let Some(loc) = ground.locate(x, z) else {
        return;
    };
    let mut cell = [
        (loc.i as i32).clamp(bounds[0][0], bounds[1][0]),
        (loc.j as i32).clamp(bounds[0][1], bounds[1][1]),
    ];
    walk_cells(
        &mut cell,
        from,
        d,
        exit,
        bounds,
        |_, b| vertex_m(l, b as u32),
        |cell| {
            if let Some(tris) = ground.cell_triangles(cell[0] as u32, cell[1] as u32) {
                for (tri, cut) in tris {
                    if !cut && let Some(t) = ray.hit(tri) {
                        keep(best, t, from, d, SurfaceKind::Ground, None);
                    }
                }
            }
            if walk.any && best.is_some() {
                return f32::NEG_INFINITY;
            }
            prune_limit(best, d, walk.prune)
        },
    );
}

/// `ground_cells` from cell `start` of `bounds` (the single walk's cell where it enters them,
/// [`GroundPath::entry`]) until the walk steps past `exit` or out of `bounds` (ADR-8884).
fn ground_cells_from(
    ground: &GroundLattice<'_>,
    bounds: [[i32; 2]; 2],
    start: [i32; 2],
    exit: f32,
    ray: &Ray,
    best: &mut Option<SegmentHit>,
    walk: Walk,
) {
    let (from, d) = (ray.from, ray.d);
    let l = ground.lattice();
    let mut cell = start;
    walk_cells(
        &mut cell,
        from,
        d,
        exit,
        bounds,
        |_, b| vertex_m(l, b as u32),
        |cell| {
            if let Some(tris) = ground.cell_triangles(cell[0] as u32, cell[1] as u32) {
                for (tri, cut) in tris {
                    if !cut && let Some(t) = ray.hit(tri) {
                        keep(best, t, from, d, SurfaceKind::Ground, None);
                    }
                }
            }
            if walk.any && best.is_some() {
                return f32::NEG_INFINITY;
            }
            prune_limit(best, d, walk.prune)
        },
    );
}

/// `GroundLattice::locate`'s search along one axis, on the axis table `axis` (`vertex_m` of
/// every lattice vertex): the cell `i` with `axis[i] <= v < axis[i + 1]`, the last cell closed;
/// `None` for `v` off the axis or NaN.
fn locate_on(axis: &[f32], pitch: f32, v: f32) -> Option<u32> {
    let cells = axis.len().checked_sub(1)?;
    if cells == 0 || v.is_nan() || v < axis[0] || v > axis[cells] {
        return None;
    }
    let est = ((v - axis[0]) / pitch).floor();
    let mut i = if est <= 0.0 {
        0
    } else if est >= (cells - 1) as f32 {
        cells - 1
    } else {
        est as usize
    };
    while i > 0 && v < axis[i] {
        i -= 1;
    }
    while i + 1 < cells && v >= axis[i + 1] {
        i += 1;
    }
    Some(i as u32)
}

/// The cell path of the single ground walk (`ground_cells` over the whole lattice, the reference
/// walk's), so the pyramid's tiles can start their walks on it (ADR-8884).
///
/// The walk (Amanatides and Woo 1987) leaves a cell along `x` at `X[p]`, the `t` of the `p`-th
/// `x` border it crosses, and along `z` at `Z[q]`; from each cell it takes the earlier of its two
/// next borders, `x` on a tie. Both lists are non-decreasing (the border coordinates are, and one
/// division by the same `d`). So the walk is the merge of the two lists, `x` first on a tie, and it
/// is in the cell `p` steps along `x` and `q` along `z` exactly when `X[p - 1] <= Z[q]` and
/// `Z[q - 1] < X[p]`. Any set of crossings closed under that order is where the walk is at some
/// cell; counting the crossings before a `t` or before a tile's border finds that cell without
/// walking to it, and the tile's walk from there steps through the same cells as the single walk.
struct GroundPath {
    /// The single walk's first cell: the lattice cell of the segment's entry into the lattice box.
    start: [i32; 2],
    /// The walk's direction along `x` and `z` (`+1` for `d` of zero, as `walk_cells`).
    step: [i32; 2],
    /// Where the segment enters and leaves the lattice box.
    enter: f32,
    exit: f32,
    /// Cells per lattice side.
    cells: i32,
    from: [f32; 2],
    d: [f32; 2],
    /// The lattice's first vertex and pitch, metres: the estimate of a count, corrected on the
    /// exact border `t`s.
    first: f32,
    pitch: f32,
}

impl GroundPath {
    /// The single walk's start, as `ground_cells` computes it over the whole lattice; `None`
    /// when that walk visits no cell.
    fn new(geom: &WorldGeom, ray: &Ray) -> Option<Self> {
        let l = &geom.cooked().lattice;
        let cells = l.cells() as i32;
        let (from, d) = (ray.from, ray.d);
        let axis = geom.seg_index().axis();
        let full = axis.len() == l.cells() as usize + 1;
        let (lo, hi) = if full {
            (axis[0], axis[axis.len() - 1])
        } else {
            (vertex_m(l, 0), vertex_m(l, l.cells()))
        };
        let (enter, exit) = clip([lo; 2], [hi; 2], from, d)?;
        let x = det_clamp(from[0] + d[0] * enter, lo, hi);
        let z = det_clamp(from[2] + d[2] * enter, lo, hi);
        // `GroundLattice::locate`'s search on the axis table (the same values and comparisons, so
        // the same cell) without its `vertex_m` reads.
        let (i, j) = if full {
            let pitch = geom.seg_index().pitch_m();
            (locate_on(axis, pitch, x)?, locate_on(axis, pitch, z)?)
        } else {
            let loc = GroundLattice::new(geom.cooked()).locate(x, z)?;
            (loc.i, loc.j)
        };
        let last = cells - 1;
        Some(Self {
            start: [(i as i32).clamp(0, last), (j as i32).clamp(0, last)],
            step: [if d[0] < 0.0 { -1 } else { 1 }, if d[2] < 0.0 { -1 } else { 1 }],
            enter,
            exit,
            cells,
            from: [from[0], from[2]],
            d: [d[0], d[2]],
            first: lo,
            pitch: geom.seg_index().pitch_m(),
        })
    }

    /// The `t` of the walk's `p`-th crossing along `axis` (0 is `x`, 1 is `z`): the expression
    /// `walk_cells` uses, `(border - from) / d`. `p` up to [`GroundPath::crossings`] (the last one
    /// leaves the lattice).
    fn crossing(&self, geom: &WorldGeom, axis: usize, p: i32) -> f32 {
        let cell = self.start[axis] + p * self.step[axis];
        let border = (cell + i32::from(self.step[axis] > 0)) as u32;
        let at = geom
            .seg_index()
            .axis_m(border)
            .unwrap_or_else(|| vertex_m(&geom.cooked().lattice, border));
        (at - self.from[axis]) / self.d[axis]
    }

    /// The crossings along `axis` that end in a lattice cell; crossing number `crossings` leaves
    /// the lattice. `None` when the segment does not move along `axis` (no crossing at all).
    fn crossings(&self, axis: usize) -> Option<i32> {
        if self.d[axis] == 0.0 {
            return None;
        }
        Some(if self.step[axis] > 0 {
            self.cells - 1 - self.start[axis]
        } else {
            self.start[axis]
        })
    }

    /// How many crossings along `axis`, of numbers `0..=crossings`, come before `t` (`t` itself
    /// counted when `inclusive`): an estimate from the lattice pitch, corrected on the exact `t`s.
    fn count_before(&self, geom: &WorldGeom, axis: usize, t: f32, inclusive: bool) -> i32 {
        let Some(last) = self.crossings(axis) else {
            return 0;
        };
        let before = |p: i32| {
            let c = self.crossing(geom, axis, p);
            if inclusive { c <= t } else { c < t }
        };
        let estimate = if t.is_nan() {
            0
        } else if t == f32::INFINITY {
            last + 1
        } else if t == f32::NEG_INFINITY {
            0
        } else {
            let v = self.from[axis] + self.d[axis] * t;
            // `as` saturates (and takes NaN to 0); only a start for the exact correction below.
            let cell = ((v - self.first) / self.pitch).floor() as i64;
            let rel = cell
                .saturating_sub(i64::from(self.start[axis]))
                .saturating_mul(i64::from(self.step[axis]));
            rel.clamp(0, i64::from(last) + 1) as i32
        };
        let mut n = estimate;
        while n > 0 && !before(n - 1) {
            n -= 1;
        }
        while n <= last && before(n) {
            n += 1;
        }
        n
    }

    /// The single walk's first cell inside the cell rectangle `bounds` that it leaves at `t` of
    /// `s` or later, and the `t` it enters that cell at; `None` when the walk has no such cell
    /// (it misses the rectangle, leaves it before `s`, or stops first).
    fn entry(&self, geom: &WorldGeom, bounds: [[i32; 2]; 2], s: f32) -> Option<([i32; 2], f32)> {
        // The rectangle in steps along the walk.
        let mut low = [0i32; 2];
        let mut high = [0i32; 2];
        for axis in 0..2 {
            let (a, b) = if self.step[axis] > 0 {
                (
                    bounds[0][axis] - self.start[axis],
                    bounds[1][axis] - self.start[axis],
                )
            } else {
                (
                    self.start[axis] - bounds[1][axis],
                    self.start[axis] - bounds[0][axis],
                )
            };
            if b < 0 {
                return None;
            }
            low[axis] = a.max(0);
            high[axis] = b;
        }
        // Three cells of the walk; the latest of them is the first that is past all three
        // conditions. Every crossing before `s`:
        let mut at = [
            self.count_before(geom, 0, s, false),
            self.count_before(geom, 1, s, false),
        ];
        // The crossing into the rectangle's first column, and every `z` crossing before it:
        if low[0] > 0 {
            let t = self.crossing(geom, 0, low[0] - 1);
            at = [at[0].max(low[0]), at[1].max(self.count_before(geom, 1, t, false))];
        }
        // The crossing into its first row, and every `x` crossing at or before it (`x` first on a
        // tie):
        if low[1] > 0 {
            let t = self.crossing(geom, 1, low[1] - 1);
            at = [at[0].max(self.count_before(geom, 0, t, true)), at[1].max(low[1])];
        }
        if at[0] > high[0] || at[1] > high[1] {
            return None;
        }
        // The walk gets there only if every crossing on the way is finite and within the
        // segment's end; the crossings along an axis are in order, so the last of each is enough.
        let mut t_in = self.enter;
        for (axis, &n) in at.iter().enumerate() {
            if n > 0 {
                let t = self.crossing(geom, axis, n - 1);
                if !(t.is_finite() && t <= self.exit) {
                    return None;
                }
                t_in = det_max(t_in, t);
            }
        }
        Some((
            [
                self.start[0] + at[0] * self.step[0],
                self.start[1] + at[1] * self.step[1],
            ],
            t_in,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cook::{CookInputs, GroundInput, cook};

    /// A 129-vertex lattice of rough ground (32-cell tiles, no layers): every tile is plain.
    fn rough_ground() -> WorldGeom {
        let n = 129u32;
        let mut seed = 0x1234_5679u32;
        let counts = (0..n * n)
            .map(|k| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                // Rolling relief plus per-vertex noise, so cells differ and slopes are steep.
                let (i, j) = (k % n, k / n);
                (600 + ((i * 7 + j * 3) % 97) * 6 + (seed >> 24) % 160) as u16
            })
            .collect();
        let input = GroundInput {
            lattice_n: n,
            width_m: 80.0,
            offset_m: -20.0,
            step_m: 0.031_25,
            counts,
        };
        let cooked = cook(&CookInputs {
            document: b"(version:1, ground_tile_cells:32, layers:[])",
            ground: &input,
            terrain: None,
            read_file: &|_| Err("no source".into()),
        })
        .expect("synthetic ground")
        .file
        .canonical;
        let bytes = crate::cooked::encode_canonical(&cooked).expect("encode");
        WorldGeom::from_canonical_bytes(&bytes).expect("load")
    }

    fn bits(h: Option<SegmentHit>) -> Option<(u32, [u32; 3], SurfaceKind, Option<TriRef>)> {
        h.map(|h| (h.t.to_bits(), h.point.map(f32::to_bits), h.kind, h.tri))
    }

    /// ADR-8815: on plain ground tiles the per-cell height skip and the triangles built from the
    /// counts give the hit of the walk of every cell, bit for bit, on rough ground: falling shells,
    /// grazing lines, near-vertical drops, lines along lattice rows and through lattice vertices,
    /// and segments that start or end off the map. The sight line agrees with it.
    #[test]
    fn plain_ground_cells_equal_the_full_walk() {
        let geom = rough_ground();
        let state = geom.new_state();
        let seg = geom.seg_index();
        let tiles = geom.cooked().lattice.tiles_per_side().pow(2);
        assert_eq!(tiles, 16);
        assert!(
            (0..tiles).all(|t| seg.tile_is_plain(t)),
            "a layerless full-pitch tile is plain"
        );
        let axis = seg.axis().to_vec();
        let mut seed = 0x9e37_79b9u32;
        let mut unit = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed >> 8) as f32 / (1u32 << 24) as f32
        };
        let (mut hits, mut clear) = (0u32, 0u32);
        for k in 0..24_000u32 {
            let p = [(unit() - 0.5) * 84.0, unit() * 60.0 - 8.0, (unit() - 0.5) * 84.0];
            let vertex = |u: f32| axis[(u * (axis.len() - 1) as f32) as usize];
            let (from, to) = match k % 8 {
                // A falling shell.
                0 => (
                    p,
                    [p[0] + (unit() - 0.5) * 30.0, -25.0, p[2] + (unit() - 0.5) * 30.0],
                ),
                // Grazing: both ends near the relief.
                1 => (
                    [p[0], unit() * 14.0, p[2]],
                    [(unit() - 0.5) * 84.0, unit() * 14.0, (unit() - 0.5) * 84.0],
                ),
                // Near vertical: millimetres of horizontal travel.
                2 => (
                    p,
                    [
                        p[0] + (unit() - 0.5) * 0.004,
                        -25.0,
                        p[2] + (unit() - 0.5) * 0.004,
                    ],
                ),
                // Exactly vertical, on a lattice vertex.
                3 => {
                    let (x, z) = (vertex(unit()), vertex(unit()));
                    ([x, 60.0, z], [x, -25.0, z])
                }
                // Along a lattice row, on the row itself.
                4 => {
                    let z = vertex(unit());
                    ([-41.0, unit() * 14.0, z], [41.0, unit() * 14.0, z])
                }
                // From under the ground, upward.
                5 => (
                    [p[0], -22.0, p[2]],
                    [p[0] + (unit() - 0.5) * 20.0, 50.0, p[2] + (unit() - 0.5) * 20.0],
                ),
                // Short.
                6 => (
                    [p[0], unit() * 14.0, p[2]],
                    [
                        p[0] + (unit() - 0.5) * 3.0,
                        unit() * 14.0,
                        p[2] + (unit() - 0.5) * 3.0,
                    ],
                ),
                // Through a lattice vertex along the cell diagonal.
                _ => {
                    let (x, z) = (vertex(unit()), vertex(unit()));
                    let y = unit() * 14.0;
                    ([x - 9.0, y + 4.0, z + 9.0], [x + 9.0, y - 4.0, z - 9.0])
                }
            };
            for mask in [FaceMask::GROUND, FaceMask::SOLID] {
                let fast = geom.segment(&state, from, to, mask);
                let full = geom.segment_without_pyramid(&state, from, to, mask);
                assert_eq!(bits(fast), bits(full), "ray {k} {from:?} -> {to:?} mask {mask:?}");
            }
            let full = geom.segment_without_pyramid(&state, from, to, FaceMask::SOLID);
            assert_eq!(
                geom.line_of_sight(&state, from, to),
                full.is_none(),
                "ray {k} {from:?} -> {to:?}"
            );
            if full.is_some() {
                hits += 1;
            } else {
                clear += 1;
            }
        }
        assert!(hits > 8_000 && clear > 2_000, "hits {hits} clear {clear}");
    }

    /// ADR-10082 (the merge of ADR-8815 and ADR-8818): on an edited state that keeps its own height
    /// ranges, a tile an edit touches, and every coarser node over it, still gets a range for the
    /// descent's height tests, and the range holds the edited ground of the tile. Under ADR-8815's
    /// rule alone a touched tile got none, so one crater turned the height tests off where it landed.
    #[test]
    fn touched_tiles_keep_a_range_that_holds_the_edited_ground() {
        use crate::GroundEdit;
        let geom = rough_ground();
        let mut state = geom.new_state();
        let crater = GroundEdit::Crater {
            x_mm: -30_000,
            z_mm: -30_000,
            radius_mm: 4_000,
            depth_mm: 3_000,
        };
        state.apply_ground_edit(&geom, &crater).expect("edit applies");
        assert!(state.ground.pyramid_bounds(), "an applied edit keeps its ranges");
        let per = geom.cooked().lattice.tiles_per_side();
        let cells = u32::from(geom.cooked().lattice.ground_tile_cells);
        let touched: Vec<u32> = (0..per * per)
            .filter(|&t| state.ground.is_promoted(t) || state.ground.owns_overrides(t))
            .collect();
        assert!(!touched.is_empty(), "the crater touches a tile");
        let ground = GroundLattice::new(geom.cooked()).with_edits(&state.ground);
        let edits = Some(&state.ground);
        for &t in &touched {
            let range = geom
                .node_range(edits, 0, t, false)
                .expect("a touched tile keeps a range");
            let (a, b) = (t % per, t / per);
            let mut lowest = f32::INFINITY;
            for j in b * cells..(b + 1) * cells {
                for i in a * cells..(a + 1) * cells {
                    for (tri, _) in ground.cell_triangles(i, j).into_iter().flatten() {
                        for v in tri {
                            lowest = det_min(lowest, v[1]);
                            assert!(
                                v[1] >= range[0] - 0.001 && v[1] <= range[1] + 0.001,
                                "tile {t}: ground at {v:?} outside {range:?}"
                            );
                        }
                    }
                }
            }
            assert!(lowest.is_finite(), "tile {t} has ground");
        }
        // Every coarser node keeps one too, down to the root.
        let top = geom.height_pyramid.levels.len() - 1;
        for level in 1..=top {
            let side = geom.height_pyramid.levels[level].side;
            for index in 0..side * side {
                assert!(geom.node_range(edits, level, index, false).is_some());
            }
        }
        // An unedited state reads the cooked range.
        assert_eq!(
            geom.node_range(None, 0, touched[0], true),
            Some(geom.height_pyramid.levels[0].ranges[touched[0] as usize])
        );
    }

    /// ADR-8815: on a state with craters and a berm (in a tile's middle, on a tile corner, at the
    /// map edge) the descent trusts a tile's cooked range only where no edit
    /// touches it and reads the touched tiles with the edits: the hit, and the sight line, are
    /// those of the walk of every cell.
    #[test]
    fn edited_ground_equals_the_full_walk() {
        use crate::GroundEdit;
        let geom = rough_ground();
        let mut state = geom.new_state();
        // Tiles are 20 m (32 cells of 0.625 m), the map spans -40..40 m.
        let edits = [
            GroundEdit::Crater {
                x_mm: -30_000,
                z_mm: -30_000,
                radius_mm: 4_000,
                depth_mm: 3_000,
            },
            GroundEdit::Crater {
                x_mm: 20_100,
                z_mm: -19_900,
                radius_mm: 2_500,
                depth_mm: 4_000,
            },
            GroundEdit::Berm {
                x_mm: 39_000,
                z_mm: 12_000,
                radius_mm: 3_000,
                height_mm: 2_500,
            },
        ];
        for edit in &edits {
            state.apply_ground_edit(&geom, edit).expect("edit applies");
        }
        assert!(!state.ground.is_empty());
        // Some tiles are touched and some are not, so both branches of the descent run.
        let per = geom.cooked().lattice.tiles_per_side();
        let touched = (0..per * per)
            .filter(|&t| {
                let (east, north) = (t % per + 1 < per, t / per + 1 < per);
                state.ground.is_promoted(t)
                    || state.ground.owns_overrides(t)
                    || (east && state.ground.owns_overrides(t + 1))
                    || (north && state.ground.owns_overrides(t + per))
                    || (east && north && state.ground.owns_overrides(t + per + 1))
            })
            .count();
        assert!((4..=12).contains(&touched), "touched {touched} of 16");

        let mut seed = 0x6d2b_79f5u32;
        let mut unit = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed >> 8) as f32 / (1u32 << 24) as f32
        };
        let centers = [[-30.0, -30.0], [20.1, -19.9], [39.0, 12.0]];
        let (mut hits, mut clear) = (0u32, 0u32);
        for k in 0..16_000u32 {
            let p = [(unit() - 0.5) * 84.0, unit() * 60.0 - 8.0, (unit() - 0.5) * 84.0];
            let c = centers[(k / 4) as usize % centers.len()];
            let (from, to) = match k % 4 {
                // A shell into an edit.
                0 => (
                    p,
                    [c[0] + (unit() - 0.5) * 8.0, -25.0, c[1] + (unit() - 0.5) * 8.0],
                ),
                // Grazing across an edit.
                1 => (
                    [
                        c[0] - 12.0 + unit() * 4.0,
                        unit() * 14.0,
                        c[1] + (unit() - 0.5) * 6.0,
                    ],
                    [
                        c[0] + 12.0 - unit() * 4.0,
                        unit() * 14.0,
                        c[1] + (unit() - 0.5) * 6.0,
                    ],
                ),
                // Anywhere, long.
                2 => (
                    [p[0], unit() * 14.0, p[2]],
                    [(unit() - 0.5) * 84.0, unit() * 14.0, (unit() - 0.5) * 84.0],
                ),
                // Straight down near an edit.
                _ => {
                    let (x, z) = (c[0] + (unit() - 0.5) * 9.0, c[1] + (unit() - 0.5) * 9.0);
                    ([x, 60.0, z], [x + (unit() - 0.5) * 0.002, -25.0, z])
                }
            };
            for mask in [FaceMask::GROUND, FaceMask::SOLID] {
                let fast = geom.segment(&state, from, to, mask);
                let full = geom.segment_without_pyramid(&state, from, to, mask);
                assert_eq!(bits(fast), bits(full), "ray {k} {from:?} -> {to:?} mask {mask:?}");
            }
            let full = geom.segment_without_pyramid(&state, from, to, FaceMask::SOLID);
            assert_eq!(
                geom.line_of_sight(&state, from, to),
                full.is_none(),
                "ray {k} {from:?} -> {to:?}"
            );
            if full.is_some() {
                hits += 1;
            } else {
                clear += 1;
            }
        }
        assert!(hits > 6_000 && clear > 500, "hits {hits} clear {clear}");
    }

    /// Segments that sit on the lattice's seams: along tile borders and lattice rows, through
    /// tile corners and lattice vertices, vertical on them, ending on a border, starting off the
    /// map (ADR-8884).
    fn seam_rays(geom: &WorldGeom) -> Vec<([f32; 3], [f32; 3])> {
        let axis = geom.seg_index().axis().to_vec();
        let last = axis.len() - 1;
        let mut seed = 0x2545_f491u32;
        let mut unit = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed >> 8) as f32 / (1u32 << 24) as f32
        };
        let mut rays = Vec::new();
        // Tile borders (every 32 cells), lattice rows next to them, and the map's own edges.
        let lines = [0, 1, 31, 32, 33, 63, 64, 65, 96, 127, last];
        for k in 0..3_000u32 {
            let line = axis[lines[k as usize % lines.len()]];
            let other = axis[lines[(k as usize / lines.len()) % lines.len()]];
            let (h0, h1) = (unit() * 14.0 + 1.0, unit() * 14.0 + 1.0);
            let (u, v) = ((unit() - 0.5) * 84.0, (unit() - 0.5) * 84.0);
            rays.push(match k % 10 {
                // Along a border line in x, then in z.
                0 => ([-41.0, h0, line], [41.0, h1, line]),
                1 => ([line, h0, 41.0], [line, h1, -41.0]),
                // Through a tile corner, along a diagonal and steeper.
                2 => (
                    [line - 9.0, h0 + 4.0, other + 9.0],
                    [line + 9.0, h1 - 4.0, other - 9.0],
                ),
                3 => ([line - 3.0, 30.0, other - 7.0], [line + 3.0, -25.0, other + 7.0]),
                // Vertical on a corner, and a millimetre off it.
                4 => ([line, 60.0, other], [line, -25.0, other]),
                5 => (
                    [line + 0.001, 60.0, other - 0.001],
                    [line + 0.001, -25.0, other - 0.001],
                ),
                // Ending, or starting, exactly on a border.
                6 => ([u, h0 + 6.0, v], [line, h1 - 6.0, other]),
                7 => ([line, h0 + 6.0, other], [u, h1 - 6.0, v]),
                // Off the map into a border line.
                8 => ([-60.0, h0 + 10.0, line], [line, -20.0, line]),
                // Short, across a corner.
                _ => ([line - 0.3, h0, other - 0.2], [line + 0.2, h1, other + 0.3]),
            });
        }
        rays
    }

    /// ADR-8884: `GroundPath::entry` names the cell, and the `t`, where the single walk over the
    /// whole lattice first is inside a tile and leaves no earlier than `s`, on seam segments,
    /// for every tile and for `s` before the segment, at each cell's own crossings (and one ulp
    /// either side) and past the end; `None` exactly when the walk has no such cell.
    #[test]
    fn ground_path_entry_is_the_single_walk_cell() {
        let geom = rough_ground();
        let l = &geom.cooked().lattice;
        let side = i32::from(l.ground_tile_cells);
        let per = l.tiles_per_side() as i32;
        let (mut checked, mut some) = (0u32, 0u32);
        for (k, (from, to)) in seam_rays(&geom).into_iter().enumerate() {
            let Some(ray) = Ray::new(from, sub(to, from)) else {
                continue;
            };
            let Some(path) = GroundPath::new(&geom, &ray) else {
                continue;
            };
            let d = ray.d;
            // The single walk, as `walk_cells` steps it: each cell, the `t` the walk enters it
            // and the `t` of the crossing it leaves it by.
            let mut cells: Vec<([i32; 2], f32, f32)> = Vec::new();
            let mut cell = path.start;
            let mut t_in = path.enter;
            let bounds = [[0, 0], [path.cells - 1; 2]];
            loop {
                let mut next = [f32::INFINITY; 2];
                for (a, world) in [0, 2].into_iter().enumerate() {
                    if d[world] != 0.0 {
                        let border = cell[a] + i32::from(path.step[a] > 0);
                        next[a] = (vertex_m(l, border as u32) - from[world]) / d[world];
                    }
                }
                let a = usize::from(next[0] > next[1]);
                cells.push((cell, t_in, next[a]));
                if next[a] > path.exit || !next[a].is_finite() {
                    break;
                }
                cell[a] += path.step[a];
                if cell[a] < bounds[0][a] || cell[a] > bounds[1][a] {
                    break;
                }
                t_in = det_max(path.enter, next[a]);
            }
            let mut times = vec![f32::NEG_INFINITY, 0.0, 0.5, 1.0, 2.0];
            for &(_, t_in, t_out) in &cells {
                for t in [t_in, t_out] {
                    if t.is_finite() {
                        times.extend([t, t.next_up(), t.next_down()]);
                    }
                }
            }
            for tile in 0..per * per {
                let (a, b) = (tile % per, tile / per);
                let tb = [[a * side, b * side], [(a + 1) * side - 1, (b + 1) * side - 1]];
                let inside = |c: [i32; 2]| (0..2).all(|i| c[i] >= tb[0][i] && c[i] <= tb[1][i]);
                for &s in &times {
                    let expected = cells
                        .iter()
                        .find(|&&(c, _, t_out)| inside(c) && t_out >= s)
                        .map(|&(c, t_in, _)| (c, t_in.to_bits()));
                    let got = path.entry(&geom, tb, s).map(|(c, t_in)| (c, t_in.to_bits()));
                    assert_eq!(got, expected, "ray {k} {from:?} -> {to:?} tile {tile} s {s}");
                    checked += 1;
                    some += u32::from(got.is_some());
                }
            }
        }
        assert!(
            checked > 500_000 && some > 50_000,
            "checked {checked} some {some}"
        );
    }

    /// ADR-8884: on seam segments, on a fresh state and with craters on tile corners and borders,
    /// `segment` and the sight line give the walk of every cell's answer, bit for bit.
    #[test]
    fn seam_segments_equal_the_full_walk() {
        use crate::GroundEdit;
        let geom = rough_ground();
        let axis = geom.seg_index().axis().to_vec();
        let mm = |v: f32| (v * 1000.0).round() as i32;
        let mut edited = geom.new_state();
        for (i, j, radius_mm) in [(32, 32, 3_000), (64, 96, 1_500), (96, 64, 800), (0, 32, 2_000)] {
            edited
                .apply_ground_edit(
                    &geom,
                    &GroundEdit::Crater {
                        x_mm: mm(axis[i]),
                        z_mm: mm(axis[j]),
                        radius_mm,
                        depth_mm: 2_500,
                    },
                )
                .expect("edit applies");
        }
        let fresh = geom.new_state();
        let mut hits = 0u32;
        for (k, (from, to)) in seam_rays(&geom).into_iter().enumerate() {
            for (name, state) in [("fresh", &fresh), ("edited", &edited)] {
                for mask in [FaceMask::GROUND, FaceMask::SOLID] {
                    let fast = geom.segment(state, from, to, mask);
                    let full = geom.segment_without_pyramid(state, from, to, mask);
                    assert_eq!(
                        bits(fast),
                        bits(full),
                        "{name} ray {k} {from:?} -> {to:?} mask {mask:?}"
                    );
                    hits += u32::from(full.is_some());
                }
                let full = geom.segment_without_pyramid(state, from, to, FaceMask::SOLID);
                assert_eq!(
                    geom.line_of_sight(state, from, to),
                    full.is_none(),
                    "{name} ray {k} {from:?} -> {to:?}"
                );
            }
        }
        assert!(hits > 4_000, "hits {hits}");
    }

    /// The review's lattices (ADR-8884): 65 vertices, 16-cell tiles, heights `1000 + (3i + 5j) %
    /// 17` millimetre counts, 2 mm wide (degenerate cells) and 20 m wide.
    fn review_lattice(width_m: f32, offset_m: f32) -> WorldGeom {
        let n = 65u32;
        let counts = (0..n * n)
            .map(|v| 1_000u16 + ((v % n * 3 + v / n * 5) % 17) as u16)
            .collect();
        let input = GroundInput {
            lattice_n: n,
            width_m,
            offset_m,
            step_m: 0.001,
            counts,
        };
        let cooked = cook(&CookInputs {
            document: b"(version:1, ground_tile_cells:16, layers:[])",
            ground: &input,
            terrain: None,
            read_file: &|_| Err("no source".into()),
        })
        .expect("synthetic ground")
        .file
        .canonical;
        let bytes = crate::cooked::encode_canonical(&cooked).expect("encode");
        WorldGeom::from_canonical_bytes(&bytes).expect("load")
    }

    /// ADR-8884: on the review's two lattices, unedited, `segment` gives the walk of every cell's
    /// answer on segments along every lattice row and column, and vertical on and beside every
    /// vertex: the seams where a tile's own walk used to start in a cell the single walk never
    /// visits.
    #[test]
    fn lattice_line_segments_equal_the_full_walk() {
        let mut checked = 0u32;
        let mut hits = 0u32;
        for (width_m, offset_m) in [(20.0, 0.25), (0.002, 0.0)] {
            let geom = review_lattice(width_m, offset_m);
            let state = geom.new_state();
            let axis = geom.seg_index().axis().to_vec();
            let (lo, hi) = (axis[0], axis[axis.len() - 1]);
            let reach = (hi - lo) * 0.1;
            let mut rays = Vec::new();
            for (k, &line) in axis.iter().enumerate() {
                // Grazing along the line, through the relief (counts 1.000-1.016 m above the
                // offset), both ways and on both axes.
                let (a, b) = (offset_m + 1.016, offset_m + 0.999);
                rays.push(([lo - reach, a, line], [hi + reach, b, line]));
                rays.push(([hi, a, line], [lo, b, line]));
                rays.push(([line, a, lo - reach], [line, b, hi]));
                rays.push(([line, a, hi + reach], [line, b, lo]));
                for &other in axis.iter().skip(k % 7).step_by(7) {
                    let top = offset_m + 30.0;
                    rays.push(([line, top, other], [line, offset_m - 30.0, other]));
                    let off = (hi - lo) / 128.0;
                    rays.push((
                        [line + off, top, other - off],
                        [line + off, offset_m - 30.0, other - off],
                    ));
                }
            }
            for (from, to) in rays {
                let fast = geom.segment(&state, from, to, FaceMask::GROUND);
                let full = geom.segment_without_pyramid(&state, from, to, FaceMask::GROUND);
                assert_eq!(bits(fast), bits(full), "width {width_m} {from:?} -> {to:?}");
                checked += 1;
                hits += u32::from(full.is_some());
            }
        }
        assert!(checked > 1_000 && hits > 500, "checked {checked} hits {hits}");
    }

    /// ADR-8884: `locate_on` (the axis table) finds `GroundLattice::locate`'s cell on both review
    /// lattices (the 2 mm one has zero-width cells) and rough ground, at every vertex, one ulp either
    /// side, cell middles and off the map.
    #[test]
    fn locate_on_the_axis_table_is_the_lattice_locate() {
        for geom in [
            review_lattice(20.0, 0.25),
            review_lattice(0.002, 0.0),
            rough_ground(),
        ] {
            let axis = geom.seg_index().axis().to_vec();
            let pitch = geom.seg_index().pitch_m();
            let ground = GroundLattice::new(geom.cooked());
            let mut probes = vec![f32::NAN, f32::INFINITY, f32::NEG_INFINITY];
            for w in axis.windows(2) {
                probes.extend([w[0], w[0].next_up(), w[0].next_down(), (w[0] + w[1]) * 0.5]);
            }
            let last = axis[axis.len() - 1];
            probes.extend([last, last.next_up(), last.next_down()]);
            for v in probes {
                let lattice = ground.locate(v, axis[0]).map(|loc| loc.i);
                assert_eq!(locate_on(&axis, pitch, v), lattice, "v {v}");
            }
        }
    }

    /// ADR-8815: the count read of a plain tile is `GroundLattice::count_in_tile` for every vertex
    /// of every cell, borders owned by the east, north and north-east tiles included.
    #[test]
    fn plain_counts_equal_the_lattice_read() {
        let geom = rough_ground();
        let c = geom.cooked();
        let l = &c.lattice;
        let ground = GroundLattice::new(c);
        let shift = u32::from(l.ground_tile_cells).trailing_zeros();
        let per = l.tiles_per_side();
        for j in 0..l.cells() {
            for i in 0..l.cells() {
                let tile = ground.tile_of_cell(i, j);
                for (vi, vj) in [(i, j), (i + 1, j), (i, j + 1), (i + 1, j + 1)] {
                    assert_eq!(
                        plain_count(c, shift, per, vi, vj),
                        ground.count_in_tile(tile, vi, vj),
                        "cell ({i}, {j}) vertex ({vi}, {vj})"
                    );
                }
            }
        }
    }
}
