// SPDX-License-Identifier: MIT
// Portions ported from Fyrox (https://github.com/FyroxEngine/Fyrox, commit 0876f67),
// fyrox-impl/src/scene/terrain/brushstroke/strokechunks.rs.
// Copyright (c) 2019-present Dmitry Stepanov and Fyrox Engine contributors, MIT licence
// (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

//! The record of which samples a brush stroke has changed since the last write-back, grouped by
//! cook tile.
//!
//! Fyrox keys this by terrain chunk, whose height textures overlap their neighbours by one
//! texel. Regia Engine's terrain is one sample grid ([`crate::TerrainMap`]) cut into
//! non-overlapping cook tiles (`docs/formats/terrain.md` § 8), so the overlap bookkeeping is
//! gone; what stays is the batching: samples accumulate per tile and are written back to the
//! grid in one [`StrokeChunks::apply`], which also yields the dirty rectangle (GPU re-upload)
//! and the touched tiles (incremental re-cook on save).

use std::collections::{BTreeSet, HashMap, HashSet};

use glam::{IVec2, UVec2};

use crate::deform::DirtyRect;

/// A cook tile coordinate `(column, row)` at mip 0.
pub type TileCoord = (u32, u32);

/// Modified samples per tile since the last [`StrokeChunks::clear`].
#[derive(Debug, Clone)]
pub struct StrokeChunks {
    /// Tile edge length in samples.
    tile_size: u32,
    /// Local positions of the written samples, per tile.
    written: HashMap<TileCoord, HashSet<UVec2>>,
    /// Samples written since the last clear (counting repeats, like the original).
    count: usize,
    /// Allocated sets kept for reuse.
    unused: Vec<HashSet<UVec2>>,
}

impl Default for StrokeChunks {
    fn default() -> Self {
        Self::new(crate::stream::DEFAULT_TILE_SIZE)
    }
}

impl StrokeChunks {
    /// An empty record for tiles of `tile_size` samples (clamped to at least 1).
    pub fn new(tile_size: u32) -> Self {
        Self {
            tile_size: tile_size.max(1),
            written: HashMap::new(),
            count: 0,
            unused: Vec::new(),
        }
    }

    /// Tile edge length in samples.
    pub fn tile_size(&self) -> u32 {
        self.tile_size
    }

    /// Samples written since the last clear.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Forgets the written samples (keeping the allocations).
    pub fn clear(&mut self) {
        self.count = 0;
        for (_, mut set) in self.written.drain() {
            set.clear();
            self.unused.push(set);
        }
    }

    /// The tile holding sample `position` (`None` for negative coordinates).
    pub fn tile_of(&self, position: IVec2) -> Option<TileCoord> {
        if position.x < 0 || position.y < 0 {
            return None;
        }
        Some((
            position.x as u32 / self.tile_size,
            position.y as u32 / self.tile_size,
        ))
    }

    /// The first sample of `tile`.
    pub fn tile_origin(&self, tile: TileCoord) -> UVec2 {
        UVec2::new(tile.0 * self.tile_size, tile.1 * self.tile_size)
    }

    /// Records that sample `position` changed.
    pub fn write(&mut self, position: IVec2) {
        let Some(tile) = self.tile_of(position) else {
            return;
        };
        let local = position.as_uvec2() - self.tile_origin(tile);
        self.count += 1;
        let unused = &mut self.unused;
        self.written
            .entry(tile)
            .or_insert_with(|| unused.pop().unwrap_or_default())
            .insert(local);
    }

    /// The tiles written since the last clear.
    pub fn tiles(&self) -> BTreeSet<TileCoord> {
        self.written.keys().copied().collect()
    }

    /// Every written sample, sorted (row-major) so write-back order never depends on hashing.
    pub fn samples(&self) -> Vec<IVec2> {
        let mut out: Vec<IVec2> = self
            .written
            .iter()
            .flat_map(|(tile, set)| {
                let origin = self.tile_origin(*tile);
                set.iter().map(move |p| (origin + *p).as_ivec2())
            })
            .collect();
        out.sort_unstable_by_key(|p| (p.y, p.x));
        out
    }

    /// The rectangle covering every written sample, clamped to a `width × height` grid.
    pub fn dirty_rect(&self, width: u32, height: u32) -> DirtyRect {
        let mut rect = DirtyRect {
            col_start: u32::MAX,
            col_end: 0,
            row_start: u32::MAX,
            row_end: 0,
        };
        for (tile, set) in &self.written {
            let origin = self.tile_origin(*tile);
            for p in set {
                let q = origin + *p;
                if q.x >= width || q.y >= height {
                    continue;
                }
                rect.col_start = rect.col_start.min(q.x);
                rect.col_end = rect.col_end.max(q.x + 1);
                rect.row_start = rect.row_start.min(q.y);
                rect.row_end = rect.row_end.max(q.y + 1);
            }
        }
        if rect.col_end == 0 {
            DirtyRect {
                col_start: 0,
                col_end: 0,
                row_start: 0,
                row_end: 0,
            }
        } else {
            rect
        }
    }

    /// Writes the latest value of every recorded sample through `write` (sorted order).
    pub fn apply<V: Copy>(&self, stroke: &super::StrokeData<V>, mut write: impl FnMut(IVec2, V)) {
        for p in self.samples() {
            if let Some(value) = stroke.latest_pixel_value(p) {
                write(p, *value);
            }
        }
    }
}

/// Merges `b` into `a`.
pub fn union_rect(a: DirtyRect, b: DirtyRect) -> DirtyRect {
    if a.is_empty() {
        return b;
    }
    if b.is_empty() {
        return a;
    }
    DirtyRect {
        col_start: a.col_start.min(b.col_start),
        col_end: a.col_end.max(b.col_end),
        row_start: a.row_start.min(b.row_start),
        row_end: a.row_end.max(b.row_end),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_mapping() {
        let chunks = StrokeChunks::new(8);
        assert_eq!(chunks.tile_of(IVec2::new(0, 0)), Some((0, 0)));
        assert_eq!(chunks.tile_of(IVec2::new(7, 7)), Some((0, 0)));
        assert_eq!(chunks.tile_of(IVec2::new(8, 3)), Some((1, 0)));
        assert_eq!(chunks.tile_of(IVec2::new(17, 31)), Some((2, 3)));
        assert_eq!(chunks.tile_of(IVec2::new(-1, 3)), None);
        assert_eq!(chunks.tile_origin((2, 3)), UVec2::new(16, 24));
    }

    #[test]
    fn writes_group_by_tile_and_clear_reuses() {
        let mut chunks = StrokeChunks::new(4);
        for p in [(0, 0), (3, 3), (4, 0), (9, 9), (9, 9)] {
            chunks.write(IVec2::new(p.0, p.1));
        }
        assert_eq!(chunks.count(), 5);
        assert_eq!(
            chunks.tiles().into_iter().collect::<Vec<_>>(),
            vec![(0, 0), (1, 0), (2, 2)]
        );
        assert_eq!(chunks.samples().len(), 4, "a repeated sample is stored once");
        let rect = chunks.dirty_rect(100, 100);
        assert_eq!(
            (rect.col_start, rect.col_end, rect.row_start, rect.row_end),
            (0, 10, 0, 10)
        );
        chunks.clear();
        assert_eq!(chunks.count(), 0);
        assert!(chunks.tiles().is_empty());
        assert!(chunks.dirty_rect(100, 100).is_empty());
    }

    #[test]
    fn samples_are_row_major() {
        let mut chunks = StrokeChunks::new(2);
        for p in [(5, 1), (0, 3), (1, 1), (0, 0)] {
            chunks.write(IVec2::new(p.0, p.1));
        }
        let got: Vec<(i32, i32)> = chunks.samples().iter().map(|p| (p.x, p.y)).collect();
        assert_eq!(got, vec![(0, 0), (1, 1), (5, 1), (0, 3)]);
    }

    #[test]
    fn union_of_rects() {
        let a = DirtyRect {
            col_start: 1,
            col_end: 3,
            row_start: 2,
            row_end: 4,
        };
        let b = DirtyRect {
            col_start: 5,
            col_end: 6,
            row_start: 0,
            row_end: 1,
        };
        let empty = DirtyRect {
            col_start: 0,
            col_end: 0,
            row_start: 0,
            row_end: 0,
        };
        assert_eq!(union_rect(a, empty), a);
        assert_eq!(union_rect(empty, b), b);
        let u = union_rect(a, b);
        assert_eq!((u.col_start, u.col_end, u.row_start, u.row_end), (1, 6, 0, 4));
    }
}
