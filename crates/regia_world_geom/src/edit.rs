// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/terrain_deform.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 1-118, mark: adapt (Stage B).
// Edits: the cone and the clamp move from a cell grid to integer millimetres on the ground lattice (spec 121 § 6);
// the berm takes the same profile raised; ceiling stop, mouth-ring clip, tile promotion, state hash and snapshot are
// original; module docs rewritten.
//! D1 ground edits, the mutable geometry state and its hash (spec 121 § 5.2 and § 6, lane W-12,
//! ADR-8313).
//!
//! [`GeomState`] is the sim-owned state over a [`WorldGeom`]: the D1 vertex overrides (the result
//! of the edits, never their history), the ground tiles an edit promoted to full pitch, the
//! blocker bits, the generation and a running state hash. Every query takes it, so a crater moves
//! `support`, `surface_at`, `floors_at` and `segment` together.
//!
//! - **Integer arithmetic.** An edit's centre and heights are `i32` millimetres; the profile is
//!   `delta = depth * (r² - d²) div_euclid r²` in `i64` over the lattice vertices inside the
//!   radius, visited in ascending vertex index. The only float steps are `m_to_mm` of a cooked
//!   count height or roof plane height, and `mm_to_m` when the query reads the override.
//! - **Ceiling stop.** A crater never lowers a vertex below the highest layer roof under it plus
//!   `min_cover_mm`; it never raises one either.
//! - **Mouth rings are fixed (E5).** A ring vertex keeps its cooked pin.
//! - **Promotion (E1 rule 6).** On a `Nested` map every ground tile holding a touched vertex goes
//!   to full pitch first. Its new vertices become millimetre overrides of the coarse surface, in
//!   ascending tile index, then ascending vertex index; a border vertex next to a coarser tile that
//!   is not promoted takes that tile's coarse edge (the stitch, seam rule 4).
//! - **State hash (E4).** The XOR of one term per override, promoted tile and flipped blocker
//!   (Zobrist), updated per changed entry, folded with the generation: O(1) per call.
//! - **Copy-on-write.** Overrides live in one block per ground tile behind an [`Arc`]: cloning a
//!   state copies one pointer per touched tile, and the first write to a shared block copies that
//!   block only.
//! - **The ground index follows the edits (ADR-8818).** A state keeps the height pyramid's ranges
//!   widened by every height it wrote, so `segment` still skips ground nodes by height on an
//!   edited map. The ranges are a bound kept for speed, not part of the state's value.
//! - **One pass per owner tile (ADR-8818).** The profile walks the footprint one owner tile at a
//!   time, row by row: the disc's `d²` comes from one squared offset per footprint column and row,
//!   a full-pitch tile's counts are read as row slices, the cooked overrides and the tile's own
//!   block are merged by cursor, each roof's plane is built once per edit for the ceiling stop,
//!   and the tile's block is rebuilt in one merge. Promotion reads each coarse cell's corners
//!   once and merges a tile's fills into its block once, and `height_rects` reads rows the same
//!   way. The answers are those of the per-vertex walk (`apply_ground_edit_reference`, kept for
//!   the equality tests).
//!
//! ```
//! # #[cfg(feature = "stage_b")] {
//! use regia_world_geom::cook::{CookInputs, GroundInput, cook};
//! use regia_world_geom::cooked::encode_canonical;
//! use regia_world_geom::{GroundEdit, WorldGeom};
//!
//! let ground = GroundInput { lattice_n: 65, width_m: 20.0, offset_m: 0.0, step_m: 0.001, counts: vec![0; 65 * 65] };
//! let out = cook(&CookInputs {
//!     document: b"(version:1, ground_tile_cells:16, layers:[])",
//!     ground: &ground,
//!     terrain: None,
//!     read_file: &|_| Err("no files".into()),
//! })
//! .unwrap();
//! let geom = WorldGeom::from_canonical_bytes(&encode_canonical(&out.file.canonical).unwrap()).unwrap();
//! let mut state = geom.new_state();
//! let fresh = state.state_hash();
//! let dirty = state
//!     .apply_ground_edit(&geom, &GroundEdit::Crater { x_mm: 0, z_mm: 0, radius_mm: 3000, depth_mm: 450 })
//!     .unwrap();
//! assert!(!dirty.is_empty());
//! assert!(geom.support(&state, 0.0, 0.0, 1.0).floor.y < -0.44);
//! assert_ne!(state.state_hash(), fresh);
//! # }
//! ```

use crate::blockers::BlockerId;
use crate::cooked::{LatticeConstants, VertexHeightMm, WorldGeomCooked};
use crate::ground::{GroundLattice, plane_height, vertex_m};
use crate::units::{m_to_mm, mm_to_m};
use crate::{GroundMode, WorldGeom};
use regia_detsim::float::{det_max, det_min};
use std::sync::Arc;

/// Most lattice samples along either side of one edit's footprint (spec 25 § 7.4 `MAX_EDIT_SIDE`):
/// the footprint of an accepted edit fits one `TerrainHeightEdits` rectangle.
pub const MAX_EDIT_SIDE: u32 = 128;
/// Edited heights are clamped to ±this, millimetres (the source's safety clamp).
pub const HEIGHT_LIMIT_MM: i32 = 1_000_000;
/// Largest crater or berm radius, millimetres.
pub const MAX_RADIUS_MM: i32 = 1_000_000;
/// Largest crater depth or berm height, millimetres.
pub const MAX_DEPTH_MM: i32 = 2_000_000;
/// The version of [`GeomState::encode`]'s layout.
pub const SNAPSHOT_VERSION: u32 = 1;
/// [`GeomState::state_hash`] of every fresh state, on every map (spec 121 § 5.2 rule 3).
pub const FRESH_STATE_HASH: u64 = 0x6875_2350_ae1d_483f;

/// A sim-owned height edit on the ground lattice, integer millimetres, applied in push order.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GroundEdit {
    /// A crater: lowers lattice vertices within `radius_mm` of the centre by up to `depth_mm`
    /// (the integer profile), never below a layer roof plus `min_cover_mm`, never on a mouth ring.
    /// The centre is world mm: a sim converts its `f32` position with [`m_to_mm`].
    Crater {
        /// Centre x, world millimetres.
        x_mm: i32,
        /// Centre z, world millimetres.
        z_mm: i32,
        /// Radius, millimetres (1..=1 000 000).
        radius_mm: i32,
        /// Centre depth, millimetres (0..=2 000 000).
        depth_mm: i32,
    },
    /// A berm: raises the same profile by up to `height_mm`.
    Berm {
        /// Centre x, world millimetres.
        x_mm: i32,
        /// Centre z, world millimetres.
        z_mm: i32,
        /// Radius, millimetres (1..=1 000 000).
        radius_mm: i32,
        /// Centre height, millimetres (0..=2 000 000).
        height_mm: i32,
    },
    /// Absolute heights of a lattice rectangle (the `TerrainHeightEdits` shape, spec 25 § 7.4),
    /// row-major with x fastest. The ceiling stop and the ring clip apply.
    Rect {
        /// First lattice column.
        col: u32,
        /// First lattice row.
        row: u32,
        /// Vertices along x (1..=128).
        width: u32,
        /// Vertices along z (1..=128).
        height: u32,
        /// `width * height` absolute heights, millimetres (±1 000 000).
        heights_mm: Vec<i32>,
    },
}

/// The lattice vertex rectangle an edit changed (inclusive start, exclusive end), for
/// `TerrainHeightEdits`, nav tiles and the query's ground index.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirtyRect {
    /// First vertex column.
    pub col0: u32,
    /// First vertex row.
    pub row0: u32,
    /// One past the last vertex column.
    pub col1: u32,
    /// One past the last vertex row.
    pub row1: u32,
}

impl DirtyRect {
    /// No vertex.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.col1 <= self.col0 || self.row1 <= self.row0
    }

    fn union(self, other: DirtyRect) -> DirtyRect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        DirtyRect {
            col0: self.col0.min(other.col0),
            row0: self.row0.min(other.row0),
            col1: self.col1.max(other.col1),
            row1: self.row1.max(other.row1),
        }
    }

    /// The ground tiles (`b * tiles_per_side + a`, ascending) holding any vertex of the rectangle:
    /// the nav tiles and the ground index blocks to refresh.
    #[must_use]
    pub fn ground_tiles(&self, lattice: &LatticeConstants) -> Vec<u32> {
        let mut out = Vec::new();
        if self.is_empty() {
            return out;
        }
        let per = lattice.tiles_per_side();
        let span = |lo: u32, hi: u32| {
            let a0 = tile_columns_of_vertex(lattice, lo).0;
            let a1 = tile_columns_of_vertex(lattice, hi - 1).1;
            (a0, a1)
        };
        let (a0, a1) = span(self.col0, self.col1);
        let (b0, b1) = span(self.row0, self.row1);
        for b in b0..=b1 {
            for a in a0..=a1 {
                out.push(b * per + a);
            }
        }
        out
    }
}

/// One `TerrainHeightEdits` rectangle (spec 25 § 7.4): absolute heights in millimetres, row-major,
/// x fastest, at most [`MAX_EDIT_SIDE`] samples per side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeightRectMm {
    /// First sample column.
    pub col: u32,
    /// First sample row.
    pub row: u32,
    /// Samples along x.
    pub width: u32,
    /// Samples along z.
    pub height: u32,
    /// `width * height` heights, millimetres.
    pub heights_mm: Vec<i32>,
}

/// Bumped by every state change that moves an answer (a ground edit; W-13's blocker flip).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Generation(pub u64);

/// Edit and blocker errors. [`GeomEditError::code`] is the agent-protocol code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GeomEditError {
    /// (`edit_out_of_map`) the edit's footprint is outside the lattice.
    #[error("the edit is outside the ground lattice")]
    OutOfMap,
    /// (`edit_too_large`) a side of the footprint over 128 lattice samples.
    #[error("the edit's footprint is over {MAX_EDIT_SIDE} samples on a side")]
    TooLarge,
    /// (`edit_out_of_range`) a height outside ±1 000 000 mm, a radius outside 1..=1 000 000 mm,
    /// a depth or berm height outside 0..=2 000 000 mm, or a rectangle whose height count is not
    /// `width * height`.
    #[error("an edit value is out of range")]
    OutOfRange,
    /// (`blocker_unknown`) no cooked blocker plug has this id (lane W-13).
    #[error("no blocker plug {}", .0.0)]
    UnknownBlocker(BlockerId),
}

impl GeomEditError {
    /// The stable lowercase code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::OutOfMap => "edit_out_of_map",
            Self::TooLarge => "edit_too_large",
            Self::OutOfRange => "edit_out_of_range",
            Self::UnknownBlocker(_) => "blocker_unknown",
        }
    }
}

/// Why a [`GeomState`] snapshot did not decode.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GeomSnapshotError {
    /// (`geom_snapshot_newer`) written by a newer engine.
    #[error("snapshot version {found} is newer than {supported}")]
    NewerVersion {
        /// The version in the bytes.
        found: u32,
        /// The newest this build reads.
        supported: u32,
    },
    /// (`geom_snapshot_other_map`) its content hash is not this `WorldGeom`'s.
    #[error("the snapshot is of another map")]
    OtherMap,
    /// (`geom_snapshot_corrupt`) truncated, trailing bytes, an index out of range, unsorted
    /// entries, or a stored hash that does not match the recomputed one.
    #[error("the snapshot is corrupt")]
    Corrupt,
}

impl GeomSnapshotError {
    /// The stable lowercase code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NewerVersion { .. } => "geom_snapshot_newer",
            Self::OtherMap => "geom_snapshot_other_map",
            Self::Corrupt => "geom_snapshot_corrupt",
        }
    }
}

/// Work one edit did, for the O(1) hash pin (`state_hash_is_o1_per_edit`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EditStats {
    /// Hash terms XORed into or out of the accumulator.
    pub hash_terms: u32,
    /// Vertex overrides written or removed.
    pub vertices_written: u32,
    /// Ground tiles promoted to full pitch.
    pub tiles_promoted: u32,
}

/// One ground tile's D1 overrides, `(vertex, mm)` ascending vertex, shared until written.
type OverrideBlock = Arc<Vec<(u32, i32)>>;

/// The D1 overrides and promoted tiles of a [`GeomState`]: one sorted override block per touched
/// ground tile (copy-on-write), and the promoted tiles ascending.
#[derive(Clone, Debug, Default)]
pub struct GroundEdits {
    /// `(owner tile, block)`, ascending tile.
    blocks: Vec<(u32, OverrideBlock)>,
    promoted: Vec<u32>,
    /// The height pyramid's ranges widened by every height this state wrote (spec 121 § 6: the
    /// ground index of the touched tiles), shared until written. `None` on a state that was never
    /// edited. Not part of the state's value: see [`EditedRanges`].
    ranges: Option<Arc<EditedRanges>>,
}

/// Two edit sets are equal when their overrides and promoted tiles are: the sim state. The ranges
/// are a conservative bound kept for speed, and they follow the edits' history.
impl PartialEq for GroundEdits {
    fn eq(&self, other: &Self) -> bool {
        self.blocks == other.blocks && self.promoted == other.promoted
    }
}
impl Eq for GroundEdits {}

/// The cooked height pyramid's `[lowest, highest]` ground height per node, metres, with an edited
/// state's heights folded in (ADR-8818): what lets `segment` keep skipping ground nodes by height
/// once the ground has been edited.
///
/// Every override an edit writes widens the range of each ground tile its vertex touches, and of
/// that tile's ancestors. A range is never narrowed (an undone crater leaves it wide), so it is a
/// bound, not a measure: it can only prevent a skip, never hide a hit, and a query's answer does
/// not depend on it (`edited_state_segments_equal_the_full_walk`).
#[derive(Clone)]
struct EditedRanges {
    /// Per pyramid level, level 0 (one node per ground tile) first: the level's first entry in
    /// `ranges` and its nodes per side.
    levels: Vec<(u32, u32)>,
    ranges: Vec<[f32; 2]>,
}

impl std::fmt::Debug for EditedRanges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditedRanges")
            .field("levels", &self.levels.len())
            .field("nodes", &self.ranges.len())
            .finish()
    }
}

impl EditedRanges {
    /// The cooked pyramid's ranges, not yet widened.
    fn of(geom: &WorldGeom) -> Self {
        let pyramid = &geom.height_pyramid.levels;
        let mut levels = Vec::with_capacity(pyramid.len());
        let mut ranges = Vec::with_capacity(pyramid.iter().map(|l| l.ranges.len()).sum());
        for level in pyramid {
            levels.push((ranges.len() as u32, level.side));
            ranges.extend_from_slice(&level.ranges);
        }
        Self { levels, ranges }
    }

    /// Widens ground tile `(a, b)` and its ancestors to hold `[lo, hi]`. An ancestor already
    /// holds its child's range, so the walk stops at the first node that does not move.
    fn widen(&mut self, (mut a, mut b): (u32, u32), lo: f32, hi: f32) {
        for &(first, side) in &self.levels {
            if a >= side || b >= side {
                return;
            }
            let Some(range) = self.ranges.get_mut((first + b * side + a) as usize) else {
                return;
            };
            let wide = [det_min(range[0], lo), det_max(range[1], hi)];
            if wide == *range {
                return;
            }
            *range = wide;
            (a, b) = (a / 2, b / 2);
        }
    }
}

/// The ground tile that stores a vertex's override block: `(min(j / g, last), min(i / g, last))`.
fn owner_tile(lattice: &LatticeConstants, vertex: u32) -> u32 {
    let n = lattice.lattice_n;
    let (i, j) = (vertex % n, vertex / n);
    let per = lattice.tiles_per_side();
    let g = u32::from(lattice.ground_tile_cells.max(1));
    let a = (i / g).min(per.saturating_sub(1));
    let b = (j / g).min(per.saturating_sub(1));
    b * per + a
}

/// The first and last ground tile column (or row) whose vertex range holds vertex index `i`.
fn tile_columns_of_vertex(lattice: &LatticeConstants, i: u32) -> (u32, u32) {
    let per = lattice.tiles_per_side();
    let g = u32::from(lattice.ground_tile_cells.max(1));
    let hi = (i / g).min(per.saturating_sub(1));
    let lo = if i > 0 && i.is_multiple_of(g) {
        (i / g - 1).min(hi)
    } else {
        hi
    };
    (lo, hi)
}

/// Widens the ranges of every ground tile a vertex owned by tile `(a, b)` may touch (the tile and
/// its west, south and south-west neighbors, which share its first column and row) to hold
/// `lo_mm..=hi_mm`.
fn widen_around(ranges: &mut EditedRanges, (a, b): (u32, u32), lo_mm: i32, hi_mm: i32) {
    let (lo, hi) = (mm_to_m(0, lo_mm), mm_to_m(0, hi_mm));
    for tb in [b.checked_sub(1), Some(b)].into_iter().flatten() {
        for ta in [a.checked_sub(1), Some(a)].into_iter().flatten() {
            ranges.widen((ta, tb), lo, hi);
        }
    }
}

impl GroundEdits {
    /// No override and no promoted tile.
    #[must_use]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.promoted.is_empty()
    }

    /// The D1 override of `vertex`, millimetres.
    #[must_use]
    #[inline]
    pub fn get(&self, lattice: &LatticeConstants, vertex: u32) -> Option<i32> {
        let tile = owner_tile(lattice, vertex);
        let k = self.blocks.binary_search_by_key(&tile, |b| b.0).ok()?;
        let block = &self.blocks[k].1;
        block
            .binary_search_by_key(&vertex, |v| v.0)
            .ok()
            .map(|i| block[i].1)
    }

    /// Whether a D1 edit promoted ground tile `tile` to full pitch.
    #[must_use]
    #[inline]
    pub fn is_promoted(&self, tile: u32) -> bool {
        self.promoted.binary_search(&tile).is_ok()
    }

    /// The promoted tiles, ascending.
    #[must_use]
    pub fn promoted(&self) -> &[u32] {
        &self.promoted
    }

    /// Whether ground tile `tile` owns any override. With [`GroundEdits::is_promoted`] it tells
    /// the segment walk which tiles still read the cooked ground (ADR-8815).
    #[must_use]
    pub(crate) fn owns_overrides(&self, tile: u32) -> bool {
        self.blocks.binary_search_by_key(&tile, |b| b.0).is_ok()
    }

    /// Number of vertex overrides.
    #[must_use]
    pub fn override_count(&self) -> usize {
        self.blocks.iter().map(|b| b.1.len()).sum()
    }

    /// Every override `(vertex, mm)`, ascending vertex (allocates; snapshots and tools).
    #[must_use]
    pub fn overrides(&self) -> Vec<(u32, i32)> {
        let mut all: Vec<(u32, i32)> = self.blocks.iter().flat_map(|b| b.1.iter().copied()).collect();
        all.sort_unstable_by_key(|v| v.0);
        all
    }

    /// Sets or removes an override; returns the old value. Copies a shared block once. The
    /// reference walk's one-vertex form.
    #[cfg(any(test, feature = "stats"))]
    fn set(&mut self, lattice: &LatticeConstants, vertex: u32, mm: Option<i32>) -> Option<i32> {
        let tile = owner_tile(lattice, vertex);
        let k = match self.blocks.binary_search_by_key(&tile, |b| b.0) {
            Ok(k) => k,
            Err(k) => {
                mm?;
                self.blocks.insert(k, (tile, Arc::new(Vec::new())));
                k
            }
        };
        let block = Arc::make_mut(&mut self.blocks[k].1);
        let old = match block.binary_search_by_key(&vertex, |v| v.0) {
            Ok(i) => {
                let old = block[i].1;
                match mm {
                    Some(mm) => block[i].1 = mm,
                    None => {
                        block.remove(i);
                    }
                }
                Some(old)
            }
            Err(i) => {
                if let Some(mm) = mm {
                    block.insert(i, (vertex, mm));
                }
                None
            }
        };
        if block.is_empty() {
            self.blocks.remove(k);
        }
        old
    }

    /// Replaces a tile's whole override block (ascending vertex); an empty block removes it.
    fn replace_block(&mut self, tile: u32, block: Vec<(u32, i32)>) {
        match self.blocks.binary_search_by_key(&tile, |b| b.0) {
            Ok(k) if block.is_empty() => {
                self.blocks.remove(k);
            }
            Ok(k) => self.blocks[k].1 = Arc::new(block),
            Err(_) if block.is_empty() => {}
            Err(k) => self.blocks.insert(k, (tile, Arc::new(block))),
        }
    }

    /// The `[lowest, highest]` ground height of height-pyramid node `index` of `level` under
    /// these edits, metres; `None` when the set keeps no ranges.
    #[inline]
    pub(crate) fn edited_range(&self, level: usize, index: usize) -> Option<[f32; 2]> {
        let ranges = self.ranges.as_deref()?;
        let &(first, _) = ranges.levels.get(level)?;
        ranges.ranges.get(first as usize + index).copied()
    }

    /// Whether the height pyramid bounds this ground: an unedited one by the cooked ranges, an
    /// edited one by its own (`edited_range`).
    #[inline]
    pub(crate) fn pyramid_bounds(&self) -> bool {
        self.is_empty() || self.ranges.is_some()
    }

    /// The ranges, ready to widen: created from the cooked pyramid and every override already
    /// here on first use (a decoded snapshot, a state the reference walk edited).
    fn ranges_mut(&mut self, geom: &WorldGeom) -> &mut EditedRanges {
        let ranges = self.ranges.get_or_insert_with(|| {
            let mut ranges = EditedRanges::of(geom);
            let per = geom.cooked().lattice.tiles_per_side().max(1);
            for (tile, block) in &self.blocks {
                let lo = block.iter().map(|e| e.1).min();
                let hi = block.iter().map(|e| e.1).max();
                if let (Some(lo), Some(hi)) = (lo, hi) {
                    widen_around(&mut ranges, (tile % per, tile / per), lo, hi);
                }
            }
            Arc::new(ranges)
        });
        Arc::make_mut(ranges)
    }

    /// Folds heights `lo_mm..=hi_mm`, written to vertices ground tile `tile` owns, into the
    /// ranges.
    fn widen_owner(&mut self, geom: &WorldGeom, tile: u32, lo_mm: i32, hi_mm: i32) {
        let per = geom.cooked().lattice.tiles_per_side().max(1);
        widen_around(self.ranges_mut(geom), (tile % per, tile / per), lo_mm, hi_mm);
    }

    /// Merges `fills` (ascending vertex, none of them in the tile's block yet) into `tile`'s block.
    fn insert_new(&mut self, tile: u32, fills: &[(u32, i32)]) {
        let old = self.block(tile);
        let mut merged = Vec::with_capacity(old.len() + fills.len());
        let mut k = 0;
        for &fill in fills {
            while let Some(&entry) = old.get(k).filter(|e| e.0 < fill.0) {
                merged.push(entry);
                k += 1;
            }
            merged.push(fill);
        }
        merged.extend_from_slice(old.get(k..).unwrap_or(&[]));
        self.replace_block(tile, merged);
    }

    /// A tile's override block, ascending vertex (empty when the tile has none).
    fn block(&self, tile: u32) -> &[(u32, i32)] {
        match self.blocks.binary_search_by_key(&tile, |b| b.0) {
            Ok(k) => &self.blocks[k].1,
            Err(_) => &[],
        }
    }

    fn promote(&mut self, tile: u32) -> bool {
        match self.promoted.binary_search(&tile) {
            Ok(_) => false,
            Err(k) => {
                self.promoted.insert(k, tile);
                true
            }
        }
    }
}

/// The mutable sim state over a [`WorldGeom`] (spec 121 § 5.1): D1 overrides, promoted tiles,
/// blocker bits, the generation and the running state hash. Cloning shares every override block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeomState {
    pub(crate) content_hash: [u8; 32],
    pub(crate) ground: GroundEdits,
    /// Blocker bits, ascending id, 64 per word (lane W-13 flips them).
    pub(crate) blockers: Vec<u64>,
    /// Set bits in `blockers`: the queries skip every blocker test while it is 0 (lane W-13).
    active_blockers: u32,
    generation: Generation,
    /// XOR of one term per state entry that differs from the cooked state (§ 5.2 rule 2).
    acc: u64,
}

// The state-hash fold (spec 121 § 5.2 rules 2-4; ADR-8313). `regia_detsim::hash` (F-04) does not
// exist yet, so the term is FNV-1a-64 (the fold the sim's own checksum uses) of the tag byte and the
// little-endian fields, finished by the SplitMix64 mixer so the XOR of terms stays well spread
// (Zobrist 1970; Steele, Lea and Flood 2014).
const TAG_BLOCKER: u8 = 1;
const TAG_VERTEX: u8 = 2;
const TAG_TILE: u8 = 3;

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn term(tag: u8, a: u32, b: Option<i32>) -> u64 {
    let mut buf = [0u8; 9];
    buf[0] = tag;
    buf[1..5].copy_from_slice(&a.to_le_bytes());
    let len = if let Some(b) = b {
        buf[5..9].copy_from_slice(&b.to_le_bytes());
        9
    } else {
        5
    };
    mix64(fnv1a64(&buf[..len]))
}

fn vertex_term(vertex: u32, mm: i32) -> u64 {
    term(TAG_VERTEX, vertex, Some(mm))
}

fn tile_term(tile: u32) -> u64 {
    term(TAG_TILE, tile, None)
}

fn blocker_term(id: u32) -> u64 {
    // The blocker id is a u16 on the wire; it is hashed as its two little-endian bytes.
    let mut buf = [0u8; 3];
    buf[0] = TAG_BLOCKER;
    buf[1..3].copy_from_slice(&(id as u16).to_le_bytes());
    mix64(fnv1a64(&buf))
}

fn fold(acc: u64, generation: Generation) -> u64 {
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&acc.to_le_bytes());
    buf[8..].copy_from_slice(&generation.0.to_le_bytes());
    mix64(fnv1a64(&buf))
}

fn initial_blocker_words(geom: &WorldGeom) -> Vec<u64> {
    let plugs = &geom.cooked().plugs;
    let mut words = vec![0u64; plugs.len().div_ceil(64)];
    for (i, plug) in plugs.iter().enumerate() {
        if plug.initially_active {
            words[i / 64] |= 1 << (i % 64);
        }
    }
    words
}

impl WorldGeom {
    /// A fresh state: every blocker at its cooked initial value, no ground edit, generation 0.
    #[must_use]
    pub fn new_state(&self) -> GeomState {
        GeomState {
            content_hash: self.content_hash(),
            ground: GroundEdits::default(),
            active_blockers: self.cooked().plugs.iter().filter(|p| p.initially_active).count() as u32,
            blockers: initial_blocker_words(self),
            generation: Generation(0),
            acc: 0,
        }
    }
}

/// One planned vertex write of the reference walk.
#[cfg(any(test, feature = "stats"))]
#[derive(Clone, Copy)]
struct Write {
    vertex: u32,
    mm: i32,
}

impl GeomState {
    /// The geometry's term in the sim checksum, O(1): the running accumulator folded with the
    /// generation (spec 121 § 5.2, E4). A host folds it as one more little-endian `u64` term.
    #[must_use]
    pub fn state_hash(&self) -> u64 {
        fold(self.acc, self.generation)
    }

    /// The generation.
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// Whether blocker bit `id` is set (no bounds error: an id past the words reads clear).
    #[inline]
    pub(crate) fn blocker_bit(&self, id: u16) -> bool {
        self.blockers
            .get(usize::from(id) / 64)
            .is_some_and(|w| w & (1u64 << (id % 64)) != 0)
    }

    /// Whether any blocker is active: one compare, so a map with every plug open pays nothing.
    #[inline]
    pub(crate) fn any_blocker_active(&self) -> bool {
        self.active_blockers != 0
    }

    /// Flips blocker bit `id` (a valid plug id): one word, one hash term (§ 5.2 rule 2), one
    /// generation. Returns the new generation.
    pub(crate) fn toggle_blocker(&mut self, id: u16, stats: &mut EditStats) -> Generation {
        let bit = 1u64 << (id % 64);
        let word = &mut self.blockers[usize::from(id) / 64];
        *word ^= bit;
        if *word & bit != 0 {
            self.active_blockers += 1;
        } else {
            self.active_blockers -= 1;
        }
        self.acc ^= blocker_term(u32::from(id));
        stats.hash_terms += 1;
        self.generation = Generation(self.generation.0 + 1);
        self.generation
    }

    /// The blocker bits as the snapshot stores them: `u64` words, ascending id, bit `id % 64` of
    /// word `id / 64` set when blocker `id` is active (spec 121 § 5.2 rule 5).
    #[must_use]
    pub fn blocker_words(&self) -> &[u64] {
        &self.blockers
    }

    /// The D1 overrides and promoted tiles.
    #[must_use]
    pub fn ground_edits(&self) -> &GroundEdits {
        &self.ground
    }

    /// Whether this state keeps its own ground height ranges, so `segment` skips ground nodes by
    /// height on it (ADR-8818). An unedited state needs none; a state the reference walk edited
    /// has none and walks every cell.
    #[cfg(any(test, feature = "stats"))]
    #[must_use]
    pub fn keeps_ground_ranges(&self) -> bool {
        self.ground.ranges.is_some()
    }

    /// The current height of lattice vertex `vertex`, millimetres: its override (a ring pin, a D1
    /// edit, a hanging vertex), else `m_to_mm` of its count height. `None` outside the lattice.
    #[must_use]
    pub fn vertex_height_mm(&self, geom: &WorldGeom, vertex: u32) -> Option<i32> {
        let lattice = GroundLattice::new(geom.cooked()).with_edits(&self.ground);
        vertex_mm(&lattice, vertex)
    }

    /// Applies one edit to the ground (spec 121 § 6). Returns the vertex rectangle it covers:
    /// the footprint (the ring clip does not shrink it) and every promoted tile. An edit that
    /// changes no vertex leaves the generation and the hash alone.
    ///
    /// # Errors
    ///
    /// [`GeomEditError`]: `edit_out_of_map`, `edit_too_large`, `edit_out_of_range`. Nothing is
    /// changed on error.
    pub fn apply_ground_edit(
        &mut self,
        geom: &WorldGeom,
        edit: &GroundEdit,
    ) -> Result<DirtyRect, GeomEditError> {
        self.apply_ground_edit_with_stats(geom, edit, &mut EditStats::default())
    }

    /// [`GeomState::apply_ground_edit`] with a work counter.
    ///
    /// # Errors
    ///
    /// As [`GeomState::apply_ground_edit`].
    pub fn apply_ground_edit_with_stats(
        &mut self,
        geom: &WorldGeom,
        edit: &GroundEdit,
        stats: &mut EditStats,
    ) -> Result<DirtyRect, GeomEditError> {
        self.apply_ground_edit_walk(geom, edit, stats, ProfileWalk::Tiles)
    }

    /// [`GeomState::apply_ground_edit_with_stats`] by the per-vertex walk it replaced (one lattice
    /// lookup, one roof search and one sorted insert per vertex), for bit-equivalence checks
    /// (ADR-8818).
    ///
    /// # Errors
    ///
    /// As [`GeomState::apply_ground_edit`].
    #[cfg(any(test, feature = "stats"))]
    pub fn apply_ground_edit_reference(
        &mut self,
        geom: &WorldGeom,
        edit: &GroundEdit,
        stats: &mut EditStats,
    ) -> Result<DirtyRect, GeomEditError> {
        self.apply_ground_edit_walk(geom, edit, stats, ProfileWalk::Reference)
    }

    fn apply_ground_edit_walk(
        &mut self,
        geom: &WorldGeom,
        edit: &GroundEdit,
        stats: &mut EditStats,
        walk: ProfileWalk,
    ) -> Result<DirtyRect, GeomEditError> {
        let _span = tracing::trace_span!("world_geom_ground_edit").entered();
        let lattice = geom.cooked().lattice;
        let footprint = Footprint::of(&lattice, edit)?;
        let mut touched = false;

        // 1. Promote every coarse tile holding a footprint vertex (E1 rule 6).
        let mut dirty = footprint.rect;
        let promote = self.tiles_to_promote(geom, &footprint);
        if !promote.is_empty() {
            // The fills are read from the ground as it is before any tile of this edit is promoted.
            let fills = match walk {
                ProfileWalk::Tiles => Fills::Buckets(self.promotion_buckets(geom, &promote)),
                #[cfg(any(test, feature = "stats"))]
                ProfileWalk::Reference => Fills::Writes(self.promotion_fills(geom, &promote)),
            };
            for &tile in &promote {
                if self.ground.promote(tile) {
                    self.acc ^= tile_term(tile);
                    stats.hash_terms += 1;
                    stats.tiles_promoted += 1;
                    touched = true;
                }
                dirty = dirty.union(tile_rect(&lattice, tile));
            }
            match fills {
                Fills::Buckets(buckets) => {
                    for bucket in buckets {
                        // A fill is a new override (a vertex that has one gets no fill).
                        let (mut lo, mut hi) = (i32::MAX, i32::MIN);
                        for &(vertex, mm) in &bucket.fills {
                            self.acc ^= vertex_term(vertex, mm);
                            (lo, hi) = (lo.min(mm), hi.max(mm));
                        }
                        stats.hash_terms += bucket.fills.len() as u32;
                        stats.vertices_written += bucket.fills.len() as u32;
                        self.ground.insert_new(bucket.owner, &bucket.fills);
                        if lo <= hi {
                            self.ground.widen_owner(geom, bucket.owner, lo, hi);
                        }
                    }
                    // Promotion alone is an edit: the state keeps ranges from here on.
                    self.ground.ranges_mut(geom);
                }
                #[cfg(any(test, feature = "stats"))]
                Fills::Writes(writes) => {
                    for w in writes {
                        self.write(&lattice, w, None, stats);
                    }
                }
            }
        }

        // 2. The profile, read against the promoted ground.
        touched |= match walk {
            ProfileWalk::Tiles => self.apply_profile(geom, edit, &footprint, stats),
            #[cfg(any(test, feature = "stats"))]
            ProfileWalk::Reference => {
                let writes = self.profile_writes(geom, edit, &footprint);
                let view = GroundLattice::new(geom.cooked());
                let mut any = false;
                for w in writes {
                    let cooked = view.cooked_override_at(w.vertex);
                    any |= self.write(&lattice, w, cooked, stats);
                }
                any
            }
        };
        if touched {
            self.generation.0 += 1;
            #[cfg(any(test, feature = "stats"))]
            if matches!(walk, ProfileWalk::Reference) {
                // The reference walk keeps no ranges: its states walk every ground cell.
                self.ground.ranges = None;
            }
        }
        Ok(dirty)
    }

    /// The profile and its writes, one owner tile at a time (ADR-8818). Returns whether any vertex
    /// changed.
    fn apply_profile(
        &mut self,
        geom: &WorldGeom,
        edit: &GroundEdit,
        f: &Footprint,
        stats: &mut EditStats,
    ) -> bool {
        let rect = f.rect;
        if rect.is_empty() {
            return false;
        }
        let c = geom.cooked();
        let l = &c.lattice;
        let n = l.lattice_n;
        let g = u32::from(l.ground_tile_cells.max(1));
        let last = l.tiles_per_side().saturating_sub(1);
        // The footprint's axis positions, metres: the ceiling stop's query points.
        let (w, h) = ((rect.col1 - rect.col0) as usize, (rect.row1 - rect.row0) as usize);
        let mut xs = [0.0f32; MAX_EDIT_SIDE as usize];
        let mut zs = [0.0f32; MAX_EDIT_SIDE as usize];
        let (Some(xs), Some(zs)) = (xs.get_mut(..w), zs.get_mut(..h)) else {
            return false; // `Footprint::of` refuses a side over `MAX_EDIT_SIDE`.
        };
        for (x, i) in xs.iter_mut().zip(rect.col0..) {
            *x = vertex_m(l, i);
        }
        for (z, j) in zs.iter_mut().zip(rect.row0..) {
            *z = vertex_m(l, j);
        }
        // The cooked overrides of the footprint's vertex range (ascending vertex in both lists).
        let (first, end) = (rect.row0 * n + rect.col0, (rect.row1 - 1) * n + rect.col1);
        let window = |list: &'_ [VertexHeightMm]| {
            let lo = list.partition_point(|v| v.vertex < first);
            let hi = lo + list[lo..].partition_point(|v| v.vertex < end);
            lo..hi
        };
        let mut walk = TileWalk {
            edit,
            f,
            ring: &c.ring_pins[window(&c.ring_pins)],
            hanging: &c.hanging_overrides[window(&c.hanging_overrides)],
            ground: GroundLattice::new(c),
            stops: crate::query::RoofStops::new(geom, xs, zs),
            min_cover: i64::from(geom.settings().min_cover_mm),
        };
        let mut touched = false;
        for b in (rect.row0 / g).min(last)..=((rect.row1 - 1) / g).min(last) {
            for a in (rect.col0 / g).min(last)..=((rect.col1 - 1) / g).min(last) {
                let Some(change) = walk.tile(c, &self.ground, a, b) else {
                    continue;
                };
                self.acc ^= change.acc;
                stats.hash_terms += change.hash_terms;
                stats.vertices_written += change.vertices_written;
                self.ground.replace_block(change.tile, change.block);
                if change.lo_mm <= change.hi_mm {
                    self.ground
                        .widen_owner(geom, change.tile, change.lo_mm, change.hi_mm);
                }
                touched = true;
            }
        }
        touched
    }

    /// Writes one override (removing it where it equals the cooked override exactly): the
    /// reference walk's one-vertex form.
    #[cfg(any(test, feature = "stats"))]
    fn write(
        &mut self,
        lattice: &LatticeConstants,
        w: Write,
        cooked: Option<i32>,
        stats: &mut EditStats,
    ) -> bool {
        let new = if cooked == Some(w.mm) { None } else { Some(w.mm) };
        let old = self.ground.get(lattice, w.vertex);
        if old == new {
            return false;
        }
        self.ground.set(lattice, w.vertex, new);
        if let Some(old) = old {
            self.acc ^= vertex_term(w.vertex, old);
            stats.hash_terms += 1;
        }
        if let Some(new) = new {
            self.acc ^= vertex_term(w.vertex, new);
            stats.hash_terms += 1;
        }
        stats.vertices_written += 1;
        true
    }

    /// The overrides promotion writes, grouped by the ground tile that stores them (ADR-8818):
    /// every vertex of a promoted tile that is not on its coarse grid and has no override, from
    /// the coarse surface (or a coarser unpromoted neighbor's edge), exactly as
    /// `promotion_fills` reads them. The coarse cell's four corners are read once per run of
    /// vertices inside it, the lattice cell of a vertex once per tile column and row, the cooked
    /// overrides by cursor, and only a border vertex can already have a fill from another tile of
    /// this edit.
    fn promotion_buckets(&self, geom: &WorldGeom, promote: &[u32]) -> Vec<FillBucket> {
        let c = geom.cooked();
        let l = &c.lattice;
        let per = l.tiles_per_side();
        let n = l.lattice_n;
        let old = GroundLattice::new(c).with_edits(&self.ground);
        let promoting = |t: u32| promote.binary_search(&t).is_ok();
        let mut buckets: Vec<FillBucket> = Vec::new();
        // Border vertices this edit already filled, ascending.
        let mut border_filled: Vec<u32> = Vec::new();
        for &tile in promote {
            let (a, b) = (tile % per, tile / per);
            let level = c.tile_levels[tile as usize];
            // A vertex is on the tile's coarse grid when both offsets are multiples of `2^level`.
            let off_grid = (1u32 << level) - 1;
            let (x0, x1) = l.tile_cells(a);
            let (z0, z1) = l.tile_cells(b);
            // The tiles that store this tile's vertices (`owner_tile`): its last column and row
            // belong to the next tile, except on the map's edge. Indexed `east + 2 * north`.
            let (east, north) = (u32::from(a + 1 < per), u32::from(b + 1 < per));
            let owners = [tile, tile + east, tile + north * per, tile + north * per + east];
            let blocks = owners.map(|owner| self.ground.block(owner));
            let mut bucket_of: [Option<usize>; 4] = [None; 4];
            // Position and lattice cell of every tile column and row (`locate` is per axis).
            let (x_first, z_first) = (vertex_m(l, x0), vertex_m(l, z0));
            let cols: Vec<(f32, Option<u32>)> = (x0..=x1)
                .map(|vi| {
                    let x = vertex_m(l, vi);
                    (x, old.locate(x, z_first).map(|cell| cell.i))
                })
                .collect();
            let rows: Vec<(f32, Option<u32>)> = (z0..=z1)
                .map(|vj| {
                    let z = vertex_m(l, vj);
                    (z, old.locate(x_first, z).map(|cell| cell.j))
                })
                .collect();
            let (first, end) = (z0 * n + x0, z1 * n + x1 + 1);
            let window = |list: &'_ [VertexHeightMm]| {
                let lo = list.partition_point(|v| v.vertex < first);
                let hi = lo + list[lo..].partition_point(|v| v.vertex < end);
                lo..hi
            };
            let ring = &c.ring_pins[window(&c.ring_pins)];
            let hanging = &c.hanging_overrides[window(&c.hanging_overrides)];
            let mut coarse = CoarseCell::default();
            for vj in z0..=z1 {
                let row_first = vj * n + x0;
                let mut rk = ring.partition_point(|v| v.vertex < row_first);
                let mut hk = hanging.partition_point(|v| v.vertex < row_first);
                let (z, cell_j) = rows[(vj - z0) as usize];
                let row_on_grid = (vj - z0) & off_grid == 0;
                let north_row = usize::from(vj == z1 && north == 1);
                for vi in x0..=x1 {
                    if row_on_grid && (vi - x0) & off_grid == 0 {
                        continue;
                    }
                    let vertex = vj * n + vi;
                    let which = usize::from(vi == x1 && east == 1) + 2 * north_row;
                    // A vertex with an override (a ring pin, a D1 edit, a hanging vertex) keeps it.
                    while ring.get(rk).is_some_and(|p| p.vertex < vertex) {
                        rk += 1;
                    }
                    while hanging.get(hk).is_some_and(|p| p.vertex < vertex) {
                        hk += 1;
                    }
                    if ring.get(rk).is_some_and(|p| p.vertex == vertex)
                        || hanging.get(hk).is_some_and(|p| p.vertex == vertex)
                        || (!blocks[which].is_empty()
                            && blocks[which].binary_search_by_key(&vertex, |e| e.0).is_ok())
                    {
                        continue;
                    }
                    let border = vi == x0 || vi == x1 || vj == z0 || vj == z1;
                    if border && border_filled.binary_search(&vertex).is_ok() {
                        continue;
                    }
                    // A border vertex next to a coarse tile that stays coarse takes that tile's edge.
                    let neighbor = if !border {
                        None
                    } else if vi == x0 && a > 0 {
                        Some(tile - 1)
                    } else if vi == x1 && a + 1 < per {
                        Some(tile + 1)
                    } else if vj == z0 && b > 0 {
                        Some(tile - per)
                    } else if vj == z1 && b + 1 < per {
                        Some(tile + per)
                    } else {
                        None
                    };
                    let source = match neighbor {
                        Some(nb) if !promoting(nb) && old.tile_level(nb) > 0 => nb,
                        _ => tile,
                    };
                    let (x, cell_i) = cols[(vi - x0) as usize];
                    let (Some(cell_i), Some(cell_j)) = (cell_i, cell_j) else {
                        continue;
                    };
                    let Some(y) = coarse.height(&old, source, [x, z], [cell_i, cell_j]) else {
                        continue;
                    };
                    let at = match bucket_of[which] {
                        Some(at) => at,
                        None => {
                            let owner = owners[which];
                            let at = buckets
                                .iter()
                                .position(|bucket| bucket.owner == owner)
                                .unwrap_or_else(|| {
                                    buckets.push(FillBucket {
                                        owner,
                                        fills: Vec::new(),
                                    });
                                    buckets.len() - 1
                                });
                            bucket_of[which] = Some(at);
                            at
                        }
                    };
                    buckets[at].fills.push((vertex, m_to_mm(y)));
                    if border && let Err(k) = border_filled.binary_search(&vertex) {
                        border_filled.insert(k, vertex);
                    }
                }
            }
        }
        // Two promoted tiles can fill one owner tile: its fills go in ascending vertex.
        for bucket in &mut buckets {
            if !bucket.fills.is_sorted_by_key(|e| e.0) {
                bucket.fills.sort_unstable_by_key(|e| e.0);
            }
        }
        buckets
    }

    fn tiles_to_promote(&self, geom: &WorldGeom, f: &Footprint) -> Vec<u32> {
        let c = geom.cooked();
        if c.ground == GroundMode::Full || f.rect.is_empty() {
            return Vec::new();
        }
        let l = &c.lattice;
        let per = l.tiles_per_side();
        let mut tiles = Vec::new();
        f.for_each_vertex(|i, j| {
            let (a0, a1) = tile_columns_of_vertex(l, i);
            let (b0, b1) = tile_columns_of_vertex(l, j);
            for b in b0..=b1 {
                for a in a0..=a1 {
                    let t = b * per + a;
                    if c.tile_levels.get(t as usize).copied().unwrap_or(0) > 0 && !self.ground.is_promoted(t)
                    {
                        tiles.push(t);
                    }
                }
            }
        });
        tiles.sort_unstable();
        tiles.dedup();
        tiles
    }

    /// The overrides promotion writes: every vertex of a promoted tile that is not on its coarse
    /// grid and has no override, from the coarse surface (or a coarser unpromoted neighbor's edge).
    /// The reference walk's form: one search per vertex, and every earlier fill compared.
    #[cfg(any(test, feature = "stats"))]
    fn promotion_fills(&self, geom: &WorldGeom, promote: &[u32]) -> Vec<Write> {
        let c = geom.cooked();
        let l = &c.lattice;
        let per = l.tiles_per_side();
        let n = l.lattice_n;
        let old = GroundLattice::new(c).with_edits(&self.ground);
        let promoting = |t: u32| promote.binary_search(&t).is_ok();
        let mut out: Vec<Write> = Vec::new();
        for &tile in promote {
            let (a, b) = (tile % per, tile / per);
            let level = c.tile_levels[tile as usize];
            let step = 1u32 << level;
            let (x0, x1) = l.tile_cells(a);
            let (z0, z1) = l.tile_cells(b);
            for vj in z0..=z1 {
                for vi in x0..=x1 {
                    if (vi - x0).is_multiple_of(step) && (vj - z0).is_multiple_of(step) {
                        continue;
                    }
                    let vertex = vj * n + vi;
                    if old.override_at(vertex).is_some() || out.iter().any(|w| w.vertex == vertex) {
                        continue;
                    }
                    // A border vertex next to a coarse tile that stays coarse takes that tile's edge.
                    let neighbor = if vi == x0 && a > 0 {
                        Some(tile - 1)
                    } else if vi == x1 && a + 1 < per {
                        Some(tile + 1)
                    } else if vj == z0 && b > 0 {
                        Some(tile - per)
                    } else if vj == z1 && b + 1 < per {
                        Some(tile + per)
                    } else {
                        None
                    };
                    let source = match neighbor {
                        Some(nb) if !promoting(nb) && old.tile_level(nb) > 0 => nb,
                        _ => tile,
                    };
                    let (x, z) = (vertex_m(l, vi), vertex_m(l, vj));
                    if let Some(y) = old.height_in_tile(source, x, z) {
                        out.push(Write {
                            vertex,
                            mm: m_to_mm(y),
                        });
                    }
                }
            }
        }
        out
    }

    /// The reference walk's planned writes: every footprint vertex by its own lookups.
    #[cfg(any(test, feature = "stats"))]
    fn profile_writes(&self, geom: &WorldGeom, edit: &GroundEdit, f: &Footprint) -> Vec<Write> {
        let c = geom.cooked();
        let l = &c.lattice;
        let n = l.lattice_n;
        let min_cover = i64::from(geom.settings().min_cover_mm);
        let ground = GroundLattice::new(c).with_edits(&self.ground);
        let mut out = Vec::new();
        f.for_each_vertex_reference(l, |i, j| {
            let vertex = j * n + i;
            if ground.is_ring_vertex(vertex) {
                return; // E5: the profile is clipped to zero on a mouth ring.
            }
            let Some(base) = vertex_mm(&ground, vertex) else {
                return;
            };
            let base = i64::from(base);
            let stop = || {
                let (x, z) = (vertex_m(l, i), vertex_m(l, j));
                geom.highest_roof_at_or_below(x, z, mm_to_m(0, base as i32))
                    .map(|roof| i64::from(m_to_mm(roof)) + min_cover)
            };
            let new = match edit {
                GroundEdit::Crater { depth_mm, .. } => {
                    let delta = f.delta_reference(l, i, j, *depth_mm);
                    if delta <= 0 {
                        return;
                    }
                    let lowered = base - delta;
                    match stop() {
                        // Never below the cover, never raised by the stop.
                        Some(stop) => lowered.max(base.min(stop)),
                        None => lowered,
                    }
                }
                GroundEdit::Berm { height_mm, .. } => {
                    let delta = f.delta_reference(l, i, j, *height_mm);
                    if delta <= 0 {
                        return;
                    }
                    base + delta
                }
                GroundEdit::Rect {
                    col,
                    row,
                    width,
                    heights_mm,
                    ..
                } => {
                    let h = i64::from(heights_mm[((j - row) * width + (i - col)) as usize]);
                    match stop() {
                        Some(stop) if h < base => h.max(base.min(stop)),
                        _ => h,
                    }
                }
            };
            let new = new.clamp(-i64::from(HEIGHT_LIMIT_MM), i64::from(HEIGHT_LIMIT_MM)) as i32;
            if i64::from(new) != base {
                out.push(Write { vertex, mm: new });
            }
        });
        out
    }

    /// The `TerrainHeightEdits` rectangles of `rect` (absolute mm, each at most
    /// [`MAX_EDIT_SIDE`] per side, row-major order of the pieces) appended to `out`.
    pub fn height_rects(&self, geom: &WorldGeom, rect: DirtyRect, out: &mut Vec<HeightRectMm>) {
        let lattice = GroundLattice::new(geom.cooked()).with_edits(&self.ground);
        let n = geom.cooked().lattice.lattice_n;
        let mut row = rect.row0;
        while row < rect.row1 {
            let height = (rect.row1 - row).min(MAX_EDIT_SIDE);
            let mut col = rect.col0;
            while col < rect.col1 {
                let width = (rect.col1 - col).min(MAX_EDIT_SIDE);
                let mut heights_mm = Vec::with_capacity((width * height) as usize);
                for j in row..row + height {
                    if j < n && col + width <= n {
                        self.row_heights(geom.cooked(), &lattice, j, col..col + width, &mut heights_mm);
                    } else {
                        // Off the lattice: the one-vertex form, whatever it answers there.
                        for i in col..col + width {
                            heights_mm.push(vertex_mm(&lattice, j * n + i).unwrap_or(0));
                        }
                    }
                }
                out.push(HeightRectMm {
                    col,
                    row,
                    width,
                    height,
                    heights_mm,
                });
                col += width;
            }
            row += height;
        }
    }

    /// The current heights of lattice vertices `cols` of row `j` (all on the lattice), millimetres,
    /// appended to `out`: `vertex_mm` of each (0 where it has none), read one owner tile at a time
    /// with the tile's count row, its override block and the cooked overrides by cursor (ADR-8818).
    fn row_heights(
        &self,
        c: &WorldGeomCooked,
        lattice: &GroundLattice<'_>,
        j: u32,
        cols: std::ops::Range<u32>,
        out: &mut Vec<i32>,
    ) {
        let l = &c.lattice;
        let n = l.lattice_n;
        let g = u32::from(l.ground_tile_cells.max(1));
        let per = l.tiles_per_side();
        let last = per.saturating_sub(1);
        let b = (j / g).min(last);
        let (offset_m, step_m) = (l.offset_m(), l.step_m());
        let row_first = j * n + cols.start;
        let mut rk = c.ring_pins.partition_point(|v| v.vertex < row_first);
        let mut hk = c.hanging_overrides.partition_point(|v| v.vertex < row_first);
        let mut i = cols.start;
        while i < cols.end {
            // The run of this row that one tile owns.
            let a = (i / g).min(last);
            let end = cols.end.min(if a == last { n } else { (a + 1) * g });
            let counts_of = TileCounts::of(c, a, b);
            let counts = counts_of.row(j, i, end);
            let block = self.ground.block(b * per + a);
            let mut k = block.partition_point(|e| e.0 < j * n + i);
            for vi in i..end {
                let vertex = j * n + vi;
                // A ring pin, then a D1 edit, then a hanging vertex, then the count.
                while c.ring_pins.get(rk).is_some_and(|p| p.vertex < vertex) {
                    rk += 1;
                }
                while block.get(k).is_some_and(|e| e.0 < vertex) {
                    k += 1;
                }
                while c.hanging_overrides.get(hk).is_some_and(|p| p.vertex < vertex) {
                    hk += 1;
                }
                let pin = c.ring_pins.get(rk).filter(|p| p.vertex == vertex);
                let edit = block.get(k).filter(|e| e.0 == vertex);
                let hanging = c.hanging_overrides.get(hk).filter(|p| p.vertex == vertex);
                let mm = if let Some(pin) = pin {
                    pin.height_mm
                } else if let Some(edit) = edit {
                    edit.1
                } else if let Some(hanging) = hanging {
                    hanging.height_mm
                } else {
                    let count = match counts {
                        Some(row) if !counts_of.shared_column(vi) => Some(row[(vi - i) as usize]),
                        _ => vertex_count(lattice, vi, j),
                    };
                    count.map_or(0, |count| m_to_mm(offset_m + (count as f32) * step_m))
                };
                out.push(mm);
            }
            i = end;
        }
    }

    /// Appends the byte-stable snapshot (spec 121 § 5.2 rule 5) to `out`: `u32` version, the
    /// 32-byte content hash, the generation (`u64`), the blocker words (`u64` each, ascending
    /// id), the promoted tiles (`u32` count, ascending `u32`), the overrides (`u32` count,
    /// ascending `u32` vertex with its `i32` mm) and the state hash (`u64`), all little-endian.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
        out.extend_from_slice(&self.content_hash);
        out.extend_from_slice(&self.generation.0.to_le_bytes());
        for w in &self.blockers {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out.extend_from_slice(&(self.ground.promoted.len() as u32).to_le_bytes());
        for t in &self.ground.promoted {
            out.extend_from_slice(&t.to_le_bytes());
        }
        let overrides = self.ground.overrides();
        out.extend_from_slice(&(overrides.len() as u32).to_le_bytes());
        for (v, mm) in overrides {
            out.extend_from_slice(&v.to_le_bytes());
            out.extend_from_slice(&mm.to_le_bytes());
        }
        out.extend_from_slice(&self.state_hash().to_le_bytes());
    }

    /// Restores a state from [`GeomState::encode`]'s bytes for `geom`.
    ///
    /// # Errors
    ///
    /// [`GeomSnapshotError`]: a newer version, another map, or corrupt bytes.
    pub fn decode(geom: &WorldGeom, bytes: &[u8]) -> Result<GeomState, GeomSnapshotError> {
        let mut r = Reader { bytes, at: 0 };
        let version = r.u32()?;
        if version > SNAPSHOT_VERSION {
            return Err(GeomSnapshotError::NewerVersion {
                found: version,
                supported: SNAPSHOT_VERSION,
            });
        }
        if version != SNAPSHOT_VERSION {
            return Err(GeomSnapshotError::Corrupt);
        }
        let hash = r.take(32)?;
        if hash != geom.content_hash() {
            return Err(GeomSnapshotError::OtherMap);
        }
        let mut state = geom.new_state();
        let c = geom.cooked();
        let l = c.lattice;
        state.generation = Generation(r.u64()?);
        let initial = state.blockers.clone();
        for (k, word) in state.blockers.iter_mut().enumerate() {
            *word = r.u64()?;
            let plugs = c.plugs.len();
            let valid = if (k + 1) * 64 <= plugs {
                u64::MAX
            } else {
                (1u64 << (plugs - k * 64)) - 1
            };
            if *word & !valid != 0 {
                return Err(GeomSnapshotError::Corrupt);
            }
            let mut diff = *word ^ initial[k];
            while diff != 0 {
                let bit = diff.trailing_zeros();
                state.acc ^= blocker_term(k as u32 * 64 + bit);
                diff &= diff - 1;
            }
        }
        state.active_blockers = state.blockers.iter().map(|w| w.count_ones()).sum();
        let tiles = r.u32()?;
        let mut last = None;
        for _ in 0..tiles {
            let t = r.u32()?;
            let level = c.tile_levels.get(t as usize).copied();
            if last.is_some_and(|p| t <= p) || !level.is_some_and(|lv| lv > 0) {
                return Err(GeomSnapshotError::Corrupt);
            }
            last = Some(t);
            state.ground.promoted.push(t);
            state.acc ^= tile_term(t);
        }
        let count = r.u32()?;
        let vertices = u64::from(l.lattice_n) * u64::from(l.lattice_n);
        let mut last = None;
        let mut blocks: Vec<(u32, Vec<(u32, i32)>)> = Vec::new();
        for _ in 0..count {
            let v = r.u32()?;
            let mm = r.i32()?;
            if last.is_some_and(|p| v <= p)
                || u64::from(v) >= vertices
                || mm.unsigned_abs() > HEIGHT_LIMIT_MM as u32
            {
                return Err(GeomSnapshotError::Corrupt);
            }
            last = Some(v);
            let tile = owner_tile(&l, v);
            match blocks.binary_search_by_key(&tile, |b| b.0) {
                Ok(k) => blocks[k].1.push((v, mm)),
                Err(k) => blocks.insert(k, (tile, vec![(v, mm)])),
            }
            state.acc ^= vertex_term(v, mm);
        }
        state.ground.blocks = blocks.into_iter().map(|(t, b)| (t, Arc::new(b))).collect();
        let stored = r.u64()?;
        if r.at != bytes.len() || stored != state.state_hash() {
            return Err(GeomSnapshotError::Corrupt);
        }
        if !state.ground.is_empty() {
            // The restored overrides bound the ground from here on (ADR-8818).
            state.ground.ranges_mut(geom);
        }
        Ok(state)
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], GeomSnapshotError> {
        let end = self.at.checked_add(n).ok_or(GeomSnapshotError::Corrupt)?;
        let s = self.bytes.get(self.at..end).ok_or(GeomSnapshotError::Corrupt)?;
        self.at = end;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32, GeomSnapshotError> {
        let mut b = [0u8; 4];
        b.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(b))
    }
    fn i32(&mut self) -> Result<i32, GeomSnapshotError> {
        Ok(self.u32()? as i32)
    }
    fn u64(&mut self) -> Result<u64, GeomSnapshotError> {
        let mut b = [0u8; 8];
        b.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(b))
    }
}

/// A vertex's current height, millimetres: its override, else `m_to_mm` of its count height from
/// the first tile holding it that stores it.
fn vertex_mm(lattice: &GroundLattice<'_>, vertex: u32) -> Option<i32> {
    let l = lattice.lattice();
    let n = l.lattice_n;
    if u64::from(vertex) >= u64::from(n) * u64::from(n) {
        return None;
    }
    if let Some(mm) = lattice.override_at(vertex) {
        return Some(mm);
    }
    let count = vertex_count(lattice, vertex % n, vertex / n)?;
    Some(m_to_mm(l.offset_m() + (count as f32) * l.step_m()))
}

/// The cooked count of lattice vertex `(i, j)`, from the first tile holding it that stores it.
fn vertex_count(lattice: &GroundLattice<'_>, i: u32, j: u32) -> Option<u16> {
    let l = lattice.lattice();
    let per = l.tiles_per_side();
    let (a0, a1) = tile_columns_of_vertex(l, i);
    let (b0, b1) = tile_columns_of_vertex(l, j);
    for b in b0..=b1 {
        for a in a0..=a1 {
            if let Some(count) = lattice.count_in_tile(b * per + a, i, j) {
                return Some(count);
            }
        }
    }
    None
}

/// The vertex rectangle of a ground tile.
fn tile_rect(l: &LatticeConstants, tile: u32) -> DirtyRect {
    let per = l.tiles_per_side();
    let (x0, x1) = l.tile_cells(tile % per);
    let (z0, z1) = l.tile_cells(tile / per);
    DirtyRect {
        col0: x0,
        row0: z0,
        col1: x1 + 1,
        row1: z1 + 1,
    }
}

/// The smallest vertex index whose coordinate is greater than `v_mm` (`cells + 1` if none).
fn first_above(l: &LatticeConstants, v_mm: i64) -> u32 {
    let (mut lo, mut hi) = (0u32, l.cells() + 1);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if l.vertex_mm(mid) > v_mm {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

/// Which walk applies an edit's profile.
#[derive(Clone, Copy)]
enum ProfileWalk {
    /// One pass per owner tile (ADR-8818).
    Tiles,
    /// The per-vertex walk, kept for the equality tests.
    #[cfg(any(test, feature = "stats"))]
    Reference,
}

/// The fills of one edit's promotions that one ground tile stores, ascending vertex.
struct FillBucket {
    owner: u32,
    fills: Vec<(u32, i32)>,
}

/// One edit's promotion fills, by the walk that planned them.
enum Fills {
    Buckets(Vec<FillBucket>),
    #[cfg(any(test, feature = "stats"))]
    Writes(Vec<Write>),
}

/// The coarse cell of a ground tile a run of promotion fills is read from: its four corners
/// (`v00`, `v01`, `v10`, `v11`, as `GroundLattice`'s triangles name them), read once.
#[derive(Default)]
struct CoarseCell {
    /// The source tile, its cell range `[x0, x1) x [z0, z1)` and its level.
    tile: Option<(u32, [u32; 4], u32)>,
    /// `(tile, i0, j0)` of `corners`.
    key: Option<(u32, u32, u32)>,
    corners: Option<[[f32; 3]; 4]>,
}

impl CoarseCell {
    /// `GroundLattice::height_in_tile(source, x, z)` for a point whose lattice cell is `cell`:
    /// the tile's coarse cell holding the point, the triangle on the point's side of the diagonal
    /// (the edge function from `(x0, z1)`), its plane height. `None` when a corner has no height.
    #[inline]
    fn height(
        &mut self,
        ground: &GroundLattice<'_>,
        source: u32,
        [x, z]: [f32; 2],
        cell: [u32; 2],
    ) -> Option<f32> {
        let l = ground.lattice();
        let ([tx0, tx1, tz0, tz1], level) = match self.tile {
            Some((tile, cells, level)) if tile == source => (cells, level),
            _ => {
                let per = l.tiles_per_side();
                let (tx0, tx1) = l.tile_cells(source % per);
                let (tz0, tz1) = l.tile_cells(source / per);
                let level = u32::from(ground.tile_level(source));
                self.tile = Some((source, [tx0, tx1, tz0, tz1], level));
                ([tx0, tx1, tz0, tz1], level)
            }
        };
        if tx1 <= tx0 || tz1 <= tz0 {
            return None;
        }
        let ci = cell[0].clamp(tx0, tx1 - 1);
        let cj = cell[1].clamp(tz0, tz1 - 1);
        // The coarse cell's first vertex: the offset rounded down to a multiple of `2^level`.
        let i0 = tx0 + (((ci - tx0) >> level) << level);
        let j0 = tz0 + (((cj - tz0) >> level) << level);
        if self.key != Some((source, i0, j0)) {
            let step = 1u32 << level;
            let (i1, j1) = (i0 + step, j0 + step);
            let corner = |vi: u32, vj: u32| {
                let y = ground.vertex_height_in_tile(source, vi, vj)?;
                Some([vertex_m(l, vi), y, vertex_m(l, vj)])
            };
            self.key = Some((source, i0, j0));
            self.corners = (|| Some([corner(i0, j0)?, corner(i0, j1)?, corner(i1, j0)?, corner(i1, j1)?]))();
        }
        let [v00, v01, v10, v11] = self.corners?;
        // Edge function of the diagonal from (x0, z1) to (x1, z0); negative on the v00 side.
        let e = (v10[0] - v01[0]) * (z - v01[2]) - (v10[2] - v01[2]) * (x - v01[0]);
        let tri = if e <= 0.0 {
            [v00, v01, v10]
        } else {
            [v10, v01, v11]
        };
        plane_height(tri, x, z)
    }
}

/// A ground tile's cooked counts as row slices, for the vertices the tile owns (ADR-8818).
///
/// A tile stored at full pitch keeps one count per owned vertex, row-major (the owned layout, or
/// the file layout with the east and north borders, as `GroundLattice::count_in_tile` tells them
/// apart). A vertex on the tile's first column or row also lies in the west or south neighbor's
/// range, and `vertex_count` asks that neighbor first: those are not served from the slice.
struct TileCounts<'a> {
    /// The count block and its row length; `None` for a tile stored coarser or of another length.
    rows: Option<(&'a [u16], u32)>,
    /// The tile's first vertex column and row.
    x0: u32,
    z0: u32,
    /// The first column and row when a west or south neighbor shares them.
    shared_col: Option<u32>,
    shared_row: Option<u32>,
}

impl<'a> TileCounts<'a> {
    fn of(c: &'a WorldGeomCooked, a: u32, b: u32) -> Self {
        let l = &c.lattice;
        let per = l.tiles_per_side();
        let tile = b * per + a;
        let (x0, x1) = l.tile_cells(a);
        let (z0, z1) = l.tile_cells(b);
        let level = c.tile_levels.get(tile as usize).copied().unwrap_or(0);
        let rows = c
            .ground_counts
            .get(tile as usize)
            .filter(|_| level == 0 && x1 >= x0 && z1 >= z0)
            .and_then(|block| {
                let (full_cols, full_rows) = (x1 - x0 + 1, z1 - z0 + 1);
                let owned_cols = if a + 1 == per { full_cols } else { full_cols - 1 };
                let owned_rows = if b + 1 == per { full_rows } else { full_rows - 1 };
                if block.len() == (owned_cols * owned_rows) as usize {
                    Some((block.as_slice(), owned_cols))
                } else if block.len() == (full_cols * full_rows) as usize {
                    Some((block.as_slice(), full_cols))
                } else {
                    None
                }
            });
        Self {
            rows,
            x0,
            z0,
            shared_col: (a > 0).then_some(x0),
            shared_row: (b > 0).then_some(z0),
        }
    }

    /// The counts of owned vertices `ia..ib` of row `j`; `None` when the tile has no slice or the
    /// row is shared with the tile below.
    #[inline]
    fn row(&self, j: u32, ia: u32, ib: u32) -> Option<&'a [u16]> {
        let (block, cols) = self.rows?;
        if self.shared_row == Some(j) || ia < self.x0 || j < self.z0 || ib < ia {
            return None;
        }
        let start = ((j - self.z0) * cols + (ia - self.x0)) as usize;
        block.get(start..start + (ib - ia) as usize)
    }

    /// Whether column `i` is shared with the tile to the west.
    #[inline]
    fn shared_column(&self, i: u32) -> bool {
        self.shared_col == Some(i)
    }
}

/// What the profile changed in one owner tile.
struct TileChange {
    tile: u32,
    /// The tile's whole new override block, ascending vertex.
    block: Vec<(u32, i32)>,
    /// XOR of the hash terms that left and entered.
    acc: u64,
    hash_terms: u32,
    vertices_written: u32,
    /// The lowest and highest height written, millimetres (`lo_mm > hi_mm` when only removals).
    lo_mm: i32,
    hi_mm: i32,
}

/// One edit's profile over its footprint, read one owner tile at a time (ADR-8818).
struct TileWalk<'a> {
    edit: &'a GroundEdit,
    f: &'a Footprint,
    /// The ring pins and the hanging overrides inside the footprint's vertex range.
    ring: &'a [VertexHeightMm],
    hanging: &'a [VertexHeightMm],
    /// The cooked ground, for the counts a row slice does not cover.
    ground: GroundLattice<'a>,
    stops: crate::query::RoofStops<'a>,
    min_cover: i64,
}

impl TileWalk<'_> {
    /// The profile over the footprint vertices ground tile `(a, b)` owns, against that tile's
    /// override block in `edits`. `None` when no vertex changes.
    ///
    /// A vertex is read exactly as `vertex_mm` reads it: its ring pin (never edited, E5), its D1
    /// override, its hanging override, then its count. The count comes from the tile's row slice
    /// when the tile is stored at full pitch and the vertex lies in no other tile's range; any
    /// other vertex takes `vertex_count`'s search.
    fn tile(&mut self, c: &WorldGeomCooked, edits: &GroundEdits, a: u32, b: u32) -> Option<TileChange> {
        let l = &c.lattice;
        let n = l.lattice_n;
        let per = l.tiles_per_side();
        let g = u32::from(l.ground_tile_cells.max(1));
        let tile = b * per + a;
        let rect = self.f.rect;
        // The vertices this tile owns (`owner_tile`): its cells' first vertices, and the map's
        // last column and row on the last tiles.
        let owned = |t: u32| (t * g, if t + 1 >= per { n } else { (t + 1) * g });
        let ((own_c0, own_c1), (own_r0, own_r1)) = (owned(a), owned(b));
        let (c0, c1) = (rect.col0.max(own_c0), rect.col1.min(own_c1));
        let (r0, r1) = (rect.row0.max(own_r0), rect.row1.min(own_r1));
        if c0 >= c1 || r0 >= r1 {
            return None;
        }
        let counts_of = TileCounts::of(c, a, b);
        let (offset_m, step_m) = (l.offset_m(), l.step_m());
        let limit = i64::from(HEIGHT_LIMIT_MM);
        let old = edits.block(tile);
        let mut k = 0usize;
        let mut change = TileChange {
            tile,
            block: Vec::with_capacity(old.len() + ((c1 - c0) * (r1 - r0)) as usize),
            acc: 0,
            hash_terms: 0,
            vertices_written: 0,
            lo_mm: i32::MAX,
            hi_mm: i32::MIN,
        };
        for j in r0..r1 {
            let Some((ia, ib)) = self.f.span(j, c0, c1) else {
                continue;
            };
            let row_first = j * n + ia;
            let mut rk = self.ring.partition_point(|v| v.vertex < row_first);
            let mut hk = self.hanging.partition_point(|v| v.vertex < row_first);
            let counts = counts_of.row(j, ia, ib);
            self.stops.begin_row((j - rect.row0) as usize);
            for i in ia..ib {
                let vertex = j * n + i;
                // The tile's block up to this vertex passes through.
                while let Some(&entry) = old.get(k).filter(|e| e.0 < vertex) {
                    change.block.push(entry);
                    k += 1;
                }
                let edited = old.get(k).filter(|e| e.0 == vertex).map(|e| e.1);
                if edited.is_some() {
                    k += 1;
                }
                let new = self.vertex(VertexRead {
                    i,
                    j,
                    vertex,
                    edited,
                    count: counts
                        .filter(|_| !counts_of.shared_column(i))
                        .map(|row| row[(i - ia) as usize]),
                    rk: &mut rk,
                    hk: &mut hk,
                    offset_m,
                    step_m,
                    limit,
                });
                let Some(entry) = new else {
                    change.block.extend(edited.map(|mm| (vertex, mm)));
                    continue;
                };
                if let Some(mm) = edited {
                    change.acc ^= vertex_term(vertex, mm);
                    change.hash_terms += 1;
                }
                if let Some(mm) = entry {
                    change.acc ^= vertex_term(vertex, mm);
                    change.hash_terms += 1;
                    change.block.push((vertex, mm));
                    change.lo_mm = change.lo_mm.min(mm);
                    change.hi_mm = change.hi_mm.max(mm);
                }
                change.vertices_written += 1;
            }
        }
        if change.vertices_written == 0 {
            return None;
        }
        change.block.extend_from_slice(old.get(k..).unwrap_or(&[]));
        Some(change)
    }

    /// One vertex of the profile: `Some(entry)` when its override entry changes to `entry` (`None`
    /// removes it, where the new height is the cooked override), `None` when it keeps its entry.
    #[inline]
    fn vertex(&mut self, v: VertexRead<'_>) -> Option<Option<i32>> {
        let VertexRead {
            i,
            j,
            vertex,
            edited,
            count,
            rk,
            hk,
            offset_m,
            step_m,
            limit,
        } = v;
        while self.ring.get(*rk).is_some_and(|p| p.vertex < vertex) {
            *rk += 1;
        }
        if self.ring.get(*rk).is_some_and(|p| p.vertex == vertex) {
            return None; // E5: the profile is clipped to zero on a mouth ring.
        }
        while self.hanging.get(*hk).is_some_and(|p| p.vertex < vertex) {
            *hk += 1;
        }
        let cooked = self
            .hanging
            .get(*hk)
            .filter(|p| p.vertex == vertex)
            .map(|p| p.height_mm);
        let base = match edited.or(cooked) {
            Some(mm) => mm,
            None => {
                let count = match count {
                    Some(count) => count,
                    None => vertex_count(&self.ground, i, j)?,
                };
                m_to_mm(offset_m + (count as f32) * step_m)
            }
        };
        let rect = self.f.rect;
        let base64 = i64::from(base);
        let min_cover = self.min_cover;
        let stops = &mut self.stops;
        let mut stop = || {
            stops
                .highest_at_or_below((i - rect.col0) as usize, mm_to_m(0, base))
                .map(|roof| i64::from(m_to_mm(roof)) + min_cover)
        };
        let new = match self.edit {
            GroundEdit::Crater { depth_mm, .. } => {
                let delta = self.f.delta(i, j, *depth_mm);
                if delta <= 0 {
                    return None;
                }
                let lowered = base64 - delta;
                match stop() {
                    // Never below the cover, never raised by the stop.
                    Some(stop) => lowered.max(base64.min(stop)),
                    None => lowered,
                }
            }
            GroundEdit::Berm { height_mm, .. } => {
                let delta = self.f.delta(i, j, *height_mm);
                if delta <= 0 {
                    return None;
                }
                base64 + delta
            }
            GroundEdit::Rect {
                col,
                row,
                width,
                heights_mm,
                ..
            } => {
                let h = i64::from(*heights_mm.get(((j - row) * width + (i - col)) as usize)?);
                // The stop only matters to a lowered vertex.
                match if h < base64 { stop() } else { None } {
                    Some(stop) => h.max(base64.min(stop)),
                    None => h,
                }
            }
        };
        let new = new.clamp(-limit, limit) as i32;
        if new == base {
            return None;
        }
        // An override equal to the cooked one is not stored.
        let entry = if cooked == Some(new) { None } else { Some(new) };
        (entry != edited).then_some(entry)
    }
}

/// One footprint vertex as [`TileWalk::vertex`] reads it.
struct VertexRead<'a> {
    i: u32,
    j: u32,
    vertex: u32,
    /// Its D1 override in the tile's block.
    edited: Option<i32>,
    /// Its count from the tile's row slice.
    count: Option<u16>,
    /// The row's cursors into the ring pins and the hanging overrides.
    rk: &'a mut usize,
    hk: &'a mut usize,
    offset_m: f32,
    step_m: f32,
    limit: i64,
}

/// A disc footprint: the centre and radius, and the squared offset of every candidate column and
/// row from the centre, so `d²` of a vertex is one add (spec 121 § 6 step 2, `i64`).
struct Disc {
    #[cfg(any(test, feature = "stats"))]
    cx: i64,
    #[cfg(any(test, feature = "stats"))]
    cz: i64,
    r: i64,
    /// The first candidate column and row: `dx2[i - col0]`, `dz2[j - row0]`.
    col0: u32,
    row0: u32,
    dx2: [i64; MAX_EDIT_SIDE as usize],
    dz2: [i64; MAX_EDIT_SIDE as usize],
}

impl Disc {
    /// `d²` of vertex `(i, j)` from the centre; `i64::MAX` outside the candidate box.
    #[inline]
    fn d2(&self, i: u32, j: u32) -> i64 {
        let dx2 = self.dx2.get(i.wrapping_sub(self.col0) as usize);
        let dz2 = self.dz2.get(j.wrapping_sub(self.row0) as usize);
        match (dx2, dz2) {
            (Some(dx2), Some(dz2)) => dx2 + dz2,
            _ => i64::MAX,
        }
    }

    /// The columns of `[c0, c1)` whose vertices on row `j` are inside the radius. The squared
    /// offset falls then rises along a row (vertex positions never decrease), so they are one run.
    fn span(&self, j: u32, c0: u32, c1: u32) -> Option<(u32, u32)> {
        let r2 = self.r * self.r;
        let (mut a, mut b) = (c0, c1);
        while a < b && self.d2(a, j) >= r2 {
            a += 1;
        }
        while b > a && self.d2(b - 1, j) >= r2 {
            b -= 1;
        }
        (a < b).then_some((a, b))
    }
}

/// An edit's vertex set: a disc (crater, berm) or a rectangle.
struct Footprint {
    rect: DirtyRect,
    disc: Option<Disc>,
}

impl Footprint {
    fn of(l: &LatticeConstants, edit: &GroundEdit) -> Result<Footprint, GeomEditError> {
        let cells = l.cells();
        match *edit {
            GroundEdit::Crater {
                x_mm,
                z_mm,
                radius_mm,
                depth_mm: amount,
            }
            | GroundEdit::Berm {
                x_mm,
                z_mm,
                radius_mm,
                height_mm: amount,
            } => {
                if !(1..=MAX_RADIUS_MM).contains(&radius_mm) || !(0..=MAX_DEPTH_MM).contains(&amount) {
                    return Err(GeomEditError::OutOfRange);
                }
                let (cx, cz, r) = (i64::from(x_mm), i64::from(z_mm), i64::from(radius_mm));
                // A disc wholly beyond the map's edge is refused; one between vertices edits nothing.
                let (first, last) = (l.vertex_mm(0), l.vertex_mm(cells));
                if [cx, cz].iter().any(|&c| c + r <= first || c - r >= last) {
                    return Err(GeomEditError::OutOfMap);
                }
                // Vertices strictly inside the radius along each axis: x(i) in (c - r, c + r).
                let axis = |c: i64| (first_above(l, c - r), first_above(l, c + r - 1).min(cells + 1));
                let ((i0, i1), (j0, j1)) = (axis(cx), axis(cz));
                if i1 <= i0 || j1 <= j0 {
                    return Ok(Footprint {
                        rect: DirtyRect::default(),
                        disc: None,
                    });
                }
                if i1 - i0 > MAX_EDIT_SIDE || j1 - j0 > MAX_EDIT_SIDE {
                    return Err(GeomEditError::TooLarge);
                }
                let mut disc = Disc {
                    #[cfg(any(test, feature = "stats"))]
                    cx,
                    #[cfg(any(test, feature = "stats"))]
                    cz,
                    r,
                    col0: i0,
                    row0: j0,
                    dx2: [0; MAX_EDIT_SIDE as usize],
                    dz2: [0; MAX_EDIT_SIDE as usize],
                };
                for (d2, i) in disc.dx2.iter_mut().zip(i0..i1) {
                    let dx = l.vertex_mm(i) - cx;
                    *d2 = dx * dx;
                }
                for (d2, j) in disc.dz2.iter_mut().zip(j0..j1) {
                    let dz = l.vertex_mm(j) - cz;
                    *d2 = dz * dz;
                }
                // Shrink to the box of the vertices actually inside the disc.
                let mut tight = DirtyRect::default();
                for j in j0..j1 {
                    if let Some((a, b)) = disc.span(j, i0, i1) {
                        tight = tight.union(DirtyRect {
                            col0: a,
                            row0: j,
                            col1: b,
                            row1: j + 1,
                        });
                    }
                }
                Ok(Footprint {
                    rect: tight,
                    disc: Some(disc),
                })
            }
            GroundEdit::Rect {
                col,
                row,
                width,
                height,
                ref heights_mm,
            } => {
                if width > MAX_EDIT_SIDE || height > MAX_EDIT_SIDE {
                    return Err(GeomEditError::TooLarge);
                }
                if width == 0
                    || height == 0
                    || u64::from(col) + u64::from(width) > u64::from(cells) + 1
                    || u64::from(row) + u64::from(height) > u64::from(cells) + 1
                {
                    return Err(GeomEditError::OutOfMap);
                }
                if heights_mm.len() as u64 != u64::from(width) * u64::from(height)
                    || heights_mm
                        .iter()
                        .any(|h| h.unsigned_abs() > HEIGHT_LIMIT_MM as u32)
                {
                    return Err(GeomEditError::OutOfRange);
                }
                Ok(Footprint {
                    rect: DirtyRect {
                        col0: col,
                        row0: row,
                        col1: col + width,
                        row1: row + height,
                    },
                    disc: None,
                })
            }
        }
    }

    /// The footprint's columns of `[c0, c1)` on row `j`: one run.
    #[inline]
    fn span(&self, j: u32, c0: u32, c1: u32) -> Option<(u32, u32)> {
        match &self.disc {
            Some(disc) => disc.span(j, c0, c1),
            None => (c0 < c1).then_some((c0, c1)),
        }
    }

    /// The integer profile at `(i, j)`: `(amount * (r² - d²)).div_euclid(r²)`, 0 outside.
    #[inline]
    fn delta(&self, i: u32, j: u32, amount: i32) -> i64 {
        let Some(disc) = &self.disc else {
            return 0;
        };
        let r2 = disc.r * disc.r;
        let d2 = disc.d2(i, j);
        if d2 >= r2 {
            return 0;
        }
        (i64::from(amount) * (r2 - d2)).div_euclid(r2)
    }

    /// Visits the footprint's vertices in ascending vertex index `j * n + i`.
    fn for_each_vertex(&self, mut f: impl FnMut(u32, u32)) {
        for j in self.rect.row0..self.rect.row1 {
            if let Some((a, b)) = self.span(j, self.rect.col0, self.rect.col1) {
                for i in a..b {
                    f(i, j);
                }
            }
        }
    }

    /// `d2` of vertex `(i, j)` from the disc centre by the lattice's own vertex positions, `i64`
    /// (the reference walk).
    #[cfg(any(test, feature = "stats"))]
    fn d2_reference(l: &LatticeConstants, (cx, cz): (i64, i64), i: u32, j: u32) -> i64 {
        let dx = l.vertex_mm(i) - cx;
        let dz = l.vertex_mm(j) - cz;
        dx * dx + dz * dz
    }

    /// [`Footprint::delta`] by the reference walk's `d2`.
    #[cfg(any(test, feature = "stats"))]
    fn delta_reference(&self, l: &LatticeConstants, i: u32, j: u32, amount: i32) -> i64 {
        let Some(disc) = &self.disc else {
            return 0;
        };
        let r2 = disc.r * disc.r;
        let d2 = Self::d2_reference(l, (disc.cx, disc.cz), i, j);
        if d2 >= r2 {
            return 0;
        }
        (i64::from(amount) * (r2 - d2)).div_euclid(r2)
    }

    /// [`Footprint::for_each_vertex`] by the reference walk: every vertex of the box, tested alone.
    #[cfg(any(test, feature = "stats"))]
    fn for_each_vertex_reference(&self, l: &LatticeConstants, mut f: impl FnMut(u32, u32)) {
        for j in self.rect.row0..self.rect.row1 {
            for i in self.rect.col0..self.rect.col1 {
                if let Some(disc) = &self.disc
                    && Self::d2_reference(l, (disc.cx, disc.cz), i, j) >= disc.r * disc.r
                {
                    continue;
                }
                f(i, j);
            }
        }
    }
}
