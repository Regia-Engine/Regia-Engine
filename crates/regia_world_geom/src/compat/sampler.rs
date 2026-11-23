// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-content/src/heightmap.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 1-478 (268-329 the floor calls), mark: adapt (Stage A: unchanged).
// Edits: module paths, doc comments (DO27 file and engine names dropped, missing docs added), lint
// attributes, helper visibility; float min/max/clamp through `regia_detsim::float::det_*` (ADR-8166).
// Re-synced from regia-absorb-1 (d6ed37677) by W-02 (ADR-8167): `skylight_height_at` and
// `sample_floor_ext` are gone, `shaft_at` is new, `sample_floor` no longer reads `sticky`.

//! Heightfield grid and world-space terrain sampling.
//!
//! One height scale for the whole tree: a raw `u16` sample is
//! [`HEIGHT_METERS_PER_COUNT`] (1/32 m) per count, so full white (65535)
//! is about 2048 m. The approved-pack PNG decoder
//! (a host's) and [`TerrainSampler`] both use it; the
//! sampler also centres the map on the origin and zeroes the centre
//! sample's height.
//!
//! Grids may be rectangular: `width` samples per row (X / east-west),
//! `height` rows (Z / north-south). A rectangular map still uses ONE
//! cell size ([`TerrainSampler::spacing`]) on both axes — every shipped
//! heightfield is square-celled (5 m) — only the sample COUNTS differ.

use regia_detsim::float::{det_clamp, det_max, det_min};

/// Metres per raw height count (1/32 m).
pub const HEIGHT_METERS_PER_COUNT: f32 = 1.0 / 32.0;

/// Sim cell, metres. Every shipped heightfield is square-celled at this size:
/// world width is `samples * SIM_CELL_M` (256 → 1280 m, 410 → 2050 m).
pub const SIM_CELL_M: f32 = 5.0;

/// World metres across `samples` cells at [`SIM_CELL_M`].
pub fn world_metres_for_samples(samples: usize) -> f32 {
    samples as f32 * SIM_CELL_M
}

/// PNG world width. When the caller already asked for `samples * 5` m
/// (the alpha 256 → 1280 case), keep that f32 so half-extents stay
/// bit-identical. Any other caller width is replaced by the grid size.
pub fn png_world_width(samples: usize, caller: f32) -> f32 {
    let sized = world_metres_for_samples(samples);
    if caller.is_finite() && (caller - sized).abs() <= 1.0e-3 {
        caller
    } else {
        sized
    }
}

/// A decoded heightmap grid (square or rectangular).
#[derive(Debug, Clone, PartialEq)]
pub struct HeightGrid {
    /// Samples per row along X (east-west).
    pub width: usize,
    /// Rows along Z (north-south).
    pub height: usize,
    /// Raw u16 height samples, row-major, `width * height` entries.
    pub heights: Vec<u16>,
}

impl HeightGrid {
    /// Height in metres at a sample coordinate (clamped to the grid).
    pub fn height_at(&self, x: usize, z: usize) -> f32 {
        let x = x.min(self.width - 1);
        let z = z.min(self.height - 1);
        self.heights[z * self.width + x] as f32 * HEIGHT_METERS_PER_COUNT
    }

    /// A grid of raw samples; `None` unless `heights.len() == width * height`
    /// and both dimensions are >= 2.
    pub fn new(width: usize, height: usize, heights: Vec<u16>) -> Option<HeightGrid> {
        (width >= 2 && height >= 2 && heights.len() == width * height).then_some(HeightGrid {
            width,
            height,
            heights,
        })
    }

    /// A square grid (`width == height`); `None` unless `heights.len() == side²`
    /// and `side >= 2`.
    pub fn square(side: usize, heights: Vec<u16>) -> Option<HeightGrid> {
        HeightGrid::new(side, side, heights)
    }

    /// Resident bytes of this grid (`width * height` u16 samples).
    pub fn resident_bytes(&self) -> u64 {
        (self.width as u64) * (self.height as u64) * 2
    }

    /// Strided downsample for render/collision copies (nearest sample every
    /// `stride` steps; stride 1 clones). The caller's full grid — the sim's
    /// own data — is never modified, so determinism is unchanged; only the
    /// returned copy is smaller. An axis that would shrink below 2 clones the
    /// grid instead of producing an invalid one. A square grid stays square.
    pub fn downsampled(&self, stride: usize) -> HeightGrid {
        let stride = stride.max(1);
        if stride == 1 {
            return self.clone();
        }
        let short_w = self.width.div_ceil(stride);
        let short_h = self.height.div_ceil(stride);
        if short_w < 2 || short_h < 2 {
            return self.clone();
        }
        let mut heights = Vec::with_capacity(short_w * short_h);
        for z in 0..short_h {
            for x in 0..short_w {
                heights.push(self.heights[(z * stride) * self.width + x * stride]);
            }
        }
        HeightGrid {
            width: short_w,
            height: short_h,
            heights,
        }
    }
}

/// World-space terrain height sampler shared by the authoritative server
/// and the renderer so both agree on ground height exactly: the map is
/// centered on the world origin and the center sample's height is zeroed.
#[derive(Debug, Clone, PartialEq)]
pub struct TerrainSampler {
    grid: HeightGrid,
    /// World units between adjacent samples (the same on X and Z).
    pub spacing: f32,
    /// Height subtracted from every sample (the map center's raw height).
    pub center_height: f32,
    /// World half-extent along X (east-west): `width * spacing * 0.5`.
    pub half_x: f32,
    /// World half-extent along Z (north-south): `height * spacing * 0.5`.
    /// Equals [`Self::half_x`] on a square map.
    pub half_z: f32,
    /// The drawn ground where it differs from the grid (tunnel-mouth funnels), see
    /// [`crate::compat::terrain_patch`]. Answers before the grid inside each patch.
    pub patches: Vec<crate::compat::terrain_patch::TerrainPatch>,
    /// T2: the terrain hole mask baked from the tunnel description
    /// ([`crate::compat::layout::MouthHole`]). Inside a hole there is no
    /// terrain: the tunnel mesh (trench plus lip) is the ground, and floor
    /// queries resolve through it. Attached at load beside the patches.
    pub holes: Vec<crate::compat::layout::MouthHole>,
}

impl TerrainSampler {
    /// Build a sampler from a decoded grid and its world width (metres
    /// across X). The cell size is `world_width / grid.width` and the Z
    /// extent derives from the row count, so a square grid spans
    /// `world_width` on both axes and a rectangular one keeps square cells.
    pub fn new(grid: HeightGrid, world_width: f32) -> Self {
        let spacing = world_width / grid.width as f32;
        let center_height = grid.height_at(grid.width / 2, grid.height / 2);
        let half_x = grid.width as f32 * spacing * 0.5;
        let half_z = grid.height as f32 * spacing * 0.5;
        TerrainSampler {
            grid,
            spacing,
            center_height,
            half_x,
            half_z,
            patches: Vec::new(),
            holes: Vec::new(),
        }
    }

    /// Build a sampler from a cell size in metres (the JSON heightfields'
    /// `cell_m`): `spacing` is exactly `cell_m` on both axes, the map is
    /// centred on the origin (sample 0 at `-cols * cell_m / 2`).
    pub fn from_cells(grid: HeightGrid, cell_m: f32) -> Self {
        let half_x = grid.width as f32 * cell_m * 0.5;
        let half_z = grid.height as f32 * cell_m * 0.5;
        Self::from_cells_at(grid, cell_m, -half_x, -half_z)
    }

    /// As [`Self::from_cells`], with the world position of the row-0/col-0
    /// sample given explicitly (`x0`/`z0`). A cell-centre grid passes
    /// `-half + cell_m / 2`. The sampler keeps square cells; only the
    /// sample-grid offset changes (`half_x = -x0`, `half_z = -z0`).
    pub fn from_cells_at(grid: HeightGrid, cell_m: f32, x0: f32, z0: f32) -> Self {
        let center_height = grid.height_at(grid.width / 2, grid.height / 2);
        TerrainSampler {
            center_height,
            spacing: cell_m,
            half_x: -x0,
            half_z: -z0,
            patches: Vec::new(),
            holes: Vec::new(),
            grid,
        }
    }

    /// The larger of the two half-extents: the bounding half-square of the
    /// map. Containment checks that only need "is this off the map" use
    /// this; anything that maps world XZ to samples must use
    /// [`Self::half_x`] / [`Self::half_z`] instead.
    pub fn half_extent(&self) -> f32 {
        det_max(self.half_x, self.half_z)
    }

    /// Samples per row (X). Square maps may also use [`Self::side`].
    pub fn width(&self) -> usize {
        self.grid.width
    }

    /// Rows (Z).
    pub fn height(&self) -> usize {
        self.grid.height
    }

    /// Samples per row on a SQUARE map (width == height). Panics in debug
    /// builds on a rectangular grid — switch the caller to
    /// [`Self::width`] / [`Self::height`].
    pub fn side(&self) -> usize {
        debug_assert_eq!(
            self.grid.width, self.grid.height,
            "rectangular grid: use width()/height(), not side()"
        );
        self.grid.width
    }

    /// Lay baked ground patches over the grid ([`crate::compat::terrain_patch`]).
    pub fn with_patches(mut self, patches: Vec<crate::compat::terrain_patch::TerrainPatch>) -> Self {
        self.patches = patches;
        self
    }

    /// Lay the baked terrain hole mask over the grid (T2): no terrain
    /// triangles are drawn inside a hole, and the tunnel lip meets the ground
    /// around its edge. Deterministic: the same description bakes the same
    /// mask on every peer.
    pub fn with_holes(mut self, holes: Vec<crate::compat::layout::MouthHole>) -> Self {
        self.holes = holes;
        self
    }

    /// True when `(world_x, world_z)` is inside a baked terrain hole: there
    /// is no heightfield ground here, only the tunnel mesh.
    pub fn hole_at(&self, world_x: f32, world_z: f32) -> bool {
        self.holes.iter().any(|h| h.contains(world_x, world_z))
    }

    /// The grid's own bilinear height, ignoring patches.
    pub fn grid_height_at_world(&self, world_x: f32, world_z: f32) -> f32 {
        let fx = det_clamp(
            (world_x + self.half_x) / self.spacing,
            0.0,
            (self.grid.width - 1) as f32,
        );
        let fz = det_clamp(
            (world_z + self.half_z) / self.spacing,
            0.0,
            (self.grid.height - 1) as f32,
        );
        let x0 = fx.floor() as usize;
        let z0 = fz.floor() as usize;
        let x1 = (x0 + 1).min(self.grid.width - 1);
        let z1 = (z0 + 1).min(self.grid.height - 1);
        let tx = fx - x0 as f32;
        let tz = fz - z0 as f32;
        let h00 = self.height_at_index(x0, z0);
        let h10 = self.height_at_index(x1, z0);
        let h01 = self.height_at_index(x0, z1);
        let h11 = self.height_at_index(x1, z1);
        let top = h00 + (h10 - h00) * tx;
        let bottom = h01 + (h11 - h01) * tx;
        top + (bottom - top) * tz
    }

    /// Take sim height 0 at raw sample `zero_counts` instead of the centre
    /// sample. Used when the PNG carries absolute landscape heights.
    pub fn with_zero_counts(mut self, zero_counts: u32) -> Self {
        self.center_height = zero_counts as f32 * HEIGHT_METERS_PER_COUNT;
        self
    }

    /// Low-resolution copy of the sim grid for the renderer/collision side
    /// at `stride` (ULTRA LOW uses 4 for render, 2 for collision). The
    /// sim keeps sampling the full grid, so this never changes simulation.
    pub fn render_copy(&self, stride: usize) -> HeightGrid {
        self.grid.downsampled(stride)
    }

    /// Centered, zero-adjusted height at integer sample coordinates.
    pub fn height_at_index(&self, x: usize, z: usize) -> f32 {
        self.grid.height_at(x, z) - self.center_height
    }

    /// Bilinear height at a world position; positions off the map clamp
    /// to the border samples. Inside a baked patch (a tunnel-mouth funnel) the
    /// patch answers, and only where it is lower than the grid: a patch never
    /// raises the ground.
    pub fn height_at_world(&self, world_x: f32, world_z: f32) -> f32 {
        let grid = self.grid_height_at_world(world_x, world_z);
        for patch in &self.patches {
            if let Some(h) = patch.sample(world_x, world_z) {
                return det_min(h, grid);
            }
        }
        grid
    }

    /// True when the baked patch that answers at this XZ is a skylight shaft (a `*_skylight`
    /// patch): drawn ground that drops from the plain through the rock into a tunnel. Where it
    /// dips into a carved span the column has no ground ([`crate::compat::surface::column`]).
    pub fn shaft_at(&self, world_x: f32, world_z: f32) -> bool {
        self.patches
            .iter()
            .find(|patch| patch.sample(world_x, world_z).is_some())
            .is_some_and(|patch| patch.id.ends_with("_skylight"))
    }

    /// The outdoor heightfield floor at `(x, z)`. `hint_y` and `sticky` are not read: this is the
    /// terrain-only sampler. A body on a map with mesh layers asks [`crate::compat::surface`] (T3).
    pub fn sample_floor(&self, x: f32, z: f32, hint_y: f32, sticky: FloorSurface) -> FloorSample {
        let _ = sticky;
        crate::compat::ground::sample_floor_with_tunnels(
            Some(self),
            x,
            z,
            hint_y,
            crate::compat::ground::SampleFloorTunnels::default(),
        )
    }
}

/// Which floor mesh a vehicle is stuck to (outdoor heightfield vs tunnel).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FloorSurface {
    #[default]
    /// The outdoor heightfield.
    Heightfield,
    /// A tunnel mesh floor.
    TunnelFloor,
}

/// Result of [`sample_floor`] / [`TerrainSampler::sample_floor`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloorSample {
    /// The floor point under the query (sim metres).
    pub point: [f32; 3],
    /// The floor's unit normal.
    pub normal: [f32; 3],
    /// Which surface answered.
    pub surface: FloorSurface,
}

/// The outdoor heightfield floor at `(x, z)`; a flat floor at y = 0 when `terrain` is `None`.
/// `hint_y` and `sticky` are not read: this is the terrain-only sampler, never a tunnel floor.
/// A body on a map with mesh layers asks [`crate::compat::surface`] (T3).
pub fn sample_floor(
    terrain: Option<&TerrainSampler>,
    x: f32,
    z: f32,
    hint_y: f32,
    sticky: FloorSurface,
) -> FloorSample {
    let _ = sticky;
    crate::compat::ground::sample_floor_with_tunnels(
        terrain,
        x,
        z,
        hint_y,
        crate::compat::ground::SampleFloorTunnels::default(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// 4x4 grid, raw heights = index (0..15).
    fn ramp_grid() -> HeightGrid {
        HeightGrid::square(4, (0..16u16).collect()).expect("grid")
    }

    #[test]
    fn world_metres_follow_the_five_metre_cell() {
        assert_eq!(world_metres_for_samples(256), 1280.0);
        assert_eq!(world_metres_for_samples(410), 2050.0);
        assert_eq!(png_world_width(256, 1280.0), 1280.0);
        assert_eq!(png_world_width(410, 1280.0), 2050.0);
        assert_eq!(png_world_width(8, 1280.0), 40.0);
    }

    #[test]
    fn one_height_scale_of_a_32nd_of_a_metre() {
        assert_eq!(HEIGHT_METERS_PER_COUNT, 1.0 / 32.0);
        let grid = HeightGrid::square(2, vec![0, 32, 64, 65535]).unwrap();
        assert_eq!(grid.height_at(1, 0), 1.0);
        assert_eq!(grid.height_at(0, 1), 2.0);
        assert!((grid.height_at(1, 1) - 2047.97).abs() < 0.01);
        assert!(HeightGrid::square(1, vec![0]).is_none());
        assert!(HeightGrid::square(3, vec![0; 8]).is_none());
        assert!(HeightGrid::new(3, 2, vec![0; 5]).is_none());
        assert_eq!(HeightGrid::square(4, (0..16u16).collect()).unwrap().width, 4);
    }

    #[test]
    fn sampler_centers_map_and_interpolates() {
        // World width 40 → spacing 10.
        let sampler = TerrainSampler::new(ramp_grid(), 40.0);
        assert_eq!(sampler.spacing, 10.0);
        assert_eq!(sampler.half_x, 20.0);
        assert_eq!(sampler.half_z, 20.0);
        assert_eq!(sampler.half_extent(), 20.0);
        assert_eq!(sampler.width(), 4);
        assert_eq!(sampler.height(), 4);
        // Center sample (2,2) height zeroes out.
        assert_eq!(sampler.height_at_index(2, 2), 0.0);
        assert!((sampler.height_at_world(0.0, 0.0)).abs() < 1e-5);
        let h_a = sampler.height_at_index(2, 2);
        let h_b = sampler.height_at_index(3, 2);
        let mid = sampler.height_at_world(5.0, 0.0);
        assert!((mid - (h_a + h_b) * 0.5).abs() < 1e-5);
        assert!(
            (h_b - HEIGHT_METERS_PER_COUNT).abs() < 1e-6,
            "one count per sample step"
        );
        // Off-map clamps instead of panicking.
        let _ = sampler.height_at_world(-9999.0, 9999.0);
    }

    /// Rectangular grids (svalbard_cut 512x256 class): one cell size on
    /// both axes, per-axis half extents, independent X/Z clamping.
    #[test]
    fn rectangular_grid_samples_per_axis() {
        // 4 columns (X) x 2 rows (Z), counts 0..8, 5 m cells. Row 0 is the
        // -Z (south) border: counts 0..4 sit at z = -5, counts 4..8 at z = +5.
        let grid = HeightGrid::new(4, 2, (0..8u16).collect()).expect("4x2 grid");
        let sampler = TerrainSampler::from_cells(grid, 5.0);
        assert_eq!(sampler.width(), 4);
        assert_eq!(sampler.height(), 2);
        assert_eq!(sampler.spacing, 5.0);
        assert!((sampler.half_x - 10.0).abs() < 1e-5, "half_x {}", sampler.half_x);
        assert!((sampler.half_z - 5.0).abs() < 1e-5, "half_z {}", sampler.half_z);
        assert!((sampler.half_extent() - 10.0).abs() < 1e-5);
        // Raw counts at the corners, before the centre-datum subtraction.
        let raw = |x: f32, z: f32| sampler.height_at_world(x, z) + sampler.center_height;
        assert!((raw(-10.0, -5.0) - 0.0).abs() < 1e-5, "south west raw");
        assert!(
            (raw(10.0, 5.0) - 7.0 * HEIGHT_METERS_PER_COUNT).abs() < 1e-5,
            "north east raw"
        );
        // Off-map Z clamps to the border row (row 1: counts 4..8): x = 0 is
        // sample index 2 (fx = (0+10)/5 = 2) → count 6. Off-map X clamps to
        // the border column: (+inf, 0) lands on fz = 1, row 1's count 7.
        assert!(
            (raw(0.0, 999.0) - 6.0 * HEIGHT_METERS_PER_COUNT).abs() < 1e-5,
            "north clamp"
        );
        assert!(
            (raw(999.0, 0.0) - 7.0 * HEIGHT_METERS_PER_COUNT).abs() < 1e-5,
            "east clamp"
        );
        let _ = sampler.height_at_world(-9999.0, 9999.0);
    }

    /// `TerrainSampler::new` with a world width keeps square cells on a
    /// rectangular grid: the Z extent derives from the row count.
    #[test]
    fn rectangular_grid_world_width_keeps_square_cells() {
        let grid = HeightGrid::new(8, 4, vec![0u16; 32]).expect("8x4 grid");
        let sampler = TerrainSampler::new(grid, 40.0);
        assert_eq!(sampler.spacing, 5.0);
        assert!((sampler.half_x - 20.0).abs() < 1e-5);
        assert!((sampler.half_z - 10.0).abs() < 1e-5);
    }

    #[test]
    fn downsampled_copies_shrink_bytes_and_leave_the_sim_grid_alone() {
        let grid = HeightGrid::square(4, (0..16u16).collect()).expect("grid");
        let full_bytes = grid.resident_bytes();
        assert_eq!(full_bytes, 4 * 4 * 2);
        let half = grid.downsampled(2);
        assert_eq!((half.width, half.height, half.heights.len()), (2, 2, 4));
        assert_eq!(half.resident_bytes(), full_bytes / 4);
        // Nearest sampling: corners survive.
        assert_eq!(half.heights, vec![0, 2, 8, 10]);
        // The sim grid is untouched.
        assert_eq!(grid.heights, (0..16u16).collect::<Vec<_>>());
        // Stride 1 clones; degenerate strides never produce an invalid grid.
        assert_eq!(grid.downsampled(1), grid);
        assert_eq!(grid.downsampled(0), grid);
        assert_eq!((grid.downsampled(99).width, grid.downsampled(99).height), (4, 4));
    }

    #[test]
    fn render_copy_matches_downsampled_without_changing_sim_answers() {
        let sampler = TerrainSampler::new(ramp_grid(), 40.0);
        let before = sampler.height_at_world(5.0, 0.0);
        let copy = sampler.render_copy(2);
        assert_eq!(copy.width * copy.height, copy.heights.len());
        // Sim answers come from the full grid, unchanged by the copy.
        assert_eq!(sampler.height_at_world(5.0, 0.0), before);
        assert_eq!(sampler.side(), 4);
    }

    #[test]
    fn hole_mask_is_empty_by_default_and_honors_rects() {
        let sampler = TerrainSampler::new(ramp_grid(), 40.0);
        assert!(!sampler.hole_at(0.0, 0.0));
        let holed = sampler.with_holes(vec![crate::compat::layout::MouthHole {
            opening: "west".into(),
            center: [5.0, -3.0],
            half: [6.0, 4.0],
            yaw: 0.0,
        }]);
        assert!(holed.hole_at(5.0, -3.0));
        assert!(holed.hole_at(10.9, 0.9));
        assert!(!holed.hole_at(11.5, -3.0));
        assert!(!holed.hole_at(5.0, 1.5));
        assert!(!holed.hole_at(0.0, 5.0));
    }

    #[test]
    fn sample_floor_sticky_tunnel_stays_heightfield_by_default() {
        let sampler = TerrainSampler::new(ramp_grid(), 40.0);
        let sample = sampler.sample_floor(0.0, 0.0, -10.0, FloorSurface::TunnelFloor);
        assert_eq!(sample.surface, FloorSurface::Heightfield);
        let free = sample_floor(Some(&sampler), 0.0, 0.0, -10.0, FloorSurface::TunnelFloor);
        assert_eq!(free.surface, FloorSurface::Heightfield);
        let none = sample_floor(None, 1.0, 2.0, -10.0, FloorSurface::TunnelFloor);
        assert_eq!(none.surface, FloorSurface::Heightfield);
        assert_eq!(none.point, [1.0, 0.0, 2.0]);
    }
}
