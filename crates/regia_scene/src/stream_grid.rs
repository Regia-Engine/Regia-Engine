// SPDX-License-Identifier: MPL-2.0
//! The streaming grid: a uniform square grid over the ground plane (X and Z) whose cells are the
//! unit of world streaming.
//!
//! Coordinates are `f64` world meters and cells are addressed by an `i64` pair, so a world far
//! larger than `f32` can resolve is still addressable. Everything here is pure arithmetic with no
//! global state, and every result is deterministic. See `book/agents/world-streaming.md`.
//!
//! The cell of a point is `floor(p / cell_size)` on each axis (a standard uniform-grid lookup), so
//! cell `(0, 0)` covers `[0, size) x [0, size)` and negative coordinates fall in negative cells with
//! no seam at the origin.

use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The largest absolute cell coordinate [`StreamGrid::cell_of`] accepts. Kept a little inside
/// `i64` so that neighbour and bounds arithmetic on a returned cell cannot overflow.
pub const MAX_CELL_COORD: i64 = 1 << 61;

/// Errors from the streaming grid and the planner built on it.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum StreamError {
    /// The cell size is not a finite number greater than zero.
    #[error("cell size must be finite and greater than zero, got {0}")]
    BadCellSize(f64),
    /// A position or radius was NaN or infinite.
    #[error("{0} is not a finite number")]
    NonFinite(&'static str),
    /// A position maps to a cell outside `+-MAX_CELL_COORD`.
    #[error("position {0} is outside the addressable cell range")]
    OutOfRange(f64),
    /// A radius is negative or otherwise unusable.
    #[error("{what} must be finite and not negative, got {value}")]
    BadRadius {
        /// Which radius.
        what: &'static str,
        /// The offending value.
        value: f64,
    },
    /// The unload radius is smaller than the load radius, which would load and unload the same cell
    /// on alternate steps (no hysteresis band).
    #[error("unload radius {unload} is smaller than load radius {load}")]
    NoHysteresis {
        /// The load radius.
        load: f64,
        /// The unload radius.
        unload: f64,
    },
    /// An area query would produce more cells than the caller allowed.
    #[error("the area covers {cells} cells, over the limit of {limit}")]
    TooManyCells {
        /// Cells the area covers.
        cells: u128,
        /// The limit passed by the caller.
        limit: u64,
    },
    /// The planner was asked for a plan with no viewer positions.
    #[error("at least one viewer position is required")]
    NoViewers,
    /// A cell was registered twice.
    #[error("cell {0} is already registered")]
    DuplicateCell(CellCoord),
    /// A plan names a cell the planner does not know.
    #[error("cell {0} is not registered")]
    UnknownCell(CellCoord),
    /// A plan loads a cell that is already loaded.
    #[error("cell {0} is already loaded")]
    AlreadyLoaded(CellCoord),
    /// A plan unloads a cell that is not loaded.
    #[error("cell {0} is not loaded")]
    NotLoaded(CellCoord),
    /// Applying a plan would leave more bytes resident than the budget allows.
    #[error("the plan leaves {resident} bytes resident, over the budget of {budget}")]
    OverBudget {
        /// Resident bytes after the plan.
        resident: u64,
        /// The memory budget.
        budget: u64,
    },
    /// A cell label did not parse as `<x>_<z>`.
    #[error("`{0}` is not a cell label of the form `<x>_<z>`")]
    BadLabel(String),
}

/// A streaming cell address: signed cell indices along X and Z.
///
/// The derived order is X-major then Z; it is the tie-break used everywhere a deterministic
/// order is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Reflect, Serialize, Deserialize)]
#[reflect(Clone)]
pub struct CellCoord {
    /// Index along X.
    pub x: i64,
    /// Index along Z.
    pub z: i64,
}

impl CellCoord {
    /// A cell address from its indices.
    pub const fn new(x: i64, z: i64) -> Self {
        Self { x, z }
    }

    /// The stable text id of the cell, `"<x>_<z>"` (for example `"-3_12"`), usable in file names
    /// and agent replies.
    pub fn label(&self) -> String {
        format!("{}_{}", self.x, self.z)
    }

    /// Parse a label made by [`CellCoord::label`].
    pub fn from_label(label: &str) -> Result<Self, StreamError> {
        let bad = || StreamError::BadLabel(label.to_string());
        let (x, z) = label.split_once('_').ok_or_else(bad)?;
        let x = x.parse::<i64>().map_err(|_| bad())?;
        let z = z.parse::<i64>().map_err(|_| bad())?;
        Ok(Self { x, z })
    }
}

impl std::fmt::Display for CellCoord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}_{}", self.x, self.z)
    }
}

/// A uniform grid of square streaming cells.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StreamGrid {
    cell_size: f64,
}

impl StreamGrid {
    /// A grid whose cells are `cell_size` meters on a side.
    pub fn new(cell_size: f64) -> Result<Self, StreamError> {
        if !cell_size.is_finite() || cell_size <= 0.0 {
            return Err(StreamError::BadCellSize(cell_size));
        }
        Ok(Self { cell_size })
    }

    /// The side length of a cell, meters.
    pub fn cell_size(&self) -> f64 {
        self.cell_size
    }

    fn axis_cell(&self, v: f64, what: &'static str) -> Result<i64, StreamError> {
        if !v.is_finite() {
            return Err(StreamError::NonFinite(what));
        }
        let c = (v / self.cell_size).floor();
        if c.abs() > MAX_CELL_COORD as f64 {
            return Err(StreamError::OutOfRange(v));
        }
        Ok(c as i64)
    }

    /// The cell containing the world point `(x, z)`.
    pub fn cell_of(&self, x: f64, z: f64) -> Result<CellCoord, StreamError> {
        Ok(CellCoord {
            x: self.axis_cell(x, "x")?,
            z: self.axis_cell(z, "z")?,
        })
    }

    /// The minimum corner of `cell` in world meters.
    pub fn cell_min(&self, cell: CellCoord) -> [f64; 2] {
        [cell.x as f64 * self.cell_size, cell.z as f64 * self.cell_size]
    }

    /// The maximum corner of `cell` in world meters.
    pub fn cell_max(&self, cell: CellCoord) -> [f64; 2] {
        [
            (cell.x as f64 + 1.0) * self.cell_size,
            (cell.z as f64 + 1.0) * self.cell_size,
        ]
    }

    /// The center of `cell` in world meters.
    pub fn cell_center(&self, cell: CellCoord) -> [f64; 2] {
        [
            (cell.x as f64 + 0.5) * self.cell_size,
            (cell.z as f64 + 0.5) * self.cell_size,
        ]
    }

    /// The distance on the ground plane from `(x, z)` to the nearest point of `cell`; zero when
    /// the point is inside it.
    pub fn distance_to_cell(&self, x: f64, z: f64, cell: CellCoord) -> f64 {
        let lo = self.cell_min(cell);
        let hi = self.cell_max(cell);
        let dx = (lo[0] - x).max(x - hi[0]).max(0.0);
        let dz = (lo[1] - z).max(z - hi[1]).max(0.0);
        dx.hypot(dz)
    }

    /// Every cell whose distance to `(x, z)` is at most `radius`, in the cell order (X-major, then Z).
    ///
    /// `max_cells` bounds the work: an area whose bounding box covers more cells than that is
    /// refused with [`StreamError::TooManyCells`] before any cell is produced.
    pub fn cells_within(
        &self,
        x: f64,
        z: f64,
        radius: f64,
        max_cells: u64,
    ) -> Result<Vec<CellCoord>, StreamError> {
        if !radius.is_finite() || radius < 0.0 {
            return Err(StreamError::BadRadius {
                what: "radius",
                value: radius,
            });
        }
        // One extra ring: a point exactly on a cell edge is at distance zero from the cell beyond it.
        let lo = self.cell_of(x - radius, z - radius)?;
        let hi = self.cell_of(x + radius, z + radius)?;
        let (lo, hi) = (
            CellCoord::new(lo.x - 1, lo.z - 1),
            CellCoord::new(hi.x + 1, hi.z + 1),
        );
        let nx = (hi.x - lo.x + 1) as u128;
        let nz = (hi.z - lo.z + 1) as u128;
        let total = nx * nz;
        if total > u128::from(max_cells) {
            return Err(StreamError::TooManyCells {
                cells: total,
                limit: max_cells,
            });
        }
        let mut out = Vec::new();
        for cx in lo.x..=hi.x {
            for cz in lo.z..=hi.z {
                let cell = CellCoord { x: cx, z: cz };
                if self.distance_to_cell(x, z, cell) <= radius {
                    out.push(cell);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> StreamGrid {
        StreamGrid::new(100.0).expect("valid grid")
    }

    fn cell_of(g: &StreamGrid, x: f64, z: f64) -> CellCoord {
        g.cell_of(x, z).expect("in range")
    }

    #[test]
    fn bad_cell_sizes_are_refused() {
        for size in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(
                matches!(StreamGrid::new(size), Err(StreamError::BadCellSize(_))),
                "{size}"
            );
        }
    }

    #[test]
    fn cell_of_uses_floor_with_no_seam_at_the_origin() {
        let g = grid();
        assert_eq!(cell_of(&g, 0.0, 0.0), CellCoord::new(0, 0));
        assert_eq!(cell_of(&g, 99.999, 100.0), CellCoord::new(0, 1));
        assert_eq!(cell_of(&g, -0.001, -100.0), CellCoord::new(-1, -1));
        assert_eq!(cell_of(&g, -100.001, 250.0), CellCoord::new(-2, 2));
    }

    #[test]
    fn cell_of_rejects_non_finite_and_out_of_range() {
        let g = grid();
        assert_eq!(g.cell_of(f64::NAN, 0.0), Err(StreamError::NonFinite("x")));
        assert_eq!(g.cell_of(0.0, f64::INFINITY), Err(StreamError::NonFinite("z")));
        assert!(matches!(g.cell_of(1.0e30, 0.0), Err(StreamError::OutOfRange(_))));
    }

    #[test]
    fn cell_corners_and_center_are_consistent_with_cell_of() {
        let g = grid();
        let c = CellCoord::new(-3, 4);
        assert_eq!(g.cell_min(c), [-300.0, 400.0]);
        assert_eq!(g.cell_max(c), [-200.0, 500.0]);
        assert_eq!(g.cell_center(c), [-250.0, 450.0]);
        let center = g.cell_center(c);
        assert_eq!(cell_of(&g, center[0], center[1]), c);
    }

    #[test]
    fn labels_round_trip_including_negatives() {
        for c in [
            CellCoord::new(0, 0),
            CellCoord::new(-3, 12),
            CellCoord::new(i64::MIN, i64::MAX),
        ] {
            assert_eq!(CellCoord::from_label(&c.label()), Ok(c));
            assert_eq!(c.to_string(), c.label());
        }
        for bad in ["", "3", "a_b", "1_", "_1", "1_2_3", "1 2"] {
            assert!(
                matches!(CellCoord::from_label(bad), Err(StreamError::BadLabel(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn distance_is_zero_inside_and_euclidean_outside() {
        let g = grid();
        let c = CellCoord::new(0, 0);
        assert_eq!(g.distance_to_cell(50.0, 50.0, c), 0.0);
        assert_eq!(g.distance_to_cell(0.0, 100.0, c), 0.0);
        assert_eq!(g.distance_to_cell(130.0, 50.0, c), 30.0);
        assert_eq!(g.distance_to_cell(-30.0, -40.0, c), 50.0);
    }

    #[test]
    fn cells_within_is_sorted_and_circular() {
        let g = grid();
        let cells = g.cells_within(50.0, 50.0, 60.0, 1000).expect("small area");
        // The home cell plus the four edge neighbours (distance 0 .. 50); corners are 70.7 away.
        assert_eq!(cells.len(), 5);
        assert!(cells.windows(2).all(|w| w[0] < w[1]));
        assert!(cells.contains(&CellCoord::new(0, 0)));
        assert!(!cells.contains(&CellCoord::new(1, 1)));
        let wide = g.cells_within(50.0, 50.0, 80.0, 1000).expect("small area");
        assert!(wide.contains(&CellCoord::new(1, 1)) && wide.contains(&CellCoord::new(-1, -1)));
    }

    #[test]
    fn cells_within_enforces_its_limit_and_validates_radius() {
        let g = grid();
        assert!(matches!(
            g.cells_within(0.0, 0.0, 10_000.0, 100),
            Err(StreamError::TooManyCells { limit: 100, .. })
        ));
        assert!(matches!(
            g.cells_within(0.0, 0.0, -1.0, 100),
            Err(StreamError::BadRadius { .. })
        ));
        assert!(matches!(
            g.cells_within(0.0, 0.0, f64::NAN, 100),
            Err(StreamError::BadRadius { .. })
        ));
        let corner = g.cells_within(0.0, 0.0, 0.0, 100).expect("zero radius");
        assert_eq!(corner.len(), 4, "a point on a corner touches four cells");
    }
}
