// SPDX-License-Identifier: MPL-2.0
//! Multi-cell stamps (`docs/specs/86-2d-toolkit.md` § 12, ADR-2597): a rectangle of raw tile
//! GIDs copied from a layer that can be rotated, mirrored and pasted back anywhere.
//!
//! Rotating or mirroring a stamp also turns every tile in it: the GID flip bits are composed with
//! the transform (the eight combinations of Tiled's diagonal, horizontal and vertical flips form
//! the dihedral group of the square), so a rotated stamp looks like the rotated picture.

use serde::{Deserialize, Serialize};

use crate::tilemap::{FLIP_DIAGONAL_BIT, FLIP_X_BIT, FLIP_Y_BIT, MapLayer, TileMapAsset, split_gid};

/// Most cells (width × height) a stamp may hold.
pub const MAX_STAMP_CELLS: u64 = 1 << 20;

/// Stamp errors. [`StampError::code`] gives the stable agent error code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StampError {
    /// The cell count does not match the size, or the size is zero or past the limit.
    #[error("invalid stamp: {0}")]
    Invalid(String),
    /// The source rectangle is empty or outside the map.
    #[error("the rectangle ({x0}, {y0})..({x1}, {y1}) is outside the map")]
    OutsideMap {
        /// Left column.
        x0: u32,
        /// Top row.
        y0: u32,
        /// Right column (inclusive).
        x1: u32,
        /// Bottom row (inclusive).
        y1: u32,
    },
    /// The layer is not a tile layer.
    #[error("layer {0} is not a tile layer")]
    NotTileLayer(usize),
    /// A tile of the stamp is in no tileset of the map.
    #[error("the stamp uses gid {0}, which no tileset of the map holds")]
    BadGid(u32),
}

impl StampError {
    /// The stable error code (`data.tile_error`, `book/agents/2d.md`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "tile_bad_stamp",
            Self::OutsideMap { .. } => "tile_outside_map",
            Self::NotTileLayer(_) => "tile_not_tile_layer",
            Self::BadGid(_) => "tile_bad_gid",
        }
    }
}

/// A transform of a stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StampOp {
    /// Quarter turn clockwise.
    RotateCw,
    /// Quarter turn counter-clockwise.
    RotateCcw,
    /// Half turn.
    Rotate180,
    /// Mirror left ↔ right.
    FlipX,
    /// Mirror top ↔ bottom.
    FlipY,
}

impl StampOp {
    /// The agent name: `rotate_cw`, `rotate_ccw`, `rotate_180`, `flip_x`, `flip_y`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::RotateCw => "rotate_cw",
            Self::RotateCcw => "rotate_ccw",
            Self::Rotate180 => "rotate_180",
            Self::FlipX => "flip_x",
            Self::FlipY => "flip_y",
        }
    }

    /// Parses an agent name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        [
            Self::RotateCw,
            Self::RotateCcw,
            Self::Rotate180,
            Self::FlipX,
            Self::FlipY,
        ]
        .into_iter()
        .find(|op| op.name() == name)
    }

    /// The op as a 2×2 integer matrix acting on (x, y) with +Y down: `[[a, b], [c, d]]`.
    fn matrix(self) -> [[i8; 2]; 2] {
        match self {
            Self::RotateCw => [[0, -1], [1, 0]],
            Self::RotateCcw => [[0, 1], [-1, 0]],
            Self::Rotate180 => [[-1, 0], [0, -1]],
            Self::FlipX => [[-1, 0], [0, 1]],
            Self::FlipY => [[1, 0], [0, -1]],
        }
    }
}

type Matrix = [[i8; 2]; 2];

fn mul(a: Matrix, b: Matrix) -> Matrix {
    let at = |i: usize, j: usize| a[i][0] * b[0][j] + a[i][1] * b[1][j];
    [[at(0, 0), at(0, 1)], [at(1, 0), at(1, 1)]]
}

/// The matrix of the flip bits of a GID: the diagonal flip (transpose) first, then the
/// horizontal, then the vertical one.
fn flags_matrix(raw: u32) -> Matrix {
    let mut m: Matrix = [[1, 0], [0, 1]];
    if raw & FLIP_DIAGONAL_BIT != 0 {
        m = mul([[0, 1], [1, 0]], m);
    }
    if raw & FLIP_X_BIT != 0 {
        m = mul([[-1, 0], [0, 1]], m);
    }
    if raw & FLIP_Y_BIT != 0 {
        m = mul([[1, 0], [0, -1]], m);
    }
    m
}

/// `raw` with its flip bits replaced by those that act as `op` applied after the old ones.
fn transform_gid(raw: u32, op: StampOp) -> u32 {
    if raw == 0 {
        return 0;
    }
    let want = mul(op.matrix(), flags_matrix(raw));
    let id = raw & !(FLIP_X_BIT | FLIP_Y_BIT | FLIP_DIAGONAL_BIT);
    // Eight bit combinations, eight distinct matrices: exactly one matches.
    for bits in 0..8u32 {
        let flags = (if bits & 1 != 0 { FLIP_X_BIT } else { 0 })
            | (if bits & 2 != 0 { FLIP_Y_BIT } else { 0 })
            | (if bits & 4 != 0 { FLIP_DIAGONAL_BIT } else { 0 });
        if flags_matrix(flags) == want {
            return id | flags;
        }
    }
    raw
}

/// A rectangle of raw GIDs (flip bits included), row 0 on top; 0 is an empty cell.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    /// Columns.
    pub width: u32,
    /// Rows.
    pub height: u32,
    /// `width * height` raw GIDs, row by row.
    pub cells: Vec<u32>,
}

impl Stamp {
    /// A stamp from a size and its cells (checked).
    pub fn new(width: u32, height: u32, cells: Vec<u32>) -> Result<Self, StampError> {
        let stamp = Self { width, height, cells };
        stamp.validate()?;
        Ok(stamp)
    }

    /// Non-zero size within [`MAX_STAMP_CELLS`] and exactly `width * height` cells.
    pub fn validate(&self) -> Result<(), StampError> {
        let count = u64::from(self.width) * u64::from(self.height);
        if self.width == 0 || self.height == 0 || count > MAX_STAMP_CELLS {
            return Err(StampError::Invalid(format!(
                "{}x{} cells (1..={MAX_STAMP_CELLS} cells)",
                self.width, self.height
            )));
        }
        if self.cells.len() as u64 != count {
            return Err(StampError::Invalid(format!(
                "{} cells for a {}x{} stamp",
                self.cells.len(),
                self.width,
                self.height
            )));
        }
        Ok(())
    }

    /// Copies the cells `x0..=x1` by `y0..=y1` of tile layer `layer` (corners in any order).
    pub fn copy(map: &TileMapAsset, layer: usize, a: (u32, u32), b: (u32, u32)) -> Result<Self, StampError> {
        let Some(MapLayer::Tiles(tiles)) = map.layers.get(layer) else {
            return Err(StampError::NotTileLayer(layer));
        };
        let (x0, x1) = (a.0.min(b.0), a.0.max(b.0));
        let (y0, y1) = (a.1.min(b.1), a.1.max(b.1));
        if x1 >= map.width || y1 >= map.height {
            return Err(StampError::OutsideMap { x0, y0, x1, y1 });
        }
        let (width, height) = (x1 - x0 + 1, y1 - y0 + 1);
        if u64::from(width) * u64::from(height) > MAX_STAMP_CELLS {
            return Err(StampError::Invalid(format!(
                "{width}x{height} cells is too large"
            )));
        }
        let mut cells = Vec::with_capacity((width * height) as usize);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let index = u64::from(y) * u64::from(map.width) + u64::from(x);
                let raw = usize::try_from(index)
                    .ok()
                    .and_then(|i| tiles.data.get(i))
                    .copied()
                    .unwrap_or(0);
                cells.push(raw);
            }
        }
        Ok(Self { width, height, cells })
    }

    /// The raw GID at column `x`, row `y`.
    #[must_use]
    pub fn get(&self, x: u32, y: u32) -> Option<u32> {
        if x >= self.width || y >= self.height {
            return None;
        }
        self.cells
            .get(y as usize * self.width as usize + x as usize)
            .copied()
    }

    /// The stamp transformed by `op`: cells move and every tile's flip bits turn with them.
    #[must_use]
    pub fn transformed(&self, op: StampOp) -> Self {
        let (w, h) = (self.width, self.height);
        let (nw, nh) = match op {
            StampOp::RotateCw | StampOp::RotateCcw => (h, w),
            _ => (w, h),
        };
        let mut cells = vec![0u32; self.cells.len()];
        for y in 0..h {
            for x in 0..w {
                // Where the source cell (x, y) lands.
                let (nx, ny) = match op {
                    StampOp::RotateCw => (h - 1 - y, x),
                    StampOp::RotateCcw => (y, w - 1 - x),
                    StampOp::Rotate180 => (w - 1 - x, h - 1 - y),
                    StampOp::FlipX => (w - 1 - x, y),
                    StampOp::FlipY => (x, h - 1 - y),
                };
                let raw = self
                    .cells
                    .get(y as usize * w as usize + x as usize)
                    .copied()
                    .unwrap_or(0);
                if let Some(slot) = cells.get_mut(ny as usize * nw as usize + nx as usize) {
                    *slot = transform_gid(raw, op);
                }
            }
        }
        Self {
            width: nw,
            height: nh,
            cells,
        }
    }

    /// Whether [`Self::paste`] would accept this stamp for `layer` of `map`: a well-formed stamp,
    /// a tile layer that holds every cell, every tile in a tileset of the map. Callers that must
    /// not touch the map on failure check first.
    pub fn check(&self, map: &TileMapAsset, layer: usize) -> Result<(), StampError> {
        self.validate()?;
        if let Some(&raw) = self.cells.iter().find(|&&raw| {
            let (gid, _) = split_gid(raw);
            gid != 0 && map.tileset_for(gid).is_none()
        }) {
            return Err(StampError::BadGid(split_gid(raw).0));
        }
        let Some(MapLayer::Tiles(tiles)) = map.layers.get(layer) else {
            return Err(StampError::NotTileLayer(layer));
        };
        if tiles.data.len() as u64 != u64::from(map.width) * u64::from(map.height) {
            return Err(StampError::Invalid(format!(
                "layer {layer} does not hold {}x{} cells",
                map.width, map.height
            )));
        }
        Ok(())
    }

    /// Pastes the stamp with its top-left cell at `(x, y)` (signed: a stamp may hang over the
    /// map edge and is clipped) into tile layer `layer`. Empty stamp cells leave the map alone
    /// when `skip_empty` is set. Returns `(x, y, before, after)` for every changed cell, sorted
    /// by `(y, x)`. Nothing is written when a tile of the stamp is in no tileset of the map.
    pub fn paste(
        &self,
        map: &mut TileMapAsset,
        layer: usize,
        at: (i64, i64),
        skip_empty: bool,
    ) -> Result<Vec<crate::autotile::TileChange>, StampError> {
        self.check(map, layer)?;
        let (width, height) = (map.width, map.height);
        let Some(MapLayer::Tiles(tiles)) = map.layers.get_mut(layer) else {
            return Err(StampError::NotTileLayer(layer));
        };
        let mut changes = Vec::new();
        for sy in 0..self.height {
            for sx in 0..self.width {
                let (mx, my) = (at.0 + i64::from(sx), at.1 + i64::from(sy));
                if mx < 0 || my < 0 || mx >= i64::from(width) || my >= i64::from(height) {
                    continue;
                }
                let raw = self.cells[sy as usize * self.width as usize + sx as usize];
                if skip_empty && raw == 0 {
                    continue;
                }
                let index = my as usize * width as usize + mx as usize;
                let before = tiles.data[index];
                if before != raw {
                    tiles.data[index] = raw;
                    changes.push(crate::autotile::TileChange {
                        x: mx as u32,
                        y: my as u32,
                        before,
                        after: raw,
                    });
                }
            }
        }
        Ok(changes)
    }
}

#[cfg(test)]
mod tests;
