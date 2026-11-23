// SPDX-License-Identifier: MPL-2.0
//! Composable heightfield operators: fractal noise, domain warp, terraces, impact craters and
//! flattened build pads (DO27 absorb row C-18; `book/agents/terrain-ops.md`).
//!
//! Every operator is a pure function: it reads a row-major height grid (meters) placed in the world
//! by a [`GridFrame`], never changes its input, and returns a new grid of the same size. Everything
//! random derives from an integer seed, and noise is evaluated at **world** positions, so two
//! windows of one map (a tiled procgen run, a sub-rectangle of a terrain) agree where they overlap.
//!
//! Conventions, shared by every operator:
//!
//! * Row `r`, column `c` of the grid sits at world `x = origin[0] + c * cell_m[0]`,
//!   `z = origin[1] + r * cell_m[1]`: the same layout as [`TerrainMap`] (row 0 is the −Z edge).
//!   [`GridFrame::of_map`] gives a map's frame.
//! * Angles are degrees, counter-clockwise from +X toward +Z.
//! * The arithmetic is `f64` with `+ − × ÷ sqrt floor` only; the few transcendentals (an angle's
//!   sine and cosine, the crater blanket's power, the central peak's exponential) go through
//!   [`regia_core::math::detmath`], so a cook gives the same bytes on every platform.
//!
//! The operators are also procgen nodes (`terrain.terrace`, `terrain.crater`, `terrain.flatten_pad`,
//! `terrain.domain_warp`, `terrain.noise` in `regia_procgen`) and the agent method
//! `terrain.apply_operator`.
//!
//! Re-implemented in Rust from DO27's terrain tools (`tools/terrain/terrain_features.py`,
//! `tools/terrain/terrain_noise.py` at d6ed37677, regia-absorb-1); see `docs/provenance.md`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::sampler::TerrainMap;

pub mod features;
pub mod noise;

pub use features::{
    CraterParams, PadParams, PadShape, TerraceParams, WarpParams, crater, domain_warp, flatten_pad, terrace,
};
pub use noise::{
    NoiseKind, NoiseParams, add_noise, fbm, fbm_at, hash_unit, noise_field, ridged, ridged_at, value_noise,
};

/// The largest grid an operator accepts, in samples (a 8193 × 8193 map).
pub const MAX_OP_SAMPLES: u64 = 8193 * 8193;

/// Why an operator refused its input. [`OpError::code`] is the agent error code.
#[derive(Clone, Debug, PartialEq, Error)]
#[non_exhaustive]
pub enum OpError {
    /// The grid is smaller than 2 × 2 samples.
    #[error("the grid must be at least 2 x 2 samples, got {cols} x {rows}")]
    GridTooSmall {
        /// Columns.
        cols: u32,
        /// Rows.
        rows: u32,
    },
    /// The grid holds more than [`MAX_OP_SAMPLES`] samples.
    #[error("the grid holds {samples} samples, more than {MAX_OP_SAMPLES}")]
    GridTooLarge {
        /// Samples asked for.
        samples: u64,
    },
    /// A cell size or the origin is not finite, or a cell size is not positive.
    #[error("the cell size {cell_m:?} must be finite and positive and the origin {origin:?} finite")]
    BadFrame {
        /// Cell sizes along X and Z, meters.
        cell_m: [f64; 2],
        /// World position of sample (0, 0).
        origin: [f64; 2],
    },
    /// The height slice does not hold `cols × rows` samples.
    #[error("the grid needs {expected} heights, got {actual}")]
    SampleCount {
        /// `cols × rows`.
        expected: usize,
        /// Heights given.
        actual: usize,
    },
    /// An input height is NaN or infinite.
    #[error("height sample {index} is not finite")]
    NonFinite {
        /// Row-major index of the first bad sample.
        index: usize,
    },
    /// A parameter is out of range.
    #[error("`{name}` {reason}")]
    BadParam {
        /// The parameter.
        name: &'static str,
        /// What it must be.
        reason: &'static str,
    },
    /// A pad without a fixed height covers no sample, so it has no mean height to take.
    #[error("the pad covers no sample of the grid; give it a height")]
    PadEmpty,
}

impl OpError {
    /// The agent error code: `op_bad_grid`, `op_bad_param`, `op_non_finite` or `op_pad_empty`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::GridTooSmall { .. }
            | Self::GridTooLarge { .. }
            | Self::BadFrame { .. }
            | Self::SampleCount { .. } => "op_bad_grid",
            Self::NonFinite { .. } => "op_non_finite",
            Self::BadParam { .. } => "op_bad_param",
            Self::PadEmpty => "op_pad_empty",
        }
    }
}

/// Where a row-major height grid sits in the world.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GridFrame {
    /// Sample columns (along +X).
    pub cols: u32,
    /// Sample rows (along +Z).
    pub rows: u32,
    /// Distance between neighboring samples along X and Z, meters.
    pub cell_m: [f64; 2],
    /// World `(x, z)` of sample (0, 0) of the full map this grid is a window of, meters.
    pub origin: [f64; 2],
    /// Full-map `(col, row)` of this grid's sample (0, 0); `[0, 0]` for a whole map. Positions are
    /// worked out from the full-map index, so two windows of one map give a shared sample the same
    /// position bit for bit (and the same noise).
    #[serde(default)]
    pub first: [u32; 2],
}

impl GridFrame {
    /// A frame, checked: at least 2 × 2 samples, at most [`MAX_OP_SAMPLES`], finite positive cells
    /// and a finite origin.
    ///
    /// # Errors
    ///
    /// [`OpError::GridTooSmall`], [`OpError::GridTooLarge`] or [`OpError::BadFrame`].
    pub fn new(cols: u32, rows: u32, cell_m: [f64; 2], origin: [f64; 2]) -> Result<Self, OpError> {
        let frame = Self {
            cols,
            rows,
            cell_m,
            origin,
            first: [0, 0],
        };
        frame.check()?;
        Ok(frame)
    }

    /// A frame of square `cell_m` cells with sample (0, 0) at the world origin (the DO27 tools'
    /// field coordinates).
    ///
    /// # Errors
    ///
    /// As [`GridFrame::new`].
    pub fn square(cols: u32, rows: u32, cell_m: f64) -> Result<Self, OpError> {
        Self::new(cols, rows, [cell_m, cell_m], [0.0, 0.0])
    }

    /// The frame of a whole terrain map: its sample grid, spacing and centered placement.
    #[must_use]
    pub fn of_map(map: &TerrainMap) -> Self {
        let half = f64::from(map.world_width()) * 0.5;
        Self {
            cols: map.width(),
            rows: map.height(),
            cell_m: [f64::from(map.sample_spacing()), f64::from(map.sample_spacing_z())],
            origin: [-half, -half],
            first: [0, 0],
        }
    }

    /// The same frame as a window starting at full-map sample `first` (`origin` stays the full
    /// map's).
    ///
    /// # Errors
    ///
    /// [`OpError::GridTooLarge`] when the window's last index overflows.
    pub fn with_first(mut self, first: [u32; 2]) -> Result<Self, OpError> {
        if first[0].checked_add(self.cols).is_none() || first[1].checked_add(self.rows).is_none() {
            return Err(OpError::GridTooLarge {
                samples: u64::from(first[0]) + u64::from(self.cols),
            });
        }
        self.first = first;
        Ok(self)
    }

    /// Samples in the grid.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cols as usize * self.rows as usize
    }

    /// Whether the grid holds no sample (never true for a checked frame).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// World `(x, z)` of sample `(col, row)`, meters.
    #[must_use]
    pub fn position(&self, col: u32, row: u32) -> [f64; 2] {
        [
            self.origin[0] + f64::from(self.first[0] + col) * self.cell_m[0],
            self.origin[1] + f64::from(self.first[1] + row) * self.cell_m[1],
        ]
    }

    /// Continuous `(col, row)` of a world point (not clamped).
    #[must_use]
    pub fn grid_of(&self, x: f64, z: f64) -> [f64; 2] {
        [
            (x - self.origin[0]) / self.cell_m[0] - f64::from(self.first[0]),
            (z - self.origin[1]) / self.cell_m[1] - f64::from(self.first[1]),
        ]
    }

    /// The sample rectangle `[col0, row0, col1, row1]` (inclusive, clamped to the grid) that holds
    /// every sample within `reach` meters of `(x, z)` along each axis, or `None` when none does.
    #[must_use]
    pub fn window(&self, x: f64, z: f64, reach: f64) -> Option<[u32; 4]> {
        let lo = self.grid_of(x - reach, z - reach);
        let hi = self.grid_of(x + reach, z + reach);
        let max = [f64::from(self.cols - 1), f64::from(self.rows - 1)];
        let finite = x.is_finite() && z.is_finite() && reach.is_finite();
        if !finite || lo[0] > max[0] || lo[1] > max[1] || hi[0] < 0.0 || hi[1] < 0.0 {
            return None;
        }
        let c0 = lo[0].floor().max(0.0) as u32;
        let r0 = lo[1].floor().max(0.0) as u32;
        let c1 = hi[0].ceil().min(max[0]) as u32;
        let r1 = hi[1].ceil().min(max[1]) as u32;
        Some([c0, r0, c1, r1])
    }

    fn check(&self) -> Result<(), OpError> {
        if self.cols < 2 || self.rows < 2 {
            return Err(OpError::GridTooSmall {
                cols: self.cols,
                rows: self.rows,
            });
        }
        let samples = u64::from(self.cols) * u64::from(self.rows);
        if samples > MAX_OP_SAMPLES {
            return Err(OpError::GridTooLarge { samples });
        }
        let cell_ok = self.cell_m.iter().all(|c| c.is_finite() && *c > 0.0);
        if !cell_ok || !self.origin.iter().all(|o| o.is_finite()) {
            return Err(OpError::BadFrame {
                cell_m: self.cell_m,
                origin: self.origin,
            });
        }
        Ok(())
    }

    /// Checks the frame and that `heights` holds exactly its samples, all finite.
    ///
    /// # Errors
    ///
    /// Any frame error, [`OpError::SampleCount`] or [`OpError::NonFinite`].
    pub fn check_heights(&self, heights: &[f32]) -> Result<(), OpError> {
        self.check()?;
        if heights.len() != self.len() {
            return Err(OpError::SampleCount {
                expected: self.len(),
                actual: heights.len(),
            });
        }
        if let Some(index) = heights.iter().position(|h| !h.is_finite()) {
            return Err(OpError::NonFinite { index });
        }
        Ok(())
    }
}

/// `3t² − 2t³` of `t` clamped to `[0, 1]`.
#[inline]
pub(crate) fn smoothstep(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// `(sin, cos)` of an angle in degrees, the same bits on every platform; exact at multiples of
/// 90°, so an axis-aligned rectangle has no rounding at its edges.
pub(crate) fn sin_cos_deg(deg: f64) -> (f64, f64) {
    let d = deg.rem_euclid(360.0);
    if d == 0.0 {
        (0.0, 1.0)
    } else if d == 90.0 {
        (1.0, 0.0)
    } else if d == 180.0 {
        (0.0, -1.0)
    } else if d == 270.0 {
        (-1.0, 0.0)
    } else {
        let (s, c) = regia_core::math::detmath::det_sin_cos(d.to_radians() as f32);
        (f64::from(s), f64::from(c))
    }
}

/// A parameter check: `Err(BadParam)` unless `ok`.
pub(crate) fn require(ok: bool, name: &'static str, reason: &'static str) -> Result<(), OpError> {
    if ok {
        Ok(())
    } else {
        Err(OpError::BadParam { name, reason })
    }
}

/// The row-major rectangle `[col0, row0, col1, row1)` holding every sample where `after` differs
/// from `before` bit for bit, or `None` when nothing changed.
#[must_use]
pub fn changed_rect(frame: &GridFrame, before: &[f32], after: &[f32]) -> Option<[u32; 4]> {
    let cols = frame.cols as usize;
    let mut rect: Option<[u32; 4]> = None;
    for (i, (a, b)) in before.iter().zip(after).enumerate() {
        if a.to_bits() != b.to_bits() {
            let (c, r) = ((i % cols) as u32, (i / cols) as u32);
            rect = Some(match rect {
                None => [c, r, c + 1, r + 1],
                Some(q) => [q[0].min(c), q[1].min(r), q[2].max(c + 1), q[3].max(r + 1)],
            });
        }
    }
    rect
}
