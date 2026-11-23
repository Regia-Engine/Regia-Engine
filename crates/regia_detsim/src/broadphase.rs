// SPDX-License-Identifier: MPL-2.0
// Height bands: portions copied from Direct Order 27 (ir-sim/src/state.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (lines 1068-1090, `body_band` and `bodies_on_different_floors`; ADR-8048). The band is
// now data on the body (the host picks its extents; DO27's per-class metres are game values and are
// not copied), and the "different floors" test takes a `FloorRule`: `UndergroundOnly` is DO27's rule
// bit for bit (bodies that are both outdoors always collide on XZ), `Bands` is Regia's default and
// also separates two outdoor bodies on stacked floors (a bridge deck over a road), the issue DO27
// lists as known. The pair loop (state.rs lines 13734-14258) is requirements only: its order (every
// `(i, j)`, `i < j`, in collider order, each pair seeing the pushes of the pairs before it) and its
// height skip are what this module reproduces; none of its code is copied.

//! A deterministic grid broadphase for disc bodies on the XZ plane, with height bands.
//!
//! The reference is the plain all-pairs loop a lockstep sim starts with:
//!
//! ```text
//! for a in 0..n { for b in a + 1..n { if overlap(a, b) { resolve(a, b) } } }
//! ```
//!
//! where `resolve` may push `a` and `b` apart, so a later pair sees the earlier pushes. Its results
//! depend on the visit order. [`DetBroadphase::for_each_contact`] visits exactly the same contacts,
//! in exactly that `(a, b)` order, with the same bits, but only tests nearby pairs: a uniform XZ grid
//! that is kept current as the visitor moves bodies. [`for_each_contact_n2`] is the reference loop
//! itself, and both use one contact test ([`contact`]), so a host can switch between them without a
//! checksum change.
//!
//! Each body also fills a vertical band; a [`FloorRule`] decides when two bodies whose discs overlap
//! are on different floors and do not touch.
//!
//! ```
//! use regia_detsim::broadphase::{BroadBody, BroadphaseConfig, DetBroadphase};
//!
//! let mut bodies = vec![
//!     BroadBody::new([0.0, 0.0, 0.0], 1.0, [-1.0, 2.0]),
//!     BroadBody::new([1.5, 0.0, 0.0], 1.0, [-1.0, 2.0]),
//!     BroadBody::new([9.0, 0.0, 0.0], 1.0, [-1.0, 2.0]),
//! ];
//! let mut grid = DetBroadphase::new(BroadphaseConfig::default());
//! let mut seen = Vec::new();
//! grid.for_each_contact(&mut bodies, |bodies, c| {
//!     seen.push((c.a, c.b));
//!     let push = (c.min_dist - regia_detsim::math::sqrt(c.dist_sq)) * 0.5;
//!     bodies[c.a].pos[0] -= push;
//!     bodies[c.b].pos[0] += push;
//! });
//! assert_eq!(seen, vec![(0, 1)]);
//! assert_eq!(bodies[1].pos[0] - bodies[0].pos[0], 2.0);
//! ```

use crate::debug::{
    DebugCapture, DebugCaptureStats, DebugKind, DebugSink, DebugSource, DebugWhat, GridShape,
};

/// A body as the broadphase sees it: a disc on XZ with a vertical band.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BroadBody {
    /// Position `[x, y, z]`. A visitor may change `x` and `z` of the two bodies of the contact it is
    /// given; see [`DetBroadphase::for_each_contact`].
    pub pos: [f32; 3],
    /// Disc radius on XZ (metres). Two discs touch when their centres are closer than the sum.
    pub radius: f32,
    /// The height `[lo, hi]` the body fills, absolute (see [`body_band`]).
    pub band: [f32; 2],
    /// The body stands in a covered space (a tunnel, a cave); used by [`FloorRule::UndergroundOnly`].
    pub underground: bool,
}

impl BroadBody {
    /// An outdoor body.
    pub fn new(pos: [f32; 3], radius: f32, band: [f32; 2]) -> Self {
        Self {
            pos,
            radius,
            band,
            underground: false,
        }
    }
}

/// How far below and above its origin a body's band reaches (metres). The host picks the values
/// per kind of body (a vehicle's hover gap, a tower's height).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BandExtent {
    /// Metres below the origin.
    pub below: f32,
    /// Metres above the origin.
    pub above: f32,
}

/// The band `[y - below, y + above]` of a body whose origin is at height `y`.
pub fn body_band(y: f32, extent: BandExtent) -> [f32; 2] {
    [y - extent.below, y + extent.above]
}

/// When two bodies whose XZ discs overlap are on different floors (and so do not touch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FloorRule {
    /// Their bands do not overlap. Separates a bridge deck from the road under it, a tunnel from the
    /// ground over it, and a body high in the air from one on the ground. The default.
    #[default]
    Bands,
    /// Either body is underground and their bands do not overlap. Two outdoor bodies always touch
    /// on XZ, whatever their heights. This is the source game's rule, kept bit for bit for a host
    /// that must keep its checksums.
    UndergroundOnly,
    /// Never: discs are infinitely tall (height-blind XZ collision).
    Never,
}

/// The source game's height-band test with a [`FloorRule`]: true when the two bodies are on
/// different floors.
pub fn bodies_on_different_floors(rule: FloorRule, a: &BroadBody, b: &BroadBody) -> bool {
    let apart = a.band[1] < b.band[0] || b.band[1] < a.band[0];
    match rule {
        FloorRule::Bands => apart,
        FloorRule::UndergroundOnly => (a.underground || b.underground) && apart,
        FloorRule::Never => false,
    }
}

/// A pair of bodies whose discs overlap, as handed to the visitor. The numbers are computed exactly
/// as the reference loop does: `dx = b.x - a.x`, `dz = b.z - a.z`, `dist_sq = dx * dx + dz * dz`,
/// `min_dist = a.radius + b.radius`, and `dist_sq < min_dist * min_dist`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Contact {
    /// Index of the first body (`a < b`).
    pub a: usize,
    /// Index of the second body.
    pub b: usize,
    /// `b.x - a.x`.
    pub dx: f32,
    /// `b.z - a.z`.
    pub dz: f32,
    /// `dx * dx + dz * dz`.
    pub dist_sq: f32,
    /// `a.radius + b.radius`.
    pub min_dist: f32,
}

/// The one contact test both loops use: `Some` when bodies `a` and `b` are on the same floor (per
/// `rule`) and their discs overlap. A NaN distance counts as overlapping, as in the reference loop
/// (`if dist_sq >= min * min { skip }` does not skip a NaN).
pub fn contact(bodies: &[BroadBody], a: usize, b: usize, rule: FloorRule) -> Option<Contact> {
    let (ba, bb) = (&bodies[a], &bodies[b]);
    if bodies_on_different_floors(rule, ba, bb) {
        return None;
    }
    let dx = bb.pos[0] - ba.pos[0];
    let dz = bb.pos[2] - ba.pos[2];
    let dist_sq = dx * dx + dz * dz;
    let min_dist = ba.radius + bb.radius;
    if dist_sq >= min_dist * min_dist {
        return None;
    }
    Some(Contact {
        a,
        b,
        dx,
        dz,
        dist_sq,
        min_dist,
    })
}

/// The reference all-pairs loop: every `(a, b)` with `a < b`, in that order, through [`contact`].
/// O(n²); use it as the oracle and for very small counts.
pub fn for_each_contact_n2<F>(bodies: &mut [BroadBody], rule: FloorRule, mut visit: F)
where
    F: FnMut(&mut [BroadBody], Contact),
{
    let n = bodies.len();
    for a in 0..n {
        for b in (a + 1)..n {
            if let Some(c) = contact(bodies, a, b, rule) {
                visit(bodies, c);
            }
        }
    }
}

/// Settings for [`DetBroadphase`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BroadphaseConfig {
    /// Grid cell edge on XZ (metres). About the diameter of a typical body is a good start; any
    /// positive finite value gives the same contacts (only the speed changes). Raised automatically
    /// when the bodies spread so wide that the grid would exceed `max_cells`.
    pub cell_size: f32,
    /// How far body `a` may be pushed during its own row before its neighbour list is gathered
    /// again (metres). Any non-negative value gives the same contacts.
    pub margin: f32,
    /// Most cells the grid may have (memory bound), clamped to `1..=2^24` by [`DetBroadphase::new`].
    pub max_cells: usize,
    /// The height rule.
    pub floor_rule: FloorRule,
    /// The play area `[min_x, min_z, max_x, max_z]` (metres), or `None` (the default) for no
    /// limit. With bounds, a finite body outside them is an *outlier*: it is kept off the grid and
    /// tested only against the rows whose padded reach can touch it, so one body parked far away
    /// (a pooled or dead entity) no longer stretches the grid and puts every other body in a few
    /// cells. The grid covers the bodies inside the bounds. Any bounds give the same contacts (only
    /// the speed changes); a NaN or an empty box (`min > max`) is treated as `None` by
    /// [`DetBroadphase::new`] (ADR-8837).
    pub world_bounds: Option<[f32; 4]>,
    /// Bodies whose `|radius|` is above this (metres) are *large*: kept off the grid and tested
    /// only against the rows whose reach can touch them, so one large body (a building, a hill)
    /// does not widen every row's gather. `None` (the default) means twice `cell_size`;
    /// `Some(f32::INFINITY)` turns the class off. Any value gives the same contacts (only the
    /// speed changes); NaN is treated as infinity and a negative value as zero (ADR-8837).
    pub large_radius: Option<f32>,
}

impl Default for BroadphaseConfig {
    fn default() -> Self {
        Self {
            cell_size: 4.0,
            margin: 0.5,
            max_cells: 1 << 16,
            floor_rule: FloorRule::Bands,
            world_bounds: None,
            large_radius: None,
        }
    }
}

impl BroadphaseConfig {
    /// The `|radius|` above which a body is large: `large_radius`, or twice `cell_size` when it is
    /// `None` (ADR-8837).
    pub fn large_threshold(&self) -> f32 {
        self.large_radius.unwrap_or(2.0 * self.cell_size)
    }

    /// True when a finite position `(x, z)` lies outside `world_bounds` (never without bounds).
    pub fn is_outside(&self, x: f32, z: f32) -> bool {
        match self.world_bounds {
            None => false,
            Some([x0, z0, x1, z1]) => !(x >= x0 && x <= x1 && z >= z0 && z <= z1),
        }
    }
}

/// Counters from the last [`DetBroadphase::for_each_contact`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BroadphaseStats {
    /// Bodies in the pass.
    pub bodies: usize,
    /// Grid cells (columns × rows).
    pub cells: usize,
    /// Candidate pairs put through [`contact`] (the reference loop tests `n(n-1)/2`).
    pub candidates: u64,
    /// Contacts handed to the visitor.
    pub contacts: u64,
    /// Bodies moved to another cell by the visitor.
    pub cell_moves: u64,
    /// Neighbour lists gathered again because the row's body moved past the margin.
    pub regathers: u64,
    /// Bodies with a non-finite x, z or radius at the start of the pass (tested against everyone,
    /// as the reference loop does).
    pub non_finite: usize,
    /// Finite bodies outside `world_bounds` at the start of the pass, kept off the grid (ADR-8837).
    pub outliers: usize,
    /// Finite bodies whose `|radius|` is above the large threshold, kept off the grid (ADR-8837).
    pub large: usize,
}

/// Where a body is filed in the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// Not filed (already past its row).
    Gone,
    /// In `cells[i]`.
    Cell(u32),
    /// In the wild list (a non-finite x, z or radius).
    Wild,
    /// In the outlier list (finite, outside `world_bounds`, not large).
    Outlier,
    /// In the large list (finite, `|radius|` above the large threshold).
    Large,
}

/// A deterministic uniform-grid broadphase that reproduces the all-pairs loop's contacts and order.
///
/// Keep one per sim and call [`Self::for_each_contact`] each tick; it reuses its buffers. It holds
/// no state between calls besides those buffers and the last [`BroadphaseStats`].
#[derive(Debug, Clone, Default)]
pub struct DetBroadphase {
    config: BroadphaseConfig,
    cells: Vec<Vec<u32>>,
    slot: Vec<Slot>,
    wild: Vec<u32>,
    outliers: Vec<u32>,
    large: Vec<u32>,
    candidates: Vec<u32>,
    origin: [f32; 2],
    inv_cell: f32,
    dims: [i64; 2],
    stats: BroadphaseStats,
    /// Debug capture (ADR-8562): `None` (the default) does no debug work and allocates nothing.
    debug: Option<Box<BroadCapture>>,
}

/// What the last pass looked like, kept only while debug capture is on (ADR-8562). Read-only
/// copies of positions: nothing here feeds back into a pass.
#[derive(Debug, Clone, Default)]
struct BroadCapture {
    caps: DebugCapture,
    /// Centers of each contact's two bodies when the contact was found (before the visitor).
    contacts: Vec<([f32; 3], [f32; 3])>,
    contacts_dropped: u64,
    /// Centers of each candidate pair when it was tested.
    candidates: Vec<([f32; 3], [f32; 3])>,
    candidates_dropped: u64,
    /// `(cell index, bodies filed)` for each cell in use at the start of the pass.
    cells: Vec<(u32, u32)>,
    cells_dropped: u64,
    grid: Option<GridShape>,
}

impl BroadCapture {
    fn new(caps: DebugCapture) -> Self {
        Self {
            caps,
            contacts: Vec::with_capacity(caps.contacts),
            candidates: Vec::with_capacity(caps.candidates),
            cells: Vec::with_capacity(caps.cells),
            ..Self::default()
        }
    }

    fn keep(list: &mut Vec<([f32; 3], [f32; 3])>, cap: usize, dropped: &mut u64, a: [f32; 3], b: [f32; 3]) {
        if list.len() < cap {
            list.push((a, b));
        } else {
            *dropped += 1;
        }
    }
}

impl DetBroadphase {
    /// A broadphase with these settings (a non-finite or non-positive cell size becomes 1 m, a
    /// negative or non-finite margin 0). `max_cells` is clamped to `1..=2^24`, keeping every cell
    /// index representable in the grid's `u32` slots and its signed dimension arithmetic. World
    /// bounds with a NaN or `min > max` become `None`; a NaN large radius becomes infinity and a
    /// negative one zero.
    pub fn new(config: BroadphaseConfig) -> Self {
        let mut config = config;
        if !(config.cell_size.is_finite() && config.cell_size > 0.0) {
            config.cell_size = 1.0;
        }
        if !(config.margin.is_finite() && config.margin >= 0.0) {
            config.margin = 0.0;
        }
        config.max_cells = config.max_cells.clamp(1, 1 << 24);
        if let Some([x0, z0, x1, z1]) = config.world_bounds
            && !(x0 <= x1 && z0 <= z1)
        {
            config.world_bounds = None;
        }
        config.large_radius = config
            .large_radius
            .map(|r| if r.is_nan() { f32::INFINITY } else { max_f(r, 0.0) });
        Self {
            config,
            ..Self::default()
        }
    }

    /// The settings.
    pub fn config(&self) -> BroadphaseConfig {
        self.config
    }

    /// Counters from the last pass.
    pub fn stats(&self) -> BroadphaseStats {
        self.stats
    }

    /// Turn debug capture on with these caps, or off with `None` (ADR-8562). Off by default.
    ///
    /// Turning it on reserves every capture buffer now; the passes after it record the cells in
    /// use, the candidate pairs and the contacts up to the caps without allocating. Capture only
    /// reads: the contacts, their order and every bit the visitor sees are unchanged.
    pub fn set_debug_capture(&mut self, capture: Option<DebugCapture>) {
        match capture {
            None => self.debug = None,
            Some(caps) => {
                if self.debug.as_ref().is_none_or(|d| d.caps != caps) {
                    self.debug = Some(Box::new(BroadCapture::new(caps)));
                }
            }
        }
    }

    /// The capture caps, `None` while capture is off.
    pub fn debug_capture(&self) -> Option<DebugCapture> {
        self.debug.as_ref().map(|d| d.caps)
    }

    /// What the capture holds from the last pass (all zero while capture is off).
    pub fn debug_capture_stats(&self) -> DebugCaptureStats {
        self.debug
            .as_ref()
            .map_or_else(DebugCaptureStats::default, |d| DebugCaptureStats {
                contacts: d.contacts.len(),
                contacts_dropped: d.contacts_dropped,
                candidates: d.candidates.len(),
                candidates_dropped: d.candidates_dropped,
                cells: d.cells.len(),
                cells_dropped: d.cells_dropped,
                queries: 0,
            })
    }

    /// Draw the last pass into `sink` (ADR-8562): with [`DebugWhat::GRID`] the grid bounds and the
    /// outline of each cell in use, with [`DebugWhat::OCCUPANCY`] each such cell's body count, with
    /// [`DebugWhat::CANDIDATES`] one line per candidate pair and with [`DebugWhat::CONTACTS`] one
    /// line per contact (between the two centers as they were when the pair was tested). Draws
    /// nothing while capture is off. Reads state only and allocates nothing itself.
    pub fn debug_emit(&self, sink: &mut dyn DebugSink, what: DebugWhat) {
        let Some(d) = self.debug.as_deref() else {
            return;
        };
        if let Some(grid) = d.grid {
            if what.contains(DebugWhat::GRID) {
                grid.emit_bounds(sink);
            }
            if what.contains(DebugWhat::GRID) || what.contains(DebugWhat::OCCUPANCY) {
                for &(cell, count) in &d.cells {
                    grid.emit_cell(sink, what, u64::from(cell), u64::from(count));
                }
            }
        }
        if what.contains(DebugWhat::CANDIDATES) {
            for &(a, b) in &d.candidates {
                sink.line(DebugKind::Candidate, a, b);
            }
        }
        if what.contains(DebugWhat::CONTACTS) {
            for &(a, b) in &d.contacts {
                sink.line(DebugKind::Contact, a, b);
            }
        }
    }

    /// Visit every contact of `bodies` exactly as [`for_each_contact_n2`] would: the same pairs, in
    /// the same `(a, b)` order, each seeing every earlier push.
    ///
    /// The visitor gets the bodies and the [`Contact`]. It may change `pos[0]` and `pos[2]` of
    /// bodies `c.a` and `c.b`, and anything outside the slice. Changing other bodies, `pos[1]`, a
    /// radius, a band or `underground` during the pass breaks the equivalence (the reference loop
    /// would see the change at once; the grid sees it at the next pass).
    pub fn for_each_contact<F>(&mut self, bodies: &mut [BroadBody], mut visit: F)
    where
        F: FnMut(&mut [BroadBody], Contact),
    {
        let rule = self.config.floor_rule;
        let n = bodies.len();
        self.build(bodies);
        if n < 2 {
            return;
        }
        // Largest finite |radius| of the bodies the grid may hold: a body with a non-finite radius is
        // wild and always a candidate, a large body stays in the large list all pass, and a body with
        // a non-finite position may become tame mid-pass, so its radius counts (ADR-8837).
        let large = self.config.large_threshold();
        let max_radius = bodies
            .iter()
            .filter(|b| b.radius.is_finite() && !above(abs_f(b.radius), large))
            .map(|b| abs_f(b.radius))
            .fold(0.0_f32, max_f);
        let margin = self.config.margin;

        for a in 0..n {
            self.unfile(a);
            let mut a_wild = !tame(&bodies[a]);
            let mut anchor = [bodies[a].pos[0], bodies[a].pos[2]];
            self.gather(bodies, a, a_wild, max_radius, a);
            let mut k = 0;
            while k < self.candidates.len() {
                let b = self.candidates[k] as usize;
                k += 1;
                self.stats.candidates += 1;
                if let Some(d) = self.debug.as_deref_mut() {
                    BroadCapture::keep(
                        &mut d.candidates,
                        d.caps.candidates,
                        &mut d.candidates_dropped,
                        bodies[a].pos,
                        bodies[b].pos,
                    );
                }
                let Some(c) = contact(bodies, a, b, rule) else {
                    continue;
                };
                self.stats.contacts += 1;
                if let Some(d) = self.debug.as_deref_mut() {
                    BroadCapture::keep(
                        &mut d.contacts,
                        d.caps.contacts,
                        &mut d.contacts_dropped,
                        bodies[a].pos,
                        bodies[b].pos,
                    );
                }
                visit(bodies, c);
                self.refile(bodies, b);
                if a_wild {
                    // Already tested against every later body.
                    continue;
                }
                // Pushed out of the area the list was gathered for: gather again from here, keeping
                // only bodies after `b`, so the order is unchanged and no pair is tested twice.
                let pa = [bodies[a].pos[0], bodies[a].pos[2]];
                let drift_x = pa[0] - anchor[0];
                let drift_z = pa[1] - anchor[1];
                // Written so a NaN drift is outside.
                let inside =
                    drift_x <= margin && drift_x >= -margin && drift_z <= margin && drift_z >= -margin;
                let wild_now = !tame(&bodies[a]);
                if wild_now || !inside {
                    self.stats.regathers += 1;
                    a_wild = wild_now;
                    anchor = pa;
                    self.gather(bodies, a, a_wild, max_radius, b);
                    k = 0;
                }
            }
        }
    }

    /// File every body into the grid for this pass.
    fn build(&mut self, bodies: &[BroadBody]) {
        let n = bodies.len();
        self.stats = BroadphaseStats {
            bodies: n,
            ..BroadphaseStats::default()
        };
        self.wild.clear();
        self.outliers.clear();
        self.large.clear();
        self.slot.clear();
        self.slot.resize(n, Slot::Gone);

        // The grid covers the bodies it will hold: tame, not large, inside the bounds.
        let mut lo = [f32::INFINITY; 2];
        let mut hi = [f32::NEG_INFINITY; 2];
        for b in bodies {
            if self.class(b) == Class::Cell {
                lo = [min_f(lo[0], b.pos[0]), min_f(lo[1], b.pos[2])];
                hi = [max_f(hi[0], b.pos[0]), max_f(hi[1], b.pos[2])];
            }
        }
        if lo[0] > hi[0] {
            lo = [0.0; 2];
            hi = [0.0; 2];
        }
        let geometry = GridGeometry::new(self.config, lo, hi);
        let dims = geometry.dims;
        self.dims = dims;
        self.origin = geometry.origin;
        self.inv_cell = geometry.inv_cell;
        let total = (dims[0] * dims[1]) as usize;
        self.stats.cells = total;
        if self.cells.len() < total {
            self.cells.resize_with(total, Vec::new);
        }
        for c in self.cells.iter_mut().take(total) {
            c.clear();
        }
        for (i, b) in bodies.iter().enumerate() {
            self.file(i, b);
        }
        self.stats.non_finite = self.wild.len();
        self.stats.outliers = self.outliers.len();
        self.stats.large = self.large.len();
        if self.debug.is_some() {
            self.capture_cells(bodies, total);
        }
    }

    /// Debug capture: the grid and the cells in use at the start of the pass (ADR-8562).
    fn capture_cells(&mut self, bodies: &[BroadBody], total: usize) {
        let Some(d) = self.debug.as_deref_mut() else {
            return;
        };
        d.contacts.clear();
        d.contacts_dropped = 0;
        d.candidates.clear();
        d.candidates_dropped = 0;
        d.cells.clear();
        d.cells_dropped = 0;
        let mut y = f32::INFINITY;
        for b in bodies.iter().filter(|b| tame(b)) {
            y = min_f(y, b.pos[1]);
        }
        d.grid = GridShape::new(self.origin, self.inv_cell, self.dims, y);
        for (index, cell) in self.cells.iter().take(total).enumerate() {
            if cell.is_empty() {
                continue;
            }
            if d.cells.len() < d.caps.cells {
                d.cells.push((index as u32, cell.len() as u32));
            } else {
                d.cells_dropped += 1;
            }
        }
    }

    /// The grid column (or row) of coordinate `v` on `axis`, clamped to the grid. Monotone in `v`.
    fn coord(&self, v: f32, axis: usize) -> i64 {
        GridGeometry {
            origin: self.origin,
            inv_cell: self.inv_cell,
            dims: self.dims,
        }
        .coord(v, axis)
    }

    fn cell_index(&self, b: &BroadBody) -> u32 {
        let cx = self.coord(b.pos[0], 0);
        let cz = self.coord(b.pos[2], 1);
        (cz * self.dims[0] + cx) as u32
    }

    /// Which list body `b` belongs in (ADR-8837): wild before large before outlier before cell.
    fn class(&self, b: &BroadBody) -> Class {
        if !tame(b) {
            Class::Wild
        } else if above(abs_f(b.radius), self.config.large_threshold()) {
            Class::Large
        } else if self.config.is_outside(b.pos[0], b.pos[2]) {
            Class::Outlier
        } else {
            Class::Cell
        }
    }

    /// The slot body `b` should be filed in now.
    fn slot_for(&self, b: &BroadBody) -> Slot {
        match self.class(b) {
            Class::Wild => Slot::Wild,
            Class::Large => Slot::Large,
            Class::Outlier => Slot::Outlier,
            Class::Cell => Slot::Cell(self.cell_index(b)),
        }
    }

    fn file(&mut self, i: usize, b: &BroadBody) {
        let slot = self.slot_for(b);
        let list = match slot {
            Slot::Gone => return,
            Slot::Cell(c) => &mut self.cells[c as usize],
            Slot::Wild => &mut self.wild,
            Slot::Outlier => &mut self.outliers,
            Slot::Large => &mut self.large,
        };
        list.push(i as u32);
        self.slot[i] = slot;
    }

    fn unfile(&mut self, i: usize) {
        let list = match self.slot[i] {
            Slot::Gone => return,
            Slot::Cell(c) => &mut self.cells[c as usize],
            Slot::Wild => &mut self.wild,
            Slot::Outlier => &mut self.outliers,
            Slot::Large => &mut self.large,
        };
        if let Some(at) = list.iter().position(|&x| x as usize == i) {
            list.swap_remove(at);
        }
        self.slot[i] = Slot::Gone;
    }

    /// Re-file body `b` after the visitor may have moved it.
    fn refile(&mut self, bodies: &[BroadBody], b: usize) {
        let body = &bodies[b];
        let want = self.slot_for(body);
        if self.slot[b] != want {
            self.unfile(b);
            self.file(b, body);
            self.stats.cell_moves += 1;
        }
    }

    /// Fill `candidates` with every filed body after `after` that may touch body `a`, ascending.
    fn gather(&mut self, bodies: &[BroadBody], a: usize, a_wild: bool, max_radius: f32, after: usize) {
        self.candidates.clear();
        let total = (self.dims[0] * self.dims[1]) as usize;
        if a_wild {
            // A non-finite body is tested against every later body, as in the reference loop.
            for c in self.cells.iter().take(total) {
                self.candidates.extend(c.iter().copied());
            }
            self.candidates.extend(self.outliers.iter().copied());
            self.candidates.extend(self.large.iter().copied());
        } else {
            let body = &bodies[a];
            // A contact needs |dx| < |a.radius + b.radius| <= |a.radius| + max_radius (see the
            // ADR-8048 proof); the margin covers the drift allowed before a re-gather and the pad
            // covers the rounding of the bounds below.
            let reach = abs_f(body.radius) + max_radius + self.config.margin;
            let reach_ok = reach.is_finite();
            let (x, z) = (body.pos[0], body.pos[2]);
            let (cx0, cx1, cz0, cz1) = if reach_ok {
                let pad_x = query_pad(reach, x);
                let pad_z = query_pad(reach, z);
                (
                    self.coord(x - reach - pad_x, 0),
                    self.coord(x + reach + pad_x, 0),
                    self.coord(z - reach - pad_z, 1),
                    self.coord(z + reach + pad_z, 1),
                )
            } else {
                (0, self.dims[0] - 1, 0, self.dims[1] - 1)
            };
            for cz in cz0..=cz1 {
                for cx in cx0..=cx1 {
                    let c = (cz * self.dims[0] + cx) as usize;
                    self.candidates.extend(self.cells[c].iter().copied());
                }
            }
            // Outliers and large bodies: the same padded window, with each body's own radius in
            // place of the grid's largest (the ADR-8048 bound holds per pair; ADR-8837).
            let ra = abs_f(body.radius) + self.config.margin;
            for &j in self.outliers.iter().chain(&self.large) {
                let other = &bodies[j as usize];
                if side_in_reach(x, z, ra + abs_f(other.radius), other) {
                    self.candidates.push(j);
                }
            }
        }
        self.candidates.extend(self.wild.iter().copied());
        self.candidates.retain(|&b| b as usize > after);
        self.candidates.sort_unstable();
    }
}

impl DebugSource for DetBroadphase {
    fn set_debug_capture(&mut self, capture: Option<DebugCapture>) {
        DetBroadphase::set_debug_capture(self, capture);
    }
    fn debug_emit(&self, sink: &mut dyn DebugSink, what: DebugWhat) {
        DetBroadphase::debug_emit(self, sink, what);
    }
}

/// The list a body is filed in (ADR-8837).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Wild,
    Large,
    Outlier,
    Cell,
}

/// True when `other` (a tame body off the grid) lies inside the padded window of half-width `reach`
/// around `(x, z)`, the same window a grid gather turns into cells. A non-finite reach (radii so
/// large their sum overflows) keeps the body.
fn side_in_reach(x: f32, z: f32, reach: f32, other: &BroadBody) -> bool {
    if !reach.is_finite() {
        return true;
    }
    let pad_x = query_pad(reach, x);
    let pad_z = query_pad(reach, z);
    let (ox, oz) = (other.pos[0], other.pos[2]);
    ox >= x - reach - pad_x && ox <= x + reach + pad_x && oz >= z - reach - pad_z && oz <= z + reach + pad_z
}

/// A body the grid can file: finite x, z and radius. Others are "wild" and tested against
/// everyone, as the reference loop does (a NaN distance or radius never skips a pair there).
fn tame(b: &BroadBody) -> bool {
    b.pos[0].is_finite() && b.pos[2].is_finite() && b.radius.is_finite()
}

fn min_f(a: f32, b: f32) -> f32 {
    if b < a { b } else { a }
}

fn max_f(a: f32, b: f32) -> f32 {
    if b > a { b } else { a }
}

/// `v > limit`: the one large-body test, shared by the filing and the grid's reach (ADR-8837).
fn above(v: f32, limit: f32) -> bool {
    v > limit
}

fn abs_f(v: f32) -> f32 {
    if v < 0.0 { -v } else { v }
}

/// Shared monotone cell geometry. Filing policy remains private to each index.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GridGeometry {
    pub(crate) origin: [f32; 2],
    pub(crate) inv_cell: f32,
    pub(crate) dims: [i64; 2],
}

impl GridGeometry {
    pub(crate) fn new(config: BroadphaseConfig, lo: [f32; 2], hi: [f32; 2]) -> Self {
        // Grid size; widen the cells until it fits `max_cells`. Spans are measured in f64 so a
        // continent-wide spread cannot overflow (the cell index math below stays in f32).
        let mut cell = config.cell_size;
        let span = [
            f64::from(hi[0]) - f64::from(lo[0]),
            f64::from(hi[1]) - f64::from(lo[1]),
        ];
        let dims = loop {
            let cols = ((span[0] / f64::from(cell)) as i64).saturating_add(1);
            let rows = ((span[1] / f64::from(cell)) as i64).saturating_add(1);
            if cols.saturating_mul(rows) <= config.max_cells as i64 || !cell.is_finite() {
                break [cols.max(1), rows.max(1)];
            }
            cell *= 2.0;
        };
        let dims = if cell.is_finite() { dims } else { [1, 1] };
        Self {
            origin: lo,
            inv_cell: if cell.is_finite() { 1.0 / cell } else { 0.0 },
            dims,
        }
    }

    pub(crate) fn coord(&self, v: f32, axis: usize) -> i64 {
        let t = (v - self.origin[axis]) * self.inv_cell;
        // `as` truncates toward zero and saturates; step down for negative non-integers so this is
        // a floor without `f32::floor`.
        let mut c = t as i64;
        if (c as f32) > t {
            c = c.saturating_sub(1);
        }
        c.clamp(0, self.dims[axis] - 1)
    }

    pub(crate) fn index(&self, pos: [f32; 3]) -> usize {
        (self.coord(pos[2], 1) * self.dims[0] + self.coord(pos[0], 0)) as usize
    }
}

/// ADR-8048 rounding envelope, also used by the range-query derivation in ADR-8318.
pub(crate) fn query_pad(reach: f32, center: f32) -> f32 {
    reach * (1.0 / 256.0) + abs_f(center) * (1.0 / 262_144.0) + 1e-3
}

/// Saves position, radius, band and the underground flag (DC-3.7a).
impl crate::state::DetState for BroadBody {
    const STATE_TAG: [u8; 8] = *b"brodbody";
    const STATE_VERSION: u32 = 1;
    fn save(&self, w: &mut crate::state::DetWriter) {
        for v in [
            self.pos[0],
            self.pos[1],
            self.pos[2],
            self.radius,
            self.band[0],
            self.band[1],
        ] {
            w.f32(v);
        }
        w.bool(self.underground);
    }
    fn load(r: &mut crate::state::DetReader<'_>) -> Result<Self, crate::state::DetStateError> {
        Ok(BroadBody {
            pos: [r.f32()?, r.f32()?, r.f32()?],
            radius: r.f32()?,
            band: [r.f32()?, r.f32()?],
            underground: r.bool()?,
        })
    }
}

/// Saves the settings (DC-3.7a). A [`DetBroadphase`] itself is a cache: a sim that saves its bodies
/// and this config resumes with `DetBroadphase::new(config)` and visits the same contacts in the same
/// order (only `stats()` restarts from zero). `load` refuses a `max_cells` that does not fit this
/// target's `usize`, an unknown floor rule and an option tag other than 0 or 1 (`bad_value`).
///
/// Version 2 (ADR-8837) adds `world_bounds` and `large_radius` after the floor rule, each as a `u8`
/// tag (0 = `None`, 1 = `Some`) and its values. A version 1 snapshot is refused with `bad_version`
/// (the version rule in [`crate::state`]: snapshots live as long as one build).
impl crate::state::DetState for BroadphaseConfig {
    const STATE_TAG: [u8; 8] = *b"brodconf";
    const STATE_VERSION: u32 = 2;
    fn save(&self, w: &mut crate::state::DetWriter) {
        w.f32(self.cell_size);
        w.f32(self.margin);
        w.u64(self.max_cells as u64);
        w.u8(match self.floor_rule {
            FloorRule::Bands => 0,
            FloorRule::UndergroundOnly => 1,
            FloorRule::Never => 2,
        });
        match self.world_bounds {
            None => w.u8(0),
            Some(bounds) => {
                w.u8(1);
                for v in bounds {
                    w.f32(v);
                }
            }
        }
        match self.large_radius {
            None => w.u8(0),
            Some(r) => {
                w.u8(1);
                w.f32(r);
            }
        }
    }
    fn load(r: &mut crate::state::DetReader<'_>) -> Result<Self, crate::state::DetStateError> {
        use crate::state::DetStateError;
        let cell_size = r.f32()?;
        let margin = r.f32()?;
        let max_cells = r.u64()?;
        let max_cells = usize::try_from(max_cells)
            .map_err(|_| DetStateError::bad_value(format_args!("max_cells {max_cells}")))?;
        let floor_rule = match r.u8()? {
            0 => FloorRule::Bands,
            1 => FloorRule::UndergroundOnly,
            2 => FloorRule::Never,
            other => return Err(DetStateError::bad_value(format_args!("floor rule {other}"))),
        };
        let world_bounds = match r.u8()? {
            0 => None,
            1 => Some([r.f32()?, r.f32()?, r.f32()?, r.f32()?]),
            other => return Err(DetStateError::bad_value(format_args!("world_bounds tag {other}"))),
        };
        let large_radius = match r.u8()? {
            0 => None,
            1 => Some(r.f32()?),
            other => return Err(DetStateError::bad_value(format_args!("large_radius tag {other}"))),
        };
        Ok(BroadphaseConfig {
            cell_size,
            margin,
            max_cells,
            floor_rule,
            world_bounds,
            large_radius,
        })
    }
}
