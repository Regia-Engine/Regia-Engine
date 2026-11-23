// SPDX-License-Identifier: MPL-2.0
//! The world-space radiance cache behind screen-space GI (`specs/79` § 12, ADR-2085; `specs/27`
//! § 4.2 stage 2a's third item).
//!
//! The probes (`crate::ddgi`) hold the frame's diffuse light at metre scale and the depth buffer
//! holds what the camera sees at pixel scale. With the cache on, screen-space GI marches short rays
//! through the depth buffer: a hit counts the surface's lit colour against what this cache holds toward
//! the ray at the pixel. The cache is the probes' light rebuilt as radiance on nested camera-centered
//! grids. Reading one cell costs a few words, so a fragment shader can afford it per ray; reading the
//! probes costs eight tiles per cascade. A miss can read the cache too (`miss_cap`), off by default
//! since ADR-2605.
//!
//! A CPU planner lists the cells the frame rewrites, up to a budget per frame: cells a scroll
//! brought in first, then a round-robin refresh weighted toward the finest level. Once the probes
//! have settled and every level has been refreshed since, nothing is rewritten, so a still view costs
//! only the read. A cell is tagged with its world cell, so one that has not been rewritten since a
//! scroll reads as absent and the forward pass's probe light stands there.
//!
//! Off by default in every profile (`RenderProfile::radiance_cache`). A frame without it records
//! the plain gather from the unchanged `ssgi.wgsl`, creates nothing and allocates nothing here.
//!
//! ## Provenance
//! Ward, Rubinstein, Clear, "A Ray Tracing Solution for Diffuse Interreflection", SIGGRAPH 1988;
//! Křivánek, Gautron, Pattanaik, Bouatouch, "Radiance Caching for Efficient Global Illumination
//! Computation", IEEE TVCG 11(5) 2005: reuse of cached directional light at nearby points. Tanner,
//! Migdal, Jones, "The Clipmap: A Virtual Mipmap", SIGGRAPH 1998: nested camera-centered grids,
//! toroidal updates. Majercik et al., JCGT 8(2) 2019: the probes that feed it.

use crate::bind_cache::BindCache;
use crate::device::TrackedDevice;
use crate::frame_work::counted::*;

/// Most levels a cache may have (the uniform holds this many).
pub const RADIANCE_CACHE_MAX_LEVELS: usize = 4;
/// Words per cell: six RGB9E5 faces and a tag (`radiance_cache_common.wgsl`).
const CELL_WORDS: u64 = 7;

const COMMON: &str = include_str!("../shaders/radiance_cache_common.wgsl");
const UPDATE: &str = include_str!("../shaders/radiance_cache_update.wgsl");
const SSGI: &str = include_str!("../shaders/ssgi.wgsl");
const SSGI_CACHED: &str = include_str!("../shaders/ssgi_cached.wgsl");
const DDGI_COMMON: &str = include_str!("../shaders/ddgi_common.wgsl");
const DDGI_SAMPLE: &str = include_str!("../shaders/ddgi_sample.wgsl");

/// The cell update shader's full source.
pub(crate) fn update_source() -> String {
    format!("{DDGI_COMMON}\n{DDGI_SAMPLE}\n{COMMON}\n{UPDATE}")
}

/// The cached gather's full source: the plain gather's file, the cache's layout, the cached gather.
pub(crate) fn gather_source() -> String {
    format!("{SSGI}\n{COMMON}\n{SSGI_CACHED}")
}

/// Size, budget and gather tunables of the radiance cache.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadianceCacheSettings {
    /// Nested grids (1..=[`RADIANCE_CACHE_MAX_LEVELS`]); each is `level_ratio` times coarser.
    pub levels: u32,
    /// Cells per axis of every level (4..=64).
    pub dims: [u32; 3],
    /// Most cells rewritten per frame (at least 1, at most every cell).
    pub budget_cells: u32,
    /// Finest cell spacing as a share of the finest probe spacing.
    pub spacing_scale: f32,
    /// Spacing growth from one level to the next.
    pub level_ratio: f32,
    /// Ray length as a share of the finest probe spacing: the probes hold what lies further.
    pub ray_scale: f32,
    /// Multiplier on the gather's difference (1 = the estimate as computed).
    pub strength: f32,
    /// A miss reads the cache where the ray last stood in free space, capped at this many times what
    /// the cache holds toward the same direction at the pixel. 0 (the default, ADR-2605) turns that read
    /// off: a miss then reads at the pixel, where it cancels, and adds nothing. On plates whose probes
    /// hold what the camera sees, the read adds noise and no accuracy.
    pub miss_cap: f32,
    /// How far the difference may take a pixel down, as a share of the probe light it holds. 0 (the
    /// default) only ever adds light: screen-space AO has already darkened the pixel once, and both
    /// lit path tracer plates scored worse with any darkening (ADR-2085).
    pub darken_limit: f32,
    /// Rays each gathered pixel marches per frame: 1, 2, 4 or 8 (the default; others round down to
    /// one of those). Fewer rays take turns through the same eight-ray pattern, one share a frame
    /// (`params.noise.x`, the temporal index), so temporal AA sums the full pattern over 8 / `rays`
    /// frames at a fraction of the cost (ADR-2605). Without temporal AA the share stays the first.
    pub rays: u32,
    /// Depth-buffer steps per ray (2..=8, default 8).
    pub steps: u32,
    /// A ray that reaches a pixel of sky stops there and reads the cache as a miss (ADR-2605): the
    /// point it stands at is in front of the sky, so it is free space, and the depth buffer holds
    /// nothing further along it that a later step could be sure of. `false` (the default) marches on.
    pub sky_early_out: bool,
    /// Count the gather's work on the GPU (pixels, rays, depth steps, cell reads) and report the
    /// last counted frame in [`RadianceCacheStatus::work`] (ADR-2605). A diagnostic: it reads back
    /// 16 bytes a few frames late and allocates for the read. `false` (the default) counts nothing.
    pub count_work: bool,
    /// What the cached gather writes (ADR-4106): the shipped image (the default), or one term of its
    /// sum on top of the frame it corrects, for attributing an error. A diagnostic like
    /// `count_work`: changing it keeps the cells and the planner, and any term but
    /// [`RadianceCacheTerm::Composite`] gathers afresh every frame (no static-screen reuse), so
    /// renderers with the same history differ only in what this frame's gather writes.
    pub attribution: RadianceCacheTerm,
}

/// One term of the cached gather's sum (`ssgi_cached.wgsl`, ADR-4106), for attribution.
///
/// Every term but [`Self::Composite`] and [`Self::Total`] is written as `frame + term`, the term
/// unclamped and positive, so `term = capture − capture(Base)` on renderers with the same history.
/// Per gathered pixel, with `k` = receiver albedo × strength / rays and `fade` the hit's distance
/// fade, the shipped bounce is `k·Σ_hit (L − co)·fade` (plus `k·Σ_miss (read − co_free)` with a miss
/// cap), clamped from below by the darken limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RadianceCacheTerm {
    /// The shipped image (a held static screen may reuse an earlier gather).
    #[default]
    Composite,
    /// The shipped image, gathered afresh this frame.
    Total,
    /// The frame the gather corrects, without it (the forward pass's probe light).
    Base,
    /// `k·Σ_hit L·fade`: the screen hits' radiance.
    HitRadiance,
    /// `k·Σ_hit co·fade`: the cache's light toward each hit, which the gather subtracts.
    HitCorrection,
    /// `k·Σ_miss min(read, co_free·cap)`: what the miss read would add (cap = the miss cap, or 4 when
    /// it is off), over the misses with a valid read.
    MissRead,
    /// `k·Σ_miss co_free`: what the miss read would subtract, over the same misses.
    MissCorrection,
    /// `k·Σ_all co`, unfaded: the cache's light at the pixel over every ray, which a consistent cache
    /// makes equal to the probe light the forward pass added (at the nominal albedo).
    Origin,
    /// Not light: `(hits, valid misses, 1) / rays` (blue 1 where the pixel gathered at all).
    Coverage,
    /// Not light: of the eight cells around the pixel, the shares that hold their own cell (the tag),
    /// that counted (on the receiver's side of its plane) and that stand in free space. Zero where the
    /// pixel did not gather, including where no cell counted.
    Cells,
}

impl RadianceCacheTerm {
    /// Every term, in shader order.
    pub const ALL: [Self; 10] = [
        Self::Composite,
        Self::Total,
        Self::Base,
        Self::HitRadiance,
        Self::HitCorrection,
        Self::MissRead,
        Self::MissCorrection,
        Self::Origin,
        Self::Coverage,
        Self::Cells,
    ];

    /// The shader's code (`rc.counts.z`).
    #[must_use]
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// The protocol name (`render.set_radiance_cache` `attribution`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Composite => "composite",
            Self::Total => "total",
            Self::Base => "base",
            Self::HitRadiance => "hit_radiance",
            Self::HitCorrection => "hit_correction",
            Self::MissRead => "miss_read",
            Self::MissCorrection => "miss_correction",
            Self::Origin => "origin",
            Self::Coverage => "coverage",
            Self::Cells => "cells",
        }
    }

    /// The term named `name` ([`Self::name`]).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == name)
    }
}

/// What the cached gather did on one frame ([`RadianceCacheSettings::count_work`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RadianceCacheWork {
    /// Pixels that gathered (inside the cache, near enough for their rays to span pixels).
    pub pixels: u32,
    /// Rays they marched.
    pub rays: u32,
    /// Depth-buffer steps of those rays (the gather's main cost).
    pub steps: u32,
    /// Cache reads of the rays that missed.
    pub cell_reads: u32,
}

impl Default for RadianceCacheSettings {
    fn default() -> Self {
        Self {
            levels: 3,
            dims: [32, 16, 32],
            budget_cells: 2048,
            spacing_scale: 0.5,
            level_ratio: 4.0,
            ray_scale: 3.0,
            strength: 1.0,
            miss_cap: 0.0,
            darken_limit: 0.0,
            rays: 8,
            steps: 8,
            sky_early_out: false,
            count_work: false,
            attribution: RadianceCacheTerm::Composite,
        }
    }
}

impl RadianceCacheSettings {
    /// The values with every field inside its range. A NaN field reads as its default (a bare
    /// `clamp` keeps NaN, and NaN settings never compare equal, so every set would rebuild).
    #[must_use]
    pub fn clamped(mut self) -> Self {
        let d = Self::default();
        let or = |v: f32, default: f32| if v.is_nan() { default } else { v };
        self.spacing_scale = or(self.spacing_scale, d.spacing_scale);
        self.level_ratio = or(self.level_ratio, d.level_ratio);
        self.ray_scale = or(self.ray_scale, d.ray_scale);
        self.strength = or(self.strength, d.strength);
        self.miss_cap = or(self.miss_cap, d.miss_cap);
        self.darken_limit = or(self.darken_limit, d.darken_limit);
        self.levels = self.levels.clamp(1, RADIANCE_CACHE_MAX_LEVELS as u32);
        self.dims = self.dims.map(|d| d.clamp(4, 64));
        self.budget_cells = self.budget_cells.clamp(1, self.cells() as u32);
        self.spacing_scale = self.spacing_scale.clamp(0.05, 8.0);
        self.level_ratio = self.level_ratio.clamp(1.5, 8.0);
        self.ray_scale = self.ray_scale.clamp(0.1, 8.0);
        self.strength = self.strength.clamp(0.0, 4.0);
        self.miss_cap = self.miss_cap.clamp(0.0, 64.0);
        self.darken_limit = self.darken_limit.clamp(0.0, 1.0);
        // A power of two, so the shares tile the eight-ray pattern.
        self.rays = 1 << self.rays.clamp(1, 8).ilog2();
        self.steps = self.steps.clamp(2, 8);
        self
    }

    /// The cheaper gather (ADR-2605): four rays a frame taking turns through the pattern, six steps
    /// each, and a ray that reaches the sky stops: at most 24 depth taps a pixel instead of 64. The
    /// cache, its budget and every other tunable are the defaults'.
    #[must_use]
    pub fn economy() -> Self {
        Self {
            rays: 4,
            steps: 6,
            sky_early_out: true,
            ..Self::default()
        }
    }

    /// Whether two settings address the same cells the same way, so what a frame wrote still holds
    /// under the other's (only how the gather reads them differs).
    fn same_cells(&self, other: &Self) -> bool {
        self.levels == other.levels
            && self.dims == other.dims
            && self.budget_cells == other.budget_cells
            && self.spacing_scale.to_bits() == other.spacing_scale.to_bits()
            && self.level_ratio.to_bits() == other.level_ratio.to_bits()
    }

    /// Most depth-buffer taps one gathered pixel's rays make per frame (`rays` × `steps`): the
    /// gather's cost by construction. The default makes 64.
    #[must_use]
    pub const fn taps_per_pixel(&self) -> u32 {
        self.rays * self.steps
    }

    /// Cells of one level.
    #[must_use]
    pub const fn cells_per_level(&self) -> u64 {
        self.dims[0] as u64 * self.dims[1] as u64 * self.dims[2] as u64
    }

    /// Cells of all levels.
    #[must_use]
    pub const fn cells(&self) -> u64 {
        self.levels as u64 * self.cells_per_level()
    }

    /// Bytes of the cell buffer.
    #[must_use]
    pub const fn cell_bytes(&self) -> u64 {
        self.cells() * CELL_WORDS * 4
    }

    /// Cell spacing of `level`, metres, for a field whose finest probe spacing is `probe_spacing`.
    #[must_use]
    pub fn spacing_of(&self, level: u32, probe_spacing: f32) -> f32 {
        probe_spacing * self.spacing_scale * self.level_ratio.powi(level as i32)
    }
}

/// The cache's state as the agent protocol reports it (`render.get_radiance_cache`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadianceCacheStatus {
    /// The renderer was asked to use the cache ([`crate::FrameRenderer::set_radiance_cache`]).
    pub enabled: bool,
    /// The last frame's screen-space GI read the cache. False while it is off, when the frame ran no
    /// screen-space GI, or when the probes did not run.
    pub active: bool,
    /// The settings in use.
    pub settings: RadianceCacheSettings,
    /// Cell spacing of the finest level on the last active frame, metres (0 before one).
    pub finest_spacing_m: f32,
    /// Half width of the coarsest level on the last active frame, metres (0 before one). A surface
    /// further from the eye keeps the forward pass's probe light.
    pub reach_m: f32,
    /// Cells rewritten by the last active frame.
    pub updated_cells: u32,
    /// Of those, cells a scroll brought in (the rest are the refresh).
    pub scrolled_cells: u32,
    /// Cells a scroll brought in that wait for a later frame's budget.
    pub pending_cells: u32,
    /// Every level has been refreshed since the probes settled: nothing is being rewritten.
    pub idle: bool,
    /// Cells rewritten since the renderer started.
    pub total_updated: u64,
    /// Bytes of the cache's GPU buffers (0 until the first active frame and after it is turned off).
    pub gpu_bytes: u64,
    /// The last frame's gather work that came back with [`RadianceCacheSettings::count_work`] on
    /// (`None` before a frame's count has come back, with it off, or with the cache off). It lags the
    /// frame by about three frames, and a frame that gathered no pixel (a held view reusing its gather)
    /// leaves the last count standing.
    pub work: Option<RadianceCacheWork>,
}

/// What one frame's plan wrote into the queue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PlanCounts {
    pub(crate) scrolled: u32,
    pub(crate) refreshed: u32,
}

/// The CPU side of the incremental update: where the grids stand and which cells are due.
///
/// A steady plan allocates nothing: the lists keep their capacity.
pub(crate) struct CachePlanner {
    settings: RadianceCacheSettings,
    spacing: [f32; RADIANCE_CACHE_MAX_LEVELS],
    bases: [[i32; 3]; RADIANCE_CACHE_MAX_LEVELS],
    placed: bool,
    /// Cells a scroll brought in, oldest first from `head`.
    scrolled: Vec<u32>,
    head: usize,
    cursor: [u32; RADIANCE_CACHE_MAX_LEVELS],
    /// Cells of a level refreshed since the probes last changed.
    refreshed: [u64; RADIANCE_CACHE_MAX_LEVELS],
}

impl CachePlanner {
    pub(crate) fn new(settings: RadianceCacheSettings) -> Self {
        Self {
            settings,
            spacing: [0.0; RADIANCE_CACHE_MAX_LEVELS],
            bases: [[0; 3]; RADIANCE_CACHE_MAX_LEVELS],
            placed: false,
            scrolled: Vec::new(),
            head: 0,
            cursor: [0; RADIANCE_CACHE_MAX_LEVELS],
            refreshed: [0; RADIANCE_CACHE_MAX_LEVELS],
        }
    }

    /// Forgets every cell (a camera cut, a new layout): the next plan refills from nothing.
    pub(crate) fn reset(&mut self) {
        self.placed = false;
        self.scrolled.clear();
        self.head = 0;
        self.cursor = [0; RADIANCE_CACHE_MAX_LEVELS];
        self.refreshed = [0; RADIANCE_CACHE_MAX_LEVELS];
    }

    pub(crate) fn bases(&self) -> &[[i32; 3]; RADIANCE_CACHE_MAX_LEVELS] {
        &self.bases
    }

    pub(crate) fn spacing(&self) -> &[f32; RADIANCE_CACHE_MAX_LEVELS] {
        &self.spacing
    }

    /// Scrolled cells still waiting for budget.
    pub(crate) fn pending(&self) -> u32 {
        (self.scrolled.len() - self.head) as u32
    }

    /// Every level has been refreshed since the probes last changed.
    pub(crate) fn idle(&self) -> bool {
        let per_level = self.settings.cells_per_level();
        self.placed
            && self.pending() == 0
            && self.refreshed[..self.settings.levels as usize]
                .iter()
                .all(|r| *r >= per_level)
    }

    /// The slot of `cell` in `level`, the shader's `rc_slot`.
    pub(crate) fn slot(&self, level: u32, cell: [i32; 3]) -> u32 {
        let d = self.settings.dims.map(|v| v as i32);
        let s = [
            cell[0].rem_euclid(d[0]),
            cell[1].rem_euclid(d[1]),
            cell[2].rem_euclid(d[2]),
        ];
        level * self.settings.cells_per_level() as u32 + (s[0] + d[0] * (s[1] + d[1] * s[2])) as u32
    }

    /// Places the grids around `eye` and writes this frame's cells into `queue` (cleared first, at
    /// most `budget_cells`). `field_changing`: the probes moved or are converging, so a refresh
    /// keeps going; once they stop, one more full refresh per level and the cache goes idle.
    pub(crate) fn plan(
        &mut self,
        eye: [f32; 3],
        probe_spacing: f32,
        field_changing: bool,
        queue: &mut Vec<u32>,
    ) -> PlanCounts {
        let levels = self.settings.levels;
        let dims = self.settings.dims.map(|d| d as i32);
        let per_level = self.settings.cells_per_level();
        let budget = self.settings.budget_cells as usize;
        queue.clear();
        queue.reserve(budget);
        for level in 0..levels {
            let spacing = self.settings.spacing_of(level, probe_spacing);
            let base = grid_base(eye, spacing, dims);
            let l = level as usize;
            if !self.placed || spacing.to_bits() != self.spacing[l].to_bits() {
                // Everything here is new; the round-robin below refills it.
                self.refreshed[l] = 0;
            } else if base != self.bases[l] {
                self.push_entering(level, self.bases[l], base);
            }
            self.spacing[l] = spacing;
            self.bases[l] = base;
        }
        self.placed = true;

        // Scrolled cells first, oldest first.
        let take = budget.min(self.scrolled.len() - self.head);
        queue.extend_from_slice(&self.scrolled[self.head..self.head + take]);
        self.head += take;
        if self.head == self.scrolled.len() {
            self.scrolled.clear();
            self.head = 0;
        }
        let scrolled = take as u32;

        // The refresh: half the remaining budget to the finest level still due, half of the rest
        // to the next, and so on (the last takes what is left).
        let mut due = [0u32; RADIANCE_CACHE_MAX_LEVELS];
        let mut due_count = 0;
        for level in 0..levels {
            if self.refreshed[level as usize] < per_level {
                due[due_count] = level;
                due_count += 1;
            }
        }
        let mut left = (budget - take) as u64;
        for (i, &level) in due[..due_count].iter().enumerate() {
            let l = level as usize;
            let share = if i + 1 == due_count {
                left
            } else {
                left.div_ceil(2)
            };
            let share = share.min(per_level);
            for _ in 0..share {
                queue.push(level * per_level as u32 + self.cursor[l]);
                self.cursor[l] += 1;
                if u64::from(self.cursor[l]) == per_level {
                    self.cursor[l] = 0;
                }
            }
            self.refreshed[l] = (self.refreshed[l] + share).min(per_level);
            left -= share;
        }
        // Cells rewritten from a moving field do not count: one full pass must see it settled.
        if field_changing {
            self.refreshed = [0; RADIANCE_CACHE_MAX_LEVELS];
        }
        PlanCounts {
            scrolled,
            refreshed: queue.len() as u32 - scrolled,
        }
    }

    /// Lists the cells of `level`'s grid at `new` that its grid at `old` did not hold. A jump past
    /// the whole grid restarts the level's refresh instead: every cell is new.
    fn push_entering(&mut self, level: u32, old: [i32; 3], new: [i32; 3]) {
        let dims = self.settings.dims.map(|d| d as i32);
        // In `i64`: a jump from one end of the cell grid to the other overflows an `i32`.
        let jumped = (0..3).any(|a| (i64::from(new[a]) - i64::from(old[a])).abs() >= i64::from(dims[a]));
        if jumped {
            self.refreshed[level as usize] = 0;
            return;
        }
        let inside = |a: usize, v: i32| v >= old[a] && v < old[a] + dims[a];
        // Compact before the list could grow past one refill per level.
        if self.head > 0 {
            self.scrolled.copy_within(self.head.., 0);
            self.scrolled.truncate(self.scrolled.len() - self.head);
            self.head = 0;
        }
        let (dz, z_first) = {
            // The z range of a column whose x and y were already inside: only the entering slab.
            let d = new[2] - old[2];
            if d > 0 {
                (d.min(dims[2]), new[2] + dims[2] - d.min(dims[2]))
            } else {
                ((-d).min(dims[2]), new[2])
            }
        };
        for x in new[0]..new[0] + dims[0] {
            let x_in = inside(0, x);
            for y in new[1]..new[1] + dims[1] {
                if x_in && inside(1, y) {
                    for z in z_first..z_first + dz {
                        let slot = self.slot(level, [x, y, z]);
                        self.scrolled.push(slot);
                    }
                } else {
                    for z in new[2]..new[2] + dims[2] {
                        let slot = self.slot(level, [x, y, z]);
                        self.scrolled.push(slot);
                    }
                }
            }
        }
        // A camera outrunning the budget: the oldest lie furthest behind it. Dropping them
        // does not queue them, and the refresh cursor may already have passed their slots,
        // so the level's refresh starts over: a dropped cell is still inside the grid and
        // must be rewritten before the cache may settle.
        let cap = self.settings.cells() as usize;
        if self.scrolled.len() > cap {
            let excess = self.scrolled.len() - cap;
            self.scrolled.copy_within(excess.., 0);
            self.scrolled.truncate(cap);
            self.refreshed[level as usize] = 0;
        }
    }
}

/// The lowest corner cell of a level's grid of `dims` cells around `eye` (spacing `spacing`).
///
/// Like the DDGI `cascade_base`, saturating: a finite eye past the `i32` cell grid (`floor() as
/// i32` saturates) does not overflow, and a non-finite eye axis lands on cell 0. The corner is
/// also kept at most `i32::MAX - dims`, so every cell of the grid (`base + dims - 1`, the scroll's
/// loops) fits an `i32`. Inside the grid it is `floor(eye / spacing) - dims / 2`, unchanged.
fn grid_base(eye: [f32; 3], spacing: f32, dims: [i32; 3]) -> [i32; 3] {
    std::array::from_fn(|a| {
        let snapped = (eye[a] / spacing).floor() as i32;
        snapped.saturating_sub(dims[a] / 2).min(i32::MAX - dims[a])
    })
}

/// `RcUniform` in `radiance_cache_common.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct RcUniformGpu {
    bases: [[i32; 4]; RADIANCE_CACHE_MAX_LEVELS],
    spacing: [f32; 4],
    dims: [u32; 4],
    counts: [u32; 4],
    view_to_world: [[f32; 4]; 4],
    gather: [f32; 4],
    limits: [f32; 4],
}

/// Device-level state: the update pipeline and the layouts (a warm worker's move to the renderer
/// it stands in for).
struct CachePipes {
    update: wgpu::ComputePipeline,
    probes_bgl: wgpu::BindGroupLayout,
    update_bgl: wgpu::BindGroupLayout,
    read_bgl: wgpu::BindGroupLayout,
}

/// The buffers of one layout and the groups that bind them.
struct CacheBuffers {
    settings: RadianceCacheSettings,
    uniform: wgpu::Buffer,
    cells: wgpu::Buffer,
    queue: wgpu::Buffer,
    /// The gather's work counters (`ssgi_cached.wgsl` `rc_work`), 4 words, cleared every counted frame.
    work: wgpu::Buffer,
    /// Where a counted frame's words are copied to be read back.
    work_staging: wgpu::Buffer,
    work_state: WorkReadback,
    update_bg: wgpu::BindGroup,
    read_bg: wgpu::BindGroup,
    /// The next update clears the cells first.
    clear: bool,
}

/// Where the work counters' read-back stands ([`RadianceCacheSettings::count_work`]). One count is in
/// flight at a time: copied in one frame's update, mapped in the next (the copy has been submitted by
/// then), read in the one after the map came back.
enum WorkReadback {
    Idle,
    Copied,
    Mapping(std::sync::Arc<std::sync::atomic::AtomicU8>),
}

/// [`WorkReadback::Mapping`]: the map has not come back, came back mapped, or failed.
const MAP_PENDING: u8 = 0;
const MAP_DONE: u8 = 1;
const MAP_FAILED: u8 = 2;

/// What one frame hands the cache.
pub(crate) struct RadianceCacheFrame<'a> {
    pub(crate) eye: [f32; 3],
    pub(crate) view_to_world: [[f32; 4]; 4],
    pub(crate) pre_exposure: f32,
    /// The probes' uniform and data buffers.
    pub(crate) probes: (&'a wgpu::Buffer, &'a wgpu::Buffer),
    pub(crate) probe_spacing: f32,
    /// The probes moved or are converging.
    pub(crate) field_changing: bool,
}

/// The renderer's radiance cache: created by the first frame that reads it.
pub(crate) struct RadianceCacheRuntime {
    enabled: bool,
    active: bool,
    settings: RadianceCacheSettings,
    planner: CachePlanner,
    pipes: Option<CachePipes>,
    buffers: Option<CacheBuffers>,
    probes_bgs: BindCache<(wgpu::Buffer, wgpu::Buffer)>,
    queue_words: Vec<u32>,
    uniform_bytes: Vec<u8>,
    updated_cells: u32,
    scrolled_cells: u32,
    total_updated: u64,
    finest_spacing: f32,
    reach: f32,
    /// The last counted frame's gather work ([`RadianceCacheSettings::count_work`]).
    work: Option<RadianceCacheWork>,
}

impl Default for RadianceCacheRuntime {
    fn default() -> Self {
        let settings = RadianceCacheSettings::default();
        Self {
            enabled: false,
            active: false,
            settings,
            planner: CachePlanner::new(settings),
            pipes: None,
            buffers: None,
            probes_bgs: BindCache::default(),
            queue_words: Vec::new(),
            uniform_bytes: Vec::new(),
            updated_cells: 0,
            scrolled_cells: 0,
            total_updated: 0,
            finest_spacing: 0.0,
            reach: 0.0,
            work: None,
        }
    }
}

impl RadianceCacheRuntime {
    /// Whether a frame should read the cache (it still needs running probes).
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Turns the cache on or off. Off frees its buffers on the spot; the pipelines stay.
    pub(crate) fn set_enabled(&mut self, on: bool) {
        if self.enabled == on {
            return;
        }
        self.enabled = on;
        if !on {
            self.active = false;
            self.buffers = None;
            self.probes_bgs.clear();
            self.planner.reset();
            self.work = None;
        }
    }

    /// New settings. A new layout starts from empty cells on the next frame. One that only changes
    /// how the gather reads the cells keeps them, so the image does not lose the cache for a whole
    /// refill; the planner starts over, because the static-screen key regathers until it settles.
    pub(crate) fn set_settings(&mut self, settings: RadianceCacheSettings) {
        let settings = settings.clamped();
        if settings == self.settings {
            return;
        }
        let same_cells = settings.same_cells(&self.settings);
        // The attribution term (ADR-4106) only changes what the gather writes, like the counters:
        // the planner keeps its place, so an attributed frame reads the cells the shipped one would.
        let same_image = RadianceCacheSettings {
            count_work: self.settings.count_work,
            attribution: self.settings.attribution,
            ..settings
        } == self.settings;
        self.settings = settings;
        if same_cells {
            if !same_image {
                self.planner.reset();
            }
            if let Some(buffers) = self.buffers.as_mut() {
                buffers.settings = settings;
            }
            return;
        }
        self.planner = CachePlanner::new(settings);
        self.buffers = None;
    }

    pub(crate) fn settings(&self) -> RadianceCacheSettings {
        self.settings
    }

    /// A camera cut: the probes restart, so the cells do too.
    pub(crate) fn camera_cut(&mut self) {
        self.planner.reset();
        if let Some(buffers) = self.buffers.as_mut() {
            buffers.clear = true;
        }
    }

    /// The frame's screen-space GI did not read the cache.
    pub(crate) fn note_inactive(&mut self) {
        self.active = false;
        self.updated_cells = 0;
        self.scrolled_cells = 0;
    }

    /// A held view reused the composite gathered from the cache: nothing was rewritten.
    pub(crate) fn note_held(&mut self) {
        self.updated_cells = 0;
        self.scrolled_cells = 0;
    }

    /// Every cell has been rewritten since the probes settled. Until then a held view must keep
    /// gathering: the composite it would reuse was read from a cache still filling.
    pub(crate) fn settled(&self) -> bool {
        // An attributed frame (ADR-4106) gathers afresh: a held screen would serve an earlier
        // frame's gather, or the shipped image, in place of the term asked for.
        self.planner.idle() && self.settings.attribution == RadianceCacheTerm::Composite
    }

    /// Takes `other`'s pipelines where this cache has none (`FrameRenderer::adopt_warm_pipelines`).
    pub(crate) fn adopt_pipelines(&mut self, other: &mut Self) -> bool {
        if self.pipes.is_none() && other.pipes.is_some() {
            self.pipes = other.pipes.take();
            other.buffers = None;
            other.probes_bgs.clear();
            return true;
        }
        false
    }

    pub(crate) fn status(&self) -> RadianceCacheStatus {
        RadianceCacheStatus {
            enabled: self.enabled,
            active: self.active,
            settings: self.settings,
            finest_spacing_m: self.finest_spacing,
            reach_m: self.reach,
            updated_cells: self.updated_cells,
            scrolled_cells: self.scrolled_cells,
            pending_cells: self.planner.pending(),
            idle: self.planner.idle(),
            total_updated: self.total_updated,
            gpu_bytes: self.buffers.as_ref().map_or(0, |b| {
                b.settings.cell_bytes()
                    + u64::from(b.settings.budget_cells) * 4
                    + UNIFORM_BYTES
                    + 2 * WORK_BYTES
            }),
            work: self.work.filter(|_| self.settings.count_work),
        }
    }

    /// Builds the update pipeline and the layouts. Returns whether it built them.
    pub(crate) fn ensure_pipes(&mut self, device: &wgpu::Device) -> Result<bool, crate::RenderError> {
        if self.pipes.is_some() {
            return Ok(false);
        }
        let d = &TrackedDevice::observe(device);
        let module = crate::shader_lib::compose_source(
            &update_source(),
            "radiance_cache_update.wgsl",
            std::collections::HashMap::new(),
        )?;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("radiance cache update"),
            source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module)),
        });
        let probes_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("radiance cache probes"),
            entries: &[
                buffer_entry(0, wgpu::ShaderStages::COMPUTE, wgpu::BufferBindingType::Uniform),
                buffer_entry(
                    1,
                    wgpu::ShaderStages::COMPUTE,
                    wgpu::BufferBindingType::Storage { read_only: true },
                ),
            ],
        });
        let update_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("radiance cache update"),
            entries: &[
                buffer_entry(0, wgpu::ShaderStages::COMPUTE, wgpu::BufferBindingType::Uniform),
                buffer_entry(
                    1,
                    wgpu::ShaderStages::COMPUTE,
                    wgpu::BufferBindingType::Storage { read_only: false },
                ),
                buffer_entry(
                    2,
                    wgpu::ShaderStages::COMPUTE,
                    wgpu::BufferBindingType::Storage { read_only: true },
                ),
            ],
        });
        let read_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("radiance cache read"),
            entries: &[
                buffer_entry(0, wgpu::ShaderStages::FRAGMENT, wgpu::BufferBindingType::Uniform),
                buffer_entry(
                    1,
                    wgpu::ShaderStages::FRAGMENT,
                    wgpu::BufferBindingType::Storage { read_only: true },
                ),
                // The work counters (ADR-2605): written only on a counted frame (`rc.counts.y`).
                buffer_entry(
                    2,
                    wgpu::ShaderStages::FRAGMENT,
                    wgpu::BufferBindingType::Storage { read_only: false },
                ),
            ],
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("radiance cache update"),
            bind_group_layouts: &[Some(&probes_bgl), Some(&update_bgl)],
            immediate_size: 0,
        });
        let update = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("radiance cache update"),
            layout: Some(&layout),
            module: &module,
            entry_point: Some("update_cells"),
            compilation_options: Default::default(),
            cache: crate::pipeline_cache::cache_for(d).as_deref(),
        });
        self.pipes = Some(CachePipes {
            update,
            probes_bgl,
            update_bgl,
            read_bgl,
        });
        Ok(true)
    }

    /// The layout of the gather's second group (built by [`Self::ensure_pipes`]).
    pub(crate) fn read_layout(&self) -> Option<&wgpu::BindGroupLayout> {
        self.pipes.as_ref().map(|p| &p.read_bgl)
    }

    fn ensure_buffers(&mut self, device: &wgpu::Device) {
        if self.buffers.is_some() {
            return;
        }
        let Some(pipes) = self.pipes.as_ref() else {
            return;
        };
        let d = &TrackedDevice::observe(device);
        let settings = self.settings;
        let storage = |label: &str, size: u64, usage: wgpu::BufferUsages| {
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        // Mapped-at-creation false and CLEAR-able: wgpu zero-initializes, so every tag starts absent.
        let cells = storage(
            "radiance cache probe cells",
            settings.cell_bytes(),
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        );
        let queue = storage(
            "radiance cache probe queue",
            u64::from(settings.budget_cells) * 4,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        );
        let uniform = storage(
            "radiance cache probe uniform",
            UNIFORM_BYTES,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        let work = storage(
            "radiance cache work counters",
            WORK_BYTES,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
        );
        let work_staging = storage(
            "radiance cache work read-back",
            WORK_BYTES,
            wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        );
        let update_bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("radiance cache update"),
            layout: &pipes.update_bgl,
            entries: &[buf(0, &uniform), buf(1, &cells), buf(2, &queue)],
        });
        let read_bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("radiance cache read"),
            layout: &pipes.read_bgl,
            entries: &[buf(0, &uniform), buf(1, &cells), buf(2, &work)],
        });
        self.buffers = Some(CacheBuffers {
            settings,
            uniform,
            cells,
            queue,
            work,
            work_staging,
            work_state: WorkReadback::Idle,
            update_bg,
            read_bg,
            clear: false,
        });
        self.work = None;
        self.uniform_bytes.clear();
        self.planner.reset();
    }

    /// Plans and records this frame's cell update and returns the group the gather reads.
    pub(crate) fn record_update(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        frame: &RadianceCacheFrame<'_>,
    ) -> Result<wgpu::BindGroup, crate::RenderError> {
        let _span = tracing::info_span!("radiance_cache_update").entered();
        let _crumb = crate::breadcrumbs::frame_pass("radiance_cache_update");
        self.ensure_pipes(device)?;
        self.ensure_buffers(device);
        let (Some(pipes), Some(buffers)) = (self.pipes.as_ref(), self.buffers.as_mut()) else {
            return Err(crate::RenderError::Shader("radiance cache: no buffers".into()));
        };
        // A new probe spacing moves every cell under its old tag.
        let finest = self.settings.spacing_of(0, frame.probe_spacing);
        if self.finest_spacing != 0.0 && self.finest_spacing.to_bits() != finest.to_bits() {
            self.planner.reset();
            buffers.clear = true;
        }
        let counts = self.planner.plan(
            frame.eye,
            frame.probe_spacing,
            frame.field_changing,
            &mut self.queue_words,
        );
        let settings = self.settings;
        let levels = settings.levels as usize;
        let mut bases = [[0i32; 4]; RADIANCE_CACHE_MAX_LEVELS];
        let mut spacing = [1.0f32; 4];
        for l in 0..levels {
            let b = self.planner.bases()[l];
            bases[l] = [b[0], b[1], b[2], 0];
            spacing[l] = self.planner.spacing()[l];
        }
        let count = self.queue_words.len() as u32;
        let uniform = RcUniformGpu {
            bases,
            spacing,
            dims: [
                settings.dims[0],
                settings.dims[1],
                settings.dims[2],
                settings.levels,
            ],
            counts: [
                count,
                u32::from(settings.count_work),
                settings.attribution.code(),
                0,
            ],
            view_to_world: frame.view_to_world,
            gather: [
                frame.pre_exposure,
                settings.ray_scale * frame.probe_spacing,
                settings.strength,
                settings.miss_cap,
            ],
            limits: [
                settings.darken_limit,
                settings.rays as f32,
                settings.steps as f32,
                if settings.sky_early_out { 1.0 } else { 0.0 },
            ],
        };
        belt.write_if_changed(
            device,
            encoder,
            &buffers.uniform,
            bytemuck::bytes_of(&uniform),
            &mut self.uniform_bytes,
        );
        if std::mem::take(&mut buffers.clear) {
            encoder.clear_buffer(&buffers.cells, 0, None);
        }
        if settings.count_work {
            // A count of no pixels is a frame that did not gather (the first read-back, a held view's
            // reused gather): it does not replace the last real count.
            if let Some(work) = step_work_readback(buffers, encoder).filter(|w| w.pixels > 0) {
                self.work = Some(work);
            }
            // This frame's gather counts from zero.
            encoder.clear_buffer(&buffers.work, 0, None);
        }
        if count > 0 {
            belt.write(
                device,
                encoder,
                &buffers.queue,
                0,
                bytemuck::cast_slice(&self.queue_words),
            );
            let d = &TrackedDevice::observe(device);
            let (probe_uniform, probe_data) = frame.probes;
            let probes_bg =
                self.probes_bgs
                    .get_or_insert_with((probe_uniform.clone(), probe_data.clone()), || {
                        d.create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("radiance cache probes"),
                            layout: &pipes.probes_bgl,
                            entries: &[buf(0, probe_uniform), buf(1, probe_data)],
                        })
                    });
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("radiance cache update"),
                timestamp_writes: None,
            });
            pass.counted_set_pipeline(&pipes.update);
            pass.counted_set_bind_group(0, &probes_bg, &[]);
            pass.counted_set_bind_group(1, &buffers.update_bg, &[]);
            pass.counted_dispatch(count.div_ceil(64), 1, 1);
        }
        self.updated_cells = count;
        self.scrolled_cells = counts.scrolled;
        self.total_updated += u64::from(count);
        self.finest_spacing = self.planner.spacing()[0];
        let dims = settings.dims[0].max(settings.dims[2]) as f32;
        self.reach = self.planner.spacing()[levels - 1] * dims * 0.5;
        self.active = true;
        Ok(buffers.read_bg.clone())
    }
}

const UNIFORM_BYTES: u64 = size_of::<RcUniformGpu>() as u64;
/// Bytes of the work counters (four `u32`).
const WORK_BYTES: u64 = 16;

/// Advances the work counters' read-back by one frame ([`WorkReadback`]) and returns a count that
/// came back. Recorded before this frame's gather, so a copy takes the previous frame's counts.
fn step_work_readback(
    buffers: &mut CacheBuffers,
    encoder: &mut wgpu::CommandEncoder,
) -> Option<RadianceCacheWork> {
    use std::sync::atomic::{AtomicU8, Ordering};
    let mut out = None;
    if let WorkReadback::Mapping(flag) = &buffers.work_state {
        match flag.load(Ordering::Acquire) {
            MAP_PENDING => return None,
            MAP_DONE => {
                if let Ok(view) = buffers.work_staging.slice(..).get_mapped_range() {
                    let bytes: &[u8] = &view;
                    let words: Vec<u32> = bytemuck::pod_collect_to_vec(bytes);
                    if let &[pixels, rays, steps, cell_reads, ..] = words.as_slice() {
                        out = Some(RadianceCacheWork {
                            pixels,
                            rays,
                            steps,
                            cell_reads,
                        });
                    }
                }
                buffers.work_staging.unmap();
            }
            _ => {}
        }
        buffers.work_state = WorkReadback::Idle;
    }
    match buffers.work_state {
        WorkReadback::Idle => {
            encoder.copy_buffer_to_buffer(&buffers.work, 0, &buffers.work_staging, 0, WORK_BYTES);
            buffers.work_state = WorkReadback::Copied;
        }
        WorkReadback::Copied => {
            // Last frame's copy has been submitted: map it.
            let flag = std::sync::Arc::new(AtomicU8::new(MAP_PENDING));
            let done = flag.clone();
            buffers
                .work_staging
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |r| {
                    done.store(if r.is_ok() { MAP_DONE } else { MAP_FAILED }, Ordering::Release);
                });
            buffers.work_state = WorkReadback::Mapping(flag);
        }
        WorkReadback::Mapping(_) => {}
    }
    out
}

const fn buffer_entry(
    binding: u32,
    visibility: wgpu::ShaderStages,
    ty: wgpu::BufferBindingType,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn buf(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "radiance_cache_update.wgsl,ssgi_cached.wgsl",
        "RcUniform",
        RcUniformGpu {
            bases,
            spacing,
            dims,
            counts,
            view_to_world,
            gather,
            limits
        }
    )]
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn small() -> RadianceCacheSettings {
        RadianceCacheSettings {
            levels: 2,
            dims: [8, 4, 8],
            budget_cells: 64,
            ..RadianceCacheSettings::default()
        }
        .clamped()
    }

    #[test]
    fn the_budget_holds_on_every_frame() {
        let s = small();
        let mut plan = CachePlanner::new(s);
        let mut q = Vec::new();
        let mut eye = [0.0f32, 1.0, 0.0];
        for frame in 0..500 {
            // A camera crossing cells most frames.
            eye[0] += 0.7;
            eye[2] -= 0.3;
            let c = plan.plan(eye, 2.0, frame % 3 == 0, &mut q);
            assert!(q.len() <= s.budget_cells as usize, "frame {frame}: {}", q.len());
            assert_eq!(c.scrolled + c.refreshed, q.len() as u32);
            let total = s.cells() as u32;
            assert!(q.iter().all(|slot| *slot < total), "frame {frame}");
        }
    }

    #[test]
    fn a_still_view_refreshes_every_cell_within_the_budget_and_then_rests() {
        let s = small();
        let mut plan = CachePlanner::new(s);
        let mut q = Vec::new();
        let mut seen = HashSet::new();
        let mut frames = 0;
        while !plan.idle() {
            plan.plan([0.0; 3], 2.0, false, &mut q);
            seen.extend(q.iter().copied());
            frames += 1;
            assert!(frames < 200, "the cache never rests");
        }
        assert_eq!(seen.len() as u64, s.cells(), "every cell was rewritten");
        // Budget 64 over 512 cells: no more than twice the fewest frames that could do it.
        assert!(
            frames <= 2 * s.cells().div_ceil(u64::from(s.budget_cells)),
            "{frames}"
        );
        let c = plan.plan([0.0; 3], 2.0, false, &mut q);
        assert_eq!(
            (q.len(), c),
            (0, PlanCounts::default()),
            "an idle cache rewrites nothing"
        );
    }

    #[test]
    fn converging_probes_keep_the_refresh_going_and_one_full_pass_follows() {
        let s = small();
        let mut plan = CachePlanner::new(s);
        let mut q = Vec::new();
        for _ in 0..100 {
            plan.plan([0.0; 3], 2.0, true, &mut q);
            assert_eq!(q.len(), s.budget_cells as usize);
            assert!(!plan.idle());
        }
        // After they stop, one full pass of each level sees the settled probes.
        let mut seen = HashSet::new();
        while !plan.idle() {
            plan.plan([0.0; 3], 2.0, false, &mut q);
            seen.extend(q.iter().copied());
        }
        assert_eq!(seen.len() as u64, s.cells());
    }

    /// The cells a scroll lists are exactly those that entered, and they go first.
    #[test]
    fn a_scroll_lists_exactly_the_cells_that_entered_and_they_go_first() {
        let s = RadianceCacheSettings {
            budget_cells: 4096,
            levels: 1,
            ..small()
        }
        .clamped();
        for delta in [
            [1, 0, 0],
            [-1, 0, 0],
            [0, 1, 0],
            [0, 0, -2],
            [2, -1, 3],
            [-7, 3, 7],
        ] {
            let mut plan = CachePlanner::new(s);
            let mut q = Vec::new();
            let spacing = s.spacing_of(0, 2.0);
            let at = |c: [i32; 3]| {
                [
                    (c[0] as f32 + 0.5) * spacing,
                    (c[1] as f32 + 0.5) * spacing,
                    (c[2] as f32 + 0.5) * spacing,
                ]
            };
            plan.plan(at([5, 5, 5]), 2.0, false, &mut q);
            while !plan.idle() {
                plan.plan(at([5, 5, 5]), 2.0, false, &mut q);
            }
            let old = plan.bases()[0];
            let c = plan.plan(at([5 + delta[0], 5 + delta[1], 5 + delta[2]]), 2.0, false, &mut q);
            let new = plan.bases()[0];
            assert_eq!(new, [old[0] + delta[0], old[1] + delta[1], old[2] + delta[2]]);
            let mut want = HashSet::new();
            let d = s.dims.map(|v| v as i32);
            for x in new[0]..new[0] + d[0] {
                for y in new[1]..new[1] + d[1] {
                    for z in new[2]..new[2] + d[2] {
                        let in_old = x >= old[0]
                            && x < old[0] + d[0]
                            && y >= old[1]
                            && y < old[1] + d[1]
                            && z >= old[2]
                            && z < old[2] + d[2];
                        if !in_old {
                            want.insert(plan.slot(0, [x, y, z]));
                        }
                    }
                }
            }
            let got: HashSet<u32> = q[..c.scrolled as usize].iter().copied().collect();
            assert_eq!(got, want, "delta {delta:?}");
            assert_eq!(c.scrolled as usize, want.len(), "each once, delta {delta:?}");
        }
    }

    #[test]
    fn slots_wrap_like_the_shader_and_stay_put_while_the_grid_scrolls() {
        let s = small();
        let plan = CachePlanner::new(s);
        // Two world cells one grid apart share a slot; a cell inside keeps its slot.
        assert_eq!(plan.slot(0, [3, 1, 2]), plan.slot(0, [3 + 8, 1 - 4, 2 + 16]));
        assert_eq!(plan.slot(0, [-1, -1, -1]), plan.slot(0, [7, 3, 7]));
        assert_eq!(plan.slot(1, [0, 0, 0]), s.cells_per_level() as u32);
    }

    /// A scroll that enters more cells than the whole grid holds used to trim the oldest
    /// unscrolled cells and still settle: those slots are inside the grid, were never queued,
    /// and no later step re-enters them. The cache may not go idle until every dropped slot
    /// has been rewritten.
    #[test]
    fn a_fast_diagonal_scroll_does_not_idle_with_unwritten_cells() {
        let s = RadianceCacheSettings {
            levels: 1,
            dims: [8, 4, 8],
            budget_cells: 2,
            ..RadianceCacheSettings::default()
        }
        .clamped();
        assert_eq!(s.cells(), 256, "one level of 8 × 4 × 8");
        let mut plan = CachePlanner::new(s);
        let mut q = Vec::new();
        // Settle at the origin.
        while !plan.idle() {
            plan.plan([0.0; 3], 2.0, false, &mut q);
        }
        // A fast diagonal scroll: each 1 m step (1 m cells) enters ~109 cells and is no jump.
        let mut seen = std::collections::HashSet::new();
        for step in 1..=3 {
            q.clear();
            plan.plan([step as f32, step as f32, step as f32], 2.0, false, &mut q);
            seen.extend(q.iter().copied());
        }
        // Draining may not settle until the trim's dropped slots have been queued too.
        for _ in 0..10_000 {
            q.clear();
            plan.plan([3.0, 3.0, 3.0], 2.0, false, &mut q);
            seen.extend(q.iter().copied());
            if plan.idle() {
                break;
            }
        }
        assert!(plan.idle(), "the cache settles again");
        assert_eq!(
            seen.len() as u64,
            s.cells(),
            "every slot inside the final grid was queued after the scroll"
        );
    }

    #[test]
    fn a_jump_past_the_grid_starts_a_new_refresh_instead_of_listing_every_cell() {
        let s = small();
        let mut plan = CachePlanner::new(s);
        let mut q = Vec::new();
        while !plan.idle() {
            plan.plan([0.0; 3], 2.0, false, &mut q);
        }
        let c = plan.plan([500.0, 0.0, 0.0], 2.0, false, &mut q);
        assert_eq!(c.scrolled, 0);
        assert_eq!(q.len(), s.budget_cells as usize);
        assert_eq!(plan.pending(), 0);
    }

    /// A far finite eye saturated `floor() as i32`, and subtracting the half grid overflowed
    /// (a panic in a debug build); a jump from one end of the cell grid to the other then
    /// overflowed the scroll's difference. Mirrors the DDGI `a_non_finite_eye_keeps_a_finite_cascade`.
    #[test]
    fn a_far_eye_keeps_the_grid_base_in_range() {
        let s = small();
        let dims = s.dims.map(|d| d as i32);
        let mut plan = CachePlanner::new(s);
        let mut q = Vec::new();
        let total = s.cells() as u32;
        let eyes = [
            [0.0, 1.0, 0.0],
            [-3.4e38, 0.0, 0.0],
            [-1.0e10, -1.0e10, -1.0e10],
            [1.0e10, 1.0e10, 1.0e10],
            [-3.4e38, -3.4e38, -3.4e38],
            [3.4e38, 3.4e38, 3.4e38],
            [f32::NAN, f32::NEG_INFINITY, f32::INFINITY],
            [0.0, 1.0, 0.0],
        ];
        for eye in eyes {
            for _ in 0..2 {
                plan.plan(eye, 2.0, false, &mut q);
                assert!(q.len() <= s.budget_cells as usize, "{eye:?}");
                assert!(q.iter().all(|slot| *slot < total), "{eye:?}");
                for (l, base) in plan.bases()[..s.levels as usize].iter().enumerate() {
                    for a in 0..3 {
                        assert!(
                            base[a].checked_add(dims[a]).is_some(),
                            "level {l} base {base:?} for {eye:?}: base + dims must fit an i32"
                        );
                    }
                }
            }
        }
        let far = plan.bases()[0];
        plan.plan([-1.0e10, 0.0, 0.0], 2.0, false, &mut q);
        assert!(plan.bases()[0][0] < far[0], "a far eye clamps, not wraps");
    }

    #[test]
    fn scrolled_cells_past_the_budget_wait_in_order() {
        let s = RadianceCacheSettings {
            levels: 1,
            budget_cells: 10,
            ..small()
        }
        .clamped();
        let mut plan = CachePlanner::new(s);
        let mut q = Vec::new();
        plan.plan([0.0; 3], 2.0, false, &mut q);
        let c = plan.plan([1.5, 0.0, 0.0], 2.0, false, &mut q);
        assert_eq!(c.scrolled, 10);
        assert_eq!(plan.pending(), 32 - 10, "an 8 × 4 slab entered");
        let mut drained = 10;
        while plan.pending() > 0 {
            drained += plan.plan([1.5, 0.0, 0.0], 2.0, false, &mut q).scrolled;
        }
        assert_eq!(drained, 32);
    }

    #[test]
    fn a_steady_plan_keeps_its_capacity() {
        let s = small();
        let mut plan = CachePlanner::new(s);
        let mut q = Vec::new();
        // Warm the list once with a scroll.
        plan.plan([0.0; 3], 2.0, true, &mut q);
        plan.plan([3.0, 0.0, 0.0], 2.0, true, &mut q);
        let cap = (q.capacity(), plan.scrolled.capacity());
        for frame in 0..200 {
            plan.plan([3.0, 0.0, 0.0], 2.0, frame < 100, &mut q);
        }
        assert_eq!((q.capacity(), plan.scrolled.capacity()), cap);
    }

    #[test]
    fn a_camera_outrunning_the_budget_keeps_the_list_bounded() {
        let s = RadianceCacheSettings {
            budget_cells: 1,
            ..small()
        }
        .clamped();
        let mut plan = CachePlanner::new(s);
        let mut q = Vec::new();
        let mut eye = [0.0f32; 3];
        for _ in 0..500 {
            eye[0] += 3.0;
            plan.plan(eye, 2.0, true, &mut q);
            assert!(u64::from(plan.pending()) <= s.cells());
        }
    }

    /// ADR-2605: what the gather does with the cells may change while they stay filled; a new layout
    /// rebuilds them. Emptying the cells made an agent's `gather` switch a visible jump for 24 frames.
    #[test]
    fn a_change_of_gather_keeps_the_cells() {
        // The shared test device: an own device per test runs the lib binary past what an NVIDIA
        // card creates in one process (unit-test devices are kept until exit there, ADR-1584).
        let device = crate::test_device();
        let mut runtime = RadianceCacheRuntime::default();
        runtime.set_enabled(true);
        runtime
            .ensure_pipes(&device.device)
            .unwrap_or_else(|err| panic!("pipes: {err}"));
        runtime.ensure_buffers(&device.device);
        let cells = |r: &RadianceCacheRuntime| r.buffers.as_ref().map(|b| b.cells.clone());
        let before = cells(&runtime);
        assert!(before.is_some(), "buffers built");
        for gather in [
            RadianceCacheSettings::economy(),
            RadianceCacheSettings {
                count_work: true,
                ..RadianceCacheSettings::economy()
            },
            RadianceCacheSettings {
                strength: 0.5,
                ..RadianceCacheSettings::default()
            },
        ] {
            runtime.set_settings(gather);
            assert_eq!(cells(&runtime), before, "{gather:?} keeps the cells");
        }
        runtime.set_settings(RadianceCacheSettings {
            budget_cells: 1024,
            ..RadianceCacheSettings::default()
        });
        assert!(cells(&runtime).is_none(), "a new budget rebuilds the queue");
    }

    /// ADR-2605: the gather's cost tunables clamp to what the shader reads (a power-of-two share of
    /// the eight-ray pattern, 2..=8 steps), and the economy gather makes 24 taps a pixel to 64.
    #[test]
    fn gather_tunables_clamp_and_economy_costs_less_by_construction() {
        let d = RadianceCacheSettings::default();
        assert_eq!(
            (d.rays, d.steps, d.sky_early_out, d.count_work),
            (8, 8, false, false)
        );
        assert_eq!(d.taps_per_pixel(), 64);
        let e = RadianceCacheSettings::economy();
        assert_eq!(e.clamped(), e);
        assert_eq!(e.taps_per_pixel(), 24);
        assert_eq!(e.taps_per_pixel() * 8, d.taps_per_pixel() * 3);
        for (asked, rays) in [(0, 1), (1, 1), (3, 2), (4, 4), (7, 4), (8, 8), (99, 8)] {
            let s = RadianceCacheSettings {
                rays: asked,
                ..RadianceCacheSettings::default()
            }
            .clamped();
            assert_eq!(s.rays, rays, "{asked}");
        }
        let s = RadianceCacheSettings {
            steps: 1,
            ..RadianceCacheSettings::default()
        }
        .clamped();
        assert_eq!(s.steps, 2);
    }

    #[test]
    fn settings_clamp_and_size() {
        let s = RadianceCacheSettings {
            levels: 9,
            dims: [1, 100, 16],
            budget_cells: u32::MAX,
            spacing_scale: -1.0,
            level_ratio: 0.0,
            ray_scale: 0.0,
            strength: 99.0,
            miss_cap: -3.0,
            darken_limit: 7.0,
            rays: 0,
            steps: 99,
            sky_early_out: true,
            count_work: true,
            attribution: RadianceCacheTerm::Origin,
        }
        .clamped();
        assert_eq!((s.rays, s.steps), (1, 8));
        assert_eq!(s.attribution, RadianceCacheTerm::Origin, "a term needs no clamp");
        assert_eq!(s.levels, RADIANCE_CACHE_MAX_LEVELS as u32);
        assert_eq!(s.dims, [4, 64, 16]);
        assert_eq!(u64::from(s.budget_cells), s.cells());
        assert_eq!(s.miss_cap, 0.0);
        assert_eq!(s.darken_limit, 1.0);
        // The default layout: three levels of 16 384 cells, seven words each.
        let d = RadianceCacheSettings::default();
        assert_eq!(d.clamped(), d, "the defaults are inside the clamp");
        // Hits only (ADR-2605), for the cheaper gather too.
        assert_eq!(d.miss_cap, 0.0);
        assert_eq!(RadianceCacheSettings::economy().miss_cap, 0.0);
        assert_eq!(d.cells(), 49_152);
        assert_eq!(d.cell_bytes(), 49_152 * 28);
        // On the tiers' 2 m probes: 1, 4 and 16 m cells, 256 m of reach.
        assert!((d.spacing_of(0, 2.0) - 1.0).abs() < 1e-6);
        assert!((d.spacing_of(2, 2.0) - 16.0).abs() < 1e-6);
    }

    /// Both modules go through the composer the frame builds them with (`compose_source`, no
    /// defines): the plain gather's file carries the blue-noise variant's `#ifdef BLUE_NOISE`
    /// (ADR-2075), which the cached gather never takes, since its group 1 is the cache's.
    #[test]
    fn the_uniform_matches_the_shader_and_the_shaders_validate() {
        for (name, file, src) in [
            ("update", "radiance_cache_update.wgsl", update_source()),
            ("gather", "ssgi_cached.wgsl", gather_source()),
        ] {
            let module = crate::shader_lib::compose_source(&src, file, std::collections::HashMap::new())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::all(),
            )
            .validate(&module)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
            let (_, ty) = module
                .types
                .iter()
                .find(|(_, t)| t.name.as_deref() == Some("RcUniform"))
                .unwrap_or_else(|| panic!("{name}: no RcUniform"));
            let naga::TypeInner::Struct { span, .. } = ty.inner else {
                panic!("{name}: RcUniform is a struct");
            };
            assert_eq!(u64::from(span), UNIFORM_BYTES, "{name}");
        }
    }

    /// ADR-4106: the attribution terms' codes are the shader's, their names round-trip, and the
    /// shipped image is the default.
    #[test]
    fn attribution_terms_match_the_shader() {
        assert_eq!(
            RadianceCacheSettings::default().attribution,
            RadianceCacheTerm::Composite
        );
        for (code, term) in RadianceCacheTerm::ALL.into_iter().enumerate() {
            assert_eq!(term.code() as usize, code, "{term:?}");
            assert_eq!(RadianceCacheTerm::from_name(term.name()), Some(term));
        }
        assert_eq!(RadianceCacheTerm::from_name("hits"), None);
        for (name, code) in [
            ("RC_TERM_BASE", RadianceCacheTerm::Base),
            ("RC_TERM_HIT_RADIANCE", RadianceCacheTerm::HitRadiance),
            ("RC_TERM_HIT_CORRECTION", RadianceCacheTerm::HitCorrection),
            ("RC_TERM_MISS_READ", RadianceCacheTerm::MissRead),
            ("RC_TERM_MISS_CORRECTION", RadianceCacheTerm::MissCorrection),
            ("RC_TERM_ORIGIN", RadianceCacheTerm::Origin),
            ("RC_TERM_COVERAGE", RadianceCacheTerm::Coverage),
            ("RC_TERM_CELLS", RadianceCacheTerm::Cells),
        ] {
            let line = format!("const {name}: u32 = {}u;", code.code());
            assert!(SSGI_CACHED.contains(&line), "{line}");
        }
    }

    /// ADR-4106: an attributed frame must read the cells and the plan the shipped frame would. A
    /// planner reset regathers different cells on that frame, so two renderers with one history
    /// would no longer differ only in the term. Any term but the composite also gathers afresh.
    #[test]
    fn an_attribution_term_keeps_the_planner_and_regathers() {
        let mut runtime = RadianceCacheRuntime::default();
        runtime.set_enabled(true);
        let mut q = Vec::new();
        for _ in 0..200 {
            runtime.planner.plan([0.0; 3], 2.0, false, &mut q);
        }
        assert!(runtime.settled(), "the default fill settles");
        for term in RadianceCacheTerm::ALL {
            runtime.set_settings(RadianceCacheSettings {
                attribution: term,
                ..RadianceCacheSettings::default()
            });
            assert!(runtime.planner.idle(), "{term:?} keeps the plan");
            assert_eq!(
                runtime.settled(),
                term == RadianceCacheTerm::Composite,
                "{term:?}"
            );
        }
        // Control: a change of how the gather reads the cells does restart the plan.
        runtime.set_settings(RadianceCacheSettings {
            strength: 0.5,
            ..RadianceCacheSettings::default()
        });
        assert!(!runtime.planner.idle(), "a gather change restarts the plan");
    }

    /// The module a frame without the cache compiles has no cache text at all.
    #[test]
    fn the_plain_gather_stays_free_of_the_cache() {
        assert!(!SSGI.contains("rc_cells"));
        assert!(!SSGI.contains("RcUniform"));
    }
}
