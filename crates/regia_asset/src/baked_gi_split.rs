// SPDX-License-Identifier: MPL-2.0
//! Split baked GI volumes (ADR-2618, ADR-357 plan item 1): a probe layout too large for one
//! portable storage binding ([`crate::baked_gi::PORTABLE_VOLUME_BYTES`]) is cut into up to [`MAX_SPLIT_PARTS`]
//! part volumes, each a complete `.fgiv` (format version 4) the renderer installs as one more
//! blended volume.
//!
//! ## The cut
//!
//! The parts tile the layout's XZ extent on a `tiles[0] × tiles[1]` grid. Cuts lie between two
//! terrain columns (a layout without a terrain layer gets a virtual column grid of its largest
//! brick spacing), so no terrain cell is ever cut, and a brick goes whole into every part that
//! may need it unless no such split fits (then it is cropped, below).
//!
//! - **Ownership.** Each terrain column and each brick has exactly one owner part: the columns
//!   between the part's cuts, and the bricks whose centre lies on them. [`VolumeSplit`] records
//!   the owned column range and every brick's owner, so a tool can count each probe once.
//! - **Band.** Neighbors overlap by one terrain cell, the band, between the last column one
//!   part owns and the first column the next owns. Each part also holds one guard column past
//!   the band, so its own lookup at any point of its rectangle (a normal-biased point one cell
//!   further in) reads the same probes as the unsplit volume would.
//! - **Bricks.** A part holds every brick whose box, grown by one brick spacing, reaches its
//!   rectangle grown by one terrain cell, in the unsplit order (the brick pick keeps the first of
//!   equal keys, so the order is part of the result).
//! - **Records.** A part's probe records are byte copies of the unsplit volume's: same encode
//!   exposure, same filtered SH, same relocation, so the band probes match on both sides.
//!
//! The runtime blends the parts across each band with the volume blend of ADR-1097: inside the
//! band one part's edge weight ramps down as the other's ramps up, the two summing to 1, and
//! since both read the same probes there the blend equals the unsplit lookup. An edge without a
//! neighbor (the map's own edge) does not fade.
//!
//! The tiling is a balanced partition by cumulative byte weight along each axis (quantile
//! cuts), each cut moved to the nearest column no brick reaches across (so parts meet along brick
//! boundaries where the layout has room), the smallest part count first. Original code.
//!
//! ## Cropped bricks (format version 5, ADR-3237)
//!
//! When no split that keeps bricks whole fits (one brick past the cap, a stack of bricks on one
//! spot), the bricks that cross a cut are cropped: each part holds whole cells of the brick over
//! its rectangle plus a margin of cells past each cut side, and owns the cells on its side of the
//! cut ([`SplitBrick::owned`]). The brick pick keeps the brick a point lies deepest in, and a crop
//! face can only lower that depth, so the margin is at least the deepest point of any overlapping
//! brick of the same class plus half a cell (`crop_margins`): inside the part's rectangle every
//! pick, coverage and interpolation cell is the unsplit one. Since the lookups agree there, the
//! band need not be a terrain cell: a cropped split cuts on a fine grid (the finest brick cell) and
//! its neighbors share a band one fine cell wide, so a cut through a tall brick costs a few of its
//! cells, not a whole terrain cell of them. Whole-brick splits are always tried first, so a layout
//! that split before splits as before.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::baked_gi::{
    BakedGiError, BakedGiVolume, DetailBrick, GI_COMPONENTS, META_RECORD_BYTES, MOMENT_TEXEL_BYTES,
    SH_RECORD_BYTES, TerrainLayer, VolumeLayout,
};

/// Most parts a split volume has: the renderer binds each part in a storage slot of its own and
/// has four (`regia_render::baked_gi::BAKED_GI_VOLUME_SLOTS`), so a minimum-spec device whose
/// binding holds one part installs all of them.
pub const MAX_SPLIT_PARTS: u32 = 4;
/// [`VolumeSplit::neighbors`] entry of an edge without a neighbor.
pub const NO_NEIGHBOR: u32 = u32::MAX;
/// Columns a part holds past its band on each inner edge (the normal-biased lookup's reach).
pub const SPLIT_GUARD_COLUMNS: u32 = 1;
/// Fewest columns a part owns along a cut axis (more than the band and the guard on both
/// sides, so the two bands of a middle part never meet).
pub const MIN_OWNED_COLUMNS: u32 = 4;

/// Fewest cells a cropped brick keeps past its part's rectangle on a cut side (ADR-3237): the
/// interpolation cell and the biased lookup point (`normal_bias + view_bias` up to one cell; the
/// defaults are 0.4). [`split_layout`] keeps more where an overlapping brick could otherwise win the
/// brick pick near the crop.
pub const CROP_MIN_MARGIN_CELLS: u32 = 2;

/// Edges of a part's rectangle, in [`VolumeSplit::band`] and [`VolumeSplit::neighbors`] order.
pub const SPLIT_EDGES: [&str; 4] = ["-x", "-z", "+x", "+z"];

/// One brick of a part ([`VolumeSplit::bricks`], in the part's brick order).
///
/// Format version 5 (ADR-3237) crops a brick that crosses a cut: the part's brick is a box of whole
/// cells of the unsplit brick (`crop_offset`, the part brick's own counts) and the part owns the
/// cells of it that lie in its columns (`owned`). A brick that is not cropped has a zero offset,
/// the unsplit counts and, in its owner part, every cell owned (none in the others).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitBrick {
    /// Index of the brick in the unsplit layout.
    pub source: u32,
    /// The brick's first probe in the unsplit layout.
    pub source_first: u32,
    /// Index of the part that owns the brick's centre (its centre lies in that part's columns).
    pub owner: u32,
    /// The unsplit brick's probe counts (x, y, z); the part brick's own counts when not cropped.
    pub source_counts: [u32; 3],
    /// The unsplit cell of the part brick's cell (0, 0, 0): `[0, 0, 0]` when not cropped.
    pub crop_offset: [u32; 3],
    /// The part brick's cells this part owns, local `[x0, z0, x1, z1]` (half-open, every y): each
    /// unsplit cell has exactly one owner part.
    pub owned: [u32; 4],
}

impl SplitBrick {
    /// The brick is a crop of a larger unsplit brick in this part.
    #[must_use]
    pub fn is_cropped(&self, part_brick: &DetailBrick) -> bool {
        self.crop_offset != [0; 3] || self.source_counts != part_brick.counts
    }

    /// Probes of the part brick this part owns.
    #[must_use]
    pub fn owned_probes(&self, part_brick: &DetailBrick) -> usize {
        let o = self.owned;
        (o[2].saturating_sub(o[0]) as usize)
            .saturating_mul(part_brick.counts[1] as usize)
            .saturating_mul(o[3].saturating_sub(o[1]) as usize)
    }
}

/// The split section of a part file (format version 4, module docs).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VolumeSplit {
    /// Identifies the set: every part of one split shares it (a hash of the bake and the
    /// layout), so parts of two different bakes never blend as neighbors.
    pub group: u64,
    /// This part's index, `tile[1] · tiles[0] + tile[0]`.
    pub index: u32,
    /// Parts in the set (2..=[`MAX_SPLIT_PARTS`]).
    pub count: u32,
    /// This part's tile (x, z).
    pub tile: [u32; 2],
    /// Tiles along x and z (`tiles[0] · tiles[1] == count`).
    pub tiles: [u32; 2],
    /// The part's blend rectangle, world `[min x, min z, max x, max z]`, metres: its owned
    /// columns and the bands on its inner edges. Outside it the part's weight is 0 at an edge
    /// that has a neighbor.
    pub rect: [f32; 4],
    /// The band width per edge ([`SPLIT_EDGES`] order), metres: the part's weight ramps from 1 to
    /// 0 over it at the rectangle's edge. 0 on an outer edge (no neighbor, no fade).
    pub band: [f32; 4],
    /// The neighbor part per edge ([`SPLIT_EDGES`] order), [`NO_NEIGHBOR`] on an outer edge.
    pub neighbors: [u32; 4],
    /// Probes of the unsplit layout.
    pub source_probes: u32,
    /// Terrain columns of the unsplit layout (x, z); `[0, 0]` without a terrain layer.
    pub source_columns: [u32; 2],
    /// The unsplit column of this part's column (0, 0).
    pub terrain_offset: [u32; 2],
    /// The columns this part owns, local `[i0, j0, i1, j1]` (half-open).
    pub owned_columns: [u32; 4],
    /// Per brick of the part: its unsplit index and first probe, its owner, its crop and the
    /// cells this part owns.
    pub bricks: Vec<SplitBrick>,
}

impl VolumeSplit {
    /// The neighbor across `edge` ([`SPLIT_EDGES`] order), `None` on an outer edge.
    #[must_use]
    pub fn neighbor(&self, edge: usize) -> Option<u32> {
        self.neighbors.get(edge).copied().filter(|n| *n != NO_NEIGHBOR)
    }

    /// The unsplit index of this part's probe `local` in `layout` (the part's own layout),
    /// `None` past the end.
    #[must_use]
    pub fn source_probe(&self, layout: &VolumeLayout, local: usize) -> Option<usize> {
        let terrain = layout.terrain.as_ref().map_or(0, TerrainLayer::probe_count);
        if local < terrain {
            let t = layout.terrain.as_ref()?;
            let (i, j, l) = t.coords(local);
            let gi = (i + self.terrain_offset[0]) as usize;
            let gj = (j + self.terrain_offset[1]) as usize;
            return Some((gj * self.source_columns[0] as usize + gi) * t.levels.len() + l as usize);
        }
        layout.bricks.iter().zip(&self.bricks).find_map(|(b, s)| {
            let first = b.first_probe as usize;
            if local < first || local >= first + b.probe_count() {
                return None;
            }
            let (x, y, z) = b.coords(local - first);
            let at = |a: usize, c: u32| (c + s.crop_offset[a]) as usize;
            let (sx, sy) = (s.source_counts[0] as usize, s.source_counts[1] as usize);
            Some(s.source_first as usize + (at(2, z) * sy + at(1, y)) * sx + at(0, x))
        })
    }

    /// This part owns its probe `local` in `layout` (each unsplit probe has exactly one owner).
    #[must_use]
    pub fn owns_probe(&self, layout: &VolumeLayout, local: usize) -> bool {
        let terrain = layout.terrain.as_ref().map_or(0, TerrainLayer::probe_count);
        if local < terrain {
            let Some(t) = layout.terrain.as_ref() else {
                return false;
            };
            let (i, j, _) = t.coords(local);
            let o = self.owned_columns;
            return i >= o[0] && i < o[2] && j >= o[1] && j < o[3];
        }
        layout.bricks.iter().zip(&self.bricks).any(|(b, s)| {
            let first = b.first_probe as usize;
            if local < first || local >= first + b.probe_count() {
                return false;
            }
            let (x, _, z) = b.coords(local - first);
            let o = s.owned;
            x >= o[0] && x < o[2] && z >= o[1] && z < o[3]
        })
    }

    /// Probes this part owns.
    #[must_use]
    pub fn owned_probe_count(&self, layout: &VolumeLayout) -> usize {
        let o = self.owned_columns;
        let levels = layout.terrain.as_ref().map_or(0, |t| t.levels.len());
        let columns = (o[2].saturating_sub(o[0]) as usize) * (o[3].saturating_sub(o[1]) as usize);
        let bricks: usize = layout
            .bricks
            .iter()
            .zip(&self.bricks)
            .map(|(b, s)| s.owned_probes(b))
            .sum();
        columns * levels + bricks
    }

    /// Bricks of this part that are crops of a larger unsplit brick (format version 5).
    #[must_use]
    pub fn cropped_bricks(&self, layout: &VolumeLayout) -> usize {
        layout
            .bricks
            .iter()
            .zip(&self.bricks)
            .filter(|(b, s)| s.is_cropped(b))
            .count()
    }

    /// Checks the section against the part's own `layout`.
    ///
    /// # Errors
    ///
    /// [`BakedGiError::Layout`] naming the first inconsistency.
    pub fn validate(&self, layout: &VolumeLayout) -> Result<(), BakedGiError> {
        let bad = |m: String| Err(BakedGiError::Layout(format!("split section: {m}")));
        if !(2..=MAX_SPLIT_PARTS).contains(&self.count) || self.index >= self.count {
            return bad(format!(
                "part {} of {} (2..={MAX_SPLIT_PARTS} parts)",
                self.index, self.count
            ));
        }
        if u64::from(self.tiles[0]) * u64::from(self.tiles[1]) != u64::from(self.count)
            || self.tile[0] >= self.tiles[0]
            || self.tile[1] >= self.tiles[1]
            || u64::from(self.tile[1]) * u64::from(self.tiles[0]) + u64::from(self.tile[0])
                != u64::from(self.index)
        {
            return bad(format!(
                "tile {:?} of {:?} is not part {}",
                self.tile, self.tiles, self.index
            ));
        }
        if self.rect.iter().any(|v| !v.is_finite())
            || self.rect[0] > self.rect[2]
            || self.rect[1] > self.rect[3]
            || self.band.iter().any(|b| !(b.is_finite() && *b >= 0.0))
        {
            return bad("rectangle or band is not finite".into());
        }
        for (e, n) in self.neighbors.iter().enumerate() {
            if *n != NO_NEIGHBOR && (*n >= self.count || *n == self.index) {
                return bad(format!("neighbor {n} across {}", SPLIT_EDGES[e]));
            }
            // Two parts meeting without a band would not fade into each other.
            if *n != NO_NEIGHBOR && self.band[e] <= 0.0 {
                return bad(format!("no band toward neighbor {n} across {}", SPLIT_EDGES[e]));
            }
        }
        if self.bricks.len() != layout.bricks.len() || self.bricks.iter().any(|b| b.owner >= self.count) {
            return bad(format!(
                "{} brick records for {} bricks",
                self.bricks.len(),
                layout.bricks.len()
            ));
        }
        for (k, (b, s)) in layout.bricks.iter().zip(&self.bricks).enumerate() {
            // The crop lies inside the unsplit brick, the owned cells inside the crop, and the
            // unsplit brick among the unsplit probes (which bounds `source_probe`'s arithmetic
            // for a hostile section).
            let inside = (0..3).all(|a| {
                u64::from(s.crop_offset[a]) + u64::from(b.counts[a]) <= u64::from(s.source_counts[a])
            });
            let o = s.owned;
            let owned_fits = o[0] <= o[2] && o[2] <= b.counts[0] && o[1] <= o[3] && o[3] <= b.counts[2];
            let source_probes = s.source_counts.iter().fold(1u128, |acc, c| acc * u128::from(*c))
                + u128::from(s.source_first);
            if !inside || !owned_fits || source_probes > u128::from(self.source_probes) {
                return bad(format!(
                    "brick record {k} (unsplit brick {}) does not fit its brick",
                    s.source
                ));
            }
        }
        let o = self.owned_columns;
        match &layout.terrain {
            Some(t) => {
                let fits = |a: usize| {
                    o[a] <= o[a + 2]
                        && o[a + 2] <= t.counts[a]
                        && u64::from(self.terrain_offset[a]) + u64::from(t.counts[a])
                            <= u64::from(self.source_columns[a])
                };
                if !(fits(0) && fits(1)) {
                    return bad("owned columns past the terrain layer".into());
                }
                // The unsplit grid's probes are among the unsplit probes, which bounds the index
                // arithmetic of `source_probe` for a hostile section.
                let unsplit = u128::from(self.source_columns[0])
                    * u128::from(self.source_columns[1])
                    * t.levels.len() as u128;
                if unsplit > u128::from(self.source_probes) {
                    return bad(format!(
                        "{unsplit} unsplit terrain probes in a layout of {}",
                        self.source_probes
                    ));
                }
            }
            None if o != [0; 4] => return bad("owned columns without a terrain layer".into()),
            None => {}
        }
        Ok(())
    }
}

/// A split brick record as format version 4 stores it (no crop, ADR-2618).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SplitBrickV4 {
    source: u32,
    source_first: u32,
    owner: u32,
}

/// The split section as format version 4 stores it: [`VolumeSplit`] with [`SplitBrickV4`] records.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct VolumeSplitV4 {
    group: u64,
    index: u32,
    count: u32,
    tile: [u32; 2],
    tiles: [u32; 2],
    rect: [f32; 4],
    band: [f32; 4],
    neighbors: [u32; 4],
    source_probes: u32,
    source_columns: [u32; 2],
    terrain_offset: [u32; 2],
    owned_columns: [u32; 4],
    bricks: Vec<SplitBrickV4>,
}

impl VolumeSplitV4 {
    /// The version 5 section of a version 4 part whose bricks are `bricks`: no brick is cropped,
    /// and the owner part owns every cell of its bricks. A record without a brick gets zero counts,
    /// which [`VolumeSplit::validate`] refuses.
    pub(crate) fn upgrade(self, bricks: &[DetailBrick]) -> VolumeSplit {
        let records = self
            .bricks
            .iter()
            .enumerate()
            .map(|(k, r)| {
                let counts = bricks.get(k).map_or([0; 3], |b| b.counts);
                SplitBrick {
                    source: r.source,
                    source_first: r.source_first,
                    owner: r.owner,
                    source_counts: counts,
                    crop_offset: [0; 3],
                    owned: if r.owner == self.index {
                        [0, 0, counts[0], counts[2]]
                    } else {
                        [0; 4]
                    },
                }
            })
            .collect();
        VolumeSplit {
            group: self.group,
            index: self.index,
            count: self.count,
            tile: self.tile,
            tiles: self.tiles,
            rect: self.rect,
            band: self.band,
            neighbors: self.neighbors,
            source_probes: self.source_probes,
            source_columns: self.source_columns,
            terrain_offset: self.terrain_offset,
            owned_columns: self.owned_columns,
            bricks: records,
        }
    }

    /// `split` as version 4 stores it, `None` when a brick is cropped or its cells are not owned
    /// whole by its owner (version 4 cannot say so).
    pub(crate) fn of(split: &VolumeSplit, bricks: &[DetailBrick]) -> Option<Self> {
        let records = split
            .bricks
            .iter()
            .zip(bricks)
            .map(|(r, b)| {
                let whole = if r.owner == split.index {
                    [0, 0, b.counts[0], b.counts[2]]
                } else {
                    [0; 4]
                };
                (!r.is_cropped(b) && r.owned == whole).then_some(SplitBrickV4 {
                    source: r.source,
                    source_first: r.source_first,
                    owner: r.owner,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        (records.len() == split.bricks.len()).then_some(Self {
            group: split.group,
            index: split.index,
            count: split.count,
            tile: split.tile,
            tiles: split.tiles,
            rect: split.rect,
            band: split.band,
            neighbors: split.neighbors,
            source_probes: split.source_probes,
            source_columns: split.source_columns,
            terrain_offset: split.terrain_offset,
            owned_columns: split.owned_columns,
            bricks: records,
        })
    }
}

/// One part of a planned split ([`split_layout`]): its own layout (brick offsets assigned) and
/// its split section (`group` 0 until [`BakedGiVolume::split_parts`] sets it).
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutPart {
    /// The part's layout.
    pub layout: VolumeLayout,
    /// The part's split section.
    pub split: VolumeSplit,
}

impl LayoutPart {
    /// [`VolumeLayout::runtime_buffer_bytes`] of the part.
    #[must_use]
    pub fn runtime_buffer_bytes(&self) -> u64 {
        self.layout.runtime_buffer_bytes()
    }
}

/// Most columns a split grid may have along one axis: the column grid ([`Columns::of`]) and the
/// crop grid ([`fine_columns`]) alike. Each column costs a `u64` weight and a cut flag per try, so a
/// brick spacing the layout check accepts (finite and positive) but far finer than any bake makes
/// (1e-6 m over a map) would otherwise ask for billions of them. 2^20 columns is a 262 km map at the
/// 0.25 m of the finest brick a bake writes.
const MAX_SPLIT_COLUMNS: u32 = 1 << 20;

/// The column grid the cut runs on: the terrain layer's, or a virtual one over the bricks.
#[derive(Clone, Copy, Debug)]
struct Columns {
    origin: [f32; 2],
    spacing: [f32; 2],
    counts: [u32; 2],
}

impl Columns {
    fn of(layout: &VolumeLayout) -> Option<Self> {
        if let Some(t) = &layout.terrain {
            return (t.counts[0] > 0 && t.counts[1] > 0).then_some(Self {
                origin: t.origin,
                spacing: t.spacing,
                counts: t.counts,
            });
        }
        let mut lo = [f32::INFINITY; 2];
        let mut hi = [f32::NEG_INFINITY; 2];
        let mut spacing = [0.0f32; 2];
        for b in layout.bricks.iter().filter(|b| b.probe_count() > 0) {
            for (a, axis) in [(0usize, 0usize), (1, 2)] {
                lo[a] = lo[a].min(b.origin[axis]);
                hi[a] = hi[a].max(b.origin[axis] + (b.counts[axis] - 1) as f32 * b.spacing[axis]);
                spacing[a] = spacing[a].max(b.spacing[axis]);
            }
        }
        if !(lo[0] <= hi[0] && lo[1] <= hi[1] && spacing[0] > 0.0 && spacing[1] > 0.0) {
            return None;
        }
        let counts = [0, 1].map(|a| (((hi[a] - lo[a]) / spacing[a]).ceil() as u32).saturating_add(1));
        Some(Self {
            origin: lo,
            spacing,
            counts,
        })
    }

    /// A layout error when the grid has more than [`MAX_SPLIT_COLUMNS`] columns along an axis,
    /// before anything is sized by its counts. `what` names the grid.
    fn check_size(&self, what: &str) -> Result<(), BakedGiError> {
        match (0..2).find(|&a| self.counts[a] > MAX_SPLIT_COLUMNS) {
            Some(a) => Err(BakedGiError::Layout(format!(
                "the {what} split grid has {} columns along {} (a {} m cell; at most {MAX_SPLIT_COLUMNS}): a brick spacing too fine to split",
                self.counts[a],
                ["x", "z"][a],
                self.spacing[a]
            ))),
            None => Ok(()),
        }
    }

    fn x(&self, a: usize, i: u32) -> f32 {
        self.origin[a] + i as f32 * self.spacing[a]
    }

    /// The cut line before column `k` along axis `a`: halfway between columns `k − 1` and `k`.
    fn line(&self, a: usize, k: u32) -> f32 {
        self.origin[a] + (k as f32 - 0.5) * self.spacing[a]
    }

    /// The column nearest world coordinate `v` along axis `a`, clamped to the grid.
    fn column(&self, a: usize, v: f32) -> u32 {
        let c = ((v - self.origin[a]) / self.spacing[a]).round();
        if c.is_nan() || c <= 0.0 {
            0
        } else {
            (c as u32).min(self.counts[a] - 1)
        }
    }
}

/// Byte weight of each column along axis `a` (terrain probes plus the bricks centred on it).
fn axis_weights(layout: &VolumeLayout, cols: &Columns, a: usize, per_probe: u64) -> Vec<u64> {
    let mut w = vec![0u64; cols.counts[a] as usize];
    if let Some(t) = &layout.terrain {
        let per_column = u64::from(cols.counts[1 - a]) * t.levels.len() as u64 * per_probe;
        w.iter_mut().for_each(|v| *v += per_column);
    }
    for b in &layout.bricks {
        let c = b.centre();
        let axis = if a == 0 { 0 } else { 2 };
        w[cols.column(a, c[axis]) as usize] += b.probe_count() as u64 * per_probe;
    }
    w
}

/// The grid a cropped split cuts on (ADR-3237): the layout's column grid refined to the finest
/// brick cell along each axis (never coarser than a terrain cell), over the same extent. A crop
/// may cut anywhere a brick has a cell boundary, so the cut need not wait for the next terrain
/// column (8 m on a typical map, a whole slab of a tall brick).
fn fine_columns(layout: &VolumeLayout, cols: &Columns) -> Columns {
    let mut spacing = cols.spacing;
    for b in layout.bricks.iter().filter(|b| b.probe_count() > 0) {
        spacing[0] = spacing[0].min(b.spacing[0]);
        spacing[1] = spacing[1].min(b.spacing[2]);
    }
    let counts = [0, 1].map(|a| {
        let extent = cols.counts[a].saturating_sub(1) as f32 * cols.spacing[a];
        ((extent / spacing[a]).round() as u32).saturating_add(1)
    });
    Columns {
        origin: cols.origin,
        spacing,
        counts,
    }
}

/// Byte weight of each column of `fine` along axis `a`: every terrain column's probes at the
/// fine column nearest it, and every brick's bytes spread over the fine columns of its cells (a
/// cropped split cuts through bricks, so a long brick weighs along its length).
fn fine_weights(layout: &VolumeLayout, cols: &Columns, fine: &Columns, a: usize, per_probe: u64) -> Vec<u64> {
    let mut w = vec![0u64; fine.counts[a] as usize];
    if let Some(t) = &layout.terrain {
        let per_column = u64::from(cols.counts[1 - a]) * t.levels.len() as u64 * per_probe;
        for c in 0..cols.counts[a] {
            w[fine.column(a, cols.x(a, c)) as usize] += per_column;
        }
    }
    let axis = if a == 0 { 0 } else { 2 };
    for b in layout.bricks.iter().filter(|b| b.probe_count() > 0) {
        let slab = (b.probe_count() / b.counts[axis] as usize) as u64 * per_probe;
        for c in 0..b.counts[axis] {
            let x = b.origin[axis] + c as f32 * b.spacing[axis];
            w[fine.column(a, x) as usize] += slab;
        }
    }
    w
}

/// The deepest any lookup point sits in brick `b`, in its cells (the brick pick's largest key
/// before the interior priority): half its shortest probe span.
fn deepest_inset(b: &DetailBrick) -> f32 {
    (0..3)
        .map(|a| 0.5 * b.counts[a].saturating_sub(1) as f32)
        .fold(f32::INFINITY, f32::min)
}

/// Per brick, the cells it keeps past its part's window when a cut crops it (ADR-3237).
///
/// The brick pick keeps the brick whose key (the point's depth in it, in its own cells, plus a
/// priority for an interior brick) is largest. A crop face `d` cells from a point can only lower
/// the cropped brick's depth there to `d`, so the pick is unchanged at every point of the part's
/// rectangle when `d` beats the deepest key any other brick of the same class (interior or not;
/// across classes the priority decides) that overlaps it can have. A lookup point moves at most half
/// a cell (the interior test's push along the normal), so the margin past the rectangle is that
/// deepest key plus half a cell, rounded up to the next whole cell, and at least
/// [`CROP_MIN_MARGIN_CELLS`].
fn crop_margins(layout: &VolumeLayout) -> Vec<u32> {
    let span = |b: &DetailBrick, a: usize, grow: f32| {
        (
            b.origin[a] - grow,
            b.origin[a] + b.counts[a].saturating_sub(1) as f32 * b.spacing[a] + grow,
        )
    };
    layout
        .bricks
        .iter()
        .enumerate()
        .map(|(k, b)| {
            let worst = layout
                .bricks
                .iter()
                .enumerate()
                .filter(|(j, c)| {
                    *j != k
                        && c.probe_count() > 0
                        && c.is_interior() == b.is_interior()
                        && (0..3).all(|a| {
                            let (b0, b1) = span(b, a, b.spacing[a]);
                            let (c0, c1) = span(c, a, 0.0);
                            c1 >= b0 && c0 <= b1
                        })
                })
                .map(|(_, c)| deepest_inset(c))
                .fold(0.0f32, f32::max);
            CROP_MIN_MARGIN_CELLS.max((worst + 0.5).floor() as u32 + 1)
        })
        .collect()
}

/// Per cut position `c` along axis `a` (the cut between columns `c − 1` and `c`, `0..=n`): a
/// detail brick reaches across it, so a cut there would put the brick in both neighbors (its box
/// grown by one brick spacing meets the band grown by one terrain cell each side).
fn blocked_cuts(layout: &VolumeLayout, cols: &Columns, a: usize) -> Vec<bool> {
    let n = cols.counts[a];
    let mut blocked = vec![false; n as usize + 1];
    let axis = if a == 0 { 0 } else { 2 };
    let s = cols.spacing[a];
    for b in layout.bricks.iter().filter(|b| b.probe_count() > 0) {
        let lo = b.origin[axis] - b.spacing[axis];
        let hi = b.origin[axis] + (b.counts[axis] - 1) as f32 * b.spacing[axis] + b.spacing[axis];
        // [x(c - 1) - s, x(c) + s] meets [lo, hi]  <=>  x(c) >= lo - s and x(c) <= hi + 2s.
        let first = ((lo - s - cols.origin[a]) / s).ceil();
        let last = ((hi + 2.0 * s - cols.origin[a]) / s).floor();
        if !(first.is_finite() && last.is_finite()) || last < 0.0 {
            continue;
        }
        let first = first.max(0.0) as u32;
        let last = (last as u32).min(n);
        for c in first..=last {
            blocked[c as usize] = true;
        }
    }
    blocked
}

/// `tiles` cuts along one axis: column indices `0 = c0 < c1 < … < c_tiles = n`, each tile owning
/// at least [`MIN_OWNED_COLUMNS`] (any number for an uncut axis), placed at the quantiles of the
/// cumulative weight and moved to the nearest position no brick reaches across (`blocked`), so
/// the parts meet along brick boundaries where the layout has room. `None` when the axis is too
/// short.
fn quantile_cuts(weights: &[u64], tiles: u32, blocked: &[bool]) -> Option<Vec<u32>> {
    let n = weights.len() as u32;
    if tiles == 1 && n > 0 {
        // An uncut axis: one tile owns every column, however few.
        return Some(vec![0, n]);
    }
    if tiles == 0 || n < tiles * MIN_OWNED_COLUMNS {
        return None;
    }
    let total: u64 = weights.iter().sum::<u64>().max(1);
    let mut cuts = vec![0u32];
    let mut acc = 0u64;
    let mut column = 0u32;
    for k in 1..tiles {
        let target = total * u64::from(k) / u64::from(tiles);
        while column < n && acc + weights[column as usize] <= target {
            acc += weights[column as usize];
            column += 1;
        }
        let prev = *cuts.last().unwrap_or(&0);
        let low = prev + MIN_OWNED_COLUMNS;
        let high = n - (tiles - k) * MIN_OWNED_COLUMNS;
        let want = column.clamp(low, high);
        let free = |c: u32| !blocked.get(c as usize).copied().unwrap_or(false);
        // The nearest free position within the limits, else the quantile itself.
        let cut = (0..=high - low)
            .flat_map(|d| [want.checked_sub(d), want.checked_add(d)])
            .flatten()
            .find(|&c| c >= low && c <= high && free(c))
            .unwrap_or(want);
        cuts.push(cut);
        // Re-sync the running sum with the clamped cut.
        acc = weights[..cut as usize].iter().sum();
        column = cut;
    }
    cuts.push(n);
    Some(cuts)
}

/// What a cropped split cuts on: the fine grid ([`fine_columns`]), the layout's own column grid
/// (the terrain layer's) and each brick's margin ([`crop_margins`]).
struct CropGrid<'a> {
    fine: Columns,
    cols: Columns,
    margins: &'a [u32],
}

/// Moves the inner cuts of a cropped split to lower its largest part (ADR-3237). The byte quantiles
/// of each axis alone balance the parts poorly where heavy bricks share a column range (a tall
/// brick beside a stack), so each inner cut is shifted by halving steps while that lowers the largest
/// part's bytes, keeping [`MIN_OWNED_COLUMNS`] fine columns per tile. Stops once the largest part is
/// at most `cap` or no step helps; deterministic (fixed order, strict improvement).
fn balance_cuts(
    layout: &VolumeLayout,
    grid: &CropGrid<'_>,
    tiles: [u32; 2],
    mut cuts: [Vec<u32>; 2],
    cap: u64,
) -> [Vec<u32>; 2] {
    let largest = |cuts: &[Vec<u32>; 2]| {
        cut_cropped(layout, grid, tiles, [&cuts[0], &cuts[1]])
            .iter()
            .map(LayoutPart::runtime_buffer_bytes)
            .max()
            .unwrap_or(0)
    };
    let mut best = largest(&cuts);
    let mut step = grid.fine.counts[0].max(grid.fine.counts[1]) / 4;
    while step > 0 && best > cap {
        let mut improved = false;
        for a in 0..2 {
            for k in 1..cuts[a].len().saturating_sub(1) {
                for delta in [-i64::from(step), i64::from(step)] {
                    let at = i64::from(cuts[a][k]) + delta;
                    let low = i64::from(cuts[a][k - 1] + MIN_OWNED_COLUMNS);
                    let high = i64::from(cuts[a][k + 1]) - i64::from(MIN_OWNED_COLUMNS);
                    if at < low || at > high {
                        continue;
                    }
                    let mut trial = cuts.clone();
                    trial[a][k] = at as u32;
                    let bytes = largest(&trial);
                    if bytes < best {
                        best = bytes;
                        cuts = trial;
                        improved = true;
                    }
                }
            }
        }
        if !improved {
            step /= 2;
        }
    }
    cuts
}

/// The tile index along an axis holding column `c`.
fn tile_of(cuts: &[u32], c: u32) -> u32 {
    cuts.windows(2)
        .position(|w| c >= w[0] && c < w[1])
        .unwrap_or(cuts.len().saturating_sub(2)) as u32
}

/// The part grids to try for `n` parts: `(tiles x, tiles z)`, cutting the longer axis first.
fn grids(n: u32, cols: &Columns) -> Vec<[u32; 2]> {
    let mut out: Vec<[u32; 2]> = (1..=n)
        .filter(|tx| n.is_multiple_of(*tx))
        .map(|tx| [tx, n / tx])
        .collect();
    let extent = [0, 1].map(|a| cols.counts[a] as f32 * cols.spacing[a]);
    let long = usize::from(extent[1] > extent[0]);
    // Most tiles along the longer axis first (a stable sort: ties keep the order).
    out.sort_by_key(|g| std::cmp::Reverse(g[long]));
    out
}

/// Cuts `layout` into 2..=[`MAX_SPLIT_PARTS`] parts of at most `cap` runtime bytes each (module
/// docs): the fewest parts that fit. `cap` is [`crate::baked_gi::PORTABLE_VOLUME_BYTES`] for a bake; tests pass a
/// lower one to force a small layout to split.
///
/// # Errors
///
/// [`BakedGiError::Layout`] when the layout is invalid, has no extent, has a brick spacing so fine
/// that its split grid passes 2^20 columns along an axis (`MAX_SPLIT_COLUMNS`), or does not fit in
/// [`MAX_SPLIT_PARTS`] parts of `cap` (a single brick above the cap, a map too large).
pub fn split_layout(layout: &VolumeLayout, cap: u64) -> Result<Vec<LayoutPart>, BakedGiError> {
    layout.validate()?;
    let cols = Columns::of(layout)
        .ok_or_else(|| BakedGiError::Layout("a split needs a terrain layer or a brick with probes".into()))?;
    cols.check_size("column")?;
    let texels = u64::from(layout.moment_resolution).pow(2);
    let per_probe =
        (META_RECORD_BYTES + GI_COMPONENTS * SH_RECORD_BYTES) as u64 + texels * MOMENT_TEXEL_BYTES as u64;
    let weights = [0, 1].map(|a| axis_weights(layout, &cols, a, per_probe));
    // Brick-boundary cuts first; when they leave a part too large, the plain quantile cuts of
    // the same grid (a brick across a cut then goes into both parts).
    let blocked = [0, 1].map(|a| blocked_cuts(layout, &cols, a));
    let free = [0, 1].map(|a| vec![false; cols.counts[a] as usize + 1]);
    let mut best: Option<u64> = None;
    let mut fits = |parts: &[LayoutPart]| {
        let largest = parts
            .iter()
            .map(LayoutPart::runtime_buffer_bytes)
            .max()
            .unwrap_or(0);
        best = Some(best.map_or(largest, |b| b.min(largest)));
        largest <= cap && parts.iter().all(|p| p.layout.validate().is_ok())
    };
    for n in 2..=MAX_SPLIT_PARTS {
        for tiles in grids(n, &cols) {
            for avoid in [&blocked, &free] {
                let (Some(cx), Some(cz)) = (
                    quantile_cuts(&weights[0], tiles[0], &avoid[0]),
                    quantile_cuts(&weights[1], tiles[1], &avoid[1]),
                ) else {
                    continue;
                };
                let parts = cut(layout, &cols, tiles, [&cx, &cz]);
                if fits(&parts) {
                    return Ok(parts);
                }
            }
        }
    }
    // Every split that keeps bricks whole is too large: crop the bricks that cross a cut
    // (ADR-3237, format version 5), the fewest parts first, cut at the quantiles of the bytes
    // with each brick spread along its length.
    let fine = fine_columns(layout, &cols);
    fine.check_size("crop")?;
    let margins = crop_margins(layout);
    let grid = CropGrid {
        fine,
        cols,
        margins: &margins,
    };
    let spread = [0, 1].map(|a| fine_weights(layout, &cols, &grid.fine, a, per_probe));
    let unblocked = [0, 1].map(|a| vec![false; grid.fine.counts[a] as usize + 1]);
    for n in 2..=MAX_SPLIT_PARTS {
        for tiles in grids(n, &cols) {
            let (Some(cx), Some(cz)) = (
                quantile_cuts(&spread[0], tiles[0], &unblocked[0]),
                quantile_cuts(&spread[1], tiles[1], &unblocked[1]),
            ) else {
                continue;
            };
            let cuts = balance_cuts(layout, &grid, tiles, [cx, cz], cap);
            let parts = cut_cropped(layout, &grid, tiles, [&cuts[0], &cuts[1]]);
            if fits(&parts) {
                return Ok(parts);
            }
        }
    }
    let mib = |b: u64| b as f64 / f64::from(1u32 << 20);
    Err(BakedGiError::Layout(format!(
        "the layout ({:.1} MiB) does not split into {MAX_SPLIT_PARTS} parts of at most {:.1} MiB{}",
        mib(layout.runtime_buffer_bytes()),
        mib(cap),
        best.map_or_else(String::new, |b| format!(
            " (the best split's largest part is {:.1} MiB)",
            mib(b)
        ))
    )))
}

/// The cells `[first, last]` of a brick along world axis `axis` (`origin`, `spacing`, `count`)
/// that a part whose window is `lo..hi` keeps with `margin` cells past it (an infinite bound does
/// not crop); `None` when the window misses the brick.
fn crop_range(
    origin: f32,
    spacing: f32,
    count: u32,
    (lo, hi): (f32, f32),
    margin: u32,
) -> Option<(u32, u32)> {
    let last_cell = count.checked_sub(1)?;
    let first = if lo.is_finite() {
        let c = ((lo - origin) / spacing).floor() - margin as f32;
        if c.is_nan() || c <= 0.0 {
            0
        } else {
            (c as u32).min(count)
        }
    } else {
        0
    };
    let last = if hi.is_finite() {
        let c = ((hi - origin) / spacing).ceil() + margin as f32;
        if c.is_nan() || c < 0.0 {
            return None;
        }
        (c as u32).min(last_cell)
    } else {
        last_cell
    };
    (first <= last).then_some((first, last))
}

/// The parts of `layout` for the grid `tiles` and the per-axis column cuts, every brick whole: a
/// brick goes into every part it reaches and its centre's part owns it.
fn cut(layout: &VolumeLayout, cols: &Columns, tiles: [u32; 2], cuts: [&[u32]; 2]) -> Vec<LayoutPart> {
    let count = tiles[0] * tiles[1];
    let owner_of = |b: &DetailBrick| {
        let c = b.centre();
        let tx = tile_of(cuts[0], cols.column(0, c[0]));
        let tz = tile_of(cuts[1], cols.column(1, c[2]));
        tz * tiles[0] + tx
    };
    let owners: Vec<u32> = layout.bricks.iter().map(owner_of).collect();
    let mut parts = Vec::with_capacity(count as usize);
    for tz in 0..tiles[1] {
        for tx in 0..tiles[0] {
            let tile = [tx, tz];
            let index = tz * tiles[0] + tx;
            let mut stored = [0u32; 4];
            let mut owned = [0u32; 4];
            let mut rect = [0.0f32; 4];
            let mut band = [0.0f32; 4];
            let mut grow = [0.0f32; 4];
            for a in 0..2 {
                let (c0, c1) = (cuts[a][tile[a] as usize], cuts[a][tile[a] as usize + 1]);
                let n = cols.counts[a];
                let inner_lo = tile[a] > 0;
                let inner_hi = tile[a] + 1 < tiles[a];
                stored[a] = if inner_lo { c0 - 1 - SPLIT_GUARD_COLUMNS } else { 0 };
                stored[a + 2] = if inner_hi {
                    (c1 + 1 + SPLIT_GUARD_COLUMNS).min(n)
                } else {
                    n
                };
                owned[a] = c0 - stored[a];
                owned[a + 2] = c1 - stored[a];
                rect[a] = cols.x(a, if inner_lo { c0 - 1 } else { 0 });
                rect[a + 2] = cols.x(a, if inner_hi { c1 } else { n - 1 });
                band[a] = if inner_lo { cols.spacing[a] } else { 0.0 };
                band[a + 2] = if inner_hi { cols.spacing[a] } else { 0.0 };
                grow[a] = if inner_lo { cols.spacing[a] } else { f32::INFINITY };
                grow[a + 2] = if inner_hi { cols.spacing[a] } else { f32::INFINITY };
            }
            let terrain = layout.terrain.as_ref().map(|t| {
                let (i0, j0, i1, j1) = (stored[0], stored[1], stored[2], stored[3]);
                let nx = t.counts[0] as usize;
                let mut ground = Vec::with_capacity(((i1 - i0) * (j1 - j0)) as usize);
                for j in j0..j1 {
                    let row = j as usize * nx;
                    ground.extend_from_slice(&t.ground[row + i0 as usize..row + i1 as usize]);
                }
                TerrainLayer {
                    origin: [cols.x(0, i0), cols.x(1, j0)],
                    spacing: t.spacing,
                    counts: [i1 - i0, j1 - j0],
                    levels: t.levels.clone(),
                    ground,
                }
            });
            let lo = [rect[0] - grow[0], rect[1] - grow[1]];
            let hi = [rect[2] + grow[2], rect[3] + grow[3]];
            let mut bricks = Vec::new();
            let mut records = Vec::new();
            for (k, b) in layout.bricks.iter().enumerate() {
                let reaches = [(0usize, 0usize), (1, 2)].iter().all(|&(a, axis)| {
                    let b_lo = b.origin[axis] - b.spacing[axis];
                    let b_hi = b.origin[axis]
                        + b.counts[axis].saturating_sub(1) as f32 * b.spacing[axis]
                        + b.spacing[axis];
                    b_hi >= lo[a] && b_lo <= hi[a]
                });
                if !(owners[k] == index || (reaches && b.probe_count() > 0)) {
                    continue;
                }
                bricks.push(*b);
                records.push(SplitBrick {
                    source: k as u32,
                    source_first: b.first_probe,
                    owner: owners[k],
                    source_counts: b.counts,
                    crop_offset: [0; 3],
                    owned: if owners[k] == index {
                        [0, 0, b.counts[0], b.counts[2]]
                    } else {
                        [0; 4]
                    },
                });
            }
            let neighbor = |dx: i64, dz: i64| {
                let (x, z) = (i64::from(tx) + dx, i64::from(tz) + dz);
                if x < 0 || z < 0 || x >= i64::from(tiles[0]) || z >= i64::from(tiles[1]) {
                    NO_NEIGHBOR
                } else {
                    (z as u32) * tiles[0] + x as u32
                }
            };
            let mut part_layout = VolumeLayout {
                terrain,
                bricks,
                moment_resolution: layout.moment_resolution,
                max_distance: layout.max_distance,
            };
            part_layout.assign_brick_offsets();
            let has_terrain = layout.terrain.is_some();
            parts.push(LayoutPart {
                layout: part_layout,
                split: VolumeSplit {
                    group: 0,
                    index,
                    count,
                    tile,
                    tiles,
                    rect,
                    band,
                    neighbors: [neighbor(-1, 0), neighbor(0, -1), neighbor(1, 0), neighbor(0, 1)],
                    source_probes: layout.probe_count() as u32,
                    source_columns: if has_terrain { cols.counts } else { [0, 0] },
                    terrain_offset: if has_terrain {
                        [stored[0], stored[1]]
                    } else {
                        [0, 0]
                    },
                    owned_columns: if has_terrain { owned } else { [0; 4] },
                    bricks: records,
                },
            });
        }
    }
    parts
}

/// The cells `[start, end)` of `count` cells from `origin` by `spacing` whose position `tile_at`
/// puts in `tile` (the tile never decreases along an axis, so they are contiguous).
fn cells_in_tile(
    origin: f32,
    spacing: f32,
    count: u32,
    tile: u32,
    tile_at: impl Fn(f32) -> u32,
) -> (u32, u32) {
    let at = |c: u32| tile_at(origin + c as f32 * spacing);
    let start = (0..count).find(|&c| at(c) >= tile).unwrap_or(count);
    let end = (start..count).find(|&c| at(c) > tile).unwrap_or(count);
    (start, end)
}

/// The parts of `layout` for the grid `tiles` and the per-axis cuts on the fine grid, cropping the
/// bricks that cross a cut (ADR-3237, module docs). A cut is the line halfway between two fine
/// columns; everything at or past it (terrain columns, brick cells, brick centres) belongs to the
/// next tile. Neighbors share a band one fine cell wide centred on the line: inside its rectangle
/// a part's lookup is the unsplit one, so the band's width does not change the light. Each part
/// keeps the terrain columns over its rectangle plus the lookup's reach and the guard, and each
/// brick's cells over its rectangle plus the brick's margin past each cut side.
fn cut_cropped(
    layout: &VolumeLayout,
    grid: &CropGrid<'_>,
    tiles: [u32; 2],
    cuts: [&[u32]; 2],
) -> Vec<LayoutPart> {
    let (fine, cols) = (&grid.fine, &grid.cols);
    let count = tiles[0] * tiles[1];
    let lines: [Vec<f32>; 2] = [0, 1].map(|a| {
        cuts[a][1..cuts[a].len().saturating_sub(1)]
            .iter()
            .map(|&k| fine.line(a, k))
            .collect()
    });
    let tile_at = |a: usize, x: f32| lines[a].iter().filter(|l| x >= **l).count() as u32;
    let owners: Vec<u32> = layout
        .bricks
        .iter()
        .map(|b| {
            let c = b.centre();
            tile_at(1, c[2]) * tiles[0] + tile_at(0, c[0])
        })
        .collect();
    let mut parts = Vec::with_capacity(count as usize);
    for tz in 0..tiles[1] {
        for tx in 0..tiles[0] {
            let tile = [tx, tz];
            let index = tz * tiles[0] + tx;
            let mut stored = [0u32; 4];
            let mut owned = [0u32; 4];
            let mut rect = [0.0f32; 4];
            let mut band = [0.0f32; 4];
            let mut grow = [f32::INFINITY; 4];
            for a in 0..2 {
                let (n, s, b) = (cols.counts[a], cols.spacing[a], fine.spacing[a]);
                let t = tile[a] as usize;
                let lo_line = t.checked_sub(1).and_then(|k| lines[a].get(k)).copied();
                let hi_line = lines[a].get(t).copied();
                rect[a] = lo_line.map_or(cols.x(a, 0), |l| l - 0.5 * b);
                rect[a + 2] = hi_line.map_or(cols.x(a, n - 1), |l| l + 0.5 * b);
                if lo_line.is_some() {
                    band[a] = b;
                    grow[a] = s;
                }
                if hi_line.is_some() {
                    band[a + 2] = b;
                    grow[a + 2] = s;
                }
                let (c0, c1) = cells_in_tile(cols.origin[a], s, n, tile[a], |x| tile_at(a, x));
                // The lookup reads the column under a biased point (up to a cell past the
                // rectangle) and the next one; the guard is one more.
                let reach = 2 + SPLIT_GUARD_COLUMNS;
                let first = ((rect[a] - cols.origin[a]) / s).floor();
                let last = ((rect[a + 2] - cols.origin[a]) / s).ceil();
                stored[a] = if lo_line.is_some() && first.is_finite() && first > reach as f32 {
                    (first as u32 - reach).min(c0)
                } else {
                    0
                };
                stored[a + 2] = if hi_line.is_some() && last.is_finite() && last >= 0.0 {
                    (last as u32)
                        .saturating_add(reach)
                        .saturating_add(1)
                        .min(n)
                        .max(c1)
                } else {
                    n
                };
                owned[a] = c0 - stored[a];
                owned[a + 2] = c1 - stored[a];
            }
            let terrain = layout.terrain.as_ref().map(|t| {
                let (i0, j0, i1, j1) = (stored[0], stored[1], stored[2], stored[3]);
                let nx = t.counts[0] as usize;
                let mut ground = Vec::with_capacity(((i1 - i0) * (j1 - j0)) as usize);
                for j in j0..j1 {
                    let row = j as usize * nx;
                    ground.extend_from_slice(&t.ground[row + i0 as usize..row + i1 as usize]);
                }
                TerrainLayer {
                    origin: [cols.x(0, i0), cols.x(1, j0)],
                    spacing: t.spacing,
                    counts: [i1 - i0, j1 - j0],
                    levels: t.levels.clone(),
                    ground,
                }
            });
            let lo = [rect[0] - grow[0], rect[1] - grow[1]];
            let hi = [rect[2] + grow[2], rect[3] + grow[3]];
            let mut bricks = Vec::new();
            let mut records = Vec::new();
            for (k, b) in layout.bricks.iter().enumerate() {
                let reaches = [(0usize, 0usize), (1, 2)].iter().all(|&(a, axis)| {
                    let b_lo = b.origin[axis] - b.spacing[axis];
                    let b_hi = b.origin[axis]
                        + b.counts[axis].saturating_sub(1) as f32 * b.spacing[axis]
                        + b.spacing[axis];
                    b_hi >= lo[a] && b_lo <= hi[a]
                });
                if !(owners[k] == index || (reaches && b.probe_count() > 0)) {
                    continue;
                }
                if b.probe_count() == 0 {
                    // Nothing to crop: whole, owned by its centre's part.
                    bricks.push(*b);
                    records.push(SplitBrick {
                        source: k as u32,
                        source_first: b.first_probe,
                        owner: owners[k],
                        source_counts: b.counts,
                        crop_offset: [0; 3],
                        owned: [0; 4],
                    });
                    continue;
                }
                let margin = grid.margins.get(k).copied().unwrap_or(CROP_MIN_MARGIN_CELLS);
                let own = [(0usize, 0usize), (1, 2)].map(|(a, axis)| {
                    cells_in_tile(b.origin[axis], b.spacing[axis], b.counts[axis], tile[a], |x| {
                        tile_at(a, x)
                    })
                });
                let owns = own.iter().all(|(s, e)| s < e);
                let mut keep = [(0u32, 0u32); 2];
                let mut empty = false;
                for (a, axis) in [(0usize, 0usize), (1, 2)] {
                    // The rectangle on an inner edge (the lookup points; the margin covers the
                    // biased point and the pick), unbounded on an outer one.
                    let window = (
                        if grow[a].is_finite() {
                            rect[a]
                        } else {
                            f32::NEG_INFINITY
                        },
                        if grow[a + 2].is_finite() {
                            rect[a + 2]
                        } else {
                            f32::INFINITY
                        },
                    );
                    let range = crop_range(b.origin[axis], b.spacing[axis], b.counts[axis], window, margin);
                    // The owned cells are inside the rectangle, so inside the window; keep them
                    // whatever the float rounding.
                    keep[a] = match (range, owns) {
                        (Some((first, last)), true) => (first.min(own[a].0), last.max(own[a].1 - 1)),
                        (Some(r), false) => r,
                        (None, true) => (own[a].0, own[a].1 - 1),
                        (None, false) => {
                            empty = true;
                            (0, 0)
                        }
                    };
                }
                if empty {
                    continue;
                }
                let crop_offset = [keep[0].0, 0, keep[1].0];
                let counts = [keep[0].1 - keep[0].0 + 1, b.counts[1], keep[1].1 - keep[1].0 + 1];
                let owned_cells = if owns {
                    [
                        own[0].0 - keep[0].0,
                        own[1].0 - keep[1].0,
                        own[0].1 - keep[0].0,
                        own[1].1 - keep[1].0,
                    ]
                } else {
                    [0; 4]
                };
                let mut part_brick = *b;
                if crop_offset != [0; 3] || counts != b.counts {
                    part_brick.counts = counts;
                    for a in [0, 2] {
                        part_brick.origin[a] = b.origin[a] + crop_offset[a] as f32 * b.spacing[a];
                    }
                }
                bricks.push(part_brick);
                records.push(SplitBrick {
                    source: k as u32,
                    source_first: b.first_probe,
                    owner: owners[k],
                    source_counts: b.counts,
                    crop_offset,
                    owned: owned_cells,
                });
            }
            let neighbor = |dx: i64, dz: i64| {
                let (x, z) = (i64::from(tx) + dx, i64::from(tz) + dz);
                if x < 0 || z < 0 || x >= i64::from(tiles[0]) || z >= i64::from(tiles[1]) {
                    NO_NEIGHBOR
                } else {
                    (z as u32) * tiles[0] + x as u32
                }
            };
            let mut part_layout = VolumeLayout {
                terrain,
                bricks,
                moment_resolution: layout.moment_resolution,
                max_distance: layout.max_distance,
            };
            part_layout.assign_brick_offsets();
            let has_terrain = layout.terrain.is_some();
            parts.push(LayoutPart {
                layout: part_layout,
                split: VolumeSplit {
                    group: 0,
                    index,
                    count,
                    tile,
                    tiles,
                    rect,
                    band,
                    neighbors: [neighbor(-1, 0), neighbor(0, -1), neighbor(1, 0), neighbor(0, 1)],
                    source_probes: layout.probe_count() as u32,
                    source_columns: if has_terrain { cols.counts } else { [0, 0] },
                    terrain_offset: if has_terrain {
                        [stored[0], stored[1]]
                    } else {
                        [0, 0]
                    },
                    owned_columns: if has_terrain { owned } else { [0; 4] },
                    bricks: records,
                },
            });
        }
    }
    parts
}

impl BakedGiVolume {
    /// A whole (unsplit) volume from encoded sections, as [`Self::meta_bytes`],
    /// [`Self::sh_bytes`] and [`Self::moment_bytes`] return them, with the encode `exposure` per
    /// component: for tools that move records between volumes, and tests that need a large
    /// volume without its full-precision probes.
    ///
    /// # Errors
    ///
    /// [`BakedGiError::Layout`] when the layout is invalid or a section's length does not match
    /// its probe count.
    pub fn from_sections(
        layout: &VolumeLayout,
        lights: crate::baked_gi::BakeLights,
        info: crate::baked_gi::BakeInfo,
        exposure: [f32; GI_COMPONENTS],
        meta: Vec<u8>,
        sh: Vec<u8>,
        moments: Vec<u8>,
    ) -> Result<Self, BakedGiError> {
        layout.validate()?;
        crate::baked_gi::check_exposure(exposure)?;
        let n = layout.probe_count();
        let texels = (layout.moment_resolution * layout.moment_resolution) as usize;
        if meta.len() != n * META_RECORD_BYTES
            || sh.len() != n * GI_COMPONENTS * SH_RECORD_BYTES
            || moments.len() != n * texels * MOMENT_TEXEL_BYTES
        {
            return Err(BakedGiError::Layout(format!(
                "sections do not match {n} probes of {texels} moment texels"
            )));
        }
        Ok(Self {
            terrain: layout.terrain.clone(),
            bricks: layout.bricks.clone(),
            lights,
            info,
            moment_resolution: layout.moment_resolution,
            max_distance: layout.max_distance,
            emitters: None,
            split: None,
            exposure,
            meta,
            sh,
            moments,
            grid_cache: Default::default(),
        })
    }

    /// The volume as parts of at most `cap` runtime bytes each ([`split_layout`]; `cap` is
    /// [`crate::baked_gi::PORTABLE_VOLUME_BYTES`] for a bake, lower in tests): one unchanged volume when it fits,
    /// else the parts, each probe record a byte copy of this volume's (module docs).
    ///
    /// # Errors
    ///
    /// The volume is already a part, or [`split_layout`] fails.
    pub fn split_parts(&self, cap: u64) -> Result<Vec<Self>, BakedGiError> {
        if self.split.is_some() {
            return Err(BakedGiError::Layout("the volume is already a split part".into()));
        }
        let layout = self.layout();
        if layout.runtime_buffer_bytes() <= cap {
            return Ok(vec![self.clone()]);
        }
        let parts = split_layout(&layout, cap)?;
        let group = self.split_group();
        let texels = (self.moment_resolution * self.moment_resolution) as usize;
        let sh_rec = GI_COMPONENTS * SH_RECORD_BYTES;
        let mom_rec = texels * MOMENT_TEXEL_BYTES;
        let mut out = Vec::with_capacity(parts.len());
        for mut part in parts {
            part.split.group = group;
            let n = part.layout.probe_count();
            let mut meta = Vec::with_capacity(n * META_RECORD_BYTES);
            let mut sh = Vec::with_capacity(n * sh_rec);
            let mut moments = Vec::with_capacity(n * mom_rec);
            for local in 0..n {
                let src = part
                    .split
                    .source_probe(&part.layout, local)
                    .filter(|s| *s < self.probe_count())
                    .ok_or_else(|| BakedGiError::Layout(format!("part probe {local} has no source")))?;
                meta.extend_from_slice(&self.meta[src * META_RECORD_BYTES..(src + 1) * META_RECORD_BYTES]);
                sh.extend_from_slice(&self.sh[src * sh_rec..(src + 1) * sh_rec]);
                moments.extend_from_slice(&self.moments[src * mom_rec..(src + 1) * mom_rec]);
            }
            out.push(Self {
                terrain: part.layout.terrain,
                bricks: part.layout.bricks,
                lights: self.lights,
                info: self.info,
                moment_resolution: self.moment_resolution,
                max_distance: self.max_distance,
                emitters: self.emitters.clone(),
                split: Some(part.split),
                exposure: self.exposure,
                meta,
                sh,
                moments,
                grid_cache: Default::default(),
            });
        }
        Ok(out)
    }

    /// The [`VolumeSplit::group`] the parts of this volume get: a hash of the bake settings, the
    /// light levels, the layout's shape and the encode exposure.
    #[must_use]
    pub fn split_group(&self) -> u64 {
        let mut h = blake3::Hasher::new();
        let i = self.info;
        for w in [
            i.seed,
            i.rays_per_probe,
            i.samples,
            i.max_bounces,
            self.moment_resolution,
        ] {
            h.update(&w.to_le_bytes());
        }
        h.update(&i.scene_hash.to_le_bytes());
        h.update(&(self.probe_count() as u64).to_le_bytes());
        h.update(&(self.bricks.len() as u64).to_le_bytes());
        for e in self.exposure {
            h.update(&e.to_le_bytes());
        }
        if let Some(t) = &self.terrain {
            for v in [t.origin[0], t.origin[1], t.spacing[0], t.spacing[1]] {
                h.update(&v.to_le_bytes());
            }
            h.update(&t.counts[0].to_le_bytes());
            h.update(&t.counts[1].to_le_bytes());
        }
        let bytes = h.finalize();
        let mut word = [0u8; 8];
        word.copy_from_slice(&bytes.as_bytes()[..8]);
        u64::from_le_bytes(word)
    }

    /// Reads the volume at `path` and, when it is part 0 of a split volume, every other part
    /// beside it ([`split_part_path`]), in index order. An unsplit volume reads as one.
    ///
    /// # Errors
    ///
    /// A part does not read, names another set (group, count or index), or `path` names a part
    /// other than 0.
    pub fn read_split_set(path: &Path) -> Result<Vec<Self>, BakedGiError> {
        let first = Self::read(path)?;
        let Some(split) = first.split.clone() else {
            return Ok(vec![first]);
        };
        if split.index != 0 {
            return Err(BakedGiError::Layout(format!(
                "{} is split part {}; name part 0 (the file without a .part suffix)",
                path.display(),
                split.index
            )));
        }
        let mut parts = vec![first];
        for k in 1..split.count {
            let at = split_part_path(path, k);
            let part = Self::read(&at)?;
            let ok = part
                .split
                .as_ref()
                .is_some_and(|s| s.group == split.group && s.count == split.count && s.index == k);
            if !ok {
                return Err(BakedGiError::Layout(format!(
                    "{} is not part {k} of the split volume {}",
                    at.display(),
                    path.display()
                )));
            }
            parts.push(part);
        }
        Ok(parts)
    }

    /// The unsplit volume a split set was cut from (`parts` as [`Self::read_split_set`] returns
    /// them; one whole volume returns a copy of it): the unsplit layout and every probe record
    /// from the part that owns it. `join_split_set(&v.split_parts(cap)?)` is `v` byte for byte,
    /// so tools that check or sample a whole bake (`regia bake-gi --check-volume`, `--sample`)
    /// read a split one as it was baked.
    ///
    /// # Errors
    ///
    /// [`BakedGiError::Layout`] when the parts are not one complete set (count, order, group),
    /// a section is inconsistent, or a probe has no owner or two.
    pub fn join_split_set(parts: &[Self]) -> Result<Self, BakedGiError> {
        let bad = |m: String| BakedGiError::Layout(format!("split set: {m}"));
        let first = parts.first().ok_or_else(|| bad("no parts".into()))?;
        let Some(head) = first.split.as_ref() else {
            return if parts.len() == 1 {
                Ok(first.clone())
            } else {
                Err(bad("a whole volume among parts".into()))
            };
        };
        if parts.len() != head.count as usize {
            return Err(bad(format!("{} parts of a set of {}", parts.len(), head.count)));
        }
        for (k, part) in parts.iter().enumerate() {
            let ok = part.split.as_ref().is_some_and(|s| {
                s.group == head.group
                    && s.count == head.count
                    && s.index == k as u32
                    && s.source_probes == head.source_probes
                    && s.source_columns == head.source_columns
            });
            if !ok || part.moment_resolution != first.moment_resolution {
                return Err(bad(format!("part {k} is not part {k} of this set")));
            }
            part.split
                .as_ref()
                .map_or(Ok(()), |s| s.validate(&part.layout()))?;
        }
        let stored_probes: usize = parts.iter().map(Self::probe_count).sum();
        let stored_bricks: usize = parts.iter().map(|p| p.bricks.len()).sum();
        // The unsplit layout: the terrain from part 0 (at column 0, 0) and every part's owned
        // columns, each brick from the part that owns it.
        let terrain = match (&first.terrain, head.source_columns) {
            (None, _) => None,
            (Some(t), cols) => {
                if head.terrain_offset != [0, 0] {
                    return Err(bad("part 0 does not start at column 0".into()));
                }
                // Each unsplit probe has an owner, so the unsplit layout holds at most the probes
                // of its parts: a section naming more is damaged, and must not size an allocation.
                let n = (cols[0] as usize)
                    .checked_mul(cols[1] as usize)
                    .filter(|n| n.saturating_mul(t.levels.len().max(1)) <= stored_probes)
                    .ok_or_else(|| bad("terrain larger than its parts hold".into()))?;
                let mut ground = vec![f32::NAN; n];
                for part in parts {
                    let (Some(pt), Some(s)) = (&part.terrain, &part.split) else {
                        return Err(bad("a part has no terrain".into()));
                    };
                    let o = s.owned_columns;
                    for j in o[1]..o[3] {
                        for i in o[0]..o[2] {
                            let (gi, gj) = (i + s.terrain_offset[0], j + s.terrain_offset[1]);
                            let at = (gj as usize)
                                .checked_mul(cols[0] as usize)
                                .and_then(|r| r.checked_add(gi as usize))
                                .filter(|&at| gi < cols[0] && at < n)
                                .ok_or_else(|| bad("owned column past the unsplit grid".into()))?;
                            let local = j as usize * pt.counts[0] as usize + i as usize;
                            ground[at] = *pt.ground.get(local).ok_or_else(|| bad("ground".into()))?;
                        }
                    }
                }
                Some(TerrainLayer {
                    origin: t.origin,
                    spacing: t.spacing,
                    counts: cols,
                    levels: t.levels.clone(),
                    ground,
                })
            }
        };
        let brick_count = parts
            .iter()
            .filter_map(|p| p.split.as_ref())
            .flat_map(|s| s.bricks.iter().map(|b| b.source as usize + 1))
            .max()
            .unwrap_or(0);
        if brick_count > stored_bricks {
            return Err(bad(format!(
                "a brick record names unsplit brick {}; the parts hold {stored_bricks}",
                brick_count - 1
            )));
        }
        // Each unsplit brick from a part that holds its first cell (a crop at offset 0, so its
        // origin is the unsplit one bit for bit) with the unsplit counts; every such copy agrees.
        let mut bricks: Vec<Option<DetailBrick>> = vec![None; brick_count];
        for part in parts {
            let Some(s) = part.split.as_ref() else { continue };
            for (b, r) in part.bricks.iter().zip(&s.bricks) {
                if r.crop_offset != [0; 3] {
                    continue;
                }
                let unsplit = DetailBrick {
                    counts: r.source_counts,
                    ..*b
                };
                let slot = &mut bricks[r.source as usize];
                match slot {
                    Some(seen) if !same_brick(seen, &unsplit) => {
                        return Err(bad(format!("brick {} differs between parts", r.source)));
                    }
                    Some(_) => {}
                    None => *slot = Some(unsplit),
                }
            }
        }
        let mut layout = VolumeLayout {
            terrain,
            bricks: bricks
                .into_iter()
                .enumerate()
                .map(|(i, b)| b.ok_or_else(|| bad(format!("brick {i} has no part at its first cell"))))
                .collect::<Result<_, _>>()?,
            moment_resolution: first.moment_resolution,
            max_distance: first.max_distance,
        };
        layout.assign_brick_offsets();
        layout.validate()?;
        let n = layout.probe_count();
        if n != head.source_probes as usize {
            return Err(bad(format!("{n} probes, the parts name {}", head.source_probes)));
        }
        let texels = (first.moment_resolution * first.moment_resolution) as usize;
        let sh_rec = GI_COMPONENTS * SH_RECORD_BYTES;
        let mom_rec = texels * MOMENT_TEXEL_BYTES;
        let mut meta = vec![0u8; n * META_RECORD_BYTES];
        let mut sh = vec![0u8; n * sh_rec];
        let mut moments = vec![0u8; n * mom_rec];
        let mut filled = vec![false; n];
        for part in parts {
            let Some(s) = part.split.as_ref() else { continue };
            let part_layout = part.layout();
            for local in 0..part.probe_count() {
                if !s.owns_probe(&part_layout, local) {
                    continue;
                }
                let src = s
                    .source_probe(&part_layout, local)
                    .filter(|&src| src < n && !filled[src])
                    .ok_or_else(|| bad(format!("part {} probe {local} has no free unsplit slot", s.index)))?;
                filled[src] = true;
                let copy = |to: &mut [u8], from: &[u8], rec: usize| {
                    to[src * rec..(src + 1) * rec].copy_from_slice(&from[local * rec..(local + 1) * rec]);
                };
                copy(&mut meta, &part.meta, META_RECORD_BYTES);
                copy(&mut sh, &part.sh, sh_rec);
                copy(&mut moments, &part.moments, mom_rec);
            }
        }
        if let Some(missing) = filled.iter().position(|f| !f) {
            return Err(bad(format!("unsplit probe {missing} has no owner")));
        }
        let mut whole = Self::from_sections(
            &layout,
            first.lights,
            first.info,
            first.exposure,
            meta,
            sh,
            moments,
        )?;
        whole.emitters = first.emitters.clone();
        Ok(whole)
    }
}

/// Two copies of one unsplit brick agree (geometry and flags; the first probe is the part's own).
fn same_brick(a: &DetailBrick, b: &DetailBrick) -> bool {
    a.origin.map(f32::to_bits) == b.origin.map(f32::to_bits)
        && a.spacing.map(f32::to_bits) == b.spacing.map(f32::to_bits)
        && a.counts == b.counts
        && a.flags == b.flags
}

/// The file of part `index` of the split volume whose part 0 is `path`: `path` itself for 0,
/// else `<stem>.part<index>.fgiv` beside it (`maps/bore.fgiv` → `maps/bore.part1.fgiv`).
#[must_use]
pub fn split_part_path(path: &Path, index: u32) -> PathBuf {
    if index == 0 {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    path.with_file_name(format!("{stem}.part{index}.fgiv"))
}

/// Writes `parts` (one volume, or a split set in index order) beside `path` ([`split_part_path`])
/// atomically (each through a temporary file and a rename) and removes stale part files of an
/// earlier, larger split. Parts 1.. are written before part 0, so a reader that sees the new
/// part 0 finds the new parts beside it. Returns the paths written, in index order.
///
/// # Errors
///
/// Encoding or file IO failed.
pub fn write_split_set(parts: &[BakedGiVolume], path: &Path) -> Result<Vec<PathBuf>, BakedGiError> {
    let written: Vec<PathBuf> = (0..parts.len())
        .map(|k| split_part_path(path, k as u32))
        .collect();
    for (part, at) in parts.iter().zip(&written).rev() {
        let tmp = at.with_extension("fgiv.tmp");
        std::fs::write(&tmp, part.to_bytes()?)?;
        std::fs::rename(&tmp, at)?;
    }
    for k in (parts.len() as u32).max(1)..=MAX_SPLIT_PARTS {
        let stale = split_part_path(path, k);
        if stale.exists() {
            std::fs::remove_file(&stale)?;
        }
    }
    Ok(written)
}

/// Field names of the `.fgiv` postcard header and its nested records, in order (the
/// `check-formats` schema fingerprint).
#[must_use]
pub fn baked_gi_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<crate::baked_gi::VolumeHeader>(),
        serde_field_names::<TerrainLayer>(),
        serde_field_names::<DetailBrick>(),
        serde_field_names::<crate::baked_gi::BakeLights>(),
        serde_field_names::<crate::baked_gi::BakeInfo>(),
        serde_field_names::<crate::baked_gi::SectionRef>(),
        serde_field_names::<crate::baked_gi::BakedEmitter>(),
        serde_field_names::<VolumeSplit>(),
        serde_field_names::<SplitBrick>(),
    ])
}

/// The versions a `.fgiv` migration step leaves (`from` side): 1 to 4, all to the current one
/// ([`migrate_baked_gi_file`]).
#[must_use]
pub fn baked_gi_migration_sources() -> Vec<u32> {
    vec![1, 2, 3, 4]
}

/// The format version of a `.fgiv` file's bytes.
///
/// # Errors
///
/// [`BakedGiError::BadMagic`] when the bytes are not a `.fgiv`.
pub fn baked_gi_version(bytes: &[u8]) -> Result<u32, BakedGiError> {
    if bytes.len() < 8 || bytes[0..4] != crate::baked_gi::BAKED_GI_MAGIC {
        return Err(BakedGiError::BadMagic);
    }
    Ok(u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]))
}

/// Rewrites an older `.fgiv` at [`crate::baked_gi::BAKED_GI_VERSION`] (`regia upgrade-project
/// --write`): decode, re-encode. Versions 1 to 3 gain an absent split section (one unsplit
/// volume); a version 4 split part's bricks gain the version 5 crop fields (no crop: the unsplit
/// counts, offset 0, every cell owned by the brick's owner, ADR-3237). Brick flags, the emitter
/// list (or its absence) and the sections are kept byte for byte, so the payload hash stays the
/// same.
///
/// # Errors
///
/// The file does not read, or it is already at the current version.
pub fn migrate_baked_gi_file(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let found = baked_gi_version(bytes).map_err(|e| e.to_string())?;
    if found >= crate::baked_gi::BAKED_GI_VERSION {
        return Err(format!(
            "baked GI volume is version {found}; only older files migrate to {}",
            crate::baked_gi::BAKED_GI_VERSION
        ));
    }
    BakedGiVolume::from_bytes(bytes)
        .and_then(|v| v.to_bytes())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;
