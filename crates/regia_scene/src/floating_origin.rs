// SPDX-License-Identifier: MPL-2.0
//! Large-world coordinates and the floating-origin rebase helper.
//!
//! An `f32` position loses resolution quickly away from zero (about 6 cm at 1 000 000 m), so a
//! world of tens or hundreds of kilometers cannot keep `f32` world positions. A [`LargePosition`]
//! stores an `i64` cell index per axis plus an `f32` offset from that cell's center; the offset
//! never exceeds half a cell, so precision is the same everywhere in the world.
//!
//! [`FloatingOrigin`] picks one cell as the origin of the "local space" that renderers and physics
//! use. Positions in local space are plain `f32` relative to the origin cell's center. When the
//! focus (usually the camera) moves further than the rebase threshold from the origin, the origin
//! jumps to the focus's cell and every local-space position is shifted by the returned [`Rebase`],
//! so nothing visibly moves. The shift is a whole number of cells, which for an integer cell size
//! up to 2^24 meters is exact in `f32`. See `book/agents/world-streaming.md`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::stream_grid::MAX_CELL_COORD;

/// The largest cell size [`LargeSpace::new`] accepts, meters. Beyond this a half-cell offset no
/// longer has useful `f32` resolution.
pub const MAX_ORIGIN_CELL_SIZE: f64 = 1.0e7;

/// Errors from large-world coordinate math.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum OriginError {
    /// The cell size is not within `(0, MAX_ORIGIN_CELL_SIZE]`.
    #[error("cell size must be within (0, {MAX_ORIGIN_CELL_SIZE}] meters, got {0}")]
    BadCellSize(f64),
    /// The rebase threshold is smaller than half a cell, so a rebase could trigger again at once.
    #[error("rebase threshold {threshold} is below half the cell size ({half})")]
    BadThreshold {
        /// The threshold given.
        threshold: f32,
        /// Half the cell size.
        half: f64,
    },
    /// A coordinate was NaN or infinite.
    #[error("{0} is not finite")]
    NonFinite(&'static str),
    /// A position is further than the addressable range of `+-MAX_CELL_COORD` cells.
    #[error("the position is outside the addressable range of cells")]
    OutOfRange,
}

/// A position in a large world: an `i64` cell per axis plus an `f32` offset from the cell center.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LargePosition {
    /// Cell index per axis `[x, y, z]`.
    pub cell: [i64; 3],
    /// Offset from the cell center, meters; within `[-cell_size / 2, cell_size / 2)` once normalized.
    pub local: [f32; 3],
}

/// The cell size that defines how world meters map to [`LargePosition`] cells.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LargeSpace {
    cell_size: f64,
}

impl LargeSpace {
    /// A space whose cells are `cell_size` meters on a side. Integer sizes that are powers of two
    /// (1024, 4096) keep all cell arithmetic exact.
    pub fn new(cell_size: f64) -> Result<Self, OriginError> {
        if !cell_size.is_finite() || cell_size <= 0.0 || cell_size > MAX_ORIGIN_CELL_SIZE {
            return Err(OriginError::BadCellSize(cell_size));
        }
        Ok(Self { cell_size })
    }

    /// The side length of a cell, meters.
    pub fn cell_size(&self) -> f64 {
        self.cell_size
    }

    /// Split one `f64` coordinate into a cell and an offset in `[-half, half)`.
    fn split(&self, v: f64, what: &'static str) -> Result<(i64, f32), OriginError> {
        if !v.is_finite() {
            return Err(OriginError::NonFinite(what));
        }
        let cell = (v / self.cell_size + 0.5).floor();
        if cell.abs() > MAX_CELL_COORD as f64 {
            return Err(OriginError::OutOfRange);
        }
        let cell = cell as i64;
        let local = v - cell as f64 * self.cell_size;
        Ok((cell, local as f32))
    }

    /// Add `add` meters to an offset in cell `cell`, carrying whole cells; returns the new cell and offset.
    fn carry(&self, cell: i64, add: f64, what: &'static str) -> Result<(i64, f32), OriginError> {
        let (shift, local) = self.split(add, what)?;
        let cell = cell.checked_add(shift).ok_or(OriginError::OutOfRange)?;
        if cell.unsigned_abs() > MAX_CELL_COORD as u64 {
            return Err(OriginError::OutOfRange);
        }
        Ok((cell, local))
    }

    /// The large position of the world point `p` (meters, `f64`).
    pub fn from_world(&self, p: [f64; 3]) -> Result<LargePosition, OriginError> {
        let (cx, lx) = self.split(p[0], "x")?;
        let (cy, ly) = self.split(p[1], "y")?;
        let (cz, lz) = self.split(p[2], "z")?;
        Ok(LargePosition {
            cell: [cx, cy, cz],
            local: [lx, ly, lz],
        })
    }

    /// The world point of `pos` in `f64` meters. Exact to `f64` resolution at that distance (about
    /// a millimeter near 10^13 m); use [`LargeSpace::relative`] when only a nearby offset is needed.
    pub fn to_world(&self, pos: &LargePosition) -> [f64; 3] {
        std::array::from_fn(|a| pos.cell[a] as f64 * self.cell_size + f64::from(pos.local[a]))
    }

    /// Carry offsets that left the half-cell range into the cell indices.
    pub fn normalize(&self, pos: &LargePosition) -> Result<LargePosition, OriginError> {
        self.offset(pos, [0.0; 3])
    }

    /// `pos` moved by `delta` meters, renormalized.
    pub fn offset(&self, pos: &LargePosition, delta: [f32; 3]) -> Result<LargePosition, OriginError> {
        // Add in f64 so a large delta does not round before it is carried into the cell.
        let sum = |a: usize| f64::from(pos.local[a]) + f64::from(delta[a]);
        let (cx, lx) = self.carry(pos.cell[0], sum(0), "offset")?;
        let (cy, ly) = self.carry(pos.cell[1], sum(1), "offset")?;
        let (cz, lz) = self.carry(pos.cell[2], sum(2), "offset")?;
        Ok(LargePosition {
            cell: [cx, cy, cz],
            local: [lx, ly, lz],
        })
    }

    /// The `f32` vector from `origin` to `pos`, computed from the cell difference and the offsets
    /// in `f64` and rounded once, so it is accurate at any distance the result itself can hold.
    pub fn relative(&self, pos: &LargePosition, origin: &LargePosition) -> [f32; 3] {
        std::array::from_fn(|a| {
            let cells = i128::from(pos.cell[a]) - i128::from(origin.cell[a]);
            (cells as f64 * self.cell_size + (f64::from(pos.local[a]) - f64::from(origin.local[a]))) as f32
        })
    }
}

/// A rebase: the origin moved by `cell_delta` cells.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rebase {
    /// How many cells the origin moved per axis.
    pub cell_delta: [i64; 3],
    /// The same move in meters: subtract it from every local-space position.
    pub shift: [f32; 3],
}

impl Rebase {
    /// Re-express a local-space position after the rebase.
    pub fn apply(&self, local: &mut [f32; 3]) {
        for (l, s) in local.iter_mut().zip(self.shift) {
            *l -= s;
        }
    }
}

/// The origin cell of local space and the rule for moving it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatingOrigin {
    space: LargeSpace,
    cell: [i64; 3],
    threshold: f32,
}

impl FloatingOrigin {
    /// An origin at `cell`. `threshold` (meters) is how far the focus may stray from the origin
    /// cell's center on any axis before a rebase; it must be at least half a cell.
    pub fn new(space: LargeSpace, cell: [i64; 3], threshold: f32) -> Result<Self, OriginError> {
        let half = space.cell_size() * 0.5;
        if !threshold.is_finite() || f64::from(threshold) < half {
            return Err(OriginError::BadThreshold { threshold, half });
        }
        if cell.iter().any(|c| c.unsigned_abs() > MAX_CELL_COORD as u64) {
            return Err(OriginError::OutOfRange);
        }
        Ok(Self {
            space,
            cell,
            threshold,
        })
    }

    /// The space.
    pub fn space(&self) -> &LargeSpace {
        &self.space
    }

    /// The origin cell.
    pub fn cell(&self) -> [i64; 3] {
        self.cell
    }

    /// A position in local space: `f32` meters from the origin cell's center.
    pub fn to_local(&self, pos: &LargePosition) -> [f32; 3] {
        let origin = LargePosition {
            cell: self.cell,
            local: [0.0; 3],
        };
        self.space.relative(pos, &origin)
    }

    /// The large position of a local-space point.
    pub fn from_local(&self, local: [f32; 3]) -> Result<LargePosition, OriginError> {
        self.space.offset(
            &LargePosition {
                cell: self.cell,
                local: [0.0; 3],
            },
            local,
        )
    }

    /// Moves the origin to `cell` whatever the focus is, and returns the [`Rebase`] to apply. For
    /// debugging; a move of zero cells is a no-op rebase. A `cell` beyond `+-MAX_CELL_COORD`
    /// (which [`FloatingOrigin::new`] and [`FloatingOrigin::update`] also refuse) moves nothing
    /// and returns the no-op rebase, so no infinite shift reaches the local positions.
    pub fn rebase_to(&mut self, cell: [i64; 3]) -> Rebase {
        if cell.iter().any(|c| c.unsigned_abs() > MAX_CELL_COORD as u64) {
            tracing::warn!(target: "regia_scene", ?cell, "rebase_to: cell out of range; ignored");
            return Rebase {
                cell_delta: [0; 3],
                shift: [0.0; 3],
            };
        }
        let delta: [i64; 3] = std::array::from_fn(|a| cell[a].saturating_sub(self.cell[a]));
        let shift = delta.map(|d| (d as f64 * self.space.cell_size()) as f32);
        self.cell = cell;
        Rebase {
            cell_delta: delta,
            shift,
        }
    }

    /// Check whether `focus` strayed beyond the threshold and, if so, move the origin to its cell.
    ///
    /// Returns the [`Rebase`] to apply to every local-space position, or `None` when nothing moved.
    /// After a rebase the focus is within half a cell of the origin, so the next call returns
    /// `None` until it travels again.
    pub fn update(&mut self, focus: &LargePosition) -> Result<Option<Rebase>, OriginError> {
        let _span = tracing::trace_span!("floating_origin_update").entered();
        let local = self.to_local(focus);
        if local.iter().any(|v| !v.is_finite()) {
            return Err(OriginError::NonFinite("focus"));
        }
        if local.iter().all(|v| v.abs() <= self.threshold) {
            return Ok(None);
        }
        let normalized = self.space.normalize(focus)?;
        let diff = |a: usize| {
            normalized.cell[a]
                .checked_sub(self.cell[a])
                .ok_or(OriginError::OutOfRange)
        };
        let delta = [diff(0)?, diff(1)?, diff(2)?];
        let shift = delta.map(|d| (d as f64 * self.space.cell_size()) as f32);
        self.cell = normalized.cell;
        Ok(Some(Rebase {
            cell_delta: delta,
            shift,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space() -> LargeSpace {
        LargeSpace::new(1024.0).expect("space")
    }

    fn split(v: f64) -> LargePosition {
        space().from_world([v, 0.0, 0.0]).expect("in range")
    }

    #[test]
    fn bad_cell_sizes_and_thresholds_are_refused() {
        for size in [0.0, -1.0, f64::NAN, f64::INFINITY, MAX_ORIGIN_CELL_SIZE * 2.0] {
            assert!(
                matches!(LargeSpace::new(size), Err(OriginError::BadCellSize(_))),
                "{size}"
            );
        }
        assert!(LargeSpace::new(MAX_ORIGIN_CELL_SIZE).is_ok());
        let s = space();
        assert!(matches!(
            FloatingOrigin::new(s, [0; 3], 511.0),
            Err(OriginError::BadThreshold { .. })
        ));
        assert!(matches!(
            FloatingOrigin::new(s, [0; 3], f32::NAN),
            Err(OriginError::BadThreshold { .. })
        ));
        assert!(FloatingOrigin::new(s, [0; 3], 512.0).is_ok());
        assert_eq!(
            FloatingOrigin::new(s, [i64::MAX, 0, 0], 600.0),
            Err(OriginError::OutOfRange)
        );
    }

    #[test]
    fn offsets_stay_within_half_a_cell() {
        // Cell centers are multiples of 1024; the offset range is [-512, 512).
        assert_eq!(split(0.0).cell[0], 0);
        assert_eq!(split(511.9).cell[0], 0);
        let edge = split(512.0);
        assert_eq!((edge.cell[0], edge.local[0]), (1, -512.0));
        let low = split(-512.0);
        assert_eq!((low.cell[0], low.local[0]), (0, -512.0));
        let neg = split(-512.5);
        assert_eq!((neg.cell[0], neg.local[0]), (-1, 511.5));
        for v in [-1.0e9, -3333.3, -0.5, 0.25, 700.0, 1.0e9 + 0.123, 4.2e12] {
            let p = split(v);
            assert!((-512.0..512.0).contains(&p.local[0]), "{v}: {}", p.local[0]);
        }
    }

    #[test]
    fn world_round_trip_keeps_f32_precision_at_any_distance() {
        let s = space();
        for v in [
            0.0,
            1.0,
            -1.0,
            12_345.678,
            -987_654.321,
            1.0e9 + 0.123,
            -4.2e12 + 7.5,
        ] {
            let p = s.from_world([v, -v, v * 0.5]).expect("in range");
            let back = s.to_world(&p);
            // The offset is an f32 of at most 512 m: its rounding error is at most 3.1e-5 m.
            assert!((back[0] - v).abs() < 4.0e-5, "{v}: {}", back[0] - v);
            assert!((back[1] + v).abs() < 4.0e-5);
        }
    }

    #[test]
    fn nearby_positions_keep_millimeter_accuracy_far_from_zero() {
        let s = space();
        let a = s.from_world([5.0e9, 0.0, 5.0e9]).expect("a");
        let b = s.from_world([5.0e9 + 1.002, 0.0, 5.0e9 - 2.5]).expect("b");
        let d = s.relative(&b, &a);
        assert!(
            (d[0] - 1.002).abs() < 1.0e-4 && (d[2] + 2.5).abs() < 1.0e-4,
            "{d:?}"
        );
        // A plain f32 world position at 5e9 m could not tell these apart at all.
        assert_eq!(5.0e9f32, (5.0e9f64 + 1.002) as f32);
    }

    #[test]
    fn normalize_carries_in_both_directions() {
        let s = space();
        let p = LargePosition {
            cell: [10, -4, 0],
            local: [2600.0, -1100.0, 511.0],
        };
        let n = s.normalize(&p).expect("ok");
        assert_eq!(n.cell, [13, -5, 0]);
        assert_eq!(n.local, [2600.0 - 3.0 * 1024.0, -1100.0 + 1024.0, 511.0]);
        let w = s.to_world(&p);
        let wn = s.to_world(&n);
        assert!(w.iter().zip(wn).all(|(a, b)| (a - b).abs() < 1.0e-6));
    }

    #[test]
    fn offset_adds_and_renormalizes() {
        let s = space();
        let p = s.from_world([1000.0, 0.0, 0.0]).expect("ok");
        let q = s.offset(&p, [100.0, 0.0, -2000.0]).expect("ok");
        let w = s.to_world(&q);
        assert!((w[0] - 1100.0).abs() < 1.0e-4 && (w[2] + 2000.0).abs() < 1.0e-4);
        assert!(q.local.iter().all(|l| (-512.0..512.0).contains(l)));
    }

    #[test]
    fn errors_for_non_finite_and_out_of_range() {
        let s = space();
        assert_eq!(
            s.from_world([f64::NAN, 0.0, 0.0]),
            Err(OriginError::NonFinite("x"))
        );
        assert_eq!(
            s.from_world([0.0, 0.0, f64::INFINITY]),
            Err(OriginError::NonFinite("z"))
        );
        assert_eq!(s.from_world([1.0e30, 0.0, 0.0]), Err(OriginError::OutOfRange));
        let edge = LargePosition {
            cell: [MAX_CELL_COORD, 0, 0],
            local: [0.0; 3],
        };
        assert_eq!(s.offset(&edge, [2000.0, 0.0, 0.0]), Err(OriginError::OutOfRange));
        let max = LargePosition {
            cell: [i64::MAX, 0, 0],
            local: [600.0, 0.0, 0.0],
        };
        assert_eq!(s.normalize(&max), Err(OriginError::OutOfRange));
        assert_eq!(
            s.offset(&edge, [f32::NAN, 0.0, 0.0]),
            Err(OriginError::NonFinite("offset"))
        );
        let bad = LargePosition {
            cell: [0; 3],
            local: [f32::NAN, 0.0, 0.0],
        };
        assert_eq!(s.normalize(&bad), Err(OriginError::NonFinite("offset")));
    }

    #[test]
    fn relative_survives_extreme_cell_differences() {
        let s = space();
        let a = LargePosition {
            cell: [-MAX_CELL_COORD, 0, 0],
            local: [0.0; 3],
        };
        let b = LargePosition {
            cell: [MAX_CELL_COORD, 0, 0],
            local: [0.0; 3],
        };
        let d = s.relative(&b, &a);
        assert!(d[0].is_finite() && d[0] > 0.0);
    }

    #[test]
    fn no_rebase_inside_the_threshold() {
        let s = space();
        let mut o = FloatingOrigin::new(s, [3, 0, 0], 800.0).expect("origin");
        let focus = LargePosition {
            cell: [3, 0, 0],
            local: [790.0, -10.0, 0.0],
        };
        assert_eq!(o.update(&focus), Ok(None));
        assert_eq!(o.cell(), [3, 0, 0]);
    }

    #[test]
    fn rebase_moves_the_origin_and_keeps_world_positions() {
        let s = space();
        let mut o = FloatingOrigin::new(s, [0, 0, 0], 1024.0).expect("origin");
        let thing = s.from_world([3000.0, 20.0, -50.0]).expect("thing");
        let mut thing_local = o.to_local(&thing);
        let focus = s.from_world([2000.0, 0.0, 0.0]).expect("focus");
        let rebase = o.update(&focus).expect("ok").expect("focus is 2000 m out");
        assert_eq!(rebase.cell_delta, [2, 0, 0]);
        assert_eq!(rebase.shift, [2048.0, 0.0, 0.0]);
        rebase.apply(&mut thing_local);
        assert_eq!(thing_local, o.to_local(&thing), "shifting equals recomputing");
        let back = s.to_world(&o.from_local(thing_local).expect("ok"));
        assert!((back[0] - 3000.0).abs() < 1.0e-3 && (back[1] - 20.0).abs() < 1.0e-3);
        // The focus is now near the new origin and a second update does nothing.
        assert!(o.to_local(&focus).iter().all(|v| v.abs() <= 512.0));
        assert_eq!(o.update(&focus), Ok(None));
    }

    #[test]
    fn rebase_to_refuses_a_cell_out_of_range() {
        let mut o = FloatingOrigin::new(space(), [1, 0, 0], 1024.0).expect("origin");
        for bad in [[i64::MAX, 0, 0], [0, i64::MIN, 0], [0, 0, MAX_CELL_COORD + 1]] {
            let rebase = o.rebase_to(bad);
            assert_eq!(rebase.cell_delta, [0; 3]);
            assert_eq!(rebase.shift, [0.0; 3]);
            assert_eq!(o.cell(), [1, 0, 0]);
        }
        let ok = o.rebase_to([MAX_CELL_COORD, 0, 0]);
        assert!(ok.shift.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn a_long_walk_rebases_without_ever_moving_anything() {
        // A deterministic zero-mean walk of 20 000 steps: a tracked entity keeps its true world
        // position through many rebases and the focus's local values stay small.
        let s = space();
        let mut o = FloatingOrigin::new(s, [0; 3], 1024.0).expect("origin");
        let entity_world = [3_333.25f64, 17.5, -941.75];
        let entity = s.from_world(entity_world).expect("entity");
        let mut entity_local = o.to_local(&entity);
        let mut focus = s.from_world([0.0; 3]).expect("focus");
        let mut rebases = 0;
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
        };
        for i in 0..20_000 {
            let step = [next() * 600.0, next() * 20.0, next() * 600.0];
            focus = s.offset(&focus, step).expect("step");
            if let Some(r) = o.update(&focus).expect("update") {
                rebases += 1;
                r.apply(&mut entity_local);
            }
            let fl = o.to_local(&focus);
            assert!(fl.iter().all(|v| v.abs() <= 1024.0 + 1.0), "step {i}: {fl:?}");
        }
        assert!(rebases > 100, "{rebases}");
        // The entity was never re-created from f64: it only went through shifts.
        let expected = o.to_local(&entity);
        assert!(
            entity_local
                .iter()
                .zip(expected)
                .all(|(a, b)| (a - b).abs() < 1.0e-3)
        );
    }

    #[test]
    fn update_reports_a_bad_focus() {
        let s = space();
        let mut o = FloatingOrigin::new(s, [0; 3], 1024.0).expect("origin");
        let bad = LargePosition {
            cell: [0; 3],
            local: [f32::NAN, 0.0, 0.0],
        };
        assert_eq!(o.update(&bad), Err(OriginError::NonFinite("focus")));
        let far = LargePosition {
            cell: [i64::MAX, 0, 0],
            local: [0.0; 3],
        };
        assert!(o.update(&far).is_err(), "out of the addressable range");
        assert_eq!(o.cell(), [0; 3], "a failed update leaves the origin alone");
    }
}
