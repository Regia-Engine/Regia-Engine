// SPDX-License-Identifier: MPL-2.0
//! Runtime height patches (ADR-1595, DO-4): gameplay rewrites a rectangle of height samples
//! while the game runs (a blast crater, a collapsing tunnel roof, a dozer), and the drawn
//! terrain, the CPU map the simulation and picks read, the physics collider and the navmesh
//! follow, at a bounded cost per frame and in a deterministic order.
//!
//! # Flow
//!
//! 1. [`TerrainHeightPatches::request`] (or `terrain.patch_heights` on the agent protocol)
//!    validates a [`HeightPatch`] against the target map, quantises its heights to the map's
//!    16-bit grid (what the simulation, a save and a reload see) and queues it with an id.
//! 2. Once a frame, `apply_terrain_height_patches` ([`crate::TerrainPlugin`], `Update`) takes
//!    [`HeightBand`]s from the front of the queue until [`TerrainHeightPatches::budget`]
//!    samples are used ([`TerrainHeightPatches::take_frame`]): whole rows of the oldest patch
//!    first, a patch larger than the budget spread over several frames. The order depends only
//!    on the queue and the budget, never on timing.
//! 3. Each band is written into the ECS [`crate::Terrain`] map and its min/max pyramid (and
//!    noted on its [`crate::TerrainEdits`] when the editor tracks one), handed to the linked
//!    [`crate::TerrainSharedDraw`] (its own map, pyramid, GPU height texture, slope and
//!    height mips, shadow key and virtual-texture surface key, on its next prepare), and
//!    announced as a [`TerrainHeightsChanged`] message with the world box it touched.
//! 4. Physics and navigation read the messages: [`crate::CollisionHeightfield::patch`]
//!    updates a collider's heights over the band (holes cut as before) and a navmesh rebuilds
//!    the tiles under [`TerrainHeightsChanged::world_min`] .. `world_max`.
//!
//! An idle queue costs one uncontended lock per frame and touches nothing else.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use regia_app::prelude::*;
use thiserror::Error;

use crate::deform::DirtyRect;
use crate::sampler::TerrainMap;

/// Most samples one [`HeightPatch`] may hold (2048 × 2048).
pub const MAX_PATCH_SAMPLES: u64 = 1 << 22;

/// Most samples the queue holds at once; a request past it is refused
/// ([`HeightPatchError::QueueFull`]) rather than growing the backlog without bound.
pub const MAX_QUEUED_PATCH_SAMPLES: u64 = 1 << 24;

/// Samples written per frame unless the host sets another budget (a 256 × 256 rectangle:
/// about 0.3 ms of CPU and a 256 KB height upload).
pub const DEFAULT_PATCH_BUDGET: u32 = 65_536;

/// Largest per-frame budget [`TerrainHeightPatches::set_budget`] accepts.
pub const MAX_PATCH_BUDGET: u32 = 1 << 22;

/// Errors of a height patch request. [`Self::code`] is the agent protocol's error code.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum HeightPatchError {
    /// The rectangle has no samples.
    #[error("the patch is empty ({width}×{height} samples)")]
    Empty {
        /// Samples along X.
        width: u32,
        /// Samples along Z.
        height: u32,
    },
    /// The rectangle leaves the map.
    #[error(
        "patch at column {col}, row {row}, {width}×{height} samples leaves the {map_width}×{map_height} map"
    )]
    OutOfBounds {
        /// First column.
        col: u32,
        /// First row.
        row: u32,
        /// Samples along X.
        width: u32,
        /// Samples along Z.
        height: u32,
        /// Map columns.
        map_width: u32,
        /// Map rows.
        map_height: u32,
    },
    /// The height list does not match the rectangle.
    #[error("a {width}×{height} patch needs {expected} heights, got {got}")]
    BadLength {
        /// Samples along X.
        width: u32,
        /// Samples along Z.
        height: u32,
        /// `width × height`.
        expected: u64,
        /// Heights given.
        got: usize,
    },
    /// A height is NaN or infinite.
    #[error("height {index} of the patch is not finite")]
    NonFinite {
        /// Row-major index in the patch.
        index: usize,
    },
    /// The patch is larger than [`MAX_PATCH_SAMPLES`].
    #[error("a patch of {samples} samples is over the {MAX_PATCH_SAMPLES} sample limit; split it")]
    TooLarge {
        /// Samples in the patch.
        samples: u64,
    },
    /// The queue would pass [`MAX_QUEUED_PATCH_SAMPLES`].
    #[error("{queued} samples are queued; {adding} more would pass the {MAX_QUEUED_PATCH_SAMPLES} limit")]
    QueueFull {
        /// Samples waiting.
        queued: u64,
        /// Samples in the refused patch.
        adding: u64,
    },
    /// No terrain map is known to the queue yet (no linked draw, no `Terrain` seen).
    #[error("no terrain to patch: link a TerrainSharedDraw or spawn a Terrain first")]
    NoTarget,
}

impl HeightPatchError {
    /// The stable lowercase code the agent protocol reports (`book/agents/terrain-runtime.md`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Empty { .. } => "patch_empty",
            Self::OutOfBounds { .. } => "patch_out_of_bounds",
            Self::BadLength { .. } => "patch_bad_length",
            Self::NonFinite { .. } => "patch_non_finite",
            Self::TooLarge { .. } => "patch_too_large",
            Self::QueueFull { .. } => "patch_queue_full",
            Self::NoTarget => "patch_no_target",
        }
    }
}

/// A rectangle of new heights: `width × height` samples starting at sample `(col, row)`
/// (row 0 is the map's −Z edge, column 0 its −X edge), row-major with X fastest, in metres.
#[derive(Clone, Debug, PartialEq)]
pub struct HeightPatch {
    /// First sample column.
    pub col: u32,
    /// First sample row.
    pub row: u32,
    /// Samples along X.
    pub width: u32,
    /// Samples along Z.
    pub height: u32,
    /// Heights in metres, `width × height`, row-major. Clamped to the map's
    /// `[height_offset, height_offset + height_scale]` and rounded to its 16-bit step.
    pub heights: Vec<f32>,
}

/// The map a queue patches: its grid and height mapping (enough to validate and quantise).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PatchTarget {
    /// Sample columns.
    pub width: u32,
    /// Sample rows.
    pub height: u32,
    /// Metres from 16-bit sample 0 to 65535.
    pub height_scale: f32,
    /// Height of sample 0, metres.
    pub height_offset: f32,
    /// World width of the map (its X extent), metres (the map is centred on
    /// [`Self::grid_origin`]).
    pub world_width: f32,
    /// World depth of the map (its Z extent), metres ([`TerrainMap::world_depth`],
    /// ADR-8369): `world_width` for a square map.
    pub world_depth: f32,
    /// World XZ of the map's centre ([`TerrainMap::grid_origin`], ADR-8179); `[0, 0]` unless
    /// the host moved the grid.
    pub grid_origin: [f32; 2],
}

impl PatchTarget {
    /// The target of `map`.
    #[must_use]
    pub fn of(map: &TerrainMap) -> Self {
        Self {
            width: map.width(),
            height: map.height(),
            height_scale: map.height_scale(),
            height_offset: map.height_offset(),
            world_width: map.world_width(),
            world_depth: map.world_depth(),
            grid_origin: map.grid_origin(),
        }
    }

    /// World XZ corners `(min, max)` of a sample rectangle, as [`TerrainMap::sample_to_world`]
    /// places the samples.
    #[must_use]
    pub fn footprint(&self, rect: &DirtyRect) -> ([f32; 2], [f32; 2]) {
        let [ox, oz] = self.grid_origin;
        let x = |col: u32| crate::sampler::grid_to_world_axis(col, self.width, self.world_width, ox);
        let z = |row: u32| crate::sampler::grid_to_world_axis(row, self.height, self.world_depth, oz);
        let (c1, r1) = (rect.col_end.max(1) - 1, rect.row_end.max(1) - 1);
        ([x(rect.col_start), z(rect.row_start)], [x(c1), z(r1)])
    }

    /// The samples whose world positions ([`TerrainMap::sample_to_world`], grid origin
    /// included) lie inside the world XZ box `min ..= max`: the sample rectangle a world-space
    /// patch writes (ADR-8179). Empty when the box holds no sample or is not finite.
    ///
    /// Patch rectangles ([`HeightPatch`], `terrain.patch_heights`) are in the render map's
    /// sample grid; this is the one conversion from a world box to that grid.
    #[must_use]
    pub fn samples_within(&self, min: [f32; 2], max: [f32; 2]) -> DirtyRect {
        let empty = DirtyRect {
            col_start: 0,
            col_end: 0,
            row_start: 0,
            row_end: 0,
        };
        let finite = min.iter().chain(&max).all(|v| v.is_finite());
        if !finite
            || self.width < 2
            || self.height < 2
            || !(self.world_width.is_finite() && self.world_width > 0.0)
            || !(self.world_depth.is_finite() && self.world_depth > 0.0)
        {
            return empty;
        }
        // The same expression as `TerrainMap::world_to_grid`, so a sample position maps back
        // to its own index.
        let grid = |v: f32, origin: f32, n: u32, extent: f32| {
            crate::sampler::world_to_grid_axis(v, n, extent, origin)
        };
        let span = |lo: f32, hi: f32, n: u32| -> Option<(u32, u32)> {
            let (first, last) = (lo.ceil().max(0.0), hi.floor().min((n - 1) as f32));
            (first <= last).then(|| (first as u32, last as u32 + 1))
        };
        let cols = span(
            grid(min[0], self.grid_origin[0], self.width, self.world_width),
            grid(max[0], self.grid_origin[0], self.width, self.world_width),
            self.width,
        );
        let rows = span(
            grid(min[1], self.grid_origin[1], self.height, self.world_depth),
            grid(max[1], self.grid_origin[1], self.height, self.world_depth),
            self.height,
        );
        match (cols, rows) {
            (Some((col_start, col_end)), Some((row_start, row_end))) => DirtyRect {
                col_start,
                col_end,
                row_start,
                row_end,
            },
            _ => empty,
        }
    }

    /// `h` metres as the map's 16-bit sample (clamped), the way a save writes it
    /// (`TerrainMap::to_heightfield`).
    #[must_use]
    pub fn quantize(&self, h: f32) -> u16 {
        let inv = if self.height_scale.abs() > f32::EPSILON {
            65535.0 / self.height_scale
        } else {
            0.0
        };
        ((h - self.height_offset) * inv).round().clamp(0.0, 65535.0) as u16
    }

    /// The height of 16-bit sample `s`, bit for bit what loading a heightfield gives
    /// ([`TerrainMap::new`]).
    #[must_use]
    pub fn dequantize(&self, s: u16) -> f32 {
        f32::from(s) / 65535.0 * self.height_scale + self.height_offset
    }

    fn check_rect(&self, col: u32, row: u32, width: u32, height: u32) -> Result<u64, HeightPatchError> {
        if width == 0 || height == 0 {
            return Err(HeightPatchError::Empty { width, height });
        }
        let inside = u64::from(col) + u64::from(width) <= u64::from(self.width)
            && u64::from(row) + u64::from(height) <= u64::from(self.height);
        if !inside {
            return Err(HeightPatchError::OutOfBounds {
                col,
                row,
                width,
                height,
                map_width: self.width,
                map_height: self.height,
            });
        }
        let samples = u64::from(width) * u64::from(height);
        if samples > MAX_PATCH_SAMPLES {
            return Err(HeightPatchError::TooLarge { samples });
        }
        Ok(samples)
    }
}

/// Resamples a coarse **cell-spaced** grid onto the samples `rect` of a finer cell-spaced
/// render grid over the same world square, in integer millimetres (ADR-8179): the heights a
/// [`HeightPatch`] needs so a crater the simulation computed on its coarse grid lands on the
/// drawn map with the simulation's heights kept exactly at the coarse samples.
///
/// Both grids place sample `i` of `n` at `(i − ⌊n / 2⌋) · world_width / n` (DO27's simulation
/// grid; a render map matched to it with [`TerrainMap::with_grid_origin`]). Render sample `j`
/// then sits at coarse coordinate `u = ((j − ⌊n_r / 2⌋) · n_c + ⌊n_c / 2⌋ · n_r) / n_r`, an
/// exact fraction. The rule:
///
/// - a render sample that coincides with a coarse sample (`u` whole on both axes) copies it;
/// - every other render sample is the bilinear of the four coarse samples around it, with the
///   weights `u`'s remainders over `n_r`, summed in integers and rounded to the nearest
///   millimetre (halves toward +∞);
/// - past the last coarse sample the grid clamps to its border, as [`TerrainMap::sample_bilinear`]
///   does.
///
/// `coarse_mm` is row-major (row 0 at −Z, X fastest), `coarse_side²` values; the result is
/// row-major over `rect`, X fastest, as [`HeightPatch::heights`] wants (divide by 1000 for
/// metres). DO27's 256-sample, 5 m simulation grid on its 4096-sample render map: render
/// sample `16 · i` is coarse sample `i`.
///
/// # Errors
///
/// [`HeightPatchError::BadLength`] when `coarse_mm` is not `coarse_side²` values (a zero side
/// included), [`HeightPatchError::Empty`] / [`HeightPatchError::OutOfBounds`] /
/// [`HeightPatchError::TooLarge`] for `rect` on a `render_side²` grid.
pub fn resample_cell_grid_mm(
    coarse_mm: &[i32],
    coarse_side: u32,
    render_side: u32,
    rect: &DirtyRect,
) -> Result<Vec<i32>, HeightPatchError> {
    let expected = u64::from(coarse_side) * u64::from(coarse_side);
    if coarse_side == 0 || coarse_mm.len() as u64 != expected {
        return Err(HeightPatchError::BadLength {
            width: coarse_side,
            height: coarse_side,
            expected,
            got: coarse_mm.len(),
        });
    }
    let (width, height) = (
        rect.col_end.saturating_sub(rect.col_start),
        rect.row_end.saturating_sub(rect.row_start),
    );
    let target = PatchTarget {
        width: render_side,
        height: render_side,
        height_scale: 1.0,
        height_offset: 0.0,
        world_width: 1.0,
        world_depth: 1.0,
        grid_origin: [0.0, 0.0],
    };
    target.check_rect(rect.col_start, rect.row_start, width, height)?;
    let (n_c, n_r) = (i64::from(coarse_side), i64::from(render_side));
    let (h_c, h_r) = (n_c / 2, n_r / 2);
    // Coarse index and remainder (over n_r) of render sample `j`, border-clamped.
    let axis = |j: u32| -> (usize, usize, i64) {
        let num = (i64::from(j) - h_r) * n_c + h_c * n_r;
        let (i0, f) = (num.div_euclid(n_r), num.rem_euclid(n_r));
        if i0 < 0 {
            (0, 0, 0)
        } else if i0 >= n_c - 1 {
            let last = (n_c - 1) as usize;
            (last, last, 0)
        } else {
            (i0 as usize, i0 as usize + 1, f)
        }
    };
    let side = coarse_side as usize;
    let at = |c: usize, r: usize| i128::from(coarse_mm[r * side + c]);
    let (r, d) = (i128::from(n_r), i128::from(n_r) * i128::from(n_r));
    let mut out = Vec::with_capacity(width as usize * height as usize);
    for row in rect.row_start..rect.row_end {
        let (z0, z1, fz) = axis(row);
        let fz = i128::from(fz);
        for col in rect.col_start..rect.col_end {
            let (x0, x1, fx) = axis(col);
            let fx = i128::from(fx);
            let sum = at(x0, z0) * (r - fx) * (r - fz)
                + at(x1, z0) * fx * (r - fz)
                + at(x0, z1) * (r - fx) * fz
                + at(x1, z1) * fx * fz;
            // Nearest millimetre, halves toward +∞; a weighted mean of i32 values fits i32.
            let mm = (2 * sum + d).div_euclid(2 * d);
            out.push(i32::try_from(mm).unwrap_or(if mm < 0 { i32::MIN } else { i32::MAX }));
        }
    }
    Ok(out)
}

/// Some rows of one patch, applied in one frame: new 16-bit samples over `rect`.
#[derive(Clone, Debug, PartialEq)]
pub struct HeightBand {
    /// Id of the patch the rows belong to ([`TerrainHeightPatches::request`]).
    pub patch: u64,
    /// The samples the band writes.
    pub rect: DirtyRect,
    /// New samples over `rect`, row-major, X fastest.
    pub samples: Arc<[u16]>,
    /// The height mapping the samples are in.
    pub target: PatchTarget,
}

impl HeightBand {
    /// Writes the band into `map` and returns the lowest and highest height over the band,
    /// before and after (the vertical extent a navmesh or collider must refresh). Nothing is
    /// written when `map` is not the band's grid (another map was swapped in).
    pub fn apply(&self, map: &mut TerrainMap) -> Option<(f32, f32)> {
        if map.width() != self.target.width || map.height() != self.target.height {
            return None;
        }
        let w = (self.rect.col_end - self.rect.col_start) as usize;
        let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
        for (r, row) in (self.rect.row_start..self.rect.row_end).enumerate() {
            let base = row as usize * map.width() as usize + self.rect.col_start as usize;
            for (c, &s) in self.samples[r * w..(r + 1) * w].iter().enumerate() {
                let old = map.height_at_index(base + c);
                let new = self.target.dequantize(s);
                lo = lo.min(old).min(new);
                hi = hi.max(old).max(new);
                map.set_height(base + c, new);
            }
        }
        Some((lo, hi))
    }

    /// Writes the band into `map` like [`Self::apply`] and returns the smallest rectangle that
    /// holds every sample whose height changed: `Some(None)` when the band wrote the heights
    /// the map already held, `None` when `map` is not the band's grid (nothing written). A
    /// draw redraws only that rectangle, and nothing at all for a band that changed nothing
    /// (ADR-2450).
    pub(crate) fn apply_changed(&self, map: &mut TerrainMap) -> Option<Option<DirtyRect>> {
        if map.width() != self.target.width || map.height() != self.target.height {
            return None;
        }
        let w = (self.rect.col_end - self.rect.col_start) as usize;
        let mut changed = None;
        for (r, row) in (self.rect.row_start..self.rect.row_end).enumerate() {
            let base = row as usize * map.width() as usize + self.rect.col_start as usize;
            for (c, &s) in self.samples[r * w..(r + 1) * w].iter().enumerate() {
                let new = self.target.dequantize(s);
                if map.height_at_index(base + c).to_bits() != new.to_bits() {
                    map.set_height(base + c, new);
                    changed = Some(DirtyRect::include(changed, self.rect.col_start + c as u32, row));
                }
            }
        }
        Some(changed)
    }

    /// Samples in the band.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// `true` when the band writes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// A patch waiting in the queue, with the rows already applied.
#[derive(Debug)]
struct QueuedPatch {
    id: u64,
    col: u32,
    row: u32,
    width: u32,
    height: u32,
    samples: Vec<u16>,
    target: PatchTarget,
    next_row: u32,
}

/// What the last [`TerrainHeightPatches::take_frame`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PatchFrameStats {
    /// Samples written.
    pub samples: u32,
    /// Bands taken.
    pub bands: u32,
    /// Patches finished.
    pub completed: u32,
}

/// A copy of a queue's state ([`TerrainHeightPatches::state`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PatchQueueState {
    /// The map the queue patches, once known.
    pub target: Option<PatchTarget>,
    /// Patches waiting (the front one may be partly applied).
    pub queued_patches: usize,
    /// Samples still to write.
    pub queued_samples: u64,
    /// Samples written per frame at most.
    pub budget: u32,
    /// Id the next request gets.
    pub next_id: u64,
    /// Id of the last patch fully applied (0 = none). Patches finish in id order.
    pub completed: u64,
    /// Samples written since the queue was made.
    pub applied_samples: u64,
    /// Bands applied since the queue was made.
    pub generation: u64,
    /// The last frame's work.
    pub last_frame: PatchFrameStats,
    /// A draw owns this queue ([`crate::TerrainSharedDraw::height_patches_handle`]).
    pub draw_linked: bool,
    /// Bands the draw has not applied yet.
    pub draw_pending: usize,
}

#[derive(Debug)]
struct PatchShared {
    target: Option<PatchTarget>,
    queue: VecDeque<QueuedPatch>,
    queued_samples: u64,
    budget: u32,
    next_id: u64,
    completed: u64,
    applied_samples: u64,
    generation: u64,
    last_frame: PatchFrameStats,
    draw_linked: bool,
    draw_pending: Vec<HeightBand>,
    /// [`TerrainHeightEdits::push`]es since the last [`TerrainHeightPatches::take_frame`]
    /// (ADR-7585: at most [`MAX_EDITS_PER_FRAME`]).
    edits_this_frame: u32,
}

impl Default for PatchShared {
    fn default() -> Self {
        Self {
            target: None,
            queue: VecDeque::new(),
            queued_samples: 0,
            budget: DEFAULT_PATCH_BUDGET,
            next_id: 1,
            completed: 0,
            applied_samples: 0,
            generation: 0,
            last_frame: PatchFrameStats::default(),
            draw_linked: false,
            draw_pending: Vec::new(),
            edits_this_frame: 0,
        }
    }
}

/// The runtime height patch queue of one terrain (module docs), shared between the agent,
/// gameplay, `apply_terrain_height_patches` and a [`crate::TerrainSharedDraw`] boxed into the
/// frame renderer. [`crate::TerrainPlugin`] inserts an unlinked one; a host with a drawn terrain
/// replaces it with `draw.height_patches_handle()`.
#[derive(Resource, Clone, Debug, Default)]
pub struct TerrainHeightPatches(Arc<Mutex<PatchShared>>);

impl TerrainHeightPatches {
    pub(crate) fn linked(map: &TerrainMap) -> Self {
        let handle = Self::default();
        {
            let mut shared = handle.lock();
            shared.draw_linked = true;
            shared.target = Some(PatchTarget::of(map));
        }
        handle
    }

    fn lock(&self) -> MutexGuard<'_, PatchShared> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sets the map the queue patches (the system does this from the first `Terrain` when no
    /// draw is linked). Patches already queued keep the mapping they were quantised with.
    pub fn set_target(&self, target: PatchTarget) {
        self.lock().target = Some(target);
    }

    /// The map the queue patches, once known.
    #[must_use]
    pub fn target(&self) -> Option<PatchTarget> {
        self.lock().target
    }

    /// Queues `patch` and returns its id. Heights are clamped and quantised to the target's
    /// 16-bit grid now, so every consumer writes the same values.
    ///
    /// # Errors
    ///
    /// [`HeightPatchError`]: no target, an empty or out-of-map rectangle, a length mismatch,
    /// a non-finite height, a patch over [`MAX_PATCH_SAMPLES`] or a full queue. Nothing is
    /// queued on error.
    pub fn request(&self, patch: HeightPatch) -> Result<u64, HeightPatchError> {
        let target = self.target().ok_or(HeightPatchError::NoTarget)?;
        let samples = target.check_rect(patch.col, patch.row, patch.width, patch.height)?;
        if patch.heights.len() as u64 != samples {
            return Err(HeightPatchError::BadLength {
                width: patch.width,
                height: patch.height,
                expected: samples,
                got: patch.heights.len(),
            });
        }
        if let Some(index) = patch.heights.iter().position(|h| !h.is_finite()) {
            return Err(HeightPatchError::NonFinite { index });
        }
        let quantized = patch.heights.iter().map(|&h| target.quantize(h)).collect();
        self.push(target, patch.col, patch.row, patch.width, patch.height, quantized)
    }

    /// Queues 16-bit samples (the simulation's own values, unchanged) over the rectangle and
    /// returns the patch id.
    ///
    /// # Errors
    ///
    /// As [`Self::request`].
    pub fn request_samples(
        &self,
        col: u32,
        row: u32,
        width: u32,
        height: u32,
        samples: Vec<u16>,
    ) -> Result<u64, HeightPatchError> {
        let target = self.target().ok_or(HeightPatchError::NoTarget)?;
        let count = target.check_rect(col, row, width, height)?;
        if samples.len() as u64 != count {
            return Err(HeightPatchError::BadLength {
                width,
                height,
                expected: count,
                got: samples.len(),
            });
        }
        self.push(target, col, row, width, height, samples)
    }

    fn push(
        &self,
        target: PatchTarget,
        col: u32,
        row: u32,
        width: u32,
        height: u32,
        samples: Vec<u16>,
    ) -> Result<u64, HeightPatchError> {
        let mut shared = self.lock();
        Self::push_locked(&mut shared, target, col, row, width, height, samples)
    }

    fn push_locked(
        shared: &mut PatchShared,
        target: PatchTarget,
        col: u32,
        row: u32,
        width: u32,
        height: u32,
        samples: Vec<u16>,
    ) -> Result<u64, HeightPatchError> {
        let adding = samples.len() as u64;
        if shared.queued_samples + adding > MAX_QUEUED_PATCH_SAMPLES {
            return Err(HeightPatchError::QueueFull {
                queued: shared.queued_samples,
                adding,
            });
        }
        let id = shared.next_id;
        shared.next_id += 1;
        shared.queued_samples += adding;
        shared.queue.push_back(QueuedPatch {
            id,
            col,
            row,
            width,
            height,
            samples,
            target,
            next_row: 0,
        });
        Ok(id)
    }

    /// Sets the samples written per frame (clamped to `1..=`[`MAX_PATCH_BUDGET`]).
    pub fn set_budget(&self, samples: u32) {
        self.lock().budget = samples.clamp(1, MAX_PATCH_BUDGET);
    }

    /// Samples written per frame at most (a single row wider than the budget is still
    /// written whole, alone in its frame).
    #[must_use]
    pub fn budget(&self) -> u32 {
        self.lock().budget
    }

    /// `true` when nothing is queued.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.lock().queue.is_empty()
    }

    /// A copy of the queue's state.
    #[must_use]
    pub fn state(&self) -> PatchQueueState {
        let shared = self.lock();
        PatchQueueState {
            target: shared.target,
            queued_patches: shared.queue.len(),
            queued_samples: shared.queued_samples,
            budget: shared.budget,
            next_id: shared.next_id,
            completed: shared.completed,
            applied_samples: shared.applied_samples,
            generation: shared.generation,
            last_frame: shared.last_frame,
            draw_linked: shared.draw_linked,
            draw_pending: shared.draw_pending.len(),
        }
    }

    /// Takes this frame's bands from the front of the queue: whole rows, oldest patch first,
    /// until the budget is used (module docs). A linked draw gets a copy of each band for its
    /// next prepare. Called once a frame by `apply_terrain_height_patches`; a host without
    /// [`crate::TerrainPlugin`] calls it itself.
    pub fn take_frame(&self) -> Vec<HeightBand> {
        let mut shared = self.lock();
        shared.edits_this_frame = 0;
        let mut bands = Vec::new();
        let mut stats = PatchFrameStats::default();
        let mut left = u64::from(shared.budget);
        while let Some(front) = shared.queue.front_mut() {
            let width = u64::from(front.width);
            let rows_left = front.height - front.next_row;
            // Whole rows that fit; one row anyway when the frame has written nothing yet.
            let fit = (left / width).min(u64::from(rows_left)) as u32;
            let rows = if fit == 0 && stats.samples == 0 { 1 } else { fit };
            if rows == 0 {
                break;
            }
            let start = front.next_row as usize * front.width as usize;
            let end = start + rows as usize * front.width as usize;
            let band = HeightBand {
                patch: front.id,
                rect: DirtyRect {
                    col_start: front.col,
                    col_end: front.col + front.width,
                    row_start: front.row + front.next_row,
                    row_end: front.row + front.next_row + rows,
                },
                samples: Arc::from(&front.samples[start..end]),
                target: front.target,
            };
            front.next_row += rows;
            let written = u64::from(rows) * width;
            left = left.saturating_sub(written);
            stats.samples = stats.samples.saturating_add(written as u32);
            stats.bands += 1;
            let done = front.next_row >= front.height;
            let id = front.id;
            if done {
                shared.queue.pop_front();
                shared.completed = id;
                stats.completed += 1;
            }
            shared.queued_samples -= written;
            bands.push(band);
            if left == 0 {
                break;
            }
        }
        shared.applied_samples += u64::from(stats.samples);
        shared.generation += u64::from(stats.bands);
        shared.last_frame = stats;
        if shared.draw_linked {
            shared.draw_pending.extend(bands.iter().cloned());
        }
        bands
    }

    /// Whether a draw takes this queue's bands.
    fn draw_is_linked(&self) -> bool {
        self.lock().draw_linked
    }

    /// Queues `rect` of `map` for the linked draw only: the ECS map already holds these heights.
    fn send_to_draw(&self, map: &TerrainMap, rect: DirtyRect) {
        let target = PatchTarget::of(map);
        let width = map.width() as usize;
        let samples: Arc<[u16]> = (rect.row_start..rect.row_end)
            .flat_map(|row| {
                (rect.col_start..rect.col_end).map(move |col| row as usize * width + col as usize)
            })
            .map(|i| target.quantize(map.height_at_index(i)))
            .collect();
        self.lock().draw_pending.push(HeightBand {
            patch: 0,
            rect,
            samples,
            target,
        });
    }

    /// The bands a linked draw has not applied yet, in order.
    pub(crate) fn take_draw_bands(&self) -> Vec<HeightBand> {
        let mut shared = self.lock();
        if shared.draw_pending.is_empty() {
            return Vec::new();
        }
        std::mem::take(&mut shared.draw_pending)
    }
}

/// Most samples along either side of one [`HeightEdit`] (DO27 contract, spec 25 § 7.4).
pub const MAX_EDIT_SIDE: u32 = 128;

/// Most [`HeightEdit`]s [`TerrainHeightEdits::push`] accepts between two applied frames
/// ([`TerrainHeightPatches::take_frame`]); the next push is refused
/// ([`HeightEditError::FrameFull`]).
pub const MAX_EDITS_PER_FRAME: u32 = 16;

impl PatchTarget {
    /// `mm` absolute millimetres as the map's 16-bit sample, or `None` when the height is
    /// outside the map's `[height_offset, height_offset + height_scale]` range (by more than
    /// half a step). Computed in `f64` from the integer, so it is the same on every machine.
    #[must_use]
    pub fn quantize_mm(&self, mm: i32) -> Option<u16> {
        let metres = f64::from(mm) * 1.0e-3 - f64::from(self.height_offset);
        let scale = f64::from(self.height_scale);
        let s = if scale > f64::from(f32::EPSILON) {
            (metres * 65535.0 / scale).round()
        } else if metres.abs() < 5.0e-4 {
            0.0
        } else {
            return None;
        };
        (0.0..=65535.0).contains(&s).then_some(s as u16)
    }
}

/// One rectangle of the DO27 contract (spec 25 § 7.4, ADR-7585): `width × height` render-grid
/// samples from sample `(col, row)` (row 0 is the map's −Z edge, column 0 its −X edge),
/// absolute heights in integer millimetres, row-major with X fastest. An absolute replace:
/// a lost or repeated edit cannot drift the surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeightEdit {
    /// First sample column.
    pub col: u32,
    /// First sample row.
    pub row: u32,
    /// Samples along X, at most [`MAX_EDIT_SIDE`].
    pub width: u32,
    /// Samples along Z, at most [`MAX_EDIT_SIDE`].
    pub height: u32,
    /// `width × height` absolute heights in millimetres, row-major. Metres = `mm × 0.001`.
    pub heights_mm: Vec<i32>,
}

/// Why [`TerrainHeightEdits::push`] refused an edit. [`Self::code`] is the stable code
/// (`book/agents/terrain-runtime.md`); nothing is queued on error.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum HeightEditError {
    /// A side is over [`MAX_EDIT_SIDE`] samples.
    #[error("a {width}×{height} edit is over the {MAX_EDIT_SIDE}×{MAX_EDIT_SIDE} sample limit; split it")]
    TooLarge {
        /// Samples along X.
        width: u32,
        /// Samples along Z.
        height: u32,
    },
    /// [`MAX_EDITS_PER_FRAME`] edits were pushed since the last applied frame.
    #[error("{pushed} edits were pushed this frame; the limit is {MAX_EDITS_PER_FRAME}")]
    FrameFull {
        /// Edits accepted since the last applied frame.
        pushed: u32,
    },
    /// A height is outside the map's 16-bit range.
    #[error("height {index} of the edit ({mm} mm) is outside the map's height range")]
    OutOfRange {
        /// Row-major index in the edit.
        index: usize,
        /// The height given, millimetres.
        mm: i32,
    },
    /// The rectangle, its length, the target or the queue refused it, as for a [`HeightPatch`].
    #[error(transparent)]
    Patch(#[from] HeightPatchError),
}

impl HeightEditError {
    /// The stable lowercase code: `edit_too_large`, `edit_frame_full`, `edit_out_of_range`, or
    /// the [`HeightPatchError::code`] of a rectangle, length, target or queue refusal.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::TooLarge { .. } => "edit_too_large",
            Self::FrameFull { .. } => "edit_frame_full",
            Self::OutOfRange { .. } => "edit_out_of_range",
            Self::Patch(e) => e.code(),
        }
    }
}

/// The DO27 contract's handle for runtime terrain edits (spec 25 § 7.4, ADR-7585): a thin
/// adapter over [`TerrainHeightPatches`] that takes [`HeightEdit`]s in integer millimetres,
/// converts them to the map's 16-bit grid and enforces the contract's limits. Edits share the
/// patch queue: they apply in push order (a later edit wins on overlap), every edit pushed
/// before a frame's apply is taken in that frame unless the per-frame sample budget
/// ([`TerrainHeightPatches::set_budget`]) spreads it, and they reach the draw, the ECS map,
/// physics and navigation exactly like a [`HeightPatch`]. Cheap to clone, `Send + Sync`.
#[derive(Resource, Clone, Debug)]
pub struct TerrainHeightEdits {
    patches: TerrainHeightPatches,
}

impl TerrainHeightEdits {
    /// An adapter over `patches` (the draw's [`crate::TerrainSharedDraw::height_patches_handle`]
    /// or the [`TerrainHeightPatches`] resource).
    #[must_use]
    pub fn new(patches: TerrainHeightPatches) -> Self {
        Self { patches }
    }

    /// The queue the edits go into.
    #[must_use]
    pub fn patches(&self) -> &TerrainHeightPatches {
        &self.patches
    }

    /// Queues `edit` and returns its patch id (finished once
    /// [`PatchQueueState::completed`] reaches it).
    ///
    /// # Errors
    ///
    /// [`HeightEditError`]: a side over [`MAX_EDIT_SIDE`], more than [`MAX_EDITS_PER_FRAME`]
    /// edits since the last applied frame, a height outside the map's range, or any
    /// [`HeightPatchError`] (no target, empty or out-of-map rectangle, wrong length, full queue).
    pub fn push(&self, edit: HeightEdit) -> Result<u64, HeightEditError> {
        if edit.width > MAX_EDIT_SIDE || edit.height > MAX_EDIT_SIDE {
            return Err(HeightEditError::TooLarge {
                width: edit.width,
                height: edit.height,
            });
        }
        let target = self.patches.target().ok_or(HeightPatchError::NoTarget)?;
        let count = target.check_rect(edit.col, edit.row, edit.width, edit.height)?;
        if edit.heights_mm.len() as u64 != count {
            return Err(HeightPatchError::BadLength {
                width: edit.width,
                height: edit.height,
                expected: count,
                got: edit.heights_mm.len(),
            }
            .into());
        }
        let samples = edit
            .heights_mm
            .iter()
            .enumerate()
            .map(|(index, &mm)| {
                target
                    .quantize_mm(mm)
                    .ok_or(HeightEditError::OutOfRange { index, mm })
            })
            .collect::<Result<Vec<u16>, _>>()?;
        let mut shared = self.patches.lock();
        if shared.edits_this_frame >= MAX_EDITS_PER_FRAME {
            return Err(HeightEditError::FrameFull {
                pushed: shared.edits_this_frame,
            });
        }
        let id = TerrainHeightPatches::push_locked(
            &mut shared,
            target,
            edit.col,
            edit.row,
            edit.width,
            edit.height,
            samples,
        )?;
        shared.edits_this_frame += 1;
        Ok(id)
    }

    /// Edits accepted since the last applied frame (at most [`MAX_EDITS_PER_FRAME`]).
    #[must_use]
    pub fn pushed_this_frame(&self) -> u32 {
        self.patches.lock().edits_this_frame
    }

    /// Bands applied so far ([`PatchQueueState::generation`]), for tests and telemetry.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.patches.state().generation
    }
}

/// `Update`, before [`apply_terrain_height_patches`]: sends what brush strokes wrote into the ECS
/// map (`terrain.stroke`, the editor's tool, undo) to the linked draw, whose map, mips and GPU
/// texture are its own. Patches do not come through here: their bands reach the draw themselves.
#[tracing::instrument(skip_all, target = "regia_terrain")]
pub fn forward_terrain_strokes(
    patches: Option<Res<TerrainHeightPatches>>,
    mut terrains: Query<(Entity, &crate::Terrain, &mut crate::TerrainEdits)>,
) {
    let Some(patches) = patches.filter(|p| p.draw_is_linked()) else {
        return;
    };
    // The draw shows one terrain, the one the patches target; another of the same size would
    // pass the band's grid check and write the wrong heights.
    let Some(target) = terrains.iter().map(|(e, ..)| e).min_by_key(|e| e.to_bits()) else {
        return;
    };
    if let Ok((_, terrain, mut edits)) = terrains.get_mut(target)
        && let Some(rect) = edits.take_draw_dirty()
    {
        patches.send_to_draw(&terrain.map, rect);
    }
}

/// Sent once per [`HeightBand`] applied (`apply_terrain_height_patches`): the samples and the
/// world box whose ground changed, for physics ([`crate::CollisionHeightfield::patch`]),
/// navigation (rebuild the navmesh tiles under the box) and anything else caching the ground.
/// Messages arrive in patch id order, rows in order within a patch.
#[derive(Message, Clone, Debug, PartialEq)]
pub struct TerrainHeightsChanged {
    /// The `Terrain` entity written (`None` when only a draw is linked).
    pub entity: Option<Entity>,
    /// The patch id.
    pub patch: u64,
    /// The samples written.
    pub rect: DirtyRect,
    /// World-space minimum corner `(x, y, z)` of the change: the samples' footprint, from the
    /// lowest of the old and new heights.
    pub world_min: [f32; 3],
    /// World-space maximum corner: to the highest of the old and new heights.
    pub world_max: [f32; 3],
    /// [`PatchQueueState::generation`] after this band.
    pub generation: u64,
}

/// `Update` ([`crate::TerrainPlugin`]): applies this frame's [`HeightBand`]s to the first
/// `Terrain` (lowest entity id, the one `terrain.stroke` edits) and announces them. Does nothing
/// while the queue is idle.
#[tracing::instrument(skip_all, target = "regia_terrain")]
pub fn apply_terrain_height_patches(
    mut commands: Commands,
    patches: Option<Res<TerrainHeightPatches>>,
    mut terrains: Query<(
        Entity,
        &mut crate::Terrain,
        Option<&mut crate::TerrainEdits>,
        Option<&mut crate::TerrainEditLayers>,
    )>,
    mut changed: MessageWriter<TerrainHeightsChanged>,
) {
    let Some(patches) = patches else { return };
    if patches.is_idle() {
        return;
    }
    let target = terrains.iter().map(|(e, ..)| e).min_by_key(|e| e.to_bits());
    if patches.target().is_none()
        && let Some(entity) = target
        && let Ok((_, terrain, _, _)) = terrains.get(entity)
    {
        patches.set_target(PatchTarget::of(&terrain.map));
    }
    let bands = patches.take_frame();
    let generation = patches.state().generation - bands.len() as u64;
    let mut new_edits = None;
    for (i, band) in bands.iter().enumerate() {
        let mut extent = None;
        if let Some(entity) = target
            && let Ok((_, mut terrain, edits, stack)) = terrains.get_mut(entity)
        {
            let terrain = &mut *terrain;
            extent = band.apply(&mut terrain.map);
            if extent.is_some() {
                let layered = stack.is_some();
                if let Some(mut stack) = stack {
                    match stack.recompute(&mut terrain.map, band.rect) {
                        Ok(_) => {
                            // A Set layer can cover a base edit. Send the canonical composite
                            // after the queued bands, and include it in the change's extent.
                            if patches.draw_is_linked() {
                                patches.send_to_draw(&terrain.map, band.rect);
                            }
                            if let Some((lo, hi)) = &mut extent {
                                for row in band.rect.row_start..band.rect.row_end {
                                    for col in band.rect.col_start..band.rect.col_end {
                                        let h = terrain.map.height_at_sample(i64::from(col), i64::from(row));
                                        *lo = lo.min(h);
                                        *hi = hi.max(h);
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            tracing::warn!(target: "regia_terrain", "height patch edit layers: {err}")
                        }
                    }
                }
                match edits {
                    Some(mut edits) => {
                        let tiles = edit_cells(&band.rect);
                        edits.note(&terrain.map, &mut terrain.minmax, band.rect, &tiles);
                        // This band already reaches the draw.
                        edits.take_draw_dirty();
                    }
                    None if layered => {
                        let edits = new_edits.get_or_insert_with(crate::TerrainEdits::default);
                        edits.note(
                            &terrain.map,
                            &mut terrain.minmax,
                            band.rect,
                            &edit_cells(&band.rect),
                        );
                        edits.take_draw_dirty();
                    }
                    None => terrain.minmax.update_region(&terrain.map, &band.rect),
                }
            }
        }
        let (lo, hi) = extent.unwrap_or_else(|| band_extent(band));
        let ([x0, z0], [x1, z1]) = band.target.footprint(&band.rect);
        changed.write(TerrainHeightsChanged {
            entity: target.filter(|_| extent.is_some()),
            patch: band.patch,
            rect: band.rect,
            world_min: [x0, lo, z0],
            world_max: [x1, hi, z1],
            generation: generation + i as u64 + 1,
        });
    }
    if let Some(entity) = target
        && let Some(edits) = new_edits
    {
        commands.entity(entity).insert(edits);
    }
}

/// The edit cells ([`crate::EDIT_CELL_SIZE`]) a rectangle touches.
fn edit_cells(rect: &DirtyRect) -> std::collections::BTreeSet<crate::brush::TileCoord> {
    let cell = crate::EDIT_CELL_SIZE;
    let mut tiles = std::collections::BTreeSet::new();
    if rect.is_empty() {
        return tiles;
    }
    for ty in rect.row_start / cell..=(rect.row_end - 1) / cell {
        for tx in rect.col_start / cell..=(rect.col_end - 1) / cell {
            tiles.insert((tx, ty));
        }
    }
    tiles
}

/// The new heights' range when no ECS map was written (only a draw is linked).
fn band_extent(band: &HeightBand) -> (f32, f32) {
    band.samples
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &s| {
            let h = band.target.dequantize(s);
            (lo.min(h), hi.max(h))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat map loaded from a 16-bit heightfield (every sample on the grid, as in a game).
    fn flat_map(n: u32) -> TerrainMap {
        let hf = regia_asset::Heightfield {
            width: n,
            height: n,
            samples: vec![6554; (n * n) as usize],
        };
        TerrainMap::new(&hf, 64.0, 100.0, 0.0).expect("map")
    }

    fn patch(col: u32, row: u32, width: u32, height: u32, h: f32) -> HeightPatch {
        HeightPatch {
            col,
            row,
            width,
            height,
            heights: (0..width * height).map(|i| h + i as f32 * 0.01).collect(),
        }
    }

    #[test]
    fn requests_are_validated_before_anything_is_queued() {
        let queue = TerrainHeightPatches::default();
        assert_eq!(
            queue.request(patch(0, 0, 2, 2, 1.0)),
            Err(HeightPatchError::NoTarget)
        );
        let map = flat_map(65);
        queue.set_target(PatchTarget::of(&map));
        let err = |p: HeightPatch| queue.request(p).expect_err("refused").code();
        assert_eq!(err(patch(0, 0, 0, 4, 1.0)), "patch_empty");
        assert_eq!(err(patch(60, 0, 6, 4, 1.0)), "patch_out_of_bounds");
        assert_eq!(err(patch(u32::MAX, 0, 2, 1, 1.0)), "patch_out_of_bounds");
        let mut short = patch(0, 0, 4, 4, 1.0);
        short.heights.pop();
        assert_eq!(err(short), "patch_bad_length");
        let mut nan = patch(0, 0, 4, 4, 1.0);
        nan.heights[5] = f32::NAN;
        assert_eq!(err(nan), "patch_non_finite");
        assert!(queue.is_idle(), "nothing queued");
        assert_eq!(queue.request(patch(0, 0, 4, 4, 1.0)), Ok(1));
        assert_eq!(queue.request(patch(4, 4, 4, 4, 1.0)), Ok(2));
        assert_eq!(queue.state().queued_samples, 32);
    }

    /// The bands depend only on the queue and the budget: a big patch spreads over frames in
    /// whole rows, oldest first, each frame within the budget.
    #[test]
    fn bands_follow_the_budget_in_queue_order() {
        let map = flat_map(129);
        let queue = TerrainHeightPatches::default();
        queue.set_target(PatchTarget::of(&map));
        queue.set_budget(100);
        let a = queue.request(patch(0, 0, 30, 10, 5.0)).expect("a"); // 300 samples
        let b = queue.request(patch(40, 40, 8, 2, 7.0)).expect("b"); // 16 samples
        let mut frames = Vec::new();
        while !queue.is_idle() {
            let bands = queue.take_frame();
            assert!(bands.iter().map(HeightBand::len).sum::<usize>() <= 100);
            frames.push(
                bands
                    .iter()
                    .map(|b| (b.patch, b.rect.row_start, b.rect.row_end))
                    .collect::<Vec<_>>(),
            );
        }
        assert_eq!(
            frames,
            vec![
                vec![(a, 0, 3)],
                vec![(a, 3, 6)],
                vec![(a, 6, 9)],
                vec![(a, 9, 10), (b, 40, 42)],
            ]
        );
        let state = queue.state();
        assert_eq!(
            (state.completed, state.queued_samples, state.applied_samples),
            (b, 0, 316)
        );
        assert_eq!(
            state.last_frame,
            PatchFrameStats {
                samples: 46,
                bands: 2,
                completed: 2
            }
        );
        // A row wider than the budget still goes, alone in its frame.
        queue.set_budget(10);
        queue.request(patch(0, 0, 25, 2, 1.0)).expect("wide");
        assert_eq!(queue.take_frame().len(), 1);
        assert_eq!(queue.state().last_frame.samples, 25);
    }

    /// Heights are clamped and quantised to the 16-bit grid: the patched map equals the map a
    /// save and reload gives, bit for bit, whatever the budget.
    #[test]
    fn patched_heights_are_what_a_reload_reads_whatever_the_budget() {
        let final_map = |budget: u32| {
            let mut map = flat_map(65);
            let queue = TerrainHeightPatches::default();
            queue.set_target(PatchTarget::of(&map));
            queue.set_budget(budget);
            let mut p = patch(10, 12, 20, 9, 33.3);
            p.heights[0] = -50.0; // below the range: clamped to the offset
            p.heights[1] = 1e6; // above: clamped to offset + scale
            queue.request(p).expect("patch");
            queue.request(patch(15, 15, 4, 4, 70.0)).expect("overlap"); // later wins
            while !queue.is_idle() {
                for band in queue.take_frame() {
                    assert!(band.apply(&mut map).is_some());
                }
            }
            map
        };
        let a = final_map(7);
        let b = final_map(1 << 20);
        assert_eq!(a.heights(), b.heights(), "the budget changes timing, not results");
        let reloaded = TerrainMap::new(&a.to_heightfield(), 64.0, 100.0, 0.0).expect("reload");
        assert_eq!(a.heights(), reloaded.heights(), "quantised exactly like a reload");
        let at = |c: u32, r: u32| a.heights()[(r * 65 + c) as usize];
        assert_eq!(at(10, 12), 0.0);
        assert_eq!(at(11, 12), 100.0);
        assert!((at(16, 16) - 70.05).abs() < 2e-3, "{}", at(16, 16));
        assert_eq!(at(0, 0), flat_map(65).heights()[0], "untouched samples stay");
    }

    #[test]
    fn a_stroke_reaches_a_linked_draw_once_and_a_patch_is_not_sent_twice() {
        let map = TerrainMap::from_heights(17, 17, vec![10.0; 289], 16.0, 100.0, 0.0).expect("map");
        let mut world = World::new();
        let queue = TerrainHeightPatches::linked(&map);
        world.insert_resource(queue.clone());
        world.init_resource::<Messages<TerrainHeightsChanged>>();
        let terrain = crate::Terrain::new(
            &map.to_heightfield(),
            16.0,
            100.0,
            0.0,
            crate::CdlodConfig::default(),
            2,
            crate::TerrainLayers::default(),
        )
        .expect("terrain");
        let entity = world.spawn((terrain, crate::TerrainEdits::default())).id();
        let mut schedule = Schedule::default();
        schedule.add_systems((forward_terrain_strokes, apply_terrain_height_patches).chain());

        let rect = DirtyRect {
            col_start: 2,
            col_end: 5,
            row_start: 3,
            row_end: 4,
        };
        let mut query = world.query::<(&mut crate::Terrain, &mut crate::TerrainEdits)>();
        {
            let (mut terrain, mut edits) = query.get_mut(&mut world, entity).expect("terrain");
            let terrain = &mut *terrain;
            for col in rect.col_start..rect.col_end {
                terrain.map.set_height((3 * 17 + col) as usize, 40.0);
            }
            edits.note(
                &terrain.map,
                &mut terrain.minmax,
                rect,
                &std::collections::BTreeSet::new(),
            );
        }
        schedule.run(&mut world);
        let bands = queue.take_draw_bands();
        assert_eq!(bands.len(), 1);
        assert_eq!(bands[0].rect, rect);
        let mut drawn = map.clone();
        assert!(bands[0].apply(&mut drawn).is_some());
        assert!((drawn.height_at_index(3 * 17 + 2) - 40.0).abs() < 0.01);

        schedule.run(&mut world);
        assert!(queue.take_draw_bands().is_empty(), "the rectangle was sent once");

        // A patch's own band goes to the draw; the note it leaves is not sent again.
        queue.request(patch(1, 1, 2, 2, 60.0)).expect("patch");
        schedule.run(&mut world);
        assert_eq!(queue.take_draw_bands().len(), 1);
        schedule.run(&mut world);
        assert!(queue.take_draw_bands().is_empty());

        // The draw shows the terrain with the lowest id: a stroke on the other stays out of it.
        let other = crate::Terrain::new(
            &map.to_heightfield(),
            16.0,
            100.0,
            0.0,
            crate::CdlodConfig::default(),
            2,
            crate::TerrainLayers::default(),
        )
        .expect("terrain");
        let second = world.spawn((other, crate::TerrainEdits::default())).id();
        let not_drawn = entity.max(second);
        {
            let (mut terrain, mut edits) = query.get_mut(&mut world, not_drawn).expect("terrain");
            let terrain = &mut *terrain;
            terrain.map.set_height(3 * 17 + 2, 90.0);
            edits.note(
                &terrain.map,
                &mut terrain.minmax,
                rect,
                &std::collections::BTreeSet::new(),
            );
        }
        schedule.run(&mut world);
        assert!(queue.take_draw_bands().is_empty());
    }

    #[test]
    fn the_queue_refuses_past_its_limit_and_a_linked_draw_gets_every_band() {
        let map = flat_map(65);
        let queue = TerrainHeightPatches::linked(&map);
        assert!(queue.state().draw_linked);
        queue.set_budget(16);
        queue.request(patch(0, 0, 8, 4, 2.0)).expect("patch");
        let first = queue.take_frame();
        let second = queue.take_frame();
        let drawn = queue.take_draw_bands();
        assert_eq!(drawn, [first, second].concat());
        assert!(queue.take_draw_bands().is_empty());
        // The backlog limit.
        let full = TerrainHeightPatches::default();
        full.set_target(PatchTarget {
            width: 4097,
            height: 4097,
            height_scale: 100.0,
            height_offset: 0.0,
            world_width: 1280.0,
            world_depth: 1280.0,
            grid_origin: [0.0; 2],
        });
        let big = |i: u32| full.request_samples(0, i, 2048, 2048, vec![0; 2048 * 2048]);
        for i in 0..4 {
            big(i).expect("fits");
        }
        assert_eq!(big(4).expect_err("full").code(), "patch_queue_full");
        assert_eq!(
            full.request_samples(0, 0, 2049, 2048, Vec::new())
                .expect_err("big")
                .code(),
            "patch_too_large"
        );
    }

    #[test]
    fn footprints_match_the_map_sample_positions() {
        let map = TerrainMap::from_heights(9, 9, vec![0.0; 81], 80.0, 10.0, 0.0).expect("map");
        let target = PatchTarget::of(&map);
        let rect = DirtyRect {
            col_start: 2,
            col_end: 5,
            row_start: 0,
            row_end: 9,
        };
        let (lo, hi) = target.footprint(&rect);
        assert_eq!(lo, map.sample_to_world(2, 0));
        assert_eq!(hi, map.sample_to_world(4, 8));
    }

    /// A crater patched band by band leaves the min/max pyramid and the height mips exactly as a
    /// rebuild from the patched map makes them, and the LOD selection around the crater draws no
    /// crack (the CPU mirror of the vertex shader, `lod_seams`).
    #[test]
    fn a_patched_crater_keeps_the_pyramids_exact_and_the_lod_seams_closed() {
        use crate::cdlod::{CdlodConfig, MinMaxPyramid, select_nodes};
        use crate::height_mips::HeightMips;
        let n = 257u32;
        let width = 256.0f32;
        let hills: Vec<f32> = (0..n * n)
            .map(|i| {
                let (col, row) = (i % n, i / n);
                let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
                let z = row as f32 / (n - 1) as f32 * width - width * 0.5;
                9.0 * (x * 0.031).sin() * (z * 0.027).cos() + 4.0 * ((x + 2.0 * z) * 0.083).sin()
            })
            .collect();
        let shaped = TerrainMap::from_heights(n, n, hills, width, 40.0, -20.0).expect("map");
        let mut map = TerrainMap::new(&shaped.to_heightfield(), width, 40.0, -20.0).expect("map");
        let root_level = 5;
        let mut pyramid = MinMaxPyramid::build(&map, root_level).expect("pyramid");
        let mut mips = HeightMips::build(&map);
        // A 9 m deep bowl, 40 samples across, centred on sample (150, 110).
        let (c0, r0, side) = (130u32, 90u32, 40u32);
        let heights: Vec<f32> = (0..side * side)
            .map(|i| {
                let (dc, dr) = ((i % side) as f32 - 19.5, (i / side) as f32 - 19.5);
                let t = ((dc * dc + dr * dr).sqrt() / 20.0).min(1.0);
                map.heights()[((r0 + i / side) * n + c0 + i % side) as usize] - 9.0 * (1.0 - t * t)
            })
            .collect();
        let queue = TerrainHeightPatches::default();
        queue.set_target(PatchTarget::of(&map));
        queue.set_budget(300);
        queue
            .request(HeightPatch {
                col: c0,
                row: r0,
                width: side,
                height: side,
                heights,
            })
            .expect("crater");
        let mut frames = 0;
        while !queue.is_idle() {
            for band in queue.take_frame() {
                band.apply(&mut map).expect("same grid");
                pyramid.update_region(&map, &band.rect);
                mips.update_region(&map, &band.rect);
            }
            frames += 1;
        }
        assert_eq!(frames, 6, "1600 samples at 300 a frame: seven rows a frame");
        let rebuilt = MinMaxPyramid::build(&map, root_level).expect("pyramid");
        assert_eq!(format!("{pyramid:?}"), format!("{rebuilt:?}"), "min/max pyramid");
        assert_eq!(
            format!("{mips:?}"),
            format!("{:?}", HeightMips::build(&map)),
            "height mips"
        );
        let config = CdlodConfig::default();
        let [ex, ez] = map.sample_to_world(150, 110);
        for eye in [[ex, ez], [ex + 40.0, ez - 25.0], [ex - 90.0, ez + 60.0]] {
            let selection = select_nodes(&map, &pyramid, &config, root_level, eye, None);
            let report = crate::lod_seams::lod_seam_report(&map, &mips, &config, root_level, &selection, eye);
            assert!(report.level_change_edges > 0, "{report:?}");
            assert!(report.is_crack_free(1e-4), "{report:?}");
        }
    }

    #[test]
    fn edit_cells_cover_the_rectangle() {
        let rect = DirtyRect {
            col_start: 30,
            col_end: 34,
            row_start: 0,
            row_end: 1,
        };
        let cells: Vec<_> = edit_cells(&rect).into_iter().collect();
        assert_eq!(cells, vec![(0, 0), (1, 0)]);
    }
}
