// SPDX-License-Identifier: MPL-2.0
//! Dirty regions of a [`super::Draw2dList`] (P-06): the target is cut into tiles, every tile gets
//! a hash of the instances that touch it (in draw order), and a tile whose hash equals last
//! frame's keeps the retained UI target's pixels: no redraw and no upload. Changed tiles are
//! merged into runs per tile row (one scissored redraw per run).
//!
//! Behaviour derived from Direct Order 27's HUD overlay (`shell-regia/game/src/hud/overlay.rs`,
//! commit c9513623f, read as requirements only: tile hashes mark only painted tiles, an unchanged
//! paint uploads nothing, a change uploads only its tile, dirty runs merge per tile row). DO27
//! hashed painted pixels; this hashes the instances that will paint them, so an unchanged frame
//! costs one pass over the list and no raster at all.

use super::{Draw2dInstance, kind};

/// Default tile width in pixels (DO27's HUD overlay used 256).
pub const TILE_W: u32 = 256;
/// Default tile height in pixels (DO27's HUD overlay used 64).
pub const TILE_H: u32 = 64;
/// The hash of a tile no instance touches. A painted tile's hash is never this.
pub const EMPTY_TILE: u64 = 0;

/// A run of dirty tiles in one tile row: columns `start..end` of row `row`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DirtyRun {
    /// Tile row.
    pub row: u32,
    /// First dirty tile column.
    pub start: u32,
    /// One past the last dirty tile column.
    pub end: u32,
}

/// What the tiles did in the last update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Draw2dTileStats {
    /// Tiles of the target (columns × rows).
    pub tiles: u32,
    /// Tiles touched by at least one instance.
    pub painted_tiles: u32,
    /// Tiles redrawn in the last update (hash changed, or everything after an invalidation).
    pub dirty_tiles: u32,
    /// Runs the dirty tiles merged into (one scissored redraw each).
    pub dirty_runs: u32,
    /// Runs the painted tiles merged into (one scissored composite each).
    pub painted_runs: u32,
    /// Instances that touch a dirty tile (the ones the last redraw uploaded and drew).
    pub redrawn_instances: u32,
    /// Tiles redrawn since start.
    pub tiles_redrawn_total: u64,
    /// Updates that redrew nothing since start.
    pub clean_frames: u64,
}

/// The tile grid, last frame's tile hashes and this frame's dirty runs. Buffers are reused: a
/// steady update allocates nothing.
#[derive(Debug, Clone)]
pub struct Draw2dTiles {
    tile_w: u32,
    tile_h: u32,
    width: u32,
    height: u32,
    cols: u32,
    rows: u32,
    hashes: Vec<u64>,
    prev: Vec<u64>,
    /// Tile bounds `[c0, r0, c1, r1)` of each instance of the last update (empty when it touches
    /// nothing).
    bounds: Vec<[u32; 4]>,
    dirty: Vec<bool>,
    runs: Vec<DirtyRun>,
    painted_runs: Vec<DirtyRun>,
    invalid: bool,
    stats: Draw2dTileStats,
}

impl Default for Draw2dTiles {
    fn default() -> Self {
        Self::new(TILE_W, TILE_H)
    }
}

/// Multiply-xor-rotate word mix (change detection only, not a secure hash).
#[inline]
fn mix(h: u64, w: u64) -> u64 {
    const K: u64 = 0x9e37_79b9_7f4a_7c15;
    (h ^ w).wrapping_mul(K).rotate_left(27)
}

/// The hash of one instance's bytes.
fn instance_hash(i: &Draw2dInstance) -> u64 {
    let words: &[u32; 20] = bytemuck::cast_ref(i);
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for pair in words.as_chunks::<2>().0 {
        h = mix(h, u64::from(pair[0]) | (u64::from(pair[1]) << 32));
    }
    h
}

/// The pixels an instance can touch, `[x0, y0, x1, y1]` (the shader's quad plus its
/// anti-aliasing margin, clipped), or `None` when it touches nothing.
#[must_use]
pub fn instance_pixel_bounds(i: &Draw2dInstance) -> Option<[f32; 4]> {
    let k = i.params[0];
    let [mut x0, mut y0, mut x1, mut y1] = if k == kind::SEGMENT {
        let m = i.params[1] + 2.0;
        [
            i.geo[0].min(i.geo[2]) - m,
            i.geo[1].min(i.geo[3]) - m,
            i.geo[0].max(i.geo[2]) + m,
            i.geo[1].max(i.geo[3]) + m,
        ]
    } else {
        let m = if k == kind::BOX {
            i.params[2].max(1.0) * 0.5 + 2.0
        } else if k == kind::OUTLINE {
            2.0
        } else {
            1.0
        };
        [i.geo[0] - m, i.geo[1] - m, i.geo[2] + m, i.geo[3] + m]
    };
    x0 = x0.max(i.clip[0]);
    y0 = y0.max(i.clip[1]);
    x1 = x1.min(i.clip[2]);
    y1 = y1.min(i.clip[3]);
    // An empty, inverted or NaN rectangle, or a fully transparent box (a gradient box's second
    // colour lives in `uv`), paints nothing.
    let empty = !(x0 < x1 && y0 < y1);
    let invisible = k == kind::BOX && i.color[3] <= 0.0 && i.uv[3] <= 0.0;
    if empty || invisible {
        return None;
    }
    Some([x0, y0, x1, y1])
}

impl Draw2dTiles {
    /// A tracker with `tile_w` × `tile_h` pixel tiles (each at least 8).
    #[must_use]
    pub fn new(tile_w: u32, tile_h: u32) -> Self {
        Self {
            tile_w: tile_w.max(8),
            tile_h: tile_h.max(8),
            width: 0,
            height: 0,
            cols: 0,
            rows: 0,
            hashes: Vec::new(),
            prev: Vec::new(),
            bounds: Vec::new(),
            dirty: Vec::new(),
            runs: Vec::new(),
            painted_runs: Vec::new(),
            invalid: true,
            stats: Draw2dTileStats::default(),
        }
    }

    /// Tile size in pixels.
    #[must_use]
    pub fn tile_size(&self) -> (u32, u32) {
        (self.tile_w, self.tile_h)
    }

    /// Tile columns and rows of the last update.
    #[must_use]
    pub fn grid(&self) -> (u32, u32) {
        (self.cols, self.rows)
    }

    /// Forgets last frame's hashes: the next update redraws every tile (the retained target was
    /// recreated, an atlas page was recycled or an image slot changed).
    pub fn invalidate(&mut self) {
        self.invalid = true;
    }

    /// Hashes `instances` per tile of a `width` × `height` target and returns the runs of tiles
    /// whose hash changed since the last update (all of them after a resize or
    /// [`Self::invalidate`]).
    pub fn update(&mut self, instances: &[Draw2dInstance], width: u32, height: u32) -> &[DirtyRun] {
        let _span = tracing::trace_span!(target: "regia_quick", "draw2d_tiles").entered();
        if width != self.width || height != self.height {
            self.width = width;
            self.height = height;
            self.cols = width.div_ceil(self.tile_w);
            self.rows = height.div_ceil(self.tile_h);
            self.invalid = true;
        }
        let n = (self.cols * self.rows) as usize;
        // Both hash buffers hold a full grid before they start swapping, so the second frame
        // after a resize allocates nothing either.
        for v in [&mut self.hashes, &mut self.prev] {
            if v.capacity() < n {
                v.reserve_exact(n - v.len());
            }
        }
        std::mem::swap(&mut self.hashes, &mut self.prev);
        self.hashes.clear();
        self.hashes.resize(n, EMPTY_TILE);
        self.bounds.clear();
        let (tw, th) = (self.tile_w as f32, self.tile_h as f32);
        let (cols, rows) = (self.cols, self.rows);
        for inst in instances {
            let Some([x0, y0, x1, y1]) = instance_pixel_bounds(inst) else {
                self.bounds.push([0; 4]);
                continue;
            };
            let c0 = (x0.max(0.0) / tw).floor() as u32;
            let r0 = (y0.max(0.0) / th).floor() as u32;
            let c1 = ((x1.max(0.0) / tw).ceil() as u32).min(cols);
            let r1 = ((y1.max(0.0) / th).ceil() as u32).min(rows);
            if c0 >= c1 || r0 >= r1 {
                self.bounds.push([0; 4]);
                continue;
            }
            self.bounds.push([c0, r0, c1, r1]);
            let h = instance_hash(inst);
            for r in r0..r1 {
                let row = (r * cols) as usize;
                for c in c0..c1 {
                    let t = &mut self.hashes[row + c as usize];
                    *t = mix(*t, h) | 1;
                }
            }
        }
        self.dirty.clear();
        if self.invalid || self.prev.len() != n {
            self.dirty.resize(n, true);
            self.invalid = false;
        } else {
            self.dirty
                .extend(self.hashes.iter().zip(&self.prev).map(|(a, b)| a != b));
        }
        dirty_runs_into(cols, rows, |i| self.dirty[i], &mut self.runs);
        dirty_runs_into(
            cols,
            rows,
            |i| self.hashes[i] != EMPTY_TILE,
            &mut self.painted_runs,
        );
        let dirty_tiles = self.dirty.iter().filter(|d| **d).count() as u32;
        let redrawn = self.bounds.iter().filter(|b| self.touches_dirty(**b)).count() as u32;
        self.stats.tiles = n as u32;
        self.stats.painted_tiles = self.hashes.iter().filter(|h| **h != EMPTY_TILE).count() as u32;
        self.stats.dirty_tiles = dirty_tiles;
        self.stats.dirty_runs = self.runs.len() as u32;
        self.stats.painted_runs = self.painted_runs.len() as u32;
        self.stats.redrawn_instances = redrawn;
        self.stats.tiles_redrawn_total += u64::from(dirty_tiles);
        if dirty_tiles == 0 {
            self.stats.clean_frames += 1;
        }
        &self.runs
    }

    fn touches_dirty(&self, [c0, r0, c1, r1]: [u32; 4]) -> bool {
        (r0..r1).any(|r| (c0..c1).any(|c| self.dirty[(r * self.cols + c) as usize]))
    }

    /// Whether instance `index` of the last update touches a dirty tile (it must be redrawn).
    #[must_use]
    pub fn instance_is_dirty(&self, index: usize) -> bool {
        self.bounds.get(index).is_some_and(|b| self.touches_dirty(*b))
    }

    /// The dirty runs of the last update.
    #[must_use]
    pub fn runs(&self) -> &[DirtyRun] {
        &self.runs
    }

    /// The runs of painted tiles of the last update (tiles any instance touches, merged per tile
    /// row): the only parts of the retained target the composite has to blend.
    #[must_use]
    pub fn painted_runs(&self) -> &[DirtyRun] {
        &self.painted_runs
    }

    /// The tile hashes of the last update, row-major ([`EMPTY_TILE`] for an untouched tile).
    #[must_use]
    pub fn hashes(&self) -> &[u64] {
        &self.hashes
    }

    /// The pixel rectangle `[x, y, w, h]` of a run, clamped to the target.
    #[must_use]
    pub fn run_rect(&self, run: DirtyRun) -> [u32; 4] {
        let x = run.start * self.tile_w;
        let y = run.row * self.tile_h;
        let x1 = (run.end * self.tile_w).min(self.width);
        let y1 = ((run.row + 1) * self.tile_h).min(self.height);
        [x, y, x1.saturating_sub(x), y1.saturating_sub(y)]
    }

    /// What the last update did.
    #[must_use]
    pub fn stats(&self) -> Draw2dTileStats {
        self.stats
    }
}

/// Runs of dirty tiles per tile row of a `cols` × `rows` grid (row-major `dirty(index)`):
/// neighbours in a row merge, rows never merge.
pub fn dirty_runs_into(cols: u32, rows: u32, dirty: impl Fn(usize) -> bool, out: &mut Vec<DirtyRun>) {
    out.clear();
    for row in 0..rows {
        let mut c = 0;
        while c < cols {
            if !dirty((row * cols + c) as usize) {
                c += 1;
                continue;
            }
            let start = c;
            while c < cols && dirty((row * cols + c) as usize) {
                c += 1;
            }
            out.push(DirtyRun { row, start, end: c });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw2d::{Draw2dList, PxRect};
    use regia_core::math::{Color, Vec2};

    fn run(row: u32, start: u32, end: u32) -> DirtyRun {
        DirtyRun { row, start, end }
    }

    #[test]
    fn dirty_runs_merge_neighbours_per_tile_row() {
        let dirty = [true, true, false, true, false, false, false, true, true];
        let mut out = Vec::new();
        dirty_runs_into(3, 3, |i| dirty[i], &mut out);
        assert_eq!(out, vec![run(0, 0, 2), run(1, 0, 1), run(2, 1, 3)]);
        dirty_runs_into(3, 3, |_| true, &mut out);
        assert_eq!(
            out,
            vec![run(0, 0, 3), run(1, 0, 3), run(2, 0, 3)],
            "a full redraw: one run a row"
        );
        dirty_runs_into(3, 3, |_| false, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn tile_hashes_mark_only_painted_tiles() {
        let mut list = Draw2dList::new();
        list.begin_frame();
        // One small box in tile (2, 1) of a 600 x 130 target: columns of 256, rows of 64.
        list.push_box(PxRect::new(520.0, 90.0, 4.0, 4.0), Color::WHITE, 0.0);
        list.end_frame();
        let mut tiles = Draw2dTiles::default();
        tiles.update(list.instances(), 600, 130);
        assert_eq!(tiles.grid(), (3, 3));
        let marked: Vec<usize> = tiles
            .hashes()
            .iter()
            .enumerate()
            .filter(|(_, h)| **h != EMPTY_TILE)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(marked, vec![3 + 2]);
        assert_eq!(tiles.stats().painted_tiles, 1);
        assert_eq!(
            tiles.stats().dirty_tiles,
            9,
            "the first update redraws everything"
        );
    }

    #[test]
    fn an_unchanged_list_dirties_nothing_and_a_change_only_its_tile() {
        let mut list = Draw2dList::new();
        let mut tiles = Draw2dTiles::default();
        let frame = |list: &mut Draw2dList, readout_x: f32, blob: bool| {
            list.begin_frame();
            if blob {
                list.push_box(PxRect::new(300.0, 70.0, 2.0, 2.0), Color::WHITE, 0.0);
            }
            list.push_box(PxRect::new(readout_x, 140.0, 6.0, 6.0), Color::GREEN, 0.0);
            list.end_frame();
        };
        frame(&mut list, 800.0, true);
        tiles.update(list.instances(), 1024, 256);
        frame(&mut list, 800.0, true);
        assert!(
            tiles.update(list.instances(), 1024, 256).is_empty(),
            "same list: nothing"
        );
        assert_eq!(tiles.stats().redrawn_instances, 0);
        frame(&mut list, 801.0, true);
        assert_eq!(tiles.update(list.instances(), 1024, 256), [run(2, 3, 4)]);
        assert_eq!(tiles.stats().redrawn_instances, 1, "only the readout redraws");
        assert!(!tiles.instance_is_dirty(0) && tiles.instance_is_dirty(1));
        // The blob goes away: its tile is redrawn once (to clear it).
        frame(&mut list, 801.0, false);
        assert_eq!(tiles.update(list.instances(), 1024, 256), [run(1, 1, 2)]);
        assert_eq!(tiles.stats().redrawn_instances, 0, "nothing left to draw there");
        assert_eq!(tiles.stats().painted_tiles, 1);
    }

    #[test]
    fn clip_resize_and_invalidate() {
        let mut list = Draw2dList::new();
        list.begin_frame();
        list.set_clip(Some(PxRect::new(0.0, 0.0, 100.0, 50.0)));
        // A segment across the whole target, clipped to the top-left tile.
        list.push_segment(Vec2::new(0.0, 0.0), Vec2::new(1000.0, 250.0), 2.0, Color::WHITE);
        list.set_clip(None);
        list.push_box(PxRect::new(-50.0, -50.0, 10.0, 10.0), Color::WHITE, 0.0);
        list.end_frame();
        let mut tiles = Draw2dTiles::default();
        tiles.update(list.instances(), 1024, 256);
        assert_eq!(
            tiles.stats().painted_tiles,
            1,
            "clip keeps the segment in one tile"
        );
        assert!(!tiles.instance_is_dirty(1), "off-screen: touches no tile");
        assert!(tiles.update(list.instances(), 1024, 256).is_empty());
        assert_eq!(
            tiles.update(list.instances(), 1024, 320).len(),
            5,
            "resize: every row"
        );
        tiles.invalidate();
        assert_eq!(tiles.update(list.instances(), 1024, 320).len(), 5);
        assert!(tiles.update(list.instances(), 1024, 320).is_empty());
        assert_eq!(tiles.run_rect(run(4, 1, 4)), [256, 256, 768, 64]);
        assert_eq!(
            tiles.run_rect(run(4, 3, 5)),
            [768, 256, 256, 64],
            "clamped to the target"
        );
    }
}
