// SPDX-License-Identifier: MPL-2.0
//! Render-free debug draw for the deterministic indexes (ADR-8562).
//!
//! [`crate::broadphase::DetBroadphase`] and [`crate::spatial::DetSpatial`] can describe what they did
//! as lines, circles, boxes and text at a point, through a [`DebugSink`]. The crate draws nothing
//! itself and has no GPU types: a host adapts the sink to its own debug lines (the framework's
//! `DetDebugDraw` feeds the gizmo buffer), or records it with [`DebugRecorder`] and replays it later
//! (another thread, another frame).
//!
//! Capture is off by default. Turn it on with `set_debug_capture(Some(DebugCapture::default()))` on
//! the index. While it is off, a pass and a query do no debug work and allocate nothing, and
//! `debug_emit` draws only what the index already holds (the grid). While it is on, the capture
//! buffers are reserved once, at the call that turns it on, and filled up to their caps (counted
//! drops beyond that), so a pass still allocates nothing. Capture reads state only: it never
//! changes a contact, a candidate, a query answer or any bit of the simulation, and it is not
//! part of any hashed state.
//!
//! ```
//! use regia_detsim::broadphase::{BroadBody, BroadphaseConfig, DetBroadphase};
//! use regia_detsim::debug::{DebugCapture, DebugKind, DebugRecorder, DebugWhat};
//!
//! let mut bodies = vec![
//!     BroadBody::new([0.0, 0.0, 0.0], 1.0, [-1.0, 2.0]),
//!     BroadBody::new([1.5, 0.0, 0.0], 1.0, [-1.0, 2.0]),
//! ];
//! let mut grid = DetBroadphase::new(BroadphaseConfig::default());
//! grid.set_debug_capture(Some(DebugCapture::default()));
//! grid.for_each_contact(&mut bodies, |_, _| {});
//! let mut lines = DebugRecorder::default();
//! grid.debug_emit(&mut lines, DebugWhat::CONTACTS);
//! assert_eq!(lines.count_kind(DebugKind::Contact), grid.stats().contacts as usize);
//! ```

/// The role of one debug primitive; a host maps each to a color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DebugKind {
    /// The grid's bounds and the outline of each cell in use.
    Grid,
    /// A cell's body count, as text at the cell center.
    Occupancy,
    /// A candidate pair the last pass tested (a line between the two centers).
    Candidate,
    /// A contact the last pass handed to its visitor (a line between the two centers).
    Contact,
    /// A query's shape: a circle of the query radius around its center.
    Query,
    /// A query hit: a line from the query center to the row.
    QueryHit,
    /// A flow field's arrow: a cell's move toward the goal (a shaft and a two-line head), or a
    /// circle on a goal cell (`regia_nav::det::flow`, ADR-8839).
    Flow,
}

impl DebugKind {
    /// Every kind, in a stable order.
    pub const ALL: [Self; 7] = [
        Self::Grid,
        Self::Occupancy,
        Self::Candidate,
        Self::Contact,
        Self::Query,
        Self::QueryHit,
        Self::Flow,
    ];

    /// Stable lowercase name (agent rows and color keys).
    pub fn name(self) -> &'static str {
        match self {
            Self::Grid => "grid",
            Self::Occupancy => "occupancy",
            Self::Candidate => "candidate",
            Self::Contact => "contact",
            Self::Query => "query",
            Self::QueryHit => "query_hit",
            Self::Flow => "flow",
        }
    }
}

/// Where debug primitives go. Every position is `[x, y, z]` in sim units; circles lie on the XZ
/// plane. A sink never feeds back into the simulation.
pub trait DebugSink {
    /// A line segment from `a` to `b`.
    fn line(&mut self, kind: DebugKind, a: [f32; 3], b: [f32; 3]);
    /// A circle on the XZ plane around `center`.
    fn circle(&mut self, kind: DebugKind, center: [f32; 3], radius: f32);
    /// An axis-aligned box from `min` to `max` (a flat rectangle when `min[1] == max[1]`).
    fn aabb(&mut self, kind: DebugKind, min: [f32; 3], max: [f32; 3]);
    /// Text anchored at a point.
    fn text(&mut self, kind: DebugKind, at: [f32; 3], text: &str);
}

/// Which parts of an index to draw: a set of flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct DebugWhat(u32);

impl DebugWhat {
    /// Nothing.
    pub const NONE: Self = Self(0);
    /// The grid bounds and the cells in use.
    pub const GRID: Self = Self(1);
    /// Body counts per cell in use.
    pub const OCCUPANCY: Self = Self(1 << 1);
    /// The candidate pairs of the last pass (needs capture).
    pub const CANDIDATES: Self = Self(1 << 2);
    /// The contacts of the last pass (needs capture).
    pub const CONTACTS: Self = Self(1 << 3);
    /// The shapes and hits of the last queries (needs capture).
    pub const QUERIES: Self = Self(1 << 4);
    /// Flow field arrows (a source that holds a flow field, such as `regia_nav`'s `FlowDebug`;
    /// ADR-8839). The indexes ignore it.
    pub const FLOW: Self = Self(1 << 5);
    /// Everything.
    pub const ALL: Self = Self(0b11_1111);

    /// Each flag with its stable lowercase name, in a stable order.
    pub const NAMED: [(&'static str, Self); 6] = [
        ("grid", Self::GRID),
        ("occupancy", Self::OCCUPANCY),
        ("candidates", Self::CANDIDATES),
        ("contacts", Self::CONTACTS),
        ("queries", Self::QUERIES),
        ("flow", Self::FLOW),
    ];

    /// The raw bits.
    pub fn bits(self) -> u32 {
        self.0
    }

    /// The set from raw bits; unknown bits are dropped.
    pub fn from_bits(bits: u32) -> Self {
        Self(bits & Self::ALL.0)
    }

    /// True when every flag of `other` is set.
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// True when no flag is set.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Both sets.
    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The flag with this name (see [`Self::NAMED`]); `all` and `none` name the whole set.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "all" => Some(Self::ALL),
            "none" => Some(Self::NONE),
            _ => Self::NAMED
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, flag)| *flag),
        }
    }

    /// The names of the set flags, in [`Self::NAMED`] order.
    pub fn names(self) -> impl Iterator<Item = &'static str> {
        Self::NAMED
            .into_iter()
            .filter(move |(_, flag)| self.contains(*flag))
            .map(|(name, _)| name)
    }

    /// True when drawing this set needs the index to capture during passes and queries.
    pub fn needs_capture(self) -> bool {
        self.0 & (Self::CANDIDATES.0 | Self::CONTACTS.0 | Self::QUERIES.0) != 0
    }
}

/// Capture caps for one index. Each buffer is reserved once when capture is turned on; a pass that
/// produces more than a cap keeps the first ones and counts the rest as dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebugCapture {
    /// Most contacts kept from the last pass.
    pub contacts: usize,
    /// Most candidate pairs kept from the last pass.
    pub candidates: usize,
    /// Most cells in use kept for the occupancy view.
    pub cells: usize,
    /// The query ring: the last N queries.
    pub queries: usize,
}

impl Default for DebugCapture {
    fn default() -> Self {
        Self {
            contacts: 4096,
            candidates: 4096,
            cells: 4096,
            queries: 32,
        }
    }
}

/// Counters of the capture buffers, for a host that shows what was cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DebugCaptureStats {
    /// Contacts kept from the last pass.
    pub contacts: usize,
    /// Contacts of the last pass beyond the cap.
    pub contacts_dropped: u64,
    /// Candidate pairs kept from the last pass.
    pub candidates: usize,
    /// Candidate pairs of the last pass beyond the cap.
    pub candidates_dropped: u64,
    /// Cells in use kept.
    pub cells: usize,
    /// Cells in use beyond the cap.
    pub cells_dropped: u64,
    /// Queries in the ring.
    pub queries: usize,
}

/// One recorded primitive (see [`DebugRecorder`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DebugPrim {
    /// [`DebugSink::line`].
    Line {
        /// Role.
        kind: DebugKind,
        /// Start.
        a: [f32; 3],
        /// End.
        b: [f32; 3],
    },
    /// [`DebugSink::circle`].
    Circle {
        /// Role.
        kind: DebugKind,
        /// Center.
        center: [f32; 3],
        /// Radius.
        radius: f32,
    },
    /// [`DebugSink::aabb`].
    Aabb {
        /// Role.
        kind: DebugKind,
        /// Low corner.
        min: [f32; 3],
        /// High corner.
        max: [f32; 3],
    },
    /// [`DebugSink::text`]; the text is `DebugRecorder::text_of`.
    Text {
        /// Role.
        kind: DebugKind,
        /// Anchor.
        at: [f32; 3],
        /// Byte range in the recorder's text buffer.
        range: (u32, u32),
    },
}

impl DebugPrim {
    /// The primitive's role.
    pub fn kind(&self) -> DebugKind {
        match *self {
            Self::Line { kind, .. }
            | Self::Circle { kind, .. }
            | Self::Aabb { kind, .. }
            | Self::Text { kind, .. } => kind,
        }
    }
}

/// Primitive counts by shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DebugCounts {
    /// Lines.
    pub lines: usize,
    /// Circles.
    pub circles: usize,
    /// Boxes.
    pub boxes: usize,
    /// Texts.
    pub texts: usize,
}

/// A [`DebugSink`] that keeps what it is given, to replay into another sink later (another thread
/// or frame). Reuse one and [`Self::clear`] it: it keeps its buffers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DebugRecorder {
    prims: Vec<DebugPrim>,
    text: String,
}

impl DebugRecorder {
    /// Drop every primitive, keeping the buffers.
    pub fn clear(&mut self) {
        self.prims.clear();
        self.text.clear();
    }

    /// The primitives, in emission order.
    pub fn prims(&self) -> &[DebugPrim] {
        &self.prims
    }

    /// Number of primitives.
    pub fn len(&self) -> usize {
        self.prims.len()
    }

    /// True when nothing was recorded.
    pub fn is_empty(&self) -> bool {
        self.prims.is_empty()
    }

    /// The text of a [`DebugPrim::Text`]; empty for other primitives.
    pub fn text_of(&self, prim: &DebugPrim) -> &str {
        match *prim {
            DebugPrim::Text { range, .. } => self.text.get(range.0 as usize..range.1 as usize).unwrap_or(""),
            _ => "",
        }
    }

    /// Primitives of one role.
    pub fn count_kind(&self, kind: DebugKind) -> usize {
        self.prims.iter().filter(|p| p.kind() == kind).count()
    }

    /// Primitives by shape.
    pub fn counts(&self) -> DebugCounts {
        let mut out = DebugCounts::default();
        for p in &self.prims {
            match p {
                DebugPrim::Line { .. } => out.lines += 1,
                DebugPrim::Circle { .. } => out.circles += 1,
                DebugPrim::Aabb { .. } => out.boxes += 1,
                DebugPrim::Text { .. } => out.texts += 1,
            }
        }
        out
    }

    /// Send every primitive to `sink`, in emission order.
    pub fn replay(&self, sink: &mut dyn DebugSink) {
        for p in &self.prims {
            match *p {
                DebugPrim::Line { kind, a, b } => sink.line(kind, a, b),
                DebugPrim::Circle { kind, center, radius } => sink.circle(kind, center, radius),
                DebugPrim::Aabb { kind, min, max } => sink.aabb(kind, min, max),
                DebugPrim::Text { kind, at, .. } => sink.text(kind, at, self.text_of(p)),
            }
        }
    }
}

impl DebugSink for DebugRecorder {
    fn line(&mut self, kind: DebugKind, a: [f32; 3], b: [f32; 3]) {
        self.prims.push(DebugPrim::Line { kind, a, b });
    }
    fn circle(&mut self, kind: DebugKind, center: [f32; 3], radius: f32) {
        self.prims.push(DebugPrim::Circle { kind, center, radius });
    }
    fn aabb(&mut self, kind: DebugKind, min: [f32; 3], max: [f32; 3]) {
        self.prims.push(DebugPrim::Aabb { kind, min, max });
    }
    fn text(&mut self, kind: DebugKind, at: [f32; 3], text: &str) {
        let start = self.text.len();
        self.text.push_str(text);
        let range = (
            u32::try_from(start).unwrap_or(u32::MAX),
            u32::try_from(self.text.len()).unwrap_or(u32::MAX),
        );
        self.prims.push(DebugPrim::Text { kind, at, range });
    }
}

/// An index that can describe itself through a [`DebugSink`] (both deterministic indexes).
pub trait DebugSource {
    /// Turn capture on with these caps (reserving its buffers now), or off with `None` (freeing them).
    fn set_debug_capture(&mut self, capture: Option<DebugCapture>);
    /// Draw the parts `what` names into `sink`. Reads state only.
    fn debug_emit(&self, sink: &mut dyn DebugSink, what: DebugWhat);
}

/// Write `n` in decimal into `buf` and return the digits, without allocating.
pub(crate) fn decimal(n: u64, buf: &mut [u8; 20]) -> &str {
    let mut at = buf.len();
    let mut v = n;
    loop {
        at -= 1;
        buf[at] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    std::str::from_utf8(&buf[at..]).unwrap_or("?")
}

/// Grid geometry for drawing: low corner, cell edge, columns and rows, and the height to draw at.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct GridShape {
    pub(crate) origin: [f32; 2],
    pub(crate) cell: f32,
    pub(crate) dims: [i64; 2],
    pub(crate) y: f32,
}

impl GridShape {
    /// The grid from the index's geometry; `None` when the cell edge is not finite.
    pub(crate) fn new(origin: [f32; 2], inv_cell: f32, dims: [i64; 2], y: f32) -> Option<Self> {
        if !(inv_cell.is_finite() && inv_cell > 0.0) {
            return None;
        }
        let cell = 1.0 / inv_cell;
        cell.is_finite().then_some(Self {
            origin,
            cell,
            dims,
            y: if y.is_finite() { y } else { 0.0 },
        })
    }

    fn cell_min(&self, index: u64) -> [f32; 3] {
        let cols = self.dims[0].max(1) as u64;
        let cx = (index % cols) as f32;
        let cz = (index / cols) as f32;
        [
            self.origin[0] + cx * self.cell,
            self.y,
            self.origin[1] + cz * self.cell,
        ]
    }

    /// The grid bounds as one flat box.
    pub(crate) fn emit_bounds(&self, sink: &mut dyn DebugSink) {
        let min = [self.origin[0], self.y, self.origin[1]];
        let max = [
            self.origin[0] + self.dims[0] as f32 * self.cell,
            self.y,
            self.origin[1] + self.dims[1] as f32 * self.cell,
        ];
        sink.aabb(DebugKind::Grid, min, max);
    }

    /// One cell in use: its outline (with `GRID`) and its count (with `OCCUPANCY`).
    pub(crate) fn emit_cell(&self, sink: &mut dyn DebugSink, what: DebugWhat, index: u64, count: u64) {
        let min = self.cell_min(index);
        if what.contains(DebugWhat::GRID) {
            sink.aabb(
                DebugKind::Grid,
                min,
                [min[0] + self.cell, self.y, min[2] + self.cell],
            );
        }
        if what.contains(DebugWhat::OCCUPANCY) {
            let mut buf = [0_u8; 20];
            let half = self.cell * 0.5;
            sink.text(
                DebugKind::Occupancy,
                [min[0] + half, self.y, min[2] + half],
                decimal(count, &mut buf),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_writes_digits_without_allocating() {
        let mut buf = [0_u8; 20];
        assert_eq!(decimal(0, &mut buf), "0");
        assert_eq!(decimal(907, &mut buf), "907");
        assert_eq!(decimal(u64::MAX, &mut buf), "18446744073709551615");
    }

    #[test]
    fn what_names_round_trip() {
        for (name, flag) in DebugWhat::NAMED {
            assert_eq!(DebugWhat::from_name(name), Some(flag));
        }
        assert_eq!(DebugWhat::from_name("all"), Some(DebugWhat::ALL));
        assert_eq!(DebugWhat::from_name("bogus"), None);
        let both = DebugWhat::GRID.union(DebugWhat::CONTACTS);
        assert_eq!(both.names().collect::<Vec<_>>(), ["grid", "contacts"]);
        assert!(both.needs_capture() && !DebugWhat::GRID.needs_capture());
        assert_eq!(DebugWhat::from_bits(u32::MAX), DebugWhat::ALL);
    }

    /// ADR-8839: flow arrows are a named part and a kind of their own; `all` includes them, every
    /// name is unique, and no capture is needed to draw them.
    #[test]
    fn flow_is_a_named_part_and_kind() {
        assert_eq!(DebugWhat::from_name("flow"), Some(DebugWhat::FLOW));
        assert!(DebugWhat::ALL.contains(DebugWhat::FLOW));
        assert!(!DebugWhat::FLOW.needs_capture());
        let union = DebugWhat::NAMED
            .iter()
            .fold(DebugWhat::NONE, |acc, (_, flag)| acc.union(*flag));
        assert_eq!(union, DebugWhat::ALL, "ALL is exactly the named parts");
        assert_eq!(DebugKind::Flow.name(), "flow");
        let mut names: Vec<_> = DebugKind::ALL.iter().map(|k| k.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), DebugKind::ALL.len());
        let mut rec = DebugRecorder::default();
        rec.line(DebugKind::Flow, [0.0; 3], [1.0, 0.0, 0.0]);
        assert_eq!(rec.count_kind(DebugKind::Flow), 1);
    }

    #[test]
    fn recorder_replays_in_order_with_text() {
        let mut a = DebugRecorder::default();
        a.line(DebugKind::Contact, [0.0; 3], [1.0, 0.0, 0.0]);
        a.text(DebugKind::Occupancy, [0.5; 3], "12");
        a.circle(DebugKind::Query, [0.0; 3], 3.0);
        let mut b = DebugRecorder::default();
        a.replay(&mut b);
        assert_eq!(a, b);
        assert_eq!(b.text_of(&b.prims()[1]), "12");
        assert_eq!(
            b.counts(),
            DebugCounts {
                lines: 1,
                circles: 1,
                boxes: 0,
                texts: 1
            }
        );
    }
}
