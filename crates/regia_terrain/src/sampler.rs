// SPDX-License-Identifier: MPL-2.0
//! The terrain height sampler: the single source of truth for terrain height on the CPU.
//!
//! [`TerrainMap`] wraps the cooked [`regia_asset::Heightfield`] with its world mapping
//! (`world_width`, height scale/offset) and answers bilinear height queries in world space.
//! The mapping is identical to [`regia_asset::mesh_from_heightfield`]: row 0 of the sample
//! grid is the −Z edge, column 0 the −X edge, the map is centred on its grid origin (the world
//! origin unless [`TerrainMap::with_grid_origin`] moves it, ADR-8179), and
//! `y = sample / 65535 * height_scale + height_offset`.
//!
//! The grid is **vertex-centred**: sample `i` sits at `i / (n − 1) · world_width −
//! world_width / 2` (on Z, `world_depth` in place of `world_width`), so the first and last
//! samples lie on the map's edges. The map is a rectangle: `world_width` spans X and
//! `world_depth` spans Z ([`TerrainMap::with_world_depth`], ADR-8369); `world_depth` equals
//! `world_width` unless set, so a map built without it is the square every earlier release
//! drew. A simulation whose
//! grid is **cell-spaced** (sample spacing `world_width / n`, sample `n / 2` at the origin, as
//! DO27's simulation grid is) does *not* agree with that by construction: with the same
//! `world_width` the spacing differs, and with the spacing matched the grid is half a sample
//! off. Match such a grid with `world_width' = world_width · (n − 1) / n` and a grid origin of
//! `−0.5 · world_width / n` on X and Z ([`TerrainMap::with_grid_origin`] has the worked
//! 1280 m / 4096 example).
//!
//! A map may instead be **cell-centered** ([`TerrainMap::with_sample_layout`], ADR-10079): the
//! extent `W` is `n` equal cells and sample `i` sits at its cell's center,
//! `(i + 0.5) · W / n − W / 2` (spacing `W / n`). The map then stores the span between its
//! outer samples, `W · (n − 1) / n`, as its `world_width` (and likewise on Z), so the
//! collider, the draw, holes and patches, which all read the map through that span, need no
//! other change.

use regia_asset::Heightfield;
pub use regia_scene::level::TerrainSampleLayout as SampleLayout;
use thiserror::Error;

/// Errors returned when building a [`TerrainMap`].
#[derive(Clone, Debug, Error)]
pub enum TerrainMapError {
    /// The heightfield must have at least 2×2 samples.
    #[error("heightfield must have at least 2x2 samples, got {w}x{h}")]
    TooSmall {
        /// Width in samples.
        w: u32,
        /// Height in samples.
        h: u32,
    },
    /// A grid origin must be finite and the same on X and Z ([`TerrainMap::with_grid_origin`]).
    #[error("grid origin {0:?} must be finite and equal on X and Z")]
    BadGridOrigin([f32; 2]),
    /// The sample buffer does not match the declared dimensions.
    #[error("heightfield sample count mismatch: {0}")]
    SampleCount(#[from] regia_asset::HeightfieldValidationError),
    /// `world_width` must be finite and positive.
    #[error("world_width must be finite and > 0, got {0}")]
    BadWorldWidth(f32),
    /// `world_depth` must be finite and positive ([`TerrainMap::with_world_depth`]).
    #[error("world_depth must be finite and > 0, got {0}")]
    BadWorldDepth(f32),
    /// Height scale/offset must be finite.
    #[error("height_scale {scale} and height_offset {offset} must be finite")]
    BadHeightParams {
        /// The rejected scale.
        scale: f32,
        /// The rejected offset.
        offset: f32,
    },
}

/// A terrain heightfield with its world mapping, answering bilinear height queries.
///
/// Heights are kept as `f32` metres (the exact `u16/65535 * scale + offset` values at the
/// samples), so bilinear interpolation over this mirror is bit-identical to bilinear
/// interpolation over the quantised samples. Deformations ([`crate::deform`]) mutate this map
/// and the GPU mirror is refreshed from it; the map stays authoritative.
///
/// # Example
///
/// ```
/// use regia_terrain::sampler::TerrainMap;
///
/// // A 2×2 grid with two flat rows: heights 0 and 10 metres.
/// let map = TerrainMap::from_heights(2, 2, vec![0.0, 0.0, 10.0, 10.0], 100.0, 1.0, 0.0)
///     .expect("a valid 2×2 map");
/// assert_eq!(map.height_at_sample(0, 0), 0.0);
/// assert_eq!(map.height_at_sample(0, 1), 10.0);
/// // Halfway between the two rows the bilinear height is 5.0 m.
/// assert!((map.sample_bilinear(0.0, 0.0) - 5.0).abs() < 1e-5);
/// ```
#[derive(Debug)]
pub struct TerrainMap {
    /// Width in sample columns.
    width: u32,
    /// Height in sample rows.
    height: u32,
    /// World-space extent along X, metres.
    world_width: f32,
    /// World-space extent along Z, metres ([`Self::with_world_depth`]); `world_width` by
    /// default.
    world_depth: f32,
    /// Height of a full-scale sample (65535), metres.
    height_scale: f32,
    /// Height of a zero sample, metres.
    height_offset: f32,
    /// World XZ of the map's centre, metres ([`Self::with_grid_origin`]); `[0, 0]` by default.
    grid_origin: [f32; 2],
    /// Where the samples sit in the extent ([`Self::with_sample_layout`], ADR-10079).
    sample_layout: SampleLayout,
    /// The authored X and Z extents, metres: `[world_width, world_depth]` for a vertex-centered
    /// map, the cells' extent for a cell-centered one ([`Self::cell_extent`]).
    cell_extent: [f32; 2],
    /// Row-major heights in metres (row 0 = −Z edge, column 0 = −X edge).
    heights: Vec<f32>,
    /// Keeps `heights` paged in ([`regia_core::residency`]): gameplay samples the map where
    /// something spawns or lands, far from where anything read it last, and on a box short of
    /// memory that read was a major page fault on the frame (the 2026-10-08 `[spawn]` stalls).
    _resident: regia_core::residency::ResidentRegion,
    /// Lazily computed lowest/highest height (reset by `set_height`).
    height_range: std::sync::OnceLock<(f32, f32)>,
}

impl Clone for TerrainMap {
    fn clone(&self) -> Self {
        let heights = self.heights.clone();
        Self {
            width: self.width,
            height: self.height,
            world_width: self.world_width,
            world_depth: self.world_depth,
            height_scale: self.height_scale,
            height_offset: self.height_offset,
            grid_origin: self.grid_origin,
            sample_layout: self.sample_layout,
            cell_extent: self.cell_extent,
            _resident: regia_core::residency::ResidentRegion::of_slice(&heights),
            height_range: std::sync::OnceLock::new(),
            heights,
        }
    }
}

impl TerrainMap {
    /// Converts a cooked heightfield with its world mapping into a [`TerrainMap`].
    ///
    /// # Errors
    ///
    /// See [`TerrainMapError`]: the grid needs ≥ 2×2 samples and matching counts, and all
    /// mapping parameters must be finite (width positive).
    pub fn new(
        heightfield: &Heightfield,
        world_width: f32,
        height_scale: f32,
        height_offset: f32,
    ) -> Result<Self, TerrainMapError> {
        heightfield.validate()?;
        if heightfield.width < 2 || heightfield.height < 2 {
            return Err(TerrainMapError::TooSmall {
                w: heightfield.width,
                h: heightfield.height,
            });
        }
        if !world_width.is_finite() || world_width <= 0.0 {
            return Err(TerrainMapError::BadWorldWidth(world_width));
        }
        if !height_scale.is_finite() || !height_offset.is_finite() {
            return Err(TerrainMapError::BadHeightParams {
                scale: height_scale,
                offset: height_offset,
            });
        }
        let heights: Vec<f32> = heightfield
            .samples
            .iter()
            .map(|&s| f32::from(s) / 65535.0 * height_scale + height_offset)
            .collect();
        Ok(Self {
            width: heightfield.width,
            height: heightfield.height,
            world_width,
            world_depth: world_width,
            height_scale,
            height_offset,
            grid_origin: [0.0, 0.0],
            sample_layout: SampleLayout::VertexCentered,
            cell_extent: [world_width, world_width],
            _resident: regia_core::residency::ResidentRegion::of_slice(&heights),
            height_range: std::sync::OnceLock::new(),
            heights,
        })
    }

    /// Builds a map directly from metre-valued heights (row-major, row 0 = −Z edge).
    ///
    /// `heights.len()` must equal `width * height`; all values and mapping parameters must be
    /// finite. Used by tests and generators.
    ///
    /// # Errors
    ///
    /// See [`TerrainMapError`].
    pub fn from_heights(
        width: u32,
        height: u32,
        heights: Vec<f32>,
        world_width: f32,
        height_scale: f32,
        height_offset: f32,
    ) -> Result<Self, TerrainMapError> {
        if width < 2 || height < 2 {
            return Err(TerrainMapError::TooSmall { w: width, h: height });
        }
        if !world_width.is_finite() || world_width <= 0.0 {
            return Err(TerrainMapError::BadWorldWidth(world_width));
        }
        if !height_scale.is_finite() || !height_offset.is_finite() {
            return Err(TerrainMapError::BadHeightParams {
                scale: height_scale,
                offset: height_offset,
            });
        }
        let expected_samples = (width as usize).saturating_mul(height as usize);
        if heights.len() != expected_samples {
            return Err(TerrainMapError::SampleCount(
                regia_asset::HeightfieldValidationError::MismatchedSampleCount {
                    w: width,
                    h: height,
                    expected: expected_samples,
                    actual: heights.len(),
                },
            ));
        }
        if heights.iter().any(|h| !h.is_finite()) {
            return Err(TerrainMapError::BadHeightParams {
                scale: height_scale,
                offset: height_offset,
            });
        }
        Ok(Self {
            width,
            height,
            world_width,
            world_depth: world_width,
            height_scale,
            height_offset,
            grid_origin: [0.0, 0.0],
            sample_layout: SampleLayout::VertexCentered,
            cell_extent: [world_width, world_width],
            _resident: regia_core::residency::ResidentRegion::of_slice(&heights),
            height_range: std::sync::OnceLock::new(),
            heights,
        })
    }

    /// The map moved so its centre sits at world XZ `origin` (ADR-8179): every sample, the
    /// collider ([`Self::collision_heightfield`]), the holes cut at sample positions, runtime
    /// patches and the GPU draw shift with it. `[0, 0]` (the default) is the map every earlier
    /// release drew, bit for bit.
    ///
    /// The origin must be finite and the same on X and Z. A cell-spaced grid's half-sample
    /// shift is the same on both axes when the spacing matches, so the origin stays equal
    /// even when [`Self::with_world_depth`] sets the Z extent apart from the X extent
    /// (ADR-8369). On a square map the draw folds the origin into `half_width` and the
    /// depth lane holds the same numbers.
    ///
    /// # Matching a cell-spaced simulation grid
    ///
    /// A simulation that places sample `i` of `n` at `(i − n / 2) · world_width / n` (spacing
    /// `world_width / n`, sample `n / 2` at the origin; DO27's simulation grid) is matched by
    /// a map of the same `n` samples with
    ///
    /// - `world_width' = world_width · (n − 1) / n` (the spacing becomes `world_width / n`), and
    /// - a grid origin of `−0.5 · world_width / n` on X and Z (sample `n / 2` lands on 0).
    ///
    /// DO27's 1280 m map at 4096 samples: `world_width' = 1280 · 4095 / 4096 = 1279.6875` m,
    /// spacing 0.3125 m, grid origin `[−0.15625, −0.15625]` m. Without the origin every drawn
    /// and collided sample sits 0.156 m toward +X and +Z of the simulation's.
    ///
    /// ```
    /// use regia_terrain::sampler::TerrainMap;
    ///
    /// let (sim_width, n) = (1280.0f32, 4096u32);
    /// let map = TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize],
    ///     sim_width * (n - 1) as f32 / n as f32, 1.0, 0.0)
    ///     .and_then(|m| m.with_grid_origin([-0.5 * sim_width / n as f32; 2]))
    ///     .expect("a valid map");
    /// assert_eq!(map.sample_to_world(n / 2, n / 2), [0.0, 0.0]);
    /// assert_eq!(map.sample_spacing(), sim_width / n as f32);
    /// ```
    ///
    /// The origin is not saved with the map: the host sets it after loading (ADR-8179).
    ///
    /// # Errors
    ///
    /// [`TerrainMapError::BadGridOrigin`] for a non-finite origin or one that differs on X
    /// and Z.
    pub fn with_grid_origin(mut self, origin: [f32; 2]) -> Result<Self, TerrainMapError> {
        if !(origin[0].is_finite() && origin[1].is_finite()) || origin[0] != origin[1] {
            return Err(TerrainMapError::BadGridOrigin(origin));
        }
        self.grid_origin = origin;
        Ok(self)
    }

    /// The map's world extent along Z set to `world_depth` metres, apart from its X extent
    /// `world_width` (ADR-8369): a rectangular map, drawn, collided, cut and patched without
    /// padding to a square. Without this call `world_depth == world_width`, the square map
    /// every earlier release drew, bit for bit.
    ///
    /// A rectangle with the same sample spacing on both axes has
    /// `world_depth = world_width · (height − 1) / (width − 1)`; any positive depth is
    /// accepted ([`Self::sample_spacing_z`] then differs from [`Self::sample_spacing`]).
    ///
    /// ```
    /// use regia_terrain::sampler::TerrainMap;
    ///
    /// // 4100 × 1540 samples, 0.3125 m apart on both axes.
    /// let (cols, rows, spacing) = (4100u32, 1540u32, 0.3125f32);
    /// let map = TerrainMap::from_heights(cols, rows, vec![0.0; (cols * rows) as usize],
    ///     (cols - 1) as f32 * spacing, 1.0, 0.0)
    ///     .and_then(|m| m.with_world_depth((rows - 1) as f32 * spacing))
    ///     .expect("a valid map");
    /// assert_eq!(map.sample_spacing_z(), map.sample_spacing());
    /// assert_eq!(map.world_min(), [-640.46875, -240.46875]);
    /// ```
    ///
    /// The depth is not saved with the map (the level file keeps one `world_width_m` until the
    /// v0.6 format batch): the host sets it after loading.
    ///
    /// # Errors
    ///
    /// [`TerrainMapError::BadWorldDepth`] for a non-finite or non-positive depth.
    pub fn with_world_depth(mut self, world_depth: f32) -> Result<Self, TerrainMapError> {
        if !world_depth.is_finite() || world_depth <= 0.0 {
            return Err(TerrainMapError::BadWorldDepth(world_depth));
        }
        // On a cell-centered map the depth is the cells' extent, like the width (ADR-10079).
        self.cell_extent[1] = world_depth;
        self.world_depth = self.sample_layout.sample_span(self.height, world_depth);
        Ok(self)
    }

    /// The map with its samples laid out as `layout` says (ADR-10079), its current extents
    /// ([`Self::cell_extent`]) kept.
    ///
    /// [`SampleLayout::CellCentered`] reads the extents as `n` equal cells per axis and puts
    /// sample `i` at its cell's center, `(i + 0.5) · W / n − W / 2` (plus the grid origin): the
    /// layout of a cell-registered raster or a cell-spaced simulation grid. The map stores the
    /// span between its outer samples, `W · (n − 1) / n`, as [`Self::world_width`] (and
    /// likewise [`Self::world_depth`] on Z), so [`Self::sample_spacing`] is `W / n` and the
    /// collider, the CDLOD draw, holes and patches cover the samples' span: a query in the outer
    /// half cell clamps to the edge sample, as a query past any map's edge does.
    /// [`SampleLayout::VertexCentered`] (every map's default) puts the first and last samples
    /// on the edges. Setting the layout a map already has changes nothing, bit for bit; a
    /// later [`Self::with_world_depth`] takes the cells' extent too.
    ///
    /// ```
    /// use regia_terrain::sampler::{SampleLayout, TerrainMap};
    ///
    /// // 8 × 8 samples, each the center of a 2 m cell of a 16 m square.
    /// let map = TerrainMap::from_heights(8, 8, vec![0.0; 64], 16.0, 1.0, 0.0)
    ///     .expect("a valid map")
    ///     .with_sample_layout(SampleLayout::CellCentered);
    /// assert_eq!(map.sample_spacing(), 2.0);
    /// assert_eq!(map.sample_to_world(0, 0), [-7.0, -7.0]);
    /// assert_eq!(map.cell_extent(), [16.0, 16.0]);
    /// ```
    ///
    /// A simulation grid with sample `n / 2` at the origin (`(i − n / 2) · W / n`, DO27's) is a
    /// cell-centered map moved by a grid origin of `−0.5 · W / n` on X and Z
    /// ([`Self::with_grid_origin`]); that stores the same numbers as the ADR-8179 recipe.
    #[must_use]
    pub fn with_sample_layout(mut self, layout: SampleLayout) -> Self {
        if layout == self.sample_layout {
            return self;
        }
        self.sample_layout = layout;
        self.world_width = layout.sample_span(self.width, self.cell_extent[0]);
        self.world_depth = layout.sample_span(self.height, self.cell_extent[1]);
        self
    }

    /// Where the samples sit in the extent ([`Self::with_sample_layout`]):
    /// [`SampleLayout::VertexCentered`] unless set.
    #[must_use]
    pub fn sample_layout(&self) -> SampleLayout {
        self.sample_layout
    }

    /// The authored X and Z extents, metres: `[world_width, world_depth]` on a vertex-centered
    /// map; on a cell-centered one the cells' extent (`n` cells of [`Self::sample_spacing`]),
    /// half a cell wider on every side than [`Self::world_min`]..[`Self::world_max`].
    #[must_use]
    pub fn cell_extent(&self) -> [f32; 2] {
        self.cell_extent
    }

    /// World XZ of the map's centre, metres: `[0, 0]` unless [`Self::with_grid_origin`]
    /// moved it.
    #[must_use]
    pub fn grid_origin(&self) -> [f32; 2] {
        self.grid_origin
    }

    /// World XZ of the map's −X/−Z corner (sample `(0, 0)`), metres.
    #[must_use]
    pub fn world_min(&self) -> [f32; 2] {
        [
            self.grid_origin[0] - self.world_width * 0.5,
            self.grid_origin[1] - self.world_depth * 0.5,
        ]
    }

    /// World XZ of the map's +X/+Z corner (the last sample), metres.
    #[must_use]
    pub fn world_max(&self) -> [f32; 2] {
        [
            self.grid_origin[0] + self.world_width * 0.5,
            self.grid_origin[1] + self.world_depth * 0.5,
        ]
    }

    /// The X map-extent lane of the GPU terrain uniforms (`half_width` in `terrain.wgsl` and
    /// `water.wgsl`). A square map maps world XZ to the grid as `(xz + lane) / world_width`
    /// and places the −X/−Z corner at `−lane`, so the grid origin folds into it (ADR-8179).
    /// `world_width / 2` for the default origin, bit for bit. A rectangular map uses this
    /// lane on X and `gpu_half_depth` on Z (ADR-8369). Grass and the GI heightfield still
    /// read this lane for both axes.
    pub(crate) fn gpu_half_width(&self) -> f32 {
        self.world_width * 0.5 - self.grid_origin[0]
    }

    /// The Z partner of [`Self::gpu_half_width`] (ADR-8369): `world_depth / 2 − origin z`, the
    /// same value for a square map.
    pub(crate) fn gpu_half_depth(&self) -> f32 {
        self.world_depth * 0.5 - self.grid_origin[1]
    }

    /// Whether the map is a rectangle the quadtree square overhangs (`world_depth !=
    /// world_width`, ADR-8369).
    #[must_use]
    pub fn is_rectangular(&self) -> bool {
        self.world_depth != self.world_width
    }

    /// World XZ clamped onto the map's rectangle when it is rectangular, where the draw clamps
    /// the vertices of a node that straddles the shorter side (ADR-8369); unchanged on a
    /// square map, bit for bit.
    pub(crate) fn clamp_to_rect(&self, x: f32, z: f32) -> [f32; 2] {
        if self.is_rectangular() {
            let ([lo_x, lo_z], [hi_x, hi_z]) = (self.world_min(), self.world_max());
            [x.clamp(lo_x, hi_x), z.clamp(lo_z, hi_z)]
        } else {
            [x, z]
        }
    }

    /// Side of the square the LOD quadtree covers, metres: the longer of [`Self::world_width`]
    /// and [`Self::world_depth`] (ADR-8369), centred on the map. Nodes past the map's shorter
    /// side are not drawn. `world_width` for a square map.
    #[must_use]
    pub fn quadtree_width(&self) -> f32 {
        self.world_width.max(self.world_depth)
    }

    /// Sample count along the side [`Self::quadtree_width`] measures: [`Self::width`], or
    /// [`Self::height`] on a map deeper than it is wide. With `quadtree_width` it is the input
    /// of [`crate::quadtree_span`] (ADR-8421).
    #[must_use]
    pub fn quadtree_samples(&self) -> u32 {
        if self.world_depth > self.world_width {
            self.height
        } else {
            self.width
        }
    }

    /// Width in sample columns.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in sample rows.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// World-space extent along X, metres (and along Z unless [`Self::with_world_depth`] set
    /// a separate depth).
    pub fn world_width(&self) -> f32 {
        self.world_width
    }

    /// World-space extent along Z, metres: [`Self::world_width`] unless
    /// [`Self::with_world_depth`] set it (ADR-8369).
    #[must_use]
    pub fn world_depth(&self) -> f32 {
        self.world_depth
    }

    /// `[world_width, world_depth]`, metres.
    #[must_use]
    pub fn world_extent(&self) -> [f32; 2] {
        [self.world_width, self.world_depth]
    }

    /// Height of a full-scale sample (65535), metres.
    pub fn height_scale(&self) -> f32 {
        self.height_scale
    }

    /// Height of a zero sample, metres.
    pub fn height_offset(&self) -> f32 {
        self.height_offset
    }

    /// The metre-valued height mirror, row-major (row 0 = −Z edge).
    pub fn heights(&self) -> &[f32] {
        &self.heights
    }

    /// Address of the height buffer. A selection cache uses it to tell two maps apart;
    /// an in-place edit keeps the address and is caught by the draw's height generation.
    pub(crate) fn heights_addr(&self) -> usize {
        self.heights.as_ptr().addr()
    }

    /// Distance between two adjacent samples along X, metres.
    ///
    /// Z spacing is `world_depth / (height − 1)` ([`Self::sample_spacing_z`]).
    pub fn sample_spacing(&self) -> f32 {
        self.world_width / (self.width - 1) as f32
    }

    /// Distance between two adjacent samples along Z, metres (`world_depth / (height − 1)`).
    ///
    /// Equal to [`Self::sample_spacing`] when the grid's aspect matches its world extent's.
    #[must_use]
    pub fn sample_spacing_z(&self) -> f32 {
        self.world_depth / (self.height - 1) as f32
    }

    /// Exact height of one sample, clamping out-of-range indices to the border.
    pub fn height_at_sample(&self, col: i64, row: i64) -> f32 {
        let col = col.clamp(0, self.width as i64 - 1) as u32 as usize;
        let row = row.clamp(0, self.height as i64 - 1) as u32 as usize;
        self.heights[row * self.width as usize + col]
    }

    /// World XZ position of a sample, metres (shifted by [`Self::grid_origin`]).
    ///
    /// With this and [`Self::world_to_grid`] every world/grid conversion of the terrain goes
    /// through one pair of functions (ADR-8179). A map with a grid origin computes both in
    /// `f64`, so a cell-spaced simulation's sample positions map to whole sample indices and
    /// back exactly; a map on the world origin keeps its historical `f32` expressions, bit
    /// for bit.
    pub fn sample_to_world(&self, col: u32, row: u32) -> [f32; 2] {
        [
            grid_to_world_axis(col, self.width, self.world_width, self.grid_origin[0]),
            grid_to_world_axis(row, self.height, self.world_depth, self.grid_origin[1]),
        ]
    }

    /// Continuous sample coordinates (column, row) of a world XZ point. Values outside the map
    /// are not clamped here; the sampling functions clamp.
    pub fn world_to_grid(&self, x: f32, z: f32) -> [f32; 2] {
        [
            world_to_grid_axis(x, self.width, self.world_width, self.grid_origin[0]),
            world_to_grid_axis(z, self.height, self.world_depth, self.grid_origin[1]),
        ]
    }

    /// Bilinear height at world XZ, clamped to the border outside the map.
    ///
    /// This is the base surface the simulation collides with (spec 26 § 1): no smoothing, no
    /// resampling — the bilinear interpolation of the quantised source samples.
    pub fn sample_bilinear(&self, x: f32, z: f32) -> f32 {
        let [u, v] = self.world_to_grid(x, z);
        self.sample_bilinear_grid(u, v)
    }

    /// Bilinear height at continuous sample coordinates (column, row), border-clamped.
    pub fn sample_bilinear_grid(&self, u: f32, v: f32) -> f32 {
        let w = self.width as usize;
        bilinear_grid(self.width, self.height, u, v, |col, row| {
            self.heights[row * w + col]
        })
    }

    /// Surface normal at world XZ by central differences of the bilinear field (border-clamped;
    /// one-sided at the border through clamping). Matches the WGSL normal of the terrain
    /// vertex shader.
    pub fn normal_at(&self, x: f32, z: f32) -> [f32; 3] {
        let spacing_x = self.world_width / (self.width - 1) as f32;
        let spacing_z = self.world_depth / (self.height - 1) as f32;
        central_normal([spacing_x, spacing_z], x, z, |x, z| self.sample_bilinear(x, z))
    }

    /// Bilinear height at sample `(col, row)` plus the fractions `(fu, fv)` in `[0, 1)`, with the
    /// same border clamp and the same arithmetic as [`TerrainMap::sample_bilinear_grid`]. The
    /// integer part never passes through `f32`, so two maps that hold the same samples at an
    /// integer offset answer with the same bits (world-anchored scatter, Q-08).
    pub(crate) fn sample_bilinear_lattice(&self, col: i64, fu: f32, row: i64, fv: f32) -> f32 {
        let w = self.width as usize;
        bilinear_lattice(self.width, self.height, col, fu, row, fv, |col, row| {
            self.heights[row * w + col]
        })
    }

    /// The samples a reader of the world XZ box `lo..hi` can touch through
    /// [`Self::sample_bilinear`] and [`Self::normal_at`] (the box grown by two samples, clamped
    /// to the map), copied out with the map's grid and its height range (ADR-7638). The window
    /// answers those queries bit for bit as the map does at any point of the box.
    pub(crate) fn window(&self, lo: [f32; 2], hi: [f32; 2]) -> HeightWindow {
        let [u0, v0] = self.world_to_grid(lo[0], lo[1]);
        let [u1, v1] = self.world_to_grid(hi[0], hi[1]);
        // Bilinear reads `floor(u) ..= floor(u) + 1`; the normal's taps sit one sample further.
        let span = |a: f32, b: f32, side: u32| {
            let first = if a.is_nan() {
                0.0
            } else {
                (a.floor() - 2.0).max(0.0)
            };
            let last = if b.is_nan() {
                0.0
            } else {
                (b.floor() + 3.0).max(0.0)
            };
            let first = (first as u32).min(side - 1);
            let last = (last as u32).clamp(first, side - 1);
            (first, last - first + 1)
        };
        let (col0, cols) = span(u0.min(u1), u0.max(u1), self.width);
        let (row0, rows) = span(v0.min(v1), v0.max(v1), self.height);
        let w = self.width as usize;
        let mut samples = Vec::with_capacity(cols as usize * rows as usize);
        for row in row0..row0 + rows {
            let base = row as usize * w + col0 as usize;
            samples.extend_from_slice(&self.heights[base..base + cols as usize]);
        }
        HeightWindow {
            width: self.width,
            height: self.height,
            world_width: self.world_width,
            world_depth: self.world_depth,
            grid_origin: self.grid_origin,
            col0,
            row0,
            cols,
            rows,
            samples,
            range: self.min_max_height(),
            edited: Vec::new(),
        }
    }

    /// Lowest and highest height in the map.
    pub fn min_max_height(&self) -> (f32, f32) {
        *self.height_range.get_or_init(|| {
            let mut min = f32::INFINITY;
            let mut max = f32::NEG_INFINITY;
            for &h in &self.heights {
                min = min.min(h);
                max = max.max(h);
            }
            (min, max)
        })
    }

    /// Height of one sample by flat index (row-major), for deformation.
    pub(crate) fn height_at_index(&self, index: usize) -> f32 {
        self.heights[index]
    }

    /// Sets the height of one sample by flat index (row-major), for deformation.
    ///
    /// A known height range stays known unless the write raises the lowest sample or lowers the
    /// highest one (then the next [`Self::min_max_height`] scans the map again): an edit between
    /// the extremes costs no scan of the whole map.
    pub(crate) fn set_height(&mut self, index: usize, value: f32) {
        let old = std::mem::replace(&mut self.heights[index], value);
        let keep = self.height_range.get_mut().is_some_and(|(lo, hi)| {
            if (old <= *lo && value > old) || (old >= *hi && value < old) {
                return false;
            }
            *lo = lo.min(value);
            *hi = hi.max(value);
            true
        });
        if !keep {
            self.height_range.take();
        }
    }
}

/// Bilinear height at continuous sample coordinates of a `width × height` grid whose sample
/// `(col, row)` is `at(col, row)`, border-clamped ([`TerrainMap::sample_bilinear_grid`]; one
/// body, so a [`HeightWindow`] answers bit for bit as the map).
#[inline]
fn bilinear_grid(width: u32, height: u32, u: f32, v: f32, at: impl Fn(usize, usize) -> f32) -> f32 {
    let w = width as usize;
    let h = height as usize;
    // A NaN coordinate (a NaN query position) reads the −X/−Z border instead of poisoning
    // the result: `clamp` passes NaN through.
    let u = if u.is_nan() { 0.0 } else { u };
    let v = if v.is_nan() { 0.0 } else { v };
    let u = u.clamp(0.0, (w - 1) as f32);
    let v = v.clamp(0.0, (h - 1) as f32);
    // Past 2^24 the `f32` bound rounds up to the side length, so clamp the indices too.
    let iu = (u.floor() as usize).min(w - 1);
    let iv = (v.floor() as usize).min(h - 1);
    let iu1 = (iu + 1).min(w - 1);
    let iv1 = (iv + 1).min(h - 1);
    let fu = u - iu as f32;
    let fv = v - iv as f32;
    let h00 = at(iu, iv);
    let h10 = at(iu1, iv);
    let h01 = at(iu, iv1);
    let h11 = at(iu1, iv1);
    let a = h00 + (h10 - h00) * fu;
    let b = h01 + (h11 - h01) * fu;
    a + (b - a) * fv
}

/// The central-difference normal of [`TerrainMap::normal_at`] over the bilinear field `sample`.
#[inline]
fn central_normal(spacing: [f32; 2], x: f32, z: f32, sample: impl Fn(f32, f32) -> f32) -> [f32; 3] {
    let [spacing_x, spacing_z] = spacing;
    let h_l = sample(x - spacing_x, z);
    let h_r = sample(x + spacing_x, z);
    let h_d = sample(x, z - spacing_z);
    let h_u = sample(x, z + spacing_z);
    normal_from_differences([h_l, h_r, h_d, h_u], spacing_x, spacing_z)
}

/// [`bilinear_grid`] at sample `(col, row)` plus the fractions `(fu, fv)`: the integer part never
/// passes through `f32` (world-anchored scatter, Q-08). The border clamp gives the same height as
/// [`bilinear_grid`] at the same point; one body, so a [`HeightWindow`] answers bit for bit as
/// the map.
#[inline]
fn bilinear_lattice(
    width: u32,
    height: u32,
    col: i64,
    fu: f32,
    row: i64,
    fv: f32,
    at: impl Fn(usize, usize) -> f32,
) -> f32 {
    let clamp = |i: i64, f: f32, n: i64| {
        if !f.is_finite() || i < 0 {
            (0, 0.0)
        } else if i >= n - 1 {
            ((n - 1) as usize, 0.0)
        } else {
            (i as usize, f)
        }
    };
    let (iu, fu) = clamp(col, fu, i64::from(width));
    let (iv, fv) = clamp(row, fv, i64::from(height));
    let iu1 = (iu + 1).min(width as usize - 1);
    let iv1 = (iv + 1).min(height as usize - 1);
    let h00 = at(iu, iv);
    let h10 = at(iu1, iv);
    let h01 = at(iu, iv1);
    let h11 = at(iu1, iv1);
    let a = h00 + (h10 - h00) * fu;
    let b = h01 + (h11 - h01) * fu;
    a + (b - a) * fv
}

/// A rectangle of a [`TerrainMap`]'s samples with the map's grid and height range
/// ([`TerrainMap::window`], ADR-7638): what one scatter tile placement reads, copied under a
/// short lock, so the placement never holds the field's map while it runs.
#[derive(Clone, Debug)]
pub(crate) struct HeightWindow {
    /// The map's sample columns.
    width: u32,
    /// The map's sample rows.
    height: u32,
    /// The map's world extent along X, metres.
    world_width: f32,
    /// The map's world extent along Z, metres ([`TerrainMap::world_depth`], ADR-8369).
    world_depth: f32,
    /// The map's grid origin ([`TerrainMap::grid_origin`], ADR-8179).
    grid_origin: [f32; 2],
    /// First column and row of the copied rectangle.
    col0: u32,
    row0: u32,
    /// Columns and rows copied.
    cols: u32,
    rows: u32,
    /// The copied samples, row-major.
    samples: Vec<f32>,
    /// The whole map's lowest and highest height when the window was taken.
    range: (f32, f32),
    /// Which copied samples a runtime edit wrote, row-major like `samples` (ADR-7646, set by
    /// the scatter field); empty = none.
    edited: Vec<bool>,
}

impl HeightWindow {
    /// World extent of the map, metres.
    pub(crate) fn world_width(&self) -> f32 {
        self.world_width
    }

    /// [`TerrainMap::sample_bilinear`], bit for bit inside the window's box. A read outside the
    /// copied rectangle takes its nearest copied sample (never a panic).
    pub(crate) fn sample_bilinear(&self, x: f32, z: f32) -> f32 {
        // The map's own world-to-grid step (origin and depth included, ADR-8421).
        let u = world_to_grid_axis(x, self.width, self.world_width, self.grid_origin[0]);
        let v = world_to_grid_axis(z, self.height, self.world_depth, self.grid_origin[1]);
        let cols = self.cols as usize;
        bilinear_grid(self.width, self.height, u, v, |col, row| {
            let c = (col as u32).clamp(self.col0, self.col0 + self.cols - 1) - self.col0;
            let r = (row as u32).clamp(self.row0, self.row0 + self.rows - 1) - self.row0;
            self.samples[r as usize * cols + c as usize]
        })
    }

    /// The map's sample spacing per axis, metres ([`TerrainMap::sample_spacing`],
    /// [`TerrainMap::sample_spacing_z`]).
    pub(crate) fn sample_spacings(&self) -> [f32; 2] {
        [
            self.world_width / (self.width - 1) as f32,
            self.world_depth / (self.height - 1) as f32,
        ]
    }

    /// World XZ of the map's sample `(0, 0)` ([`TerrainMap::world_min`]).
    pub(crate) fn world_min(&self) -> [f32; 2] {
        [
            self.grid_origin[0] - self.world_width * 0.5,
            self.grid_origin[1] - self.world_depth * 0.5,
        ]
    }

    /// [`TerrainMap::sample_bilinear_lattice`], bit for bit when the samples it reads lie in the
    /// copied rectangle (a read outside takes its nearest copied sample, never a panic).
    pub(crate) fn sample_bilinear_lattice(&self, col: i64, fu: f32, row: i64, fv: f32) -> f32 {
        let cols = self.cols as usize;
        bilinear_lattice(self.width, self.height, col, fu, row, fv, |col, row| {
            let c = (col as u32).clamp(self.col0, self.col0 + self.cols - 1) - self.col0;
            let r = (row as u32).clamp(self.row0, self.row0 + self.rows - 1) - self.row0;
            self.samples[r as usize * cols + c as usize]
        })
    }

    /// [`TerrainMap::normal_at`], bit for bit inside the window's box.
    pub(crate) fn normal_at(&self, x: f32, z: f32) -> [f32; 3] {
        let spacing_x = self.world_width / (self.width - 1) as f32;
        let spacing_z = self.world_depth / (self.height - 1) as f32;
        central_normal([spacing_x, spacing_z], x, z, |x, z| self.sample_bilinear(x, z))
    }

    /// The whole map's height range when the window was taken ([`TerrainMap::min_max_height`]).
    pub(crate) fn min_max_height(&self) -> (f32, f32) {
        self.range
    }

    /// Records which copied samples a runtime edit wrote: `edited(col, row)` (map indices) for
    /// every sample of the window (ADR-7646). Nothing edited keeps the record empty.
    pub(crate) fn set_edited(&mut self, edited: impl Fn(u32, u32) -> bool) {
        let mut flags = Vec::with_capacity(self.samples.len());
        for row in self.row0..self.row0 + self.rows {
            flags.extend((self.col0..self.col0 + self.cols).map(|col| edited(col, row)));
        }
        self.edited = if flags.contains(&true) { flags } else { Vec::new() };
    }

    /// Whether a runtime edit wrote a sample [`Self::sample_bilinear`] weighs at world XZ (its
    /// bilinear stencil: the sample at or below the point on each axis, and the next one when
    /// the point lies past it), so whether the height there can differ from the unedited map's
    /// (ADR-7646). The same set of points as the edited samples' footprints grown by one sample
    /// spacing, open. A read outside the copied rectangle takes its nearest copied sample.
    pub(crate) fn edited_near(&self, x: f32, z: f32) -> bool {
        if self.edited.is_empty() {
            return false;
        }
        // The map's own world-to-grid step (origin and depth included, ADR-8421).
        let u = world_to_grid_axis(x, self.width, self.world_width, self.grid_origin[0]);
        let v = world_to_grid_axis(z, self.height, self.world_depth, self.grid_origin[1]);
        // The index math of `bilinear_grid`, keeping a corner only when it has weight.
        let corners = |t: f32, side: u32| {
            let t = if t.is_nan() { 0.0 } else { t };
            let t = t.clamp(0.0, (side - 1) as f32);
            let i = (t.floor() as u32).min(side - 1);
            let next = if t > i as f32 { (i + 1).min(side - 1) } else { i };
            [i, next]
        };
        let cols = self.cols as usize;
        let flag = |col: u32, row: u32| {
            let c = col.clamp(self.col0, self.col0 + self.cols - 1) - self.col0;
            let r = row.clamp(self.row0, self.row0 + self.rows - 1) - self.row0;
            self.edited[r as usize * cols + c as usize]
        };
        let [c0, c1] = corners(u, self.width);
        let [r0, r1] = corners(v, self.height);
        flag(c0, r0) || flag(c1, r0) || flag(c0, r1) || flag(c1, r1)
    }
}

/// Normal from the four central-difference heights `[left, right, down, up]` (the formula of
/// [`TerrainMap::normal_at`]).
pub(crate) fn normal_from_differences(h: [f32; 4], spacing_x: f32, spacing_z: f32) -> [f32; 3] {
    let [h_l, h_r, h_d, h_u] = h;
    let n = [
        (h_l - h_r) * spacing_z,
        2.0 * spacing_x * spacing_z,
        (h_d - h_u) * spacing_x,
    ];
    let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if len.is_finite() && len > 0.0 {
        [n[0] / len, n[1] / len, n[2] / len]
    } else {
        [0.0, 1.0, 0.0]
    }
}

/// World coordinate of sample `i` of `n` on one axis of a map `world_width` wide centred on
/// `origin` ([`TerrainMap::sample_to_world`]).
pub(crate) fn grid_to_world_axis(i: u32, n: u32, world_width: f32, origin: f32) -> f32 {
    let last = n.max(2) - 1;
    if origin == 0.0 {
        // The historical expression, bit for bit.
        i as f32 / last as f32 * world_width - world_width * 0.5
    } else {
        // `i·w − w·last/2` is exact in f64 (both products fit its mantissa), so the one
        // division is the only rounding: a position a cell-spaced grid can name exactly comes
        // out exactly (sample n/2 of a matched map is 0, not 1e-13).
        let (w, o, last) = (f64::from(world_width), f64::from(origin), f64::from(last));
        ((f64::from(i) * w - 0.5 * w * last) / last + o) as f32
    }
}

/// Continuous sample coordinate of world coordinate `v` on one axis
/// ([`TerrainMap::world_to_grid`]).
pub(crate) fn world_to_grid_axis(v: f32, n: u32, world_width: f32, origin: f32) -> f32 {
    let last = n.max(2) - 1;
    if origin == 0.0 {
        (v + world_width * 0.5) / world_width * last as f32
    } else {
        // Multiply first and divide once, so a sample position maps to its whole index.
        let (w, o) = (f64::from(world_width), f64::from(origin));
        ((f64::from(v) - o + w * 0.5) * f64::from(last) / w) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-7638: a window answers `sample_bilinear`, `normal_at` and `min_max_height` bit for bit
    /// as its map at every point of its box, the map's border included, and copies only the box
    /// grown by two samples.
    #[test]
    fn a_window_samples_its_box_bit_for_bit_as_the_map() {
        let n = 129;
        let map = TerrainMap::from_heights(
            n,
            n,
            crate::synthetic_mountains(n, 512.0, 90.0, 3),
            512.0,
            1.0,
            0.0,
        )
        .expect("map");
        let spacing = map.sample_spacing();
        for (lo, hi) in [
            ([-40.0f32, 10.0f32], [8.0f32, 58.0f32]),
            ([-256.0, -256.0], [-200.0, -230.0]),
            ([200.0, 230.0], [256.0, 256.0]),
            ([-3.3, -3.3], [-3.3, -3.3]),
        ] {
            let window = map.window(lo, hi);
            let cols = ((hi[0] - lo[0]) / spacing).ceil() as usize + 6;
            let rows = ((hi[1] - lo[1]) / spacing).ceil() as usize + 6;
            assert!(
                window.samples.len() <= cols * rows,
                "{} samples for {lo:?}..{hi:?}",
                window.samples.len()
            );
            assert_eq!(window.min_max_height(), map.min_max_height());
            for i in 0..=40 {
                for j in 0..=40 {
                    let x = lo[0] + (hi[0] - lo[0]) * i as f32 / 40.0;
                    let z = lo[1] + (hi[1] - lo[1]) * j as f32 / 40.0;
                    assert_eq!(
                        window.sample_bilinear(x, z).to_bits(),
                        map.sample_bilinear(x, z).to_bits(),
                        "({x}, {z})"
                    );
                    assert_eq!(
                        window.normal_at(x, z).map(f32::to_bits),
                        map.normal_at(x, z).map(f32::to_bits),
                        "({x}, {z})"
                    );
                }
            }
        }
    }

    /// ADR-7638: `set_height` keeps a known height range up to date without a scan of the map
    /// unless it raises the lowest sample or lowers the highest; either way the range equals a
    /// fresh scan after every write.
    #[test]
    fn the_height_range_follows_every_write() {
        let n = 33;
        let mut map = TerrainMap::from_heights(
            n,
            n,
            crate::synthetic_mountains(n, 128.0, 40.0, 5),
            128.0,
            1.0,
            0.0,
        )
        .expect("map");
        let scan = |m: &TerrainMap| {
            m.heights()
                .iter()
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &h| {
                    (lo.min(h), hi.max(h))
                })
        };
        map.min_max_height();
        let (lo_index, hi_index) = {
            let h = map.heights();
            let lo = (0..h.len())
                .min_by(|&a, &b| h[a].total_cmp(&h[b]))
                .expect("samples");
            let hi = (0..h.len())
                .max_by(|&a, &b| h[a].total_cmp(&h[b]))
                .expect("samples");
            (lo, hi)
        };
        let (mut kept, mut dropped) = (0, 0);
        for (k, (index, value)) in [
            (100usize, 12.5f32),
            (101, -30.0),
            (102, 400.0),
            (lo_index, 1.0e3),
            (hi_index, -1.0e3),
            (500, 7.0),
            // Raises the lowest sample (-30 at 101): the range must be scanned again.
            (101, 5.0),
        ]
        .into_iter()
        .enumerate()
        {
            let was_known = map.height_range.get().is_some();
            map.set_height(index, value);
            kept += usize::from(was_known && map.height_range.get().is_some());
            dropped += usize::from(was_known && map.height_range.get().is_none());
            assert_eq!(map.min_max_height(), scan(&map), "write {k}");
        }
        assert_eq!(
            (kept, dropped),
            (6, 1),
            "writes between the extremes keep the range"
        );
    }

    /// A side past 2^24 rounds the `f32` clamp bound up to the side length; the far corner must
    /// read the last sample instead of indexing one past the buffer.
    #[test]
    fn far_corner_of_a_side_past_two_pow_24_stays_in_bounds() {
        let w = 16_777_220_u32;
        let map =
            TerrainMap::from_heights(w, 2, vec![0.0; w as usize * 2], 100.0, 1.0, 0.0).expect("valid map");
        assert_eq!(map.sample_bilinear_grid(w as f32, 1.0), 0.0);
    }

    fn ramp_map() -> TerrainMap {
        // 3x3 grid, height rises linearly with column: h = col.
        let mut samples = vec![0u16; 9];
        for row in 0..3 {
            for col in 0..3 {
                samples[row * 3 + col] = (col * 21_845) as u16; // 0, 0.333, 0.667 of full scale
            }
        }
        let hf = Heightfield {
            width: 3,
            height: 3,
            samples,
        };
        TerrainMap::new(&hf, 4.0, 3.0, 0.0).expect("valid map")
    }

    #[test]
    fn samples_convert_to_metres() {
        let map = ramp_map();
        // col 2 sample = 43690/65535 * 3.0 = 2.0 m exactly (43690 = 2*21845).
        assert!((map.height_at_sample(2, 0) - 2.0).abs() < 1e-4);
        assert!((map.height_at_sample(0, 1) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn bilinear_hits_sample_values_at_vertices() {
        let map = ramp_map();
        for row in 0..3 {
            for col in 0..3 {
                let [x, z] = map.sample_to_world(col, row);
                let sampled = map.sample_bilinear(x, z);
                let exact = map.height_at_sample(col as i64, row as i64);
                assert!(
                    (sampled - exact).abs() < 1e-4,
                    "vertex ({col},{row}): {sampled} vs {exact}"
                );
            }
        }
    }

    #[test]
    fn bilinear_interpolates_midpoints() {
        let map = ramp_map();
        // Midpoint between col 0 (0 m) and col 1 (1 m) at row 0.
        let [x0, z] = map.sample_to_world(0, 0);
        let [x1, _] = map.sample_to_world(1, 0);
        let h = map.sample_bilinear((x0 + x1) * 0.5, z);
        assert!((h - 0.5).abs() < 1e-3, "midpoint height {h}");
    }

    #[test]
    fn bilinear_clamps_outside_the_map() {
        let map = ramp_map();
        let inside = map.sample_bilinear(-2.0, -2.0); // the -X/-Z corner sample
        let outside = map.sample_bilinear(-1000.0, -1000.0);
        assert!((inside - outside).abs() < 1e-4, "border clamp");
    }

    #[test]
    fn rejects_bad_parameters() {
        let hf = Heightfield {
            width: 2,
            height: 2,
            samples: vec![0; 4],
        };
        assert!(matches!(
            TerrainMap::new(&hf, 0.0, 1.0, 0.0),
            Err(TerrainMapError::BadWorldWidth(_))
        ));
        assert!(matches!(
            TerrainMap::new(&hf, 8.0, f32::NAN, 0.0),
            Err(TerrainMapError::BadHeightParams { .. })
        ));
        let tiny = Heightfield {
            width: 1,
            height: 1,
            samples: vec![0],
        };
        assert!(matches!(
            TerrainMap::new(&tiny, 8.0, 1.0, 0.0),
            Err(TerrainMapError::TooSmall { .. })
        ));
    }

    #[test]
    fn normal_points_up_on_flat_ground() {
        let hf = Heightfield {
            width: 3,
            height: 3,
            samples: vec![16384; 9],
        };
        let map = TerrainMap::new(&hf, 10.0, 5.0, 0.0).expect("valid map");
        let n = map.normal_at(0.0, 0.0);
        assert!((n[0]).abs() < 1e-5 && (n[1] - 1.0).abs() < 1e-5 && (n[2]).abs() < 1e-5);
    }

    #[test]
    fn normal_tilts_against_an_upslope() {
        // Rising toward +X: normals must tilt toward -X.
        let map = ramp_map();
        let n = map.normal_at(0.0, 0.0);
        assert!(n[0] < 0.0, "normal {n:?}");
        assert!(n[1] > 0.0, "normal {n:?}");
    }

    #[test]
    fn two_by_two_map_samples_corners_and_centre() {
        let map = TerrainMap::from_heights(2, 2, vec![0.0, 4.0, 8.0, 12.0], 10.0, 1.0, 0.0).expect("map");
        assert_eq!(map.sample_bilinear(-5.0, -5.0), 0.0);
        assert_eq!(map.sample_bilinear(5.0, -5.0), 4.0);
        assert_eq!(map.sample_bilinear(-5.0, 5.0), 8.0);
        assert_eq!(map.sample_bilinear(5.0, 5.0), 12.0);
        assert!((map.sample_bilinear(0.0, 0.0) - 6.0).abs() < 1e-5);
        assert_eq!(map.sample_spacing(), 10.0);
    }

    #[test]
    fn one_by_n_and_n_by_one_maps_are_rejected() {
        for (w, h) in [(1, 1), (1, 8), (8, 1), (0, 4), (4, 0)] {
            let n = w as usize * h as usize;
            assert!(
                matches!(
                    TerrainMap::from_heights(w, h, vec![0.0; n], 8.0, 1.0, 0.0),
                    Err(TerrainMapError::TooSmall { .. })
                ),
                "{w}x{h}"
            );
        }
    }

    #[test]
    fn non_square_grid_maps_each_axis_independently() {
        // 5 columns x 3 rows over a 8 m wide map: h = col, so the field varies along X only.
        let heights: Vec<f32> = (0..3).flat_map(|_| (0..5).map(|c| c as f32)).collect();
        let map = TerrainMap::from_heights(5, 3, heights, 8.0, 1.0, 0.0).expect("map");
        assert_eq!(map.sample_to_world(4, 2), [4.0, 4.0]);
        assert_eq!(map.world_to_grid(4.0, 4.0), [4.0, 2.0]);
        assert!((map.sample_bilinear(1.0, -3.0) - 2.5).abs() < 1e-5);
        assert!((map.sample_bilinear(1.0, 3.0) - 2.5).abs() < 1e-5);
        // Flat along Z: the normal has no Z lean, and tilts against +X.
        let n = map.normal_at(0.0, 0.0);
        assert!(n[2].abs() < 1e-5 && n[0] < 0.0, "normal {n:?}");
    }

    #[test]
    fn map_edges_clamp_and_stay_finite() {
        let map = ramp_map();
        let edge = map.sample_bilinear(2.0, 0.0);
        assert!((edge - 2.0).abs() < 1e-3);
        for &(x, z) in &[
            (2.0, 2.0),
            (-2.0, -2.0),
            (1e30, -1e30),
            (f32::INFINITY, f32::NEG_INFINITY),
        ] {
            assert!(map.sample_bilinear(x, z).is_finite(), "({x}, {z})");
            let n = map.normal_at(x, z);
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            assert!((len - 1.0).abs() < 1e-4, "unit normal at ({x}, {z}): {n:?}");
        }
        assert_eq!(map.height_at_sample(-5, 99), map.height_at_sample(0, 2));
        assert_eq!(
            map.height_at_sample(i64::MAX, i64::MIN),
            map.height_at_sample(2, 0)
        );
    }

    #[test]
    fn nan_queries_read_the_border_instead_of_poisoning_results() {
        let map = ramp_map();
        assert!(map.sample_bilinear(f32::NAN, 0.0).is_finite());
        assert!(map.sample_bilinear(0.0, f32::NAN).is_finite());
        assert!(map.sample_bilinear_grid(f32::NAN, f32::NAN).is_finite());
        let n = map.normal_at(f32::NAN, f32::NAN);
        assert!(n.iter().all(|c| c.is_finite()), "normal {n:?}");
    }

    #[test]
    fn nan_and_infinite_heights_are_rejected() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut heights = vec![0.0; 9];
            heights[4] = bad;
            assert!(matches!(
                TerrainMap::from_heights(3, 3, heights, 8.0, 1.0, 0.0),
                Err(TerrainMapError::BadHeightParams { .. })
            ));
        }
        assert!(matches!(
            TerrainMap::from_heights(2, 2, vec![0.0; 4], f32::NAN, 1.0, 0.0),
            Err(TerrainMapError::BadWorldWidth(_))
        ));
        assert!(matches!(
            TerrainMap::from_heights(3, 3, vec![0.0; 8], 8.0, 1.0, 0.0),
            Err(TerrainMapError::SampleCount(_))
        ));
    }

    #[test]
    fn min_max_and_offset_apply_to_quantised_samples() {
        let hf = Heightfield {
            width: 2,
            height: 2,
            samples: vec![0, 65535, 0, 65535],
        };
        let map = TerrainMap::new(&hf, 4.0, 10.0, -5.0).expect("map");
        let (lo, hi) = map.min_max_height();
        assert!((lo + 5.0).abs() < 1e-5 && (hi - 5.0).abs() < 1e-5, "{lo}..{hi}");
    }

    /// ADR-7646: `edited_near` holds exactly where a marked sample weighs in the bilinear height
    /// (within one sample spacing of it on both axes, open; the point that exactly hits a
    /// neighbouring sample does not count), border-clamped, and a window with nothing marked
    /// keeps no record.
    #[test]
    fn edited_near_is_the_marked_samples_open_one_spacing_box() {
        let n = 17u32;
        // 160 m over 16 cells: 10 m spacing; a 2.5 m lattice hits samples and cell midpoints
        // exactly in f32.
        let map = TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], 160.0, 1.0, 0.0).expect("map");
        let marked = |col: u32, row: u32| ((5..=6).contains(&col) && row == 8) || (col, row) == (16, 0);
        let mut window = map.window([-80.0, -80.0], [80.0, 80.0]);
        window.set_edited(marked);
        let (mut near, mut far) = (0, 0);
        for iz in -36..=36 {
            for ix in -36..=36 {
                // Up to 10 m past the border on each side (clamped reads).
                let (x, z) = (ix as f32 * 2.5, iz as f32 * 2.5);
                let u = ((x + 80.0) / 10.0).clamp(0.0, 16.0);
                let v = ((z + 80.0) / 10.0).clamp(0.0, 16.0);
                let expected = (0..n).any(|row| {
                    (0..n).any(|col| {
                        marked(col, row) && (u - col as f32).abs() < 1.0 && (v - row as f32).abs() < 1.0
                    })
                });
                assert_eq!(window.edited_near(x, z), expected, "({x}, {z})");
                if expected {
                    near += 1;
                } else {
                    far += 1;
                }
            }
        }
        assert!(near > 20 && far > 1000, "{near} near, {far} far");
        let mut clean = map.window([-80.0, -80.0], [80.0, 80.0]);
        clean.set_edited(|_, _| false);
        assert!(clean.edited.is_empty(), "nothing edited, no record");
        assert!(!clean.edited_near(-30.0, 0.0));
    }

    /// A map whose heights are `col + 100 · row` at sample `(col, row)`.
    fn indexed(cols: u32, rows: u32, extent: f32) -> TerrainMap {
        let heights = (0..rows)
            .flat_map(|r| (0..cols).map(move |c| c as f32 + 100.0 * r as f32))
            .collect();
        TerrainMap::from_heights(cols, rows, heights, extent, 1.0, 0.0).expect("map")
    }

    /// ADR-10079 (W-24): a cell-centered map's heights sit at its cells' centers: sample `i` of `n`
    /// across `W` at `(i + 0.5) · W / n − W / 2`, the bilinear surface returns each stored height
    /// there, and a world point inside a cell maps to that cell's sample. Fails before the change
    /// (no `with_sample_layout`; the base's map puts sample 0 on the edge, `−W / 2`).
    #[test]
    fn cell_centered_heights_sit_at_cell_centers() {
        let (n, w) = (8u32, 16.0f32);
        let map = indexed(n, n, w).with_sample_layout(SampleLayout::CellCentered);
        assert_eq!(map.sample_layout(), SampleLayout::CellCentered);
        assert_eq!(map.cell_extent(), [16.0, 16.0]);
        assert_eq!(map.sample_spacing(), 2.0, "spacing W / n");
        assert_eq!(map.sample_spacing_z(), 2.0);
        assert_eq!(map.world_min(), [-7.0, -7.0], "the outer samples, half a cell in");
        assert_eq!(map.world_max(), [7.0, 7.0]);
        for row in 0..n {
            for col in 0..n {
                let x = SampleLayout::CellCentered.sample_position(col, n, w);
                let z = SampleLayout::CellCentered.sample_position(row, n, w);
                assert_eq!(x, -8.0 + (col as f32 + 0.5) * 2.0, "the reference");
                let [px, pz] = map.sample_to_world(col, row);
                assert!(
                    (px - x).abs() <= 1e-5 && (pz - z).abs() <= 1e-5,
                    "({col}, {row}): {px}, {pz}"
                );
                let stored = col as f32 + 100.0 * row as f32;
                let got = map.sample_bilinear(x, z);
                assert!((got - stored).abs() <= 1e-3, "({col}, {row}): {got} vs {stored}");
                // Anywhere inside the cell (a quarter cell off its center) is nearest that sample.
                let [u, v] = map.world_to_grid(x + 0.5, z - 0.5);
                assert_eq!((u.round() as u32, v.round() as u32), (col, row));
            }
        }
        // The vertex-centered default of the same numbers puts sample 0 on the edge.
        let vertex = indexed(n, n, w);
        assert_eq!(vertex.sample_layout(), SampleLayout::VertexCentered);
        assert_eq!(vertex.sample_to_world(0, 0), [-8.0, -8.0]);
        assert_eq!(vertex.cell_extent(), [16.0, 16.0]);
    }

    /// ADR-10079: the vertex-centered layout is every map's default and setting it changes
    /// nothing, bit for bit; switching to cell-centered and back restores the map.
    #[test]
    fn vertex_centered_stays_the_default_bit_for_bit() {
        let base = indexed(33, 17, 100.0).with_world_depth(50.0).expect("depth");
        let same = base.clone().with_sample_layout(SampleLayout::VertexCentered);
        let round = base
            .clone()
            .with_sample_layout(SampleLayout::CellCentered)
            .with_sample_layout(SampleLayout::VertexCentered);
        for map in [&same, &round] {
            assert_eq!(
                map.world_extent().map(f32::to_bits),
                base.world_extent().map(f32::to_bits)
            );
            for (col, row) in [(0, 0), (1, 3), (16, 8), (32, 16)] {
                assert_eq!(
                    map.sample_to_world(col, row).map(f32::to_bits),
                    base.sample_to_world(col, row).map(f32::to_bits)
                );
            }
        }
    }

    /// ADR-10079 on a rectangle: X and Z each take their own cell count and extent, and a depth
    /// set after the layout is the cells' extent too.
    #[test]
    fn a_cell_centered_rectangle_centers_each_axis_on_its_own_cells() {
        let (cols, rows) = (8u32, 4u32);
        let map = indexed(cols, rows, 16.0)
            .with_sample_layout(SampleLayout::CellCentered)
            .with_world_depth(8.0)
            .expect("depth");
        assert_eq!(map.cell_extent(), [16.0, 8.0]);
        assert_eq!(map.world_extent(), [14.0, 6.0]);
        assert_eq!(map.sample_spacing(), 2.0);
        assert_eq!(map.sample_spacing_z(), 2.0);
        for row in 0..rows {
            let z = SampleLayout::CellCentered.sample_position(row, rows, 8.0);
            assert!((map.sample_to_world(0, row)[1] - z).abs() <= 1e-5, "row {row}");
        }
        // The same map with the depth set first.
        let first = indexed(cols, rows, 16.0)
            .with_world_depth(8.0)
            .expect("depth")
            .with_sample_layout(SampleLayout::CellCentered);
        assert_eq!(
            first.world_extent().map(f32::to_bits),
            map.world_extent().map(f32::to_bits)
        );
    }

    /// ADR-10079 and ADR-8179: a cell-centered map moved by `−0.5 · W / n` is the cell-spaced
    /// simulation grid (sample `n / 2` at the origin) and stores the same numbers as the
    /// ADR-8179 recipe (`W' = W (n − 1) / n` and the same origin), so both draw and collide alike.
    #[test]
    fn cell_centered_plus_half_cell_origin_is_the_adr_8179_recipe() {
        let (n, w) = (256u32, 80.0f32);
        let origin = [-0.5 * w / n as f32; 2];
        let cell = indexed(n, n, w)
            .with_sample_layout(SampleLayout::CellCentered)
            .with_grid_origin(origin)
            .expect("origin");
        let recipe = indexed(n, n, w * (n - 1) as f32 / n as f32)
            .with_grid_origin(origin)
            .expect("origin");
        assert_eq!(
            cell.world_extent().map(f32::to_bits),
            recipe.world_extent().map(f32::to_bits)
        );
        assert_eq!(cell.sample_to_world(n / 2, n / 2), [0.0, 0.0]);
        assert_eq!(cell.sample_spacing(), 0.3125);
        for i in [0, 1, 127, 128, 200, 255] {
            assert_eq!(
                cell.sample_to_world(i, i).map(f32::to_bits),
                recipe.sample_to_world(i, i).map(f32::to_bits),
                "sample {i}"
            );
        }
        let (a, b) = (
            cell.collision_heightfield(None),
            recipe.collision_heightfield(None),
        );
        assert_eq!(a.scale.map(f32::to_bits), b.scale.map(f32::to_bits));
        assert_eq!(a.center.map(f32::to_bits), b.center.map(f32::to_bits));
    }
}
