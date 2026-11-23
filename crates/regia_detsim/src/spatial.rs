// SPDX-License-Identifier: MPL-2.0
//! Reusable, conservative XZ queries in caller array-slot order (ADR-8318).
//!
//! Build once per tick. [`DetSpatial::within`] only prunes: retain the game's exact
//! float distance test in the callback. Candidates may lie outside the radius.
//! The query center is a [`BroadBody`] so the same [`crate::broadphase::FloorRule`] as the collision
//! pass can filter height bands; its `radius` is ignored in favor of the explicit
//! query radius. A mask matches when it shares any bit with the row's group mask.
//!
//! Original uniform-grid implementation, based on Ericson, *Real-Time Collision
//! Detection*, chapter 7. Shared geometry and rounding envelope come from Regia's
//! broadphase. No hash containers, platform float operations or new dependencies.
//!
//! Segment and shape queries (ADR-8478): [`DetSpatial::along_segment`] walks the cells
//! a padded capsule covers (an original cell walk after Amanatides and Woo, "A Fast
//! Voxel Traversal Algorithm for Ray Tracing", Eurographics 1987);
//! [`DetSpatial::within_sector`] and [`DetSpatial::within_box`] prune by a padded
//! bounding box. All three visit candidates in ascending slot order, like `within`.
//! [`DetSpatial::build_from`] builds from any slice through a projection, for example
//! a lag-compensation rewind index over the game's own history rows.

use std::ops::ControlFlow;

use crate::broadphase::{
    BroadBody, BroadphaseConfig, DetBroadphase, GridGeometry, bodies_on_different_floors, query_pad,
};
use crate::debug::{
    DebugCapture, DebugCaptureStats, DebugKind, DebugSink, DebugSource, DebugWhat, GridShape,
};
use crate::float::{det_clamp, det_clamp_f64, det_max, det_min};

const NONE: usize = usize::MAX;
/// Segment endpoints farther than this many cells from the grid origin scan every cell:
/// the cell walk's f64 rounding stays far below its 1/1024-cell allowance inside it.
const WALK_LIMIT: f64 = (1_u64 << 30) as f64;

/// One host-array row; slots must form a permutation of `0..rows.len()`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpatialRow {
    /// Caller array index, never an entity id. Input order need not equal slot order.
    pub slot: usize,
    /// Position, radius, absolute height band and underground status.
    pub body: BroadBody,
    /// Team bits or other host-defined groups. Zero matches no query mask.
    pub group_mask: u64,
    /// Opaque host flags; preserved by the index, never interpreted as group bits.
    pub flags: u32,
}

/// Invalid row-slot permutation or an update to an absent slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpatialError;
impl SpatialError {
    /// Stable lowercase code for host agent rows.
    pub fn code(self) -> &'static str {
        "spatial_slot"
    }
}
impl std::fmt::Display for SpatialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("spatial_slot: slots must be a permutation of the row array indices")
    }
}
impl std::error::Error for SpatialError {}

/// Counters since construction or [`DetSpatial::reset_stats`]. No wall-clock reads.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SpatialStats {
    /// Successful builds.
    pub builds: u64,
    /// Range, nearest or first-k calls (a first-k call counts once).
    pub queries: u64,
    /// Mask/band-matching candidates delivered to a callback or nearest metric.
    pub candidates: u64,
    /// Selected nearest results and exact first-k predicate hits. Plain range
    /// callbacks cannot report exact hits, so those calls do not increment this.
    pub hits: u64,
    /// Updates that change a row's cell or wild-list membership.
    pub refiles: u64,
    /// Current rows with non-finite position, radius or height band (a gauge).
    pub wild_rows: usize,
    /// [`DetSpatial::along_segment`] calls, including those from
    /// [`DetSpatial::first_along_segment`]. Not counted in `queries`.
    pub segment_queries: u64,
    /// [`DetSpatial::within_sector`] and [`DetSpatial::within_box`] calls. Not counted
    /// in `queries`.
    pub shape_queries: u64,
    /// Chebyshev cell rings [`DetSpatial::nearest`] walked (ring 0, the point's own cell,
    /// counts). Rings past the early-out (ADR-8875) are not walked and do not count.
    pub nearest_rings: u64,
    /// Grid cells, plus the wild and large lists when not empty, whose rows
    /// [`DetSpatial::first_k_within`] and [`DetSpatial::first_k_within_by`] gathered. The gather
    /// stops once the k-th hit lies in a slot range every remaining list starts after
    /// (ADR-8875), so a small `k` usually leaves cells unread.
    pub first_k_lists: u64,
    /// Grid cells whose lists segment, sector and box queries gathered (the wild list
    /// is not a cell). Range and nearest queries do not add to it.
    pub cells_walked: u64,
    /// Rows outside the config's `world_bounds` at the last build (a gauge, ADR-8837). They
    /// are filed in the grid's edge cells, so they no longer stretch the grid.
    pub outlier_rows: usize,
    /// Current rows whose `|radius|` is above the config's large threshold (a gauge,
    /// ADR-8837). They are always candidates, like wild rows, so they no longer widen
    /// every query's reach.
    pub large_rows: usize,
}

#[derive(Debug, Clone, Copy)]
struct Link {
    prev: usize,
    next: usize,
    cell: usize,
}

/// Uniform grid with fixed per-row links, reusable candidate buffers and counters.
///
/// Build allocates only when the row high-water mark grows (cell storage is sized
/// to `max_cells` on the first build). Updates and queries allocate nothing;
/// caller-owned output vectors must have enough capacity. Wrap each query batch
/// in the host's tick profiling span; this crate has no logging dependency.
#[derive(Debug, Clone)]
pub struct DetSpatial {
    config: BroadphaseConfig,
    geometry: GridGeometry,
    rows: Vec<SpatialRow>,
    links: Vec<Link>,
    heads: Vec<usize>,
    /// Lowest slot in each list (`NONE` when empty), for the first-k early stop (ADR-8875).
    list_min: Vec<usize>,
    /// Scratch: the first-k query's non-empty lists by lowest slot (capacity `max_cells + 2`).
    order: Vec<usize>,
    seen: Vec<bool>,
    candidates: Vec<usize>,
    candidate_bits: Vec<u64>,
    staging: Vec<SpatialRow>,
    max_radius: f32,
    revision: u64,
    stats: SpatialStats,
    /// Debug capture (ADR-8562): `None` (the default) does no debug work and allocates nothing.
    debug: Option<Box<SpatialCapture>>,
}

/// Hits kept inline per recorded query; a query with more keeps the first ones and its total.
const QUERY_HITS: usize = 16;

/// One recorded query: its shape and the slots it handed back (ADR-8562).
#[derive(Debug, Clone, Copy)]
struct QueryRecord {
    center: [f32; 3],
    r: f32,
    hits: [u32; QUERY_HITS],
    kept: u8,
    total: u32,
}

impl QueryRecord {
    const EMPTY: Self = Self {
        center: [0.0; 3],
        r: 0.0,
        hits: [0; QUERY_HITS],
        kept: 0,
        total: 0,
    };

    fn hit(&mut self, slot: usize) {
        if usize::from(self.kept) < QUERY_HITS {
            self.hits[usize::from(self.kept)] = u32::try_from(slot).unwrap_or(u32::MAX);
            self.kept += 1;
        }
        self.total = self.total.saturating_add(1);
    }
}

/// The query ring, kept only while debug capture is on. A fixed array: recording allocates nothing.
#[derive(Debug, Clone)]
struct SpatialCapture {
    caps: DebugCapture,
    ring: Vec<QueryRecord>,
    /// Index of the newest record.
    last: usize,
    /// Records written, saturating at the ring length.
    filled: usize,
}

impl SpatialCapture {
    fn new(caps: DebugCapture) -> Self {
        Self {
            caps,
            ring: vec![QueryRecord::EMPTY; caps.queries],
            last: 0,
            filled: 0,
        }
    }

    fn begin(&mut self, center: [f32; 3], r: f32) {
        if self.ring.is_empty() {
            return;
        }
        self.last = if self.filled == 0 {
            0
        } else {
            (self.last + 1) % self.ring.len()
        };
        self.filled = (self.filled + 1).min(self.ring.len());
        self.ring[self.last] = QueryRecord {
            center,
            r,
            ..QueryRecord::EMPTY
        };
    }

    fn hit(&mut self, slot: usize) {
        if let Some(record) = self.ring.get_mut(self.last)
            && self.filled > 0
        {
            record.hit(slot);
        }
    }

    fn set_hits(&mut self, slots: &[usize]) {
        if let Some(record) = self.ring.get_mut(self.last)
            && self.filled > 0
        {
            record.kept = 0;
            record.total = 0;
            for &slot in slots {
                record.hit(slot);
            }
        }
    }

    /// Records oldest first.
    fn records(&self) -> impl Iterator<Item = &QueryRecord> {
        let len = self.ring.len();
        let first = (self.last + len + 1 - self.filled) % len.max(1);
        (0..self.filled).map(move |i| &self.ring[(first + i) % len])
    }
}

impl Default for DetSpatial {
    fn default() -> Self {
        Self::new(BroadphaseConfig::default())
    }
}
impl DetSpatial {
    /// Use the pair grid's settings and normalization. `margin` is unused: each
    /// explicit update is immediately visible. A 40 m cell is useful for 80 m queries.
    pub fn new(config: BroadphaseConfig) -> Self {
        let config = DetBroadphase::new(config).config();
        Self {
            config,
            geometry: GridGeometry::new(config, [0.0; 2], [0.0; 2]),
            rows: Vec::new(),
            links: Vec::new(),
            heads: Vec::new(),
            list_min: Vec::new(),
            order: Vec::new(),
            seen: Vec::new(),
            candidates: Vec::new(),
            candidate_bits: Vec::new(),
            staging: Vec::new(),
            max_radius: 0.0,
            revision: 0,
            stats: SpatialStats::default(),
            debug: None,
        }
    }
    /// Normalized settings; `margin` has no effect on this index.
    pub fn config(&self) -> BroadphaseConfig {
        self.config
    }
    /// Copy the profiler counters without resetting them.
    pub fn stats(&self) -> SpatialStats {
        self.stats
    }
    /// Reset cumulative counters, preserving the current wild-row, outlier and large-row gauges.
    pub fn reset_stats(&mut self) {
        self.stats = SpatialStats {
            wild_rows: self.stats.wild_rows,
            outlier_rows: self.stats.outlier_rows,
            large_rows: self.stats.large_rows,
            ..Default::default()
        };
    }
    /// Current row for a slot, including any explicit updates.
    pub fn row(&self, slot: usize) -> Option<&SpatialRow> {
        self.rows.get(slot)
    }

    /// Replace all rows and reuse buffers. Invalid/duplicate slots return
    /// `spatial_slot` and leave the previous index and stats intact.
    pub fn build(&mut self, rows: &[SpatialRow]) -> Result<(), SpatialError> {
        self.seen.clear();
        self.seen.resize(rows.len(), false);
        for row in rows {
            let Some(seen) = self.seen.get_mut(row.slot) else {
                return Err(SpatialError);
            };
            if *seen {
                return Err(SpatialError);
            }
            *seen = true;
        }
        // The grid spans the tame rows inside `world_bounds` that are not large; outliers clamp
        // into its edge cells (cell indices are monotone, so every query stays conservative)
        // and large rows go to their own always-candidate list (ADR-8837).
        let mut lo = [f32::INFINITY; 2];
        let mut hi = [f32::NEG_INFINITY; 2];
        self.max_radius = 0.0;
        let mut outliers = 0;
        for row in rows {
            let b = &row.body;
            if self.is_large(b) {
                continue;
            }
            if tame(b) {
                if self.config.is_outside(b.pos[0], b.pos[2]) {
                    outliers += 1;
                } else {
                    lo = [det_min(lo[0], b.pos[0]), det_min(lo[1], b.pos[2])];
                    hi = [det_max(hi[0], b.pos[0]), det_max(hi[1], b.pos[2])];
                }
            }
            if b.radius.is_finite() {
                self.max_radius = det_max(self.max_radius, abs(b.radius));
            }
        }
        if lo[0] > hi[0] {
            lo = [0.0; 2];
            hi = [0.0; 2];
        }
        self.geometry = GridGeometry::new(self.config, lo, hi);
        self.heads.resize(self.config.max_cells + 2, NONE);
        self.heads.fill(NONE);
        self.list_min.resize(self.config.max_cells + 2, NONE);
        self.list_min.fill(NONE);
        self.order.clear();
        self.order.reserve(self.config.max_cells + 2);
        self.rows.clear();
        self.rows.extend_from_slice(rows);
        for row in rows {
            self.rows[row.slot] = *row;
        }
        self.links.clear();
        self.links.resize(
            rows.len(),
            Link {
                prev: NONE,
                next: NONE,
                cell: NONE,
            },
        );
        self.candidates.clear();
        self.candidates.reserve(rows.len());
        self.candidate_bits.resize(rows.len().div_ceil(64), 0);
        self.stats.wild_rows = 0;
        self.stats.large_rows = 0;
        self.stats.outlier_rows = outliers;
        for slot in 0..rows.len() {
            let cell = self.destination(&self.rows[slot].body);
            self.file(slot, cell);
        }
        self.stats.builds += 1;
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    /// [`Self::build`] from any slice: `to_row(index, item)` projects each element to a
    /// row, so the game's own row type (for example its history rows for a past tick)
    /// needs no intermediate vector. The projected slots must form a permutation of
    /// `0..items.len()`, exactly as for `build`; otherwise `spatial_slot` is returned
    /// and the previous index and stats are kept.
    ///
    /// Lag compensation: keep a second `DetSpatial` and build it from the history rows
    /// of the rewind tick, then run [`Self::along_segment`] on it. Regia stores no
    /// history. The cost is one build (linear in rows) per distinct rewind tick per
    /// tick; cache the index by rewind tick in the caller and reuse it for every shot
    /// that rewinds to the same tick. A projection is called once per item; it allocates
    /// only when the row high-water mark grows.
    pub fn build_from<T>(
        &mut self,
        items: &[T],
        mut to_row: impl FnMut(usize, &T) -> SpatialRow,
    ) -> Result<(), SpatialError> {
        let mut staging = std::mem::take(&mut self.staging);
        staging.clear();
        staging.extend(items.iter().enumerate().map(|(index, item)| to_row(index, item)));
        let result = self.build(&staging);
        self.staging = staging;
        result
    }

    /// Immediately move a slot to XZ `pos`, including movement outside the build
    /// bounds or to/from non-finite coordinates. An absent slot returns `spatial_slot`.
    ///
    /// Like the pair visitor, a mutable query visitor may change XZ only. Update
    /// the host row and call this method before continuing the pass. Radius, Y,
    /// band, underground status, masks and flags remain fixed until the next build.
    pub fn update(&mut self, slot: usize, pos: [f32; 2]) -> Result<(), SpatialError> {
        let Some(row) = self.rows.get_mut(slot) else {
            return Err(SpatialError);
        };
        row.body.pos[0] = pos[0];
        row.body.pos[2] = pos[1];
        let cell = self.destination(&self.rows[slot].body);
        if cell != self.links[slot].cell {
            self.unfile(slot);
            self.file(slot, cell);
            self.stats.refiles += 1;
            self.revision = self.revision.wrapping_add(1);
        }
        Ok(())
    }

    /// Visit conservative candidates in ascending slot order, once each, stopping
    /// immediately on `Break`. Apply the exact distance predicate in `visit`.
    ///
    /// The envelope covers squared XZ disc distance against `(r + row.radius)^2`
    /// using f32 subtraction/products/addition (either strict or inclusive). Wild
    /// rows are always candidates subject to the same mask and floor filter.
    /// Custom scores are safe only after applying that geometric acceptance test.
    pub fn within(
        &mut self,
        center: BroadBody,
        r: f32,
        mask: u64,
        mut visit: impl FnMut(usize) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        self.stats.queries += 1;
        if let Some(d) = self.debug.as_deref_mut() {
            d.begin(center.pos, r);
        }
        if !self.begin_gather() {
            return ControlFlow::Continue(());
        }
        // The marked slots are visited straight from the bitset (ADR-8875): the same ascending
        // sequence `gather` would list, without writing the candidate list first.
        let [x0, x1, z0, z1] = self.bounds(&center, r);
        for z in z0..=z1 {
            for x in x0..=x1 {
                self.gather_cell((z * self.geometry.dims[0] + x) as usize);
            }
        }
        self.gather_cell(self.config.max_cells);
        self.gather_cell(self.large_list());
        self.visit_marked(0, self.rows.len(), &center, mask, &mut visit)
    }

    /// Ordered query that permits [`Self::update`] inside the visitor. After a
    /// refile, regather slots strictly after the last visited slot. Moves never
    /// revisit an earlier slot. This matches a linear scan that updates rows at
    /// its accepted visits; do not attach side effects to rejected candidates.
    ///
    /// The center and query radius are fixed for the call. The visitor may update
    /// any row's XZ and external state, but may not build, nest queries, or change
    /// other row fields. Read-only `row`/`stats` access is permitted.
    pub fn within_mut(
        &mut self,
        center: BroadBody,
        r: f32,
        mask: u64,
        mut visit: impl FnMut(&mut Self, usize) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        self.stats.queries += 1;
        if let Some(d) = self.debug.as_deref_mut() {
            d.begin(center.pos, r);
        }
        self.gather(&center, r);
        let mut at = 0;
        while let Some(&slot) = self.candidates.get(at) {
            at += 1;
            if !self.matches(slot, &center, mask) {
                continue;
            }
            self.stats.candidates += 1;
            if let Some(d) = self.debug.as_deref_mut() {
                d.hit(slot);
            }
            let revision = self.revision;
            visit(self, slot)?;
            if revision != self.revision {
                self.gather(&center, r);
                at = self.candidates.partition_point(|&s| s <= slot);
            }
        }
        ControlFlow::Continue(())
    }

    /// The first `k` conservative candidates in slot order, replacing `out`.
    /// Candidates are not exact hits; use [`Self::first_k_within_by`] to supply
    /// the game's exact predicate. Reserve output capacity to avoid allocation.
    pub fn first_k_within(&mut self, center: BroadBody, r: f32, mask: u64, k: usize, out: &mut Vec<usize>) {
        out.clear();
        self.first_k_visit(&center, r, mask, |slot| {
            if out.len() < k {
                out.push(slot);
            }
            if out.len() == k {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
    }

    /// First `k` exact hits in ascending slot order, replacing `out`. `accept`
    /// retains the host's exact geometric predicate; stop at the kth acceptance.
    pub fn first_k_within_by(
        &mut self,
        center: BroadBody,
        r: f32,
        mask: u64,
        k: usize,
        out: &mut Vec<usize>,
        mut accept: impl FnMut(usize) -> bool,
    ) {
        out.clear();
        self.first_k_visit(&center, r, mask, |slot| {
            if out.len() == k {
                return ControlFlow::Break(());
            }
            if accept(slot) {
                out.push(slot);
            }
            if out.len() == k {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        self.stats.hits += out.len() as u64;
        if let Some(d) = self.debug.as_deref_mut() {
            // Exact hits replace the conservative candidates `within` recorded.
            d.set_hits(out);
        }
    }

    /// Visit every slot whose disc may touch the capsule from `from` to `to` with
    /// radius `r`, in ascending slot order (not in order along the segment), once
    /// each, stopping on `Break`. Apply the exact capsule test in `visit`.
    ///
    /// `from` gives the start point (its XZ) and the height band and underground flag
    /// for the floor rule; its `radius` is ignored. `to` is the XZ end point. A
    /// projectile's "first entity hit in entity order" is
    /// [`Self::first_along_segment`].
    ///
    /// The envelope covers the closest-point test in f32 (`t = dot(p - a, b - a) /
    /// |b - a|²` clamped to `[0, 1]`, squared distance from `p` to `a + t (b - a)`,
    /// against `(r + row.radius)²`, strict or inclusive). A NaN `t` (zero length or an
    /// overflow) must select an endpoint. The index pads the capsule by
    /// `(reach + |dx| + |dz|) / 256 + max|coordinate| / 262144 + 0.001` and walks every
    /// cell within that distance, so long segments and radii larger than a cell lose
    /// no candidates. Non-finite endpoints or radius, an overflowed padded reach or
    /// reach squared, and endpoints more than 2^30 cells from the grid scan every
    /// cell. Wild rows are always candidates; mask and floor rule always apply.
    pub fn along_segment(
        &mut self,
        from: BroadBody,
        to: [f32; 2],
        r: f32,
        mask: u64,
        mut visit: impl FnMut(usize) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        self.stats.segment_queries += 1;
        if self.begin_gather() {
            match self.segment_walk(&from, to, r) {
                Some(walk) => self.walk_segment(walk),
                None => self.gather_rect(self.all_cells()),
            }
            self.finish_gather();
        }
        self.visit_candidates(&from, mask, &mut visit)
    }

    /// The lowest slot along the segment that `accept` (the game's exact capsule test)
    /// accepts, or `None`. Candidates and envelope as [`Self::along_segment`]; the walk
    /// stops at the first acceptance.
    pub fn first_along_segment(
        &mut self,
        from: BroadBody,
        to: [f32; 2],
        r: f32,
        mask: u64,
        mut accept: impl FnMut(usize) -> bool,
    ) -> Option<usize> {
        let mut found = None;
        let _ = self.along_segment(from, to, r, mask, |slot| {
            if accept(slot) {
                found = Some(slot);
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        if found.is_some() {
            self.stats.hits += 1;
        }
        found
    }

    /// Visit candidates for the sector at `apex` around `dir` (any non-zero length)
    /// with half-angle cosine `cos_half` and radius `r`, in ascending slot order. The
    /// index prunes by the sector's padded bounding box (plus the largest row radius);
    /// `visit` must apply the exact cone test, including the distance test against
    /// `r + row.radius`. `apex` supplies the floor-rule band.
    ///
    /// `cos_half` is clamped to `[-1, 1]` for the box (`-1` is a full disc). A zero,
    /// subnormal-length or non-finite `dir`, or a NaN `cos_half`, prunes by the
    /// bounding circle as [`Self::within`] does. The box pad `reach / 256` covers an
    /// angular rounding of the caller's cone test up to about 1/256 radian.
    pub fn within_sector(
        &mut self,
        apex: BroadBody,
        dir: [f32; 2],
        cos_half: f32,
        r: f32,
        mask: u64,
        mut visit: impl FnMut(usize) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        self.stats.shape_queries += 1;
        if self.begin_gather() {
            let bounds = self.sector_bounds(&apex, dir, cos_half, r);
            self.gather_rect(bounds);
            self.finish_gather();
        }
        self.visit_candidates(&apex, mask, &mut visit)
    }

    /// Visit candidates for the XZ box with corners `min` and `max` (sorted per axis,
    /// so inverted corners mean the same box), in ascending slot order. The index pads
    /// the box by the largest row radius plus a rounding pad; `visit` applies the exact
    /// box test. `probe` supplies only the floor-rule band and underground flag. A NaN
    /// corner scans every cell.
    pub fn within_box(
        &mut self,
        probe: BroadBody,
        min: [f32; 2],
        max: [f32; 2],
        mask: u64,
        mut visit: impl FnMut(usize) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        self.stats.shape_queries += 1;
        if self.begin_gather() {
            let bounds = self.box_bounds(min, max);
            self.gather_rect(bounds);
            self.finish_gather();
        }
        self.visit_candidates(&probe, mask, &mut visit)
    }

    /// Nearest row by the caller's pure squared XZ distance, with `d <= max_r²`.
    /// NaN distances do not win; equal distances choose the lowest slot, including
    /// equality at the radius. Negative/NaN `max_r` returns `None`.
    ///
    /// Cells are visited in expanding Chebyshev rings, then wild and large rows. `dist_fn`
    /// must compute the squared XZ distance from this center to the current row, f32 as
    /// the game writes it (and may return NaN to reject a row); it must not have
    /// order-dependent effects. A score is safe when it is never below the squared XZ
    /// distance from the center to the row's disc: the center distance, the distance to
    /// the disc, or a larger penalized value. Arbitrary priority scores require
    /// [`Self::within`] and a host reduction. The result uses only callback floats, never
    /// a recomputed result distance.
    ///
    /// The walk stops at the first ring whose rows all lie farther than the incumbent
    /// (ADR-8875): such rows can neither win nor tie, so the result (the lowest slot
    /// among equal scores included) equals a linear scan's, but `dist_fn` is not called
    /// for them. [`SpatialStats::nearest_rings`] counts the rings walked.
    pub fn nearest(
        &mut self,
        point: BroadBody,
        mask: u64,
        max_r: f32,
        mut dist_fn: impl FnMut(usize) -> f32,
    ) -> Option<usize> {
        self.stats.queries += 1;
        if max_r.is_nan() || max_r < 0.0 || self.rows.is_empty() {
            return None;
        }
        if let Some(d) = self.debug.as_deref_mut() {
            d.begin(point.pos, max_r);
        }
        let bounds = self.bounds(&point, max_r);
        let cx = self.geometry.coord(point.pos[0], 0);
        let cz = self.geometry.coord(point.pos[2], 1);
        let rings = (cx - bounds[0])
            .max(bounds[1] - cx)
            .max(cz - bounds[2])
            .max(bounds[3] - cz);
        let mut best = None;
        let mut best_d = max_r * max_r;
        // Ring early-out (ADR-8875): every row in ring k or beyond lies farther than `RingCut`'s
        // floor for ring k, so once that floor squared is above the incumbent, no later row can
        // win or tie. Wild and large rows are not in the rings and are always tested below.
        let cut = self.ring_cut(&point);
        for ring in 0..=rings {
            if let Some(cut) = cut
                && cut.excludes(ring, best_d)
            {
                break;
            }
            self.stats.nearest_rings += 1;
            ring_cells(bounds, [cx, cz], ring, |cell| {
                self.nearest_cell(cell, &point, mask, &mut dist_fn, &mut best, &mut best_d);
            });
        }
        for list in [self.config.max_cells, self.large_list()] {
            self.nearest_list(list, &point, mask, &mut dist_fn, &mut best, &mut best_d);
        }
        if let Some(slot) = best {
            self.stats.hits += 1;
            if let Some(d) = self.debug.as_deref_mut() {
                d.hit(slot);
            }
        }
        best
    }

    /// Read-only probe: true when `accept` returns true for any conservative candidate of the same
    /// envelope as [`Self::within`]. Candidates come cell by cell (not in slot order), each once,
    /// and the walk stops at the first acceptance. No counters move and no buffer is touched, so
    /// shared (`&self`) callers can ask from several places; use it only for order-free questions
    /// whose `accept` has no order-dependent effects (fog of war's "does any watcher see this").
    pub fn any_within(
        &self,
        center: BroadBody,
        r: f32,
        mask: u64,
        mut accept: impl FnMut(usize) -> bool,
    ) -> bool {
        if self.rows.is_empty() || self.heads.is_empty() {
            return false;
        }
        let [x0, x1, z0, z1] = self.bounds(&center, r);
        for z in z0..=z1 {
            for x in x0..=x1 {
                if self.any_in_cell(
                    (z * self.geometry.dims[0] + x) as usize,
                    &center,
                    mask,
                    &mut accept,
                ) {
                    return true;
                }
            }
        }
        self.any_in_cell(self.config.max_cells, &center, mask, &mut accept)
            || self.any_in_cell(self.large_list(), &center, mask, &mut accept)
    }

    fn any_in_cell(
        &self,
        cell: usize,
        center: &BroadBody,
        mask: u64,
        accept: &mut impl FnMut(usize) -> bool,
    ) -> bool {
        let Some(&head) = self.heads.get(cell) else {
            return false;
        };
        let mut slot = head;
        while slot != NONE {
            if self.matches(slot, center, mask) && accept(slot) {
                return true;
            }
            slot = self.links[slot].next;
        }
        false
    }

    /// Turn debug capture on with these caps, or off with `None` (ADR-8562). Off by default.
    ///
    /// Turning it on allocates the query ring (`caps.queries` records) now; queries after it
    /// record their center, radius and hits into the ring without allocating. Capture only reads:
    /// every answer, visit order and counter is unchanged.
    pub fn set_debug_capture(&mut self, capture: Option<DebugCapture>) {
        match capture {
            None => self.debug = None,
            Some(caps) => {
                if self.debug.as_ref().is_none_or(|d| d.caps != caps) {
                    self.debug = Some(Box::new(SpatialCapture::new(caps)));
                }
            }
        }
    }

    /// The capture caps, `None` while capture is off.
    pub fn debug_capture(&self) -> Option<DebugCapture> {
        self.debug.as_ref().map(|d| d.caps)
    }

    /// What the capture holds (all zero while capture is off). `cells` counts the cells in use now.
    pub fn debug_capture_stats(&self) -> DebugCaptureStats {
        let Some(d) = self.debug.as_deref() else {
            return DebugCaptureStats::default();
        };
        let mut cells = 0;
        self.for_each_cell_in_use(|_, _| cells += 1);
        DebugCaptureStats {
            cells,
            queries: d.filled,
            ..DebugCaptureStats::default()
        }
    }

    /// Draw the index into `sink` (ADR-8562): with [`DebugWhat::GRID`] the grid bounds and the
    /// outline of each cell in use now, with [`DebugWhat::OCCUPANCY`] each such cell's row count,
    /// and with [`DebugWhat::QUERIES`] the last queries of the ring, oldest first: a circle of the
    /// query radius around its center and a line from the center to each hit (the delivered
    /// candidates of a range query, the exact hits of `first_k_within_by`, the result of
    /// `nearest`; at most 16 per query). Draws nothing while capture is off. Reads state only.
    pub fn debug_emit(&self, sink: &mut dyn DebugSink, what: DebugWhat) {
        let Some(d) = self.debug.as_deref() else {
            return;
        };
        if (what.contains(DebugWhat::GRID) || what.contains(DebugWhat::OCCUPANCY)) && !self.rows.is_empty() {
            let mut y = f32::INFINITY;
            for row in self.rows.iter().filter(|row| tame(&row.body)) {
                y = det_min(y, row.body.pos[1]);
            }
            let g = &self.geometry;
            if let Some(grid) = GridShape::new(g.origin, g.inv_cell, g.dims, y) {
                if what.contains(DebugWhat::GRID) {
                    grid.emit_bounds(sink);
                }
                let mut left = d.caps.cells;
                self.for_each_cell_in_use(|cell, count| {
                    if left > 0 {
                        left -= 1;
                        grid.emit_cell(sink, what, cell as u64, count);
                    }
                });
            }
        }
        if what.contains(DebugWhat::QUERIES) {
            for record in d.records() {
                if record.r.is_finite() && record.r >= 0.0 {
                    sink.circle(DebugKind::Query, record.center, record.r);
                }
                for &slot in &record.hits[..usize::from(record.kept)] {
                    if let Some(row) = self.rows.get(slot as usize) {
                        sink.line(DebugKind::QueryHit, record.center, row.body.pos);
                    }
                }
            }
        }
    }

    /// Each grid cell holding at least one row now: `(cell index, rows)`, ascending. The wild list
    /// is not a cell and is skipped.
    fn for_each_cell_in_use(&self, mut visit: impl FnMut(usize, u64)) {
        let total = (self.geometry.dims[0] * self.geometry.dims[1]) as usize;
        for (cell, &head) in self.heads.iter().take(total).enumerate() {
            let mut count = 0_u64;
            let mut slot = head;
            while slot != NONE {
                count += 1;
                slot = self.links[slot].next;
            }
            if count > 0 {
                visit(cell, count);
            }
        }
    }

    /// [`Self::within`]'s visit (same candidates, same ascending slot order, same counters and
    /// debug records) with a gather that stops early (ADR-8875): the lists in the query's cell
    /// rectangle (plus the wild and large lists) are read in ascending order of their lowest slot.
    /// After reading list `i`, every candidate below the next list's lowest slot is already
    /// marked, so those slots are visited before the next list is read. A `Break` from `visit`
    /// (the k-th hit) leaves the remaining lists unread.
    fn first_k_visit(
        &mut self,
        center: &BroadBody,
        r: f32,
        mask: u64,
        mut visit: impl FnMut(usize) -> ControlFlow<()>,
    ) {
        self.stats.queries += 1;
        if let Some(d) = self.debug.as_deref_mut() {
            d.begin(center.pos, r);
        }
        if self.rows.is_empty() {
            return;
        }
        self.candidate_bits.fill(0);
        let mut order = std::mem::take(&mut self.order);
        order.clear();
        let [x0, x1, z0, z1] = self.bounds(center, r);
        for z in z0..=z1 {
            for x in x0..=x1 {
                let cell = (z * self.geometry.dims[0] + x) as usize;
                if self.list_min[cell] != NONE {
                    order.push(cell);
                }
            }
        }
        for list in [self.config.max_cells, self.large_list()] {
            if self.list_min[list] != NONE {
                order.push(list);
            }
        }
        // A slot sits in one list, so the lowest slots are distinct: the order is total.
        order.sort_unstable_by_key(|&list| self.list_min[list]);
        let mut from = 0;
        for (i, &list) in order.iter().enumerate() {
            self.gather_cell(list);
            self.stats.first_k_lists += 1;
            let until = order
                .get(i + 1)
                .map_or(self.rows.len(), |&next| self.list_min[next]);
            if self
                .visit_marked(from, until, center, mask, &mut visit)
                .is_break()
            {
                break;
            }
            from = until;
        }
        self.order = order;
    }
    /// Visit the marked candidates in `[from, until)` in ascending slot order, as `within` does.
    fn visit_marked(
        &mut self,
        from: usize,
        until: usize,
        probe: &BroadBody,
        mask: u64,
        visit: &mut impl FnMut(usize) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        let mut at = from;
        while at < until {
            let word = at / 64;
            let base = word * 64;
            let mut bits = self.candidate_bits[word] >> (at - base) << (at - base);
            if until - base < 64 {
                bits &= (1_u64 << (until - base)) - 1;
            }
            while bits != 0 {
                let slot = base + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if self.matches(slot, probe, mask) {
                    self.stats.candidates += 1;
                    if let Some(d) = self.debug.as_deref_mut() {
                        d.hit(slot);
                    }
                    visit(slot)?;
                }
            }
            at = base + 64;
        }
        ControlFlow::Continue(())
    }
    /// The ring early-out's inputs for this point, or `None` (no early-out) for a point that is
    /// not tame or a grid without finite cells (ADR-8875).
    fn ring_cut(&self, point: &BroadBody) -> Option<RingCut> {
        let inv = self.geometry.inv_cell;
        if !tame(point) || !inv.is_finite() || inv <= 0.0 || !self.max_radius.is_finite() {
            return None;
        }
        let cell = 1.0 / inv;
        let g = &self.geometry;
        let big = det_max(
            det_max(abs(point.pos[0]), abs(point.pos[2])),
            det_max(abs(g.origin[0]), abs(g.origin[1])),
        );
        cell.is_finite().then_some(RingCut {
            cell,
            max_radius: self.max_radius,
            big,
        })
    }
    fn nearest_cell(
        &mut self,
        [x, z]: [i64; 2],
        point: &BroadBody,
        mask: u64,
        dist_fn: &mut impl FnMut(usize) -> f32,
        best: &mut Option<usize>,
        best_d: &mut f32,
    ) {
        let cell = (z * self.geometry.dims[0] + x) as usize;
        self.nearest_list(cell, point, mask, dist_fn, best, best_d);
    }
    fn nearest_list(
        &mut self,
        cell: usize,
        point: &BroadBody,
        mask: u64,
        dist_fn: &mut impl FnMut(usize) -> f32,
        best: &mut Option<usize>,
        best_d: &mut f32,
    ) {
        let mut slot = self.heads[cell];
        while slot != NONE {
            if self.matches(slot, point, mask) {
                self.stats.candidates += 1;
                let d = dist_fn(slot);
                if d <= *best_d && (best.is_none() || d < *best_d || best.is_some_and(|old| slot < old)) {
                    *best = Some(slot);
                    *best_d = d;
                }
            }
            slot = self.links[slot].next;
        }
    }
    fn matches(&self, slot: usize, center: &BroadBody, mask: u64) -> bool {
        let row = &self.rows[slot];
        row.group_mask & mask != 0 && !bodies_on_different_floors(self.config.floor_rule, center, &row.body)
    }
    /// The list after the wild list (`max_cells`): rows whose `|radius|` is above the large
    /// threshold (ADR-8837).
    fn large_list(&self) -> usize {
        self.config.max_cells + 1
    }
    /// A row whose finite `|radius|` is above the large threshold.
    fn is_large(&self, body: &BroadBody) -> bool {
        body.radius.is_finite() && abs(body.radius) > self.config.large_threshold()
    }
    fn destination(&self, body: &BroadBody) -> usize {
        if !tame(body) {
            self.config.max_cells
        } else if self.is_large(body) {
            self.large_list()
        } else {
            self.geometry.index(body.pos)
        }
    }
    fn file(&mut self, slot: usize, cell: usize) {
        let next = self.heads[cell];
        if next != NONE {
            self.links[next].prev = slot;
        }
        self.heads[cell] = slot;
        self.links[slot] = Link {
            prev: NONE,
            next,
            cell,
        };
        self.list_min[cell] = self.list_min[cell].min(slot);
        if cell == self.config.max_cells {
            self.stats.wild_rows += 1;
        } else if cell == self.large_list() {
            self.stats.large_rows += 1;
        }
    }
    fn unfile(&mut self, slot: usize) {
        let Link { prev, next, cell } = self.links[slot];
        if prev == NONE {
            self.heads[cell] = next;
        } else {
            self.links[prev].next = next;
        }
        if next != NONE {
            self.links[next].prev = prev;
        }
        if self.list_min[cell] == slot {
            let mut low = NONE;
            let mut at = self.heads[cell];
            while at != NONE {
                low = low.min(at);
                at = self.links[at].next;
            }
            self.list_min[cell] = low;
        }
        if cell == self.config.max_cells {
            self.stats.wild_rows -= 1;
        } else if cell == self.large_list() {
            self.stats.large_rows -= 1;
        }
    }
    fn bounds(&self, center: &BroadBody, r: f32) -> [i64; 4] {
        let reach = abs(r) + self.max_radius;
        if !tame(center) || !reach.is_finite() || !(reach * reach).is_finite() {
            return [0, self.geometry.dims[0] - 1, 0, self.geometry.dims[1] - 1];
        }
        let x = center.pos[0];
        let z = center.pos[2];
        let px = query_pad(reach, x);
        let pz = query_pad(reach, z);
        [
            self.geometry.coord(x - reach - px, 0),
            self.geometry.coord(x + reach + px, 0),
            self.geometry.coord(z - reach - pz, 1),
            self.geometry.coord(z + reach + pz, 1),
        ]
    }
    fn gather(&mut self, center: &BroadBody, r: f32) {
        self.candidates.clear();
        if self.rows.is_empty() {
            return;
        }
        self.candidate_bits.fill(0);
        let [x0, x1, z0, z1] = self.bounds(center, r);
        for z in z0..=z1 {
            for x in x0..=x1 {
                self.gather_cell((z * self.geometry.dims[0] + x) as usize);
            }
        }
        self.finish_gather();
    }
    /// Clear the candidate set; false when there are no rows (nothing to gather).
    fn begin_gather(&mut self) -> bool {
        self.candidates.clear();
        if self.rows.is_empty() {
            return false;
        }
        self.candidate_bits.fill(0);
        true
    }
    /// Gather the wild and large lists and emit the marked slots in ascending order.
    fn finish_gather(&mut self) {
        self.gather_cell(self.config.max_cells);
        self.gather_cell(self.large_list());
        // Enumerating word bits produces ascending slots without sorting each query.
        for (word_index, &bits) in self.candidate_bits.iter().enumerate() {
            let mut remaining = bits;
            while remaining != 0 {
                self.candidates
                    .push(word_index * 64 + remaining.trailing_zeros() as usize);
                remaining &= remaining - 1;
            }
        }
    }
    fn visit_candidates(
        &mut self,
        probe: &BroadBody,
        mask: u64,
        visit: &mut impl FnMut(usize) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        for &slot in &self.candidates {
            if self.matches(slot, probe, mask) {
                self.stats.candidates += 1;
                visit(slot)?;
            }
        }
        ControlFlow::Continue(())
    }
    fn all_cells(&self) -> [i64; 4] {
        [0, self.geometry.dims[0] - 1, 0, self.geometry.dims[1] - 1]
    }
    /// Gather an inclusive cell rectangle (shape queries; counts walked cells).
    fn gather_rect(&mut self, [x0, x1, z0, z1]: [i64; 4]) {
        for z in z0..=z1 {
            for x in x0..=x1 {
                self.gather_cell((z * self.geometry.dims[0] + x) as usize);
            }
        }
        if x0 <= x1 && z0 <= z1 {
            self.stats.cells_walked += ((x1 - x0 + 1) * (z1 - z0 + 1)) as u64;
        }
    }
    /// Cell rectangle of the world box `[lo - pad, hi + pad]`. Monotone `coord` keeps it
    /// conservative; infinities clamp to the grid edge. A NaN scans every cell.
    fn padded_rect(&self, lo: [f32; 2], hi: [f32; 2], pad: f32) -> [i64; 4] {
        if lo.iter().chain(&hi).any(|v| v.is_nan()) || pad.is_nan() {
            return self.all_cells();
        }
        let g = &self.geometry;
        [
            g.coord(lo[0] - pad, 0),
            g.coord(hi[0] + pad, 0),
            g.coord(lo[1] - pad, 1),
            g.coord(hi[1] + pad, 1),
        ]
    }
    fn box_bounds(&self, min: [f32; 2], max: [f32; 2]) -> [i64; 4] {
        if min.iter().chain(&max).any(|v| v.is_nan()) {
            return self.all_cells();
        }
        let lo = [det_min(min[0], max[0]), det_min(min[1], max[1])];
        let hi = [det_max(min[0], max[0]), det_max(min[1], max[1])];
        let big = det_max(det_max(abs(lo[0]), abs(lo[1])), det_max(abs(hi[0]), abs(hi[1])));
        let pad = self.max_radius + query_pad(self.max_radius, big);
        self.padded_rect(lo, hi, pad)
    }
    fn sector_bounds(&self, apex: &BroadBody, dir: [f32; 2], cos_half: f32, r: f32) -> [i64; 4] {
        let reach = abs(r) + self.max_radius;
        let len2 = dir[0] * dir[0] + dir[1] * dir[1];
        let sharp = tame(apex)
            && reach.is_finite()
            && (reach * reach).is_finite()
            && len2.is_finite()
            && len2 >= f32::MIN_POSITIVE
            && !cos_half.is_nan();
        if !sharp {
            return self.bounds(apex, r);
        }
        let len = crate::math::sqrt(len2);
        let u = [dir[0] / len, dir[1] / len];
        let c = det_clamp(cos_half, -1.0, 1.0);
        let s = crate::math::sqrt(det_max(1.0 - c * c, 0.0));
        let ra = abs(r);
        // The two edge rays (rotate u by -half and +half) and the apex bound the box;
        // an axis extreme joins it when that axis direction lies inside the cone.
        let edges = [
            [ra * (u[0] * c - u[1] * s), ra * (u[1] * c + u[0] * s)],
            [ra * (u[0] * c + u[1] * s), ra * (u[1] * c - u[0] * s)],
        ];
        let mut lo = [0.0_f32; 2];
        let mut hi = [0.0_f32; 2];
        for axis in [0_usize, 1] {
            for e in &edges {
                lo[axis] = det_min(lo[axis], e[axis]);
                hi[axis] = det_max(hi[axis], e[axis]);
            }
            if u[axis] >= c {
                hi[axis] = ra;
            }
            if -u[axis] >= c {
                lo[axis] = -ra;
            }
        }
        let p = [apex.pos[0], apex.pos[2]];
        let big = det_max(abs(p[0]), abs(p[1]));
        let pad = self.max_radius + query_pad(reach, big);
        self.padded_rect([p[0] + lo[0], p[1] + lo[1]], [p[0] + hi[0], p[1] + hi[1]], pad)
    }
    /// Cell-space segment and window half-width, or `None` to scan every cell.
    fn segment_walk(&self, from: &BroadBody, to: [f32; 2], r: f32) -> Option<SegmentWalk> {
        if !tame(from) || !to.iter().all(|v| v.is_finite()) {
            return None;
        }
        let a = [from.pos[0], from.pos[2]];
        let reach = abs(r) + self.max_radius;
        // |dx| + |dz| bounds the length without a square root.
        let span = abs(to[0] - a[0]) + abs(to[1] - a[1]);
        let big = det_max(det_max(abs(a[0]), abs(a[1])), det_max(abs(to[0]), abs(to[1])));
        let padded = reach + query_pad(reach + span, big);
        if !padded.is_finite() || !(padded * padded).is_finite() {
            return None;
        }
        let g = &self.geometry;
        let inv = f64::from(g.inv_cell);
        let cell = |v: f32, axis: usize| (f64::from(v) - f64::from(g.origin[axis])) * inv;
        let ua = [cell(a[0], 0), cell(a[1], 1)];
        let ub = [cell(to[0], 0), cell(to[1], 1)];
        let near = |u: f64| (-WALK_LIMIT..=WALK_LIMIT).contains(&u);
        let max_dim = g.dims[0].max(g.dims[1]);
        // Window reach in cells: the padded capsule, plus 1/1024 cell for the walk's
        // rounding and the f32 rounding of `coord` (relative 2^-23 within the grid).
        let reach_cells = f64::from(padded) * inv + 1.0 / 1024.0 + max_dim as f64 / 2_097_152.0;
        if !(ua.iter().chain(&ub).all(|&u| near(u)) && reach_cells < max_dim as f64) {
            return None;
        }
        Some(SegmentWalk {
            a: ua,
            b: ub,
            // A point within d cells (per axis) of a cell's closed box lies at most
            // floor(d) + 1 cells from it.
            k: reach_cells as i64 + 1,
        })
    }
    /// Gather every cell within `k` cells of the walk over the segment clamped to the
    /// grid. Rows outside the build bounds live in the clamped edge cells, and clamping
    /// is 1-Lipschitz, so the clamped walk keeps every candidate. The clamped segment is
    /// straight between the points where it crosses the grid's outer lines.
    fn walk_segment(&mut self, w: SegmentWalk) {
        let dims = self.geometry.dims;
        let size = [dims[0] as f64, dims[1] as f64];
        let mut ts = [0.0_f64, 1.0, 1.0, 1.0, 1.0, 1.0];
        let mut n = 2;
        for axis in [0_usize, 1] {
            let du = w.b[axis] - w.a[axis];
            if du != 0.0 {
                for edge in [0.0, size[axis]] {
                    let t = (edge - w.a[axis]) / du;
                    if t > 0.0 && t < 1.0 {
                        ts[n] = t;
                        n += 1;
                    }
                }
            }
        }
        // Insertion sort of at most six finite parameters.
        for i in 1..n {
            let mut j = i;
            while j > 0 && ts[j - 1] > ts[j] {
                ts.swap(j - 1, j);
                j -= 1;
            }
        }
        let at = |t: f64| {
            let p = if t >= 1.0 {
                w.b
            } else {
                [w.a[0] + t * (w.b[0] - w.a[0]), w.a[1] + t * (w.b[1] - w.a[1])]
            };
            [
                det_clamp_f64(p[0], 0.0, size[0]),
                det_clamp_f64(p[1], 0.0, size[1]),
            ]
        };
        let mut p = at(ts[0]);
        let mut cur = [cell_of(p[0], dims[0]), cell_of(p[1], dims[1])];
        let k = w.k;
        self.gather_rect([
            (cur[0] - k).max(0),
            (cur[0] + k).min(dims[0] - 1),
            (cur[1] - k).max(0),
            (cur[1] + k).min(dims[1] - 1),
        ]);
        for &t in &ts[1..n] {
            let q = at(t);
            self.walk_piece(p, q, &mut cur, k);
            p = q;
        }
    }
    /// Amanatides-Woo cell walk from `p` to `q` (cell units, inside the grid box),
    /// starting in `cur`, gathering the leading strip of the window at each step.
    /// Steps are forced toward the end cell, so the walk always terminates there.
    fn walk_piece(&mut self, p: [f64; 2], q: [f64; 2], cur: &mut [i64; 2], k: i64) {
        let dims = self.geometry.dims;
        let end = [cell_of(q[0], dims[0]), cell_of(q[1], dims[1])];
        let d = [q[0] - p[0], q[1] - p[1]];
        let step = |axis: usize, cur: &[i64; 2]| if end[axis] > cur[axis] { 1 } else { -1 };
        // Parameter at which the walk leaves the current cell on `axis`. Recomputed from
        // the start point each step, so no error accumulates along long walks.
        let leave = |axis: usize, cur: &[i64; 2]| {
            let edge = if step(axis, cur) > 0 {
                cur[axis] + 1
            } else {
                cur[axis]
            };
            (edge as f64 - p[axis]) / d[axis]
        };
        while *cur != end {
            let axis = if cur[0] == end[0] {
                1
            } else if cur[1] == end[1] || leave(0, cur) <= leave(1, cur) {
                0
            } else {
                1
            };
            let s = step(axis, cur);
            cur[axis] += s;
            let lead = cur[axis] + s * k;
            if lead < 0 || lead >= dims[axis] {
                continue;
            }
            let other = 1 - axis;
            let lo = (cur[other] - k).max(0);
            let hi = (cur[other] + k).min(dims[other] - 1);
            for o in lo..=hi {
                let (x, z) = if axis == 0 { (lead, o) } else { (o, lead) };
                self.gather_cell((z * dims[0] + x) as usize);
            }
            self.stats.cells_walked += (hi - lo + 1) as u64;
        }
    }
    fn gather_cell(&mut self, cell: usize) {
        let mut slot = self.heads[cell];
        while slot != NONE {
            self.candidate_bits[slot / 64] |= 1 << (slot % 64);
            slot = self.links[slot].next;
        }
    }
}
/// Cell-space segment for [`DetSpatial::along_segment`]'s walk.
#[derive(Debug, Clone, Copy)]
struct SegmentWalk {
    a: [f64; 2],
    b: [f64; 2],
    k: i64,
}
/// Inputs of [`DetSpatial::nearest`]'s ring early-out (ADR-8875).
///
/// A row filed in a cell `k` rings from the point's cell lies more than `k - 1` cells away on
/// one axis: cell indices are floors, and the clamp into the grid's edge cells is 1-Lipschitz,
/// so a clamped row or point only looks closer than it is. The f32 rounding of the cell index
/// (`(v - origin) * inv_cell`, about 3 ulp relative) and of `1 / inv_cell` stay far inside the
/// ADR-8048 pad (`gap / 256 + big / 262144 + 0.001`), as does the caller's own rounding of the
/// squared distance. Rows in cells have `|radius| <= max_radius`, so a callback that measures to
/// the row's disc instead of its center is covered too.
#[derive(Debug, Clone, Copy)]
struct RingCut {
    /// World length of one cell (`1 / inv_cell`).
    cell: f32,
    /// Largest `|radius|` of a row that can sit in a cell.
    max_radius: f32,
    /// Largest coordinate magnitude of the point and the grid origin.
    big: f32,
}

impl RingCut {
    /// True when no row in ring `ring` or beyond can score at or below `best_d`.
    fn excludes(self, ring: i64, best_d: f32) -> bool {
        if ring < 2 {
            return false;
        }
        let gap = (ring - 1) as f32 * self.cell;
        let floor = gap - self.max_radius - query_pad(gap, self.big);
        // NaN or infinite inputs make one of these comparisons false: no early-out.
        floor > 0.0 && floor * floor > best_d
    }
}

/// Cell index of a clamped cell-space coordinate in `[0, dim]`.
fn cell_of(u: f64, dim: i64) -> i64 {
    (u as i64).clamp(0, dim - 1)
}
impl DebugSource for DetSpatial {
    fn set_debug_capture(&mut self, capture: Option<DebugCapture>) {
        DetSpatial::set_debug_capture(self, capture);
    }
    fn debug_emit(&self, sink: &mut dyn DebugSink, what: DebugWhat) {
        DetSpatial::debug_emit(self, sink, what);
    }
}

fn tame(b: &BroadBody) -> bool {
    b.pos.iter().all(|v| v.is_finite()) && b.radius.is_finite() && b.band.iter().all(|v| v.is_finite())
}
fn abs(v: f32) -> f32 {
    if v < 0.0 { -v } else { v }
}

// Row-major ring perimeter, clipped before walking. When both side columns are
// outside the bounds, skip the interior rows outright: a 1 × N grid is O(N),
// not O(N²) empty row checks. This preserves the original cell visit sequence.
fn ring_cells(bounds: [i64; 4], [cx, cz]: [i64; 2], ring: i64, mut visit: impl FnMut([i64; 2])) {
    let [bx0, bx1, bz0, bz1] = bounds;
    let left = cx - ring;
    let right = cx + ring;
    let top = cz - ring;
    let bottom = cz + ring;
    let x0 = left.max(bx0);
    let x1 = right.min(bx1);
    if top >= bz0 {
        for x in x0..=x1 {
            visit([x, top]);
        }
    }
    if ring == 0 {
        return;
    }
    if left >= bx0 || right <= bx1 {
        for z in (top + 1).max(bz0)..=(bottom - 1).min(bz1) {
            if left >= bx0 {
                visit([left, z]);
            }
            if right <= bx1 {
                visit([right, z]);
            }
        }
    }
    if bottom <= bz1 {
        for x in x0..=x1 {
            visit([x, bottom]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ring_cells;

    #[test]
    fn clipped_rings_visit_each_cell_once_in_the_original_order() {
        for dims in [[1_i64, 1_i64], [1, 200], [200, 1], [3, 17], [15, 11]] {
            for center in [[0, 0], [dims[0] / 2, dims[1] / 2], [dims[0] - 1, dims[1] - 1]] {
                let bounds = [0, dims[0] - 1, 0, dims[1] - 1];
                let mut actual = Vec::new();
                let mut expected = Vec::new();
                let rings = dims[0].max(dims[1]);
                for ring in 0..rings {
                    ring_cells(bounds, center, ring, |cell| actual.push(cell));
                    for z in 0..dims[1] {
                        for x in 0..dims[0] {
                            if (x - center[0]).abs().max((z - center[1]).abs()) == ring {
                                expected.push([x, z]);
                            }
                        }
                    }
                }
                assert_eq!(actual, expected);
                assert_eq!(actual.len(), (dims[0] * dims[1]) as usize);
            }
        }
    }
}
