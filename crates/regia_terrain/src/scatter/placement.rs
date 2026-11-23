// SPDX-License-Identifier: MPL-2.0
//! Deterministic placement of a [`ScatterSet`] over a [`TerrainMap`].
//!
//! Each class walks a grid of `cell_size` cells. Cell `(cx, cz)` of class *c* seeds a PCG32
//! stream from the set seed, the FNV-1a hash of the class name and the cell coordinate
//! (SplitMix64 avalanche, as in v1), draws its candidate count, then draws a **fixed** number
//! of values per candidate whether or not the candidate survives the filters. So:
//!
//! - placement is identical on every client, every run and in any cell order;
//! - it never depends on the render LOD (positions come from the bilinear source heightfield,
//!   the same surface the simulation reads);
//! - changing a density map or an exclusion only removes or adds the candidates it covers —
//!   everything else stays exactly where it was.
//!
//! Positions, scales, mesh picks and yaw are pure `f32` arithmetic on PCG32 output. The up
//! vector is normalised with `sqrt` only (no trigonometry), so it is reproducible too.

use regia_core::math::{Rng, det_acos, det_powf};

use super::set::{ScatterClass, ScatterSet};
use super::{ScatterError, cell_seed};
use crate::holes::HolesMask;
use crate::layers::TerrainLayers;
use crate::sampler::{HeightWindow, TerrainMap};

/// Values drawn per candidate (x, z, mesh, scale, yaw, tilt x, tilt z, shade, thin, spare).
const DRAWS_PER_CANDIDATE: usize = 10;

/// A grayscale density map over the whole terrain (the heightfield's mapping).
#[derive(Clone, Debug, PartialEq)]
pub struct DensityMap {
    width: u32,
    height: u32,
    values: Vec<f32>,
}

impl DensityMap {
    /// Builds a map from 0..=1 values (row-major, row 0 at −Z).
    ///
    /// # Errors
    ///
    /// [`ScatterError::Asset`] when the size does not match or a value is not finite.
    pub fn new(width: u32, height: u32, values: Vec<f32>) -> Result<Self, ScatterError> {
        let bad = |reason: String| ScatterError::Asset {
            path: "<density map>".into(),
            reason,
        };
        if width < 2 || height < 2 {
            return Err(bad(format!("{width}x{height} is smaller than 2x2")));
        }
        if values.len() != (width as usize) * (height as usize) {
            return Err(bad(format!("{} values for {width}x{height}", values.len())));
        }
        if values.iter().any(|v| !v.is_finite()) {
            return Err(bad("non-finite value".into()));
        }
        let values = values.into_iter().map(|v| v.clamp(0.0, 1.0)).collect();
        Ok(Self {
            width,
            height,
            values,
        })
    }

    /// Builds a map from a cooked 16-bit grayscale image (the heightfield importer's output; an
    /// 8-bit PNG is widened by the importer).
    ///
    /// # Errors
    ///
    /// See [`DensityMap::new`].
    pub fn from_heightfield(image: &regia_asset::Heightfield) -> Result<Self, ScatterError> {
        let values = image.samples.iter().map(|&s| f32::from(s) / 65535.0).collect();
        Self::new(image.width, image.height, values)
    }

    /// A map that is `value` everywhere (tests, defaults).
    #[must_use]
    pub fn constant(value: f32) -> Self {
        Self {
            width: 2,
            height: 2,
            values: vec![value.clamp(0.0, 1.0); 4],
        }
    }

    /// Bilinear sample at world `(x, z)` on a map of `world_width` metres centred on the origin.
    #[must_use]
    pub fn sample(&self, x: f32, z: f32, world_width: f32) -> f32 {
        let half = world_width * 0.5;
        let u = ((x + half) / world_width).clamp(0.0, 1.0) * (self.width - 1) as f32;
        let v = ((z + half) / world_width).clamp(0.0, 1.0) * (self.height - 1) as f32;
        let (c0, r0) = (u.floor() as usize, v.floor() as usize);
        let (c1, r1) = (
            (c0 + 1).min(self.width as usize - 1),
            (r0 + 1).min(self.height as usize - 1),
        );
        let (fu, fv) = (u - c0 as f32, v - r0 as f32);
        let w = self.width as usize;
        let at = |c: usize, r: usize| self.values[r * w + c];
        let top = at(c0, r0) + (at(c1, r0) - at(c0, r0)) * fu;
        let bottom = at(c0, r1) + (at(c1, r1) - at(c0, r1)) * fu;
        top + (bottom - top) * fv
    }
}

/// Density maps by the asset path the `.scatter.ron` names.
pub type DensityMaps = std::collections::HashMap<String, DensityMap>;

/// One placed instance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlacedInstance {
    /// Index into [`ScatterSet::classes`].
    pub class: u16,
    /// Index into the class's `meshes`.
    pub mesh: u16,
    /// Ground contact point (on the bilinear source surface), metres.
    pub position: [f32; 3],
    /// Unit up axis of the instance (ground normal blend, tilt cap and jitter applied).
    pub up: [f32; 3],
    /// Yaw about `up`, radians.
    pub yaw: f32,
    /// Uniform scale.
    pub scale: f32,
    /// Brightness multiplier (1 ± `tint_jitter`).
    pub shade: f32,
    /// Unit ground normal at the position (for the base sink on slopes).
    pub normal: [f32; 3],
}

/// Inputs every class needs: the terrain and its dressing.
#[derive(Clone, Copy)]
pub struct PlacementInputs<'a> {
    /// Source-of-truth heightfield.
    pub map: &'a TerrainMap,
    /// Tunnel holes (instances avoid them when the class asks).
    pub holes: Option<&'a HolesMask>,
    /// Layer stack for the layer mask.
    pub layers: Option<&'a TerrainLayers>,
    /// Density maps referenced by the set.
    pub density_maps: Option<&'a DensityMaps>,
}

/// Where a class's placement cell grid is anchored (Q-08, ADR-8029).
///
/// A cell's instances are a pure function of its cell index, so the anchor decides which
/// rocks a map gets. With [`PlacementOrigin::MapEdge`] (the default, and every placement
/// before v0.5.1) cell `(0, 0)` starts at the map's −X/−Z corner: growing the map (a backdrop
/// ring) or shrinking it (a low tier that drops the ring) renumbers every cell and moves every
/// rock. With [`PlacementOrigin::World`] cells are counted from a fixed world point, so the
/// rocks inside a map stay where they are when the map grows or shrinks around them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum PlacementOrigin {
    /// Cell `(0, 0)` starts at the map's −X/−Z corner (bit-identical to every earlier release).
    #[default]
    MapEdge,
    /// Cells are counted from a fixed world point ([`WorldAnchor`]).
    World(WorldAnchor),
}

/// A world-anchored placement grid ([`PlacementOrigin::World`]).
///
/// Rocks are bit-identical across two maps when both hold the same samples at the same world
/// points with the same spacing, and the anchor sits on a sample of each (see
/// [`WorldAnchor::lattice_aligned`]); then heights and normals are read relative to the anchor,
/// never through the map's own extent. Two things still follow the map and are not anchored:
/// a class density map is stretched over the map it is given with, and the map's altitude range
/// decides whether a layer band reaches the lowest or highest ground (pin it with
/// [`WorldAnchor::altitude_range`]). Instances within one sample of a map's edge read a one-sided
/// normal there, so they may differ from the same spot on a larger map.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WorldAnchor {
    /// World XZ of the corner of cell `(0, 0)`, metres. Cells extend both ways from it.
    pub origin: [f32; 2],
    /// The `[lowest, highest]` altitude the layer mask treats as the map's range. `None` reads
    /// [`TerrainMap::min_max_height`] of the map placed on (which a backdrop ring may change).
    pub altitude_range: Option<[f32; 2]>,
}

impl WorldAnchor {
    /// An anchor at world `origin` with the map's own altitude range.
    #[must_use]
    pub fn at(origin: [f32; 2]) -> Self {
        Self {
            origin,
            altitude_range: None,
        }
    }

    /// An anchor at world `origin` whose layer altitude range is pinned to `map`'s (the inner,
    /// playable map), so a backdrop ring of another altitude does not change layer weights.
    #[must_use]
    pub fn pinned_to(origin: [f32; 2], map: &TerrainMap) -> Self {
        let (lo, hi) = map.min_max_height();
        Self {
            origin,
            altitude_range: Some([lo, hi]),
        }
    }

    /// The anchor's sample offset in `map` per axis when it sits on a sample (within 1/1024 of a
    /// spacing), else `None`. Placement reads the ground relative to that sample, so two aligned
    /// maps with the same spacing give the same bits; an unaligned anchor reads the map the
    /// edge-anchored way (still deterministic, but not bit-stable across map extents).
    #[must_use]
    pub fn lattice_aligned(&self, map: &TerrainMap) -> Option<[i64; 2]> {
        self.lattice_aligned_on(map)
    }

    /// [`Self::lattice_aligned`] over any [`PlacementHeights`] (a tile's height window answers as
    /// its map).
    fn lattice_aligned_on<H: PlacementHeights + ?Sized>(&self, map: &H) -> Option<[i64; 2]> {
        // Sample (0, 0) sits at the map's −X/−Z corner, shifted by its grid origin (ADR-8179);
        // `-half` for a map at the world origin, where `origin - (-half)` is `origin + half` exactly.
        let corner = map.world_min();
        let [spacing_x, spacing_z] = map.sample_spacings();
        let axis = |origin: f32, corner: f32, spacing: f32| {
            let k = f64::from(origin - corner) / f64::from(spacing);
            let rounded = k.round();
            ((k - rounded).abs() <= 1.0 / 1024.0 && rounded.abs() < 1e15).then_some(rounded as i64)
        };
        Some([
            axis(self.origin[0], corner[0], spacing_x)?,
            axis(self.origin[1], corner[1], spacing_z)?,
        ])
    }
}

/// The cells of one class over one map under a [`PlacementOrigin`]: cell `(cx, cz)` covers
/// `corner + (cx, cz) · cell_size` to one cell further. Streaming hosts walk `first..end`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlacementGrid {
    /// World XZ of the corner of cell `(0, 0)`, metres.
    pub corner: [f32; 2],
    /// Cell edge, metres (the class `cell_size`).
    pub cell_size: f32,
    /// First cell index per axis that touches the map.
    pub first: [i32; 2],
    /// One past the last cell index per axis that touches the map.
    pub end: [i32; 2],
}

impl PlacementGrid {
    /// The grid of a class with `cell_size` over `map`.
    #[must_use]
    pub fn new(map: &TerrainMap, cell_size: f32, origin: &PlacementOrigin) -> Self {
        let width = map.world_width();
        let half = width * 0.5;
        match origin {
            PlacementOrigin::MapEdge => {
                let cells = (width / cell_size).ceil() as i32;
                Self {
                    corner: [-half, -half],
                    cell_size,
                    first: [0, 0],
                    end: [cells, cells],
                }
            }
            PlacementOrigin::World(anchor) => {
                let range = |o: f32| {
                    let first = ((-half - o) / cell_size).floor() as i32;
                    let end = ((half - o) / cell_size).ceil() as i32;
                    (first, end.max(first))
                };
                let (fx, ex) = range(anchor.origin[0]);
                let (fz, ez) = range(anchor.origin[1]);
                Self {
                    corner: anchor.origin,
                    cell_size,
                    first: [fx, fz],
                    end: [ex, ez],
                }
            }
        }
    }

    /// Number of cells the grid walks.
    #[must_use]
    pub fn cell_count(&self) -> u64 {
        let span = |a: i32, b: i32| u64::from((b - a).max(0) as u32);
        span(self.first[0], self.end[0]) * span(self.first[1], self.end[1])
    }
}

/// How placement reads the ground: through the map's extent (edge-anchored) or relative to the
/// anchor sample (world-anchored and aligned).
#[derive(Clone, Copy)]
pub(crate) enum Ground {
    Map,
    Lattice {
        origin: [f32; 2],
        sample: [i64; 2],
        spacing: [f32; 2],
    },
}

impl Ground {
    /// The reader placement under `origin` uses on `map` (a [`HeightWindow`] of a map answers as
    /// the map).
    pub(crate) fn new<H: PlacementHeights + ?Sized>(map: &H, origin: &PlacementOrigin) -> Self {
        match origin {
            PlacementOrigin::World(anchor) => match anchor.lattice_aligned_on(map) {
                Some(sample) => Self::Lattice {
                    origin: anchor.origin,
                    sample,
                    spacing: map.sample_spacings(),
                },
                None => Self::Map,
            },
            PlacementOrigin::MapEdge => Self::Map,
        }
    }

    /// Height at world `(x, z)`.
    pub(crate) fn height<H: PlacementHeights + ?Sized>(&self, map: &H, x: f32, z: f32) -> f32 {
        match *self {
            Self::Map => map.sample_bilinear(x, z),
            Self::Lattice {
                origin,
                sample,
                spacing,
            } => {
                let (lu, lv) = Self::local(origin, spacing, x, z);
                Self::at(map, sample, lu, lv)
            }
        }
    }

    /// Unit normal at world `(x, z)` (central differences, as [`TerrainMap::normal_at`]).
    fn normal<H: PlacementHeights + ?Sized>(&self, map: &H, x: f32, z: f32) -> [f32; 3] {
        match *self {
            Self::Map => map.normal_at(x, z),
            Self::Lattice {
                origin,
                sample,
                spacing,
            } => {
                let (lu, lv) = Self::local(origin, spacing, x, z);
                let h = [
                    Self::at(map, sample, lu - 1.0, lv),
                    Self::at(map, sample, lu + 1.0, lv),
                    Self::at(map, sample, lu, lv - 1.0),
                    Self::at(map, sample, lu, lv + 1.0),
                ];
                crate::sampler::normal_from_differences(h, spacing[0], spacing[1])
            }
        }
    }

    /// Lattice coordinates relative to the anchor sample: they depend only on the anchor, the
    /// point and the spacing, never on the map's extent.
    fn local(origin: [f32; 2], spacing: [f32; 2], x: f32, z: f32) -> (f32, f32) {
        ((x - origin[0]) / spacing[0], (z - origin[1]) / spacing[1])
    }

    fn at<H: PlacementHeights + ?Sized>(map: &H, sample: [i64; 2], u: f32, v: f32) -> f32 {
        let (iu, iv) = (u.floor(), v.floor());
        map.sample_bilinear_lattice(iu as i64 + sample[0], u - iu, iv as i64 + sample[1], v - iv)
    }
}

/// FNV-1a of a class name (stable across builds and platforms).
pub(super) fn name_hash(name: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Places every class of `set`. Instances are grouped by class, row-major by cell inside a class.
///
/// # Errors
///
/// [`ScatterError::Asset`] when a class names a density map that `inputs` does not supply.
pub fn place_set(set: &ScatterSet, inputs: PlacementInputs<'_>) -> Result<Vec<PlacedInstance>, ScatterError> {
    place_set_with_origin(set, inputs, &PlacementOrigin::MapEdge)
}

/// [`place_set`] on a grid anchored at `origin` ([`PlacementOrigin`]). `MapEdge` gives exactly
/// [`place_set`]'s instances.
///
/// # Errors
///
/// As [`place_set`].
pub fn place_set_with_origin(
    set: &ScatterSet,
    inputs: PlacementInputs<'_>,
    origin: &PlacementOrigin,
) -> Result<Vec<PlacedInstance>, ScatterError> {
    let mut out = Vec::new();
    for (index, class) in set.classes.iter().enumerate() {
        let density = resolve_density_map(class, inputs.density_maps)?;
        let grid = PlacementGrid::new(inputs.map, class.cell_size, origin);
        for cz in grid.first[1]..grid.end[1] {
            for cx in grid.first[0]..grid.end[0] {
                place_class_cell_with_origin(
                    set.seed, index, class, density, inputs, origin, cx, cz, &mut out,
                );
            }
        }
    }
    Ok(out)
}

fn resolve_density_map<'a>(
    class: &ScatterClass,
    maps: Option<&'a DensityMaps>,
) -> Result<Option<&'a DensityMap>, ScatterError> {
    match &class.density_map {
        None => Ok(None),
        Some(path) => maps
            .and_then(|m| m.get(path))
            .map(Some)
            .ok_or_else(|| ScatterError::Asset {
                path: path.clone(),
                reason: format!("density map of class '{}' was not supplied", class.name),
            }),
    }
}

/// Places one cell of one class, appending to `out`. Public so streaming code can place cells in
/// any order with identical results.
#[allow(clippy::too_many_arguments)] // each argument is an independent placement input
pub fn place_class_cell(
    seed: u64,
    class_index: usize,
    class: &ScatterClass,
    density_map: Option<&DensityMap>,
    inputs: PlacementInputs<'_>,
    cx: i32,
    cz: i32,
    out: &mut Vec<PlacedInstance>,
) {
    place_class_cell_with_origin(
        seed,
        class_index,
        class,
        density_map,
        inputs,
        &PlacementOrigin::MapEdge,
        cx,
        cz,
        out,
    );
}

/// [`place_class_cell`] on a grid anchored at `origin`: `(cx, cz)` index the cells of
/// [`PlacementGrid::new`] (negative indices are valid under [`PlacementOrigin::World`]).
/// Instances outside the map are dropped. `MapEdge` gives exactly [`place_class_cell`]'s
/// instances.
#[allow(clippy::too_many_arguments)] // each argument is an independent placement input
pub fn place_class_cell_with_origin(
    seed: u64,
    class_index: usize,
    class: &ScatterClass,
    density_map: Option<&DensityMap>,
    inputs: PlacementInputs<'_>,
    origin: &PlacementOrigin,
    cx: i32,
    cz: i32,
    out: &mut Vec<PlacedInstance>,
) {
    place_class_cell_on(
        inputs.map,
        seed,
        class_index,
        class,
        density_map,
        inputs,
        origin,
        cx,
        cz,
        out,
    );
}

/// What placement reads of the heights: the whole map, or a [`HeightWindow`] a streaming tile
/// copied for itself (ADR-7638). Both answer bit for bit alike inside the window, the
/// world-anchored lattice reads (Q-08) included.
pub(crate) trait PlacementHeights {
    /// World extent, metres.
    fn world_width(&self) -> f32;
    /// Sample spacing per axis, metres ([`TerrainMap::sample_spacing`],
    /// [`TerrainMap::sample_spacing_z`]).
    fn sample_spacings(&self) -> [f32; 2];
    /// World XZ of sample `(0, 0)` ([`TerrainMap::world_min`]).
    fn world_min(&self) -> [f32; 2];
    /// [`TerrainMap::sample_bilinear`].
    fn sample_bilinear(&self, x: f32, z: f32) -> f32;
    /// [`TerrainMap::sample_bilinear_lattice`].
    fn sample_bilinear_lattice(&self, col: i64, fu: f32, row: i64, fv: f32) -> f32;
    /// [`TerrainMap::normal_at`].
    fn normal_at(&self, x: f32, z: f32) -> [f32; 3];
    /// [`TerrainMap::min_max_height`].
    fn min_max_height(&self) -> (f32, f32);
}

impl PlacementHeights for TerrainMap {
    fn world_width(&self) -> f32 {
        TerrainMap::world_width(self)
    }
    fn sample_spacings(&self) -> [f32; 2] {
        [self.sample_spacing(), self.sample_spacing_z()]
    }
    fn world_min(&self) -> [f32; 2] {
        TerrainMap::world_min(self)
    }
    fn sample_bilinear(&self, x: f32, z: f32) -> f32 {
        TerrainMap::sample_bilinear(self, x, z)
    }
    fn sample_bilinear_lattice(&self, col: i64, fu: f32, row: i64, fv: f32) -> f32 {
        TerrainMap::sample_bilinear_lattice(self, col, fu, row, fv)
    }
    fn normal_at(&self, x: f32, z: f32) -> [f32; 3] {
        TerrainMap::normal_at(self, x, z)
    }
    fn min_max_height(&self) -> (f32, f32) {
        TerrainMap::min_max_height(self)
    }
}

impl PlacementHeights for HeightWindow {
    fn world_width(&self) -> f32 {
        HeightWindow::world_width(self)
    }
    fn sample_spacings(&self) -> [f32; 2] {
        HeightWindow::sample_spacings(self)
    }
    fn world_min(&self) -> [f32; 2] {
        HeightWindow::world_min(self)
    }
    fn sample_bilinear(&self, x: f32, z: f32) -> f32 {
        HeightWindow::sample_bilinear(self, x, z)
    }
    fn sample_bilinear_lattice(&self, col: i64, fu: f32, row: i64, fv: f32) -> f32 {
        HeightWindow::sample_bilinear_lattice(self, col, fu, row, fv)
    }
    fn normal_at(&self, x: f32, z: f32) -> [f32; 3] {
        HeightWindow::normal_at(self, x, z)
    }
    fn min_max_height(&self) -> (f32, f32) {
        HeightWindow::min_max_height(self)
    }
}

/// [`place_class_cell_with_origin`] reading the heights from `map` (`inputs.map` is not read).
#[allow(clippy::too_many_arguments)] // each argument is an independent placement input
pub(crate) fn place_class_cell_on<H: PlacementHeights + ?Sized>(
    map: &H,
    seed: u64,
    class_index: usize,
    class: &ScatterClass,
    density_map: Option<&DensityMap>,
    inputs: PlacementInputs<'_>,
    origin: &PlacementOrigin,
    cx: i32,
    cz: i32,
    out: &mut Vec<PlacedInstance>,
) {
    let width = map.world_width();
    let half = width * 0.5;
    let cell = class.cell_size;
    let (corner, world) = match origin {
        PlacementOrigin::MapEdge => ([-half, -half], false),
        PlacementOrigin::World(anchor) => (anchor.origin, true),
    };
    let base_x = corner[0] + cx as f32 * cell;
    let base_z = corner[1] + cz as f32 * cell;
    if base_x >= half || base_z >= half || class.meshes.is_empty() {
        return;
    }
    // Under a world anchor a cell may start before the map's −X/−Z edge.
    if world && (base_x + cell <= -half || base_z + cell <= -half) {
        return;
    }
    let ground = Ground::new(map, origin);
    let altitude_range = || match origin {
        PlacementOrigin::World(WorldAnchor {
            altitude_range: Some(range),
            ..
        }) => *range,
        _ => {
            let (lo, hi) = map.min_max_height();
            [lo, hi]
        }
    };
    let mut rng = Rng::with_stream(
        cell_seed(seed ^ name_hash(&class.name), cx, cz),
        0x5ca7_7e52_0002_0001,
    );
    let full = class.density.floor();
    let count = full as u32 + u32::from(rng.chance(class.density - full));
    let total_weight: f32 = class.meshes.iter().map(|m| m.weight).sum();
    let mut u = [0.0f32; DRAWS_PER_CANDIDATE];
    for _ in 0..count {
        for value in &mut u {
            *value = rng.f32();
        }
        let x = base_x + u[0] * cell;
        let z = base_z + u[1] * cell;
        if x >= half || z >= half || (world && (x < -half || z < -half)) {
            continue;
        }
        // Thinning first: density map × procedural noise is the keep probability.
        let mut keep = 1.0;
        if let Some(density) = density_map {
            keep *= density.sample(x, z, width);
        }
        if class.noise_amount > 0.0 && class.noise_scale > 0.0 {
            let n = crate::noise::fbm2(
                x / class.noise_scale,
                z / class.noise_scale,
                (name_hash(&class.name) ^ seed) as u32,
            );
            // fbm2 is ~0..1; stretch the contrast so patches are bare or dense, not uniform.
            let patch = ((n - 0.5) * 2.5 + 0.5).clamp(0.0, 1.0);
            keep *= 1.0 - class.noise_amount + class.noise_amount * patch;
        }
        if u[8] >= keep {
            continue;
        }
        if class.avoid_holes
            && let Some(holes) = inputs.holes
            && holes.contains(x, z)
        {
            continue;
        }
        if class.exclusions.iter().any(|zone| zone.contains(x, z)) {
            continue;
        }
        let y = ground.height(map, x, z);
        if y < class.height_min || y > class.height_max {
            continue;
        }
        let normal = ground.normal(map, x, z);
        // slope ∈ [min, max] ⇔ cos(max) ≤ n.y ≤ cos(min); the cosines come from a fixed series.
        if normal[1] > cos_approx(class.slope_min) || normal[1] < cos_approx(class.slope_max) {
            continue;
        }
        if class.layer_mask != 0
            && let Some(layers) = inputs.layers
        {
            let weights = layers.weights_in(
                // det_acos: the platform `acos` may differ in the last bit between targets, and
                // the layer weight decides whether the instance exists (spec 26 § 1, ADR-5629).
                det_acos(normal[1].clamp(-1.0, 1.0)),
                y,
                Some(altitude_range()),
            );
            let sum: f32 = weights
                .iter()
                .enumerate()
                .filter(|(i, _)| *i < 8 && class.layer_mask & (1u8 << i) != 0)
                .map(|(_, w)| *w)
                .sum();
            if sum < class.layer_min_weight {
                continue;
            }
        }
        // Weighted mesh pick.
        let mut pick = u[2] * total_weight;
        let mut mesh = class.meshes.len() - 1;
        for (i, m) in class.meshes.iter().enumerate() {
            if pick < m.weight {
                mesh = i;
                break;
            }
            pick -= m.weight;
        }
        let m = &class.meshes[mesh];
        // det_powf, not `powf`: the platform `powf` gave different scale bits on x86_64 Linux and
        // aarch64 macOS (`placement_bits_are_pinned_for_every_platform`, ADR-5629).
        let scale = m.min_scale + (m.max_scale - m.min_scale) * det_powf(u[3], m.scale_bias);
        out.push(PlacedInstance {
            class: class_index as u16,
            mesh: mesh as u16,
            position: [x, y, z],
            up: tilted_up(normal, class, [u[5], u[6]]),
            yaw: u[4] * class.yaw_jitter,
            scale,
            shade: 1.0 + (u[7] * 2.0 - 1.0) * class.tint_jitter,
            normal,
        });
    }
}

/// Up axis: blend +Y toward the ground normal, cap the tilt, then add the random jitter. Square
/// roots only, so the result is reproducible across platforms.
pub(super) fn tilted_up(normal: [f32; 3], class: &ScatterClass, jitter: [f32; 2]) -> [f32; 3] {
    let a = class.align_to_normal;
    let mut up = [normal[0] * a, 1.0 - a + normal[1] * a, normal[2] * a];
    normalise(&mut up);
    // Cap: tan(tilt) = horizontal / vertical.
    let horizontal = (up[0] * up[0] + up[2] * up[2]).sqrt();
    let cap = tan_approx(class.tilt_max.min(1.5));
    if up[1] > 0.0 && horizontal > cap * up[1] && horizontal > 0.0 {
        let k = cap * up[1] / horizontal;
        up[0] *= k;
        up[2] *= k;
        normalise(&mut up);
    }
    // Jitter: a horizontal nudge of up to `tilt_jitter` (small-angle: tan ≈ angle).
    up[0] += (jitter[0] * 2.0 - 1.0) * class.tilt_jitter * up[1];
    up[2] += (jitter[1] * 2.0 - 1.0) * class.tilt_jitter * up[1];
    normalise(&mut up);
    up
}

fn normalise(v: &mut [f32; 3]) {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len > 1e-12 {
        v[0] /= len;
        v[1] /= len;
        v[2] /= len;
    } else {
        *v = [0.0, 1.0, 0.0];
    }
}

/// `sin`/`cos` by short Taylor series on [0, 1.6]; relative error < 2e-4 there. Used instead of
/// libm so filters and the tilt cap are identical on every platform (C runtimes may differ in
/// the last bit of `cos`/`tan`).
fn sin_cos_approx(angle: f32) -> (f32, f32) {
    let x = angle.clamp(0.0, 1.6);
    let x2 = x * x;
    let sin = x * (1.0 - x2 / 6.0 * (1.0 - x2 / 20.0 * (1.0 - x2 / 42.0 * (1.0 - x2 / 72.0))));
    let cos = 1.0 - x2 / 2.0 * (1.0 - x2 / 12.0 * (1.0 - x2 / 30.0 * (1.0 - x2 / 56.0 * (1.0 - x2 / 90.0))));
    (sin, cos)
}

/// Reproducible `cos` for angles in [0, π/2] (larger angles clamp to 1.6 rad).
pub(super) fn cos_approx(angle: f32) -> f32 {
    sin_cos_approx(angle).1
}

/// Reproducible `tan` for angles in [0, 1.5].
fn tan_approx(angle: f32) -> f32 {
    let (sin, cos) = sin_cos_approx(angle.min(1.5));
    sin / cos.max(1e-4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scatter::set::{ScatterExclusion, ScatterMeshRef};

    fn hills() -> TerrainMap {
        let n = 129u32;
        let width = 256.0f32;
        let mut heights = Vec::new();
        for row in 0..n {
            for col in 0..n {
                let x = col as f32 / (n - 1) as f32 * width - width * 0.5;
                let z = row as f32 / (n - 1) as f32 * width - width * 0.5;
                heights.push(
                    20.0 + 6.0 * (x * 0.04).sin()
                        + 4.0 * (z * 0.05).cos()
                        + if x > 80.0 { (x - 80.0) * 1.5 } else { 0.0 },
                );
            }
        }
        TerrainMap::from_heights(n, n, heights, width, 1.0, 0.0).expect("map")
    }

    fn set() -> ScatterSet {
        ScatterSet {
            seed: 11,
            classes: vec![
                ScatterClass {
                    name: "boulders".into(),
                    meshes: vec![
                        ScatterMeshRef {
                            mesh: "a.glb".into(),
                            weight: 3.0,
                            min_scale: 0.4,
                            max_scale: 2.0,
                            scale_bias: 2.5,
                        },
                        ScatterMeshRef {
                            mesh: "b.glb".into(),
                            weight: 1.0,
                            min_scale: 1.0,
                            max_scale: 1.0,
                            scale_bias: 1.0,
                        },
                    ],
                    cell_size: 4.0,
                    density: 2.5,
                    slope_max: 0.8,
                    ..ScatterClass::default()
                },
                ScatterClass {
                    name: "gravel".into(),
                    meshes: vec![ScatterMeshRef {
                        mesh: "a.glb".into(),
                        weight: 1.0,
                        min_scale: 0.1,
                        max_scale: 0.3,
                        scale_bias: 1.0,
                    }],
                    cell_size: 2.0,
                    density: 1.0,
                    noise_scale: 30.0,
                    noise_amount: 0.8,
                    ..ScatterClass::default()
                },
            ],
            ..ScatterSet::default()
        }
    }

    fn inputs(map: &TerrainMap) -> PlacementInputs<'_> {
        PlacementInputs {
            map,
            holes: None,
            layers: None,
            density_maps: None,
        }
    }

    #[test]
    fn same_inputs_same_placement() {
        let map = hills();
        let a = place_set(&set(), inputs(&map)).expect("place");
        let b = place_set(&set(), inputs(&map)).expect("place");
        assert!(a.len() > 1000, "{}", a.len());
        assert_eq!(a, b);
    }

    #[test]
    fn reordering_classes_does_not_move_instances() {
        let map = hills();
        let forward = place_set(&set(), inputs(&map)).expect("place");
        let mut swapped = set();
        swapped.classes.swap(0, 1);
        let backward = place_set(&swapped, inputs(&map)).expect("place");
        let key = |i: &PlacedInstance| {
            (
                i.position[0].to_bits(),
                i.position[2].to_bits(),
                i.scale.to_bits(),
            )
        };
        let mut f: Vec<_> = forward.iter().map(key).collect();
        let mut b: Vec<_> = backward.iter().map(key).collect();
        f.sort_unstable();
        b.sort_unstable();
        assert_eq!(f, b);
    }

    #[test]
    fn instances_sit_on_the_bilinear_surface_and_respect_slope() {
        let map = hills();
        let placed = place_set(&set(), inputs(&map)).expect("place");
        for p in &placed {
            assert!((p.position[1] - map.sample_bilinear(p.position[0], p.position[2])).abs() < 1e-4);
            assert!(p.normal[1] >= 0.9f32.cos() - 1e-3);
            let len = (p.up[0] * p.up[0] + p.up[1] * p.up[1] + p.up[2] * p.up[2]).sqrt();
            assert!((len - 1.0).abs() < 1e-4 && p.up[1] > 0.5);
        }
        // The cliff beyond x = 80 m (slope ≈ 0.98 rad) is empty for the boulders; the normal is a
        // central difference over two 2 m samples, so the edge softens by one sample.
        // (`normal_at` is one-sided at the map border, so the last sample is excluded.)
        let max_x = placed
            .iter()
            .filter(|p| p.class == 0 && p.position[0] < 126.0)
            .map(|p| p.position[0])
            .fold(f32::MIN, f32::max);
        assert!(max_x < 82.5, "boulder at x = {max_x}");
    }

    #[test]
    fn small_scales_dominate_with_a_bias() {
        let map = hills();
        let placed = place_set(&set(), inputs(&map)).expect("place");
        let boulders: Vec<_> = placed.iter().filter(|p| p.class == 0 && p.mesh == 0).collect();
        let small = boulders.iter().filter(|p| p.scale < 1.2).count();
        assert!(small * 10 > boulders.len() * 7, "{small} of {}", boulders.len());
        // Mesh weights 3:1.
        let b = placed.iter().filter(|p| p.class == 0 && p.mesh == 1).count();
        let ratio = boulders.len() as f32 / b.max(1) as f32;
        assert!((2.4..3.8).contains(&ratio), "{ratio}");
    }

    #[test]
    fn density_map_thins_only_where_it_is_dark() {
        let map = hills();
        // Left half white, right half black.
        let mut values = Vec::new();
        for _row in 0..16 {
            for col in 0..16 {
                values.push(if col < 8 { 1.0 } else { 0.0 });
            }
        }
        let mut maps = DensityMaps::new();
        maps.insert("mask.png".into(), DensityMap::new(16, 16, values).expect("map"));
        let mut masked = set();
        masked.classes[0].density_map = Some("mask.png".into());
        let full = place_set(&set(), inputs(&map)).expect("place");
        let thinned = place_set(
            &masked,
            PlacementInputs {
                density_maps: Some(&maps),
                ..inputs(&map)
            },
        )
        .expect("place");
        let boulders = |v: &[PlacedInstance]| v.iter().filter(|p| p.class == 0).copied().collect::<Vec<_>>();
        let (full, thinned) = (boulders(&full), boulders(&thinned));
        // Columns 7 and 8 sit at x = ∓8.53 m; the bilinear ramp between them is the only grey.
        assert!(thinned.iter().all(|p| p.position[0] < 8.6));
        // Every survivor is an instance of the unmasked placement (stable streams).
        assert!(thinned.iter().all(|p| full.contains(p)));
        let left = full.iter().filter(|p| p.position[0] < -10.0).count();
        let kept = thinned.iter().filter(|p| p.position[0] < -10.0).count();
        assert_eq!(left, kept);
        // Missing map is an error, not a silent empty class.
        assert!(place_set(&masked, inputs(&map)).is_err());
    }

    #[test]
    fn exclusions_and_noise_apply() {
        let map = hills();
        let mut road = set();
        let zone = ScatterExclusion {
            points: vec![[-100.0, 0.0], [100.0, 0.0]],
            radius: 6.0,
        };
        road.classes[0].exclusions.push(zone.clone());
        let placed = place_set(&road, inputs(&map)).expect("place");
        assert!(
            placed
                .iter()
                .filter(|p| p.class == 0)
                .all(|p| !zone.contains(p.position[0], p.position[2]))
        );
        // Noise leaves bare patches: gravel density varies by > 3× between 32 m blocks.
        let mut blocks = std::collections::HashMap::<(i32, i32), u32>::new();
        for p in placed.iter().filter(|p| p.class == 1) {
            *blocks
                .entry((
                    (p.position[0] / 32.0).floor() as i32,
                    (p.position[2] / 32.0).floor() as i32,
                ))
                .or_default() += 1;
        }
        let max = blocks.values().copied().max().unwrap_or(0);
        let min = blocks.values().copied().min().unwrap_or(0);
        assert!(max > min * 3 + 10, "min {min} max {max}");
    }

    #[test]
    fn tilt_follows_slope_up_to_the_cap() {
        let class = ScatterClass {
            align_to_normal: 1.0,
            tilt_max: 0.3,
            tilt_jitter: 0.0,
            ..ScatterClass::default()
        };
        let n = [0.6f32, 0.8, 0.0]; // 0.64 rad slope
        let up = tilted_up(n, &class, [0.5, 0.5]);
        let tilt = up[1].acos();
        assert!((tilt - 0.3).abs() < 0.01, "{tilt}");
        let gentle = tilted_up([0.1, 0.995, 0.0], &class, [0.5, 0.5]);
        assert!((gentle[0] - 0.1).abs() < 0.01);
        assert!((tan_approx(0.3) - 0.3f32.tan()).abs() < 1e-4);
        assert!((tan_approx(1.2) - 1.2f32.tan()).abs() < 2e-3 * 1.2f32.tan());
        assert!((cos_approx(1.5) - 1.5f32.cos()).abs() < 1e-3);
    }

    /// A heightfield built from integer arithmetic only (no `sin`/`cos`), so the input bits are
    /// the same on every platform: rolling steps plus a cliff past column 96.
    fn exact_hills() -> TerrainMap {
        let n = 129u32;
        let mut heights = Vec::new();
        for row in 0..n {
            for col in 0..n {
                let ripple = ((col * 7 + row * 3) % 23) as f32 * 0.25;
                let cliff = col.saturating_sub(96) as f32 * 1.5;
                heights.push(20.0 + ripple + cliff + row as f32 * 0.0625);
            }
        }
        TerrainMap::from_heights(n, n, heights, 256.0, 1.0, 0.0).expect("map")
    }

    /// FNV-1a over every field of every instance, floats by their bits.
    fn placement_digest(placed: &[PlacedInstance]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |word: u32| {
            for b in word.to_le_bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        for p in placed {
            eat((u32::from(p.class) << 16) | u32::from(p.mesh));
            for v in p.position.iter().chain(&p.up).chain(&p.normal) {
                eat(v.to_bits());
            }
            for v in [p.yaw, p.scale, p.shade] {
                eat(v.to_bits());
            }
        }
        h
    }

    /// Spec 26 § 1 "same seed ⇒ same placement on every client": the placement of a fixed set on
    /// a fixed map is pinned bit for bit, so a run on another OS or CPU (the gate on x86_64 Linux,
    /// the Mac on aarch64, Windows) checks that every client places the same instances. The set
    /// exercises the noise thinning, an exclusion path, the slope and height filters, the tilt
    /// cap and jitter, the yaw and tint jitter and a weighted mesh pick, once with `scale_bias` 1
    /// and once with 2.5. Before ADR-5629 the biased run went through the platform `powf` and its
    /// digest differed between x86_64 Linux (`0x12f919df95aa577f`) and aarch64 macOS
    /// (`0x0ece3486cc6264ed`).
    #[test]
    fn placement_bits_are_pinned_for_every_platform() {
        let map = exact_hills();
        let mut set = set();
        set.seed = 0x5eed_2026;
        set.classes[0].meshes[0].scale_bias = 1.0;
        set.classes[0].align_to_normal = 0.6;
        set.classes[0].tilt_max = 0.3;
        set.classes[0].tilt_jitter = 0.1;
        set.classes[0].yaw_jitter = 6.0;
        set.classes[0].tint_jitter = 0.2;
        set.classes[0].height_max = 60.0;
        set.classes[1].exclusions = vec![ScatterExclusion {
            points: vec![[-100.0, -100.0], [40.0, 30.0]],
            radius: 6.0,
        }];
        let placed = place_set(&set, inputs(&map)).expect("place");
        let digest = placement_digest(&placed);
        let mut biased = set.clone();
        biased.classes[0].meshes[0].scale_bias = 2.5;
        let biased_digest = placement_digest(&place_set(&biased, inputs(&map)).expect("place"));
        eprintln!(
            "scatter placement: {} instances, digest {digest:#018x}, scale_bias 2.5 digest {biased_digest:#018x}",
            placed.len()
        );
        assert_eq!(
            (placed.len(), digest, biased_digest),
            (PINNED_COUNT, PINNED_DIGEST, PINNED_BIASED_DIGEST),
            "scatter placement changed on this platform or build"
        );
    }

    /// `exact_hills` grown by a 64 m backdrop ring on every side: the same 2 m sample spacing,
    /// the inner samples unchanged, the ring a lower apron (integer arithmetic only).
    fn exact_hills_with_backdrop() -> TerrainMap {
        let inner = exact_hills();
        let n = 193u32;
        let mut heights = Vec::new();
        for row in 0..n {
            for col in 0..n {
                let (ic, ir) = (i64::from(col) - 32, i64::from(row) - 32);
                if (0..129).contains(&ic) && (0..129).contains(&ir) {
                    heights.push(inner.height_at_sample(ic, ir));
                } else {
                    heights.push(4.0 + ((col * 5 + row * 11) % 17) as f32 * 0.5);
                }
            }
        }
        TerrainMap::from_heights(n, n, heights, 384.0, 1.0, 0.0).expect("map")
    }

    /// Bits of everything a drawn rock shows.
    fn rock_bits(p: &PlacedInstance) -> [u32; 11] {
        [
            (u32::from(p.class) << 16) | u32::from(p.mesh),
            p.position[0].to_bits(),
            p.position[1].to_bits(),
            p.position[2].to_bits(),
            p.up[0].to_bits(),
            p.up[1].to_bits(),
            p.up[2].to_bits(),
            p.yaw.to_bits(),
            p.scale.to_bits(),
            p.shade.to_bits(),
            p.normal[1].to_bits(),
        ]
    }

    /// The set the backdrop tests place: boulders with tilt and yaw jitter, and gravel on a
    /// cell size that does not divide the 64 m ring.
    fn backdrop_set() -> ScatterSet {
        let mut set = set();
        set.seed = 0x0a27_0008;
        set.classes[0].align_to_normal = 0.5;
        set.classes[0].tilt_jitter = 0.1;
        set.classes[0].yaw_jitter = 6.0;
        set.classes[0].height_max = 60.0;
        set.classes[1].cell_size = 3.0;
        set
    }

    /// Interior instances of `placed` (one sample in from the inner map's edge), sorted bits.
    fn interior_rocks(placed: &[PlacedInstance]) -> Vec<[u32; 11]> {
        let mut rocks: Vec<_> = placed
            .iter()
            .filter(|p| p.position[0].abs() < 126.0 && p.position[2].abs() < 126.0)
            .map(rock_bits)
            .collect();
        rocks.sort_unstable();
        rocks
    }

    /// `(moved, added)`: interior rocks of `a` missing from `b`, and of `b` missing from `a`.
    fn rock_diff(a: &[[u32; 11]], b: &[[u32; 11]]) -> (usize, usize) {
        (
            a.iter().filter(|r| b.binary_search(r).is_err()).count(),
            b.iter().filter(|r| a.binary_search(r).is_err()).count(),
        )
    }

    /// Q-08: a host that grows its map with a backdrop ring (or drops the ring on a low tier)
    /// must not move, add or remove one rock inside the original map. The one-sample strip at
    /// the inner edge is excluded: there the inner map's normal is one-sided by definition.
    #[test]
    fn backdrop_extension_moves_no_rock() {
        let inner = exact_hills();
        let extended = exact_hills_with_backdrop();
        let set = backdrop_set();
        let origin = PlacementOrigin::World(WorldAnchor::pinned_to([0.0, 0.0], &inner));
        assert_eq!(
            WorldAnchor::at([0.0, 0.0]).lattice_aligned(&inner),
            Some([64, 64])
        );
        assert_eq!(
            WorldAnchor::at([0.0, 0.0]).lattice_aligned(&extended),
            Some([96, 96])
        );
        let place = |map: &TerrainMap| place_set_with_origin(&set, inputs(map), &origin).expect("place");
        let (a, b) = (interior_rocks(&place(&inner)), interior_rocks(&place(&extended)));
        assert!(a.len() > 1000, "{} interior rocks", a.len());
        let (moved, added) = rock_diff(&a, &b);
        assert_eq!(
            (moved, added),
            (0, 0),
            "the backdrop moved {moved} of {} interior rocks and added {added}",
            a.len()
        );
        // Nothing is placed outside the map it was given.
        for p in place(&inner) {
            assert!(
                p.position[0] >= -128.0 && p.position[0] < 128.0,
                "{:?}",
                p.position
            );
            assert!(
                p.position[2] >= -128.0 && p.position[2] < 128.0,
                "{:?}",
                p.position
            );
        }
    }

    /// The default stays edge-anchored and bit-identical to `place_set`: the backdrop still
    /// renumbers its cells, which is why the world anchor is an opt-in.
    #[test]
    fn map_edge_origin_is_place_set_and_still_renumbers_cells() {
        let set = backdrop_set();
        let inner = exact_hills();
        let edge = place_set_with_origin(&set, inputs(&inner), &PlacementOrigin::MapEdge).expect("place");
        assert_eq!(edge, place_set(&set, inputs(&inner)).expect("place"));
        let extended = place_set(&set, inputs(&exact_hills_with_backdrop())).expect("place");
        let (a, b) = (interior_rocks(&edge), interior_rocks(&extended));
        let (moved, _) = rock_diff(&a, &b);
        assert!(
            moved * 2 > a.len(),
            "edge anchoring moved only {moved} of {}",
            a.len()
        );
    }

    /// A layer band that reaches the inner map's lowest ground keeps full weight there only if
    /// the altitude range is pinned: the backdrop's lower apron would otherwise move the band.
    #[test]
    fn pinned_altitude_range_keeps_layer_masked_rocks() {
        let inner = exact_hills();
        let extended = exact_hills_with_backdrop();
        let layers = TerrainLayers {
            layers: vec![
                // Full weight everywhere on both maps.
                crate::layers::LayerDesc {
                    name: "apron".into(),
                    slope_min: 0.0,
                    altitude_min: -10.0,
                    altitude_max: 200.0,
                    slope_max: 1.6,
                    blend_sharpness: 50.0,
                    ..crate::layers::LayerDesc::ground()
                },
                // Starts at the inner map's lowest ground (20 m), with a 50 m ramp: full weight
                // on the inner map, ramping in on the extended one (lowest ground 4 m).
                crate::layers::LayerDesc {
                    name: "upland".into(),
                    slope_min: 0.0,
                    altitude_min: 20.0,
                    altitude_max: 200.0,
                    slope_max: 1.6,
                    blend_sharpness: 50.0,
                    ..crate::layers::LayerDesc::ground()
                },
            ],
            ..TerrainLayers::default()
        };
        let mut set = backdrop_set();
        set.classes[0].layer_mask = 0b10;
        // Upland is exactly half of the normalised weight where both layers are full.
        set.classes[0].layer_min_weight = 0.45;
        let place = |map: &TerrainMap, anchor: WorldAnchor| {
            let with_layers = PlacementInputs {
                layers: Some(&layers),
                ..inputs(map)
            };
            let placed =
                place_set_with_origin(&set, with_layers, &PlacementOrigin::World(anchor)).expect("place");
            interior_rocks(&placed)
        };
        let pinned = WorldAnchor::pinned_to([0.0, 0.0], &inner);
        let a = place(&inner, pinned);
        assert_eq!(rock_diff(&a, &place(&extended, pinned)), (0, 0));
        // Without the pin the apron widens the map's range and the band edge thins the rocks.
        let loose = WorldAnchor::at([0.0, 0.0]);
        assert_eq!(place(&inner, loose), a);
        let (moved, _) = rock_diff(&a, &place(&extended, loose));
        assert!(
            moved > 0,
            "the unpinned range changed nothing; the test lost its point"
        );
    }

    /// The grid covers the map from any anchor, including negative cells and an anchor outside
    /// the map; an anchor between samples falls back to reading the map by its extent.
    /// ADR-8496: a world anchor's sample is counted from the map's sample `(0, 0)`, which a grid
    /// origin (ADR-8179) moves: the same world point names a sample of a moved map exactly when it
    /// names one of the map at the world origin, offset by the move in samples.
    #[test]
    fn lattice_alignment_follows_the_grid_origin() {
        let map = || TerrainMap::from_heights(129, 129, vec![0.0; 129 * 129], 256.0, 1.0, 0.0).expect("map");
        let at = WorldAnchor::at([6.0, -10.0]);
        assert_eq!(at.lattice_aligned(&map()), Some([67, 59]));
        // Moved by +3 samples (6 m) on both axes: the same world point is 3 samples nearer the corner.
        let moved = map().with_grid_origin([6.0, 6.0]).expect("origin");
        assert_eq!(at.lattice_aligned(&moved), Some([64, 56]));
        // Moved by half a sample: the point sits between samples.
        let half = map().with_grid_origin([1.0, 1.0]).expect("origin");
        assert_eq!(at.lattice_aligned(&half), None);
    }

    #[test]
    fn world_grid_covers_the_map_from_any_anchor() {
        let map = exact_hills();
        for origin in [[0.0, 0.0], [-128.0, -128.0], [37.0, -5.5], [1000.0, -3000.0]] {
            let grid = PlacementGrid::new(&map, 3.0, &PlacementOrigin::World(WorldAnchor::at(origin)));
            let lo = |a: usize| grid.corner[a] + grid.first[a] as f32 * grid.cell_size;
            let hi = |a: usize| grid.corner[a] + grid.end[a] as f32 * grid.cell_size;
            for a in 0..2 {
                assert!(
                    lo(a) <= -128.0 && lo(a) + 3.0 > -128.0,
                    "{origin:?} axis {a} starts at {}",
                    lo(a)
                );
                assert!(
                    hi(a) >= 128.0 && hi(a) - 3.0 < 128.0,
                    "{origin:?} axis {a} ends at {}",
                    hi(a)
                );
            }
            assert!(grid.cell_count() >= 86 * 86);
        }
        assert_eq!(WorldAnchor::at([37.0, -5.5]).lattice_aligned(&map), None);
        assert_eq!(
            WorldAnchor::at([-128.0, 2.0]).lattice_aligned(&map),
            Some([0, 65])
        );
        let edge = PlacementGrid::new(&map, 3.0, &PlacementOrigin::MapEdge);
        assert_eq!(
            (edge.corner, edge.first, edge.end),
            ([-128.0, -128.0], [0, 0], [86, 86])
        );
        // An unaligned anchor still places deterministically, inside the map.
        let set = backdrop_set();
        let origin = PlacementOrigin::World(WorldAnchor::at([37.0, -5.5]));
        let a = place_set_with_origin(&set, inputs(&map), &origin).expect("place");
        assert_eq!(
            a,
            place_set_with_origin(&set, inputs(&map), &origin).expect("place")
        );
        assert!(a.len() > 1000);
        assert!(
            a.iter()
                .all(|p| p.position[0].abs() <= 128.0 && p.position[2].abs() <= 128.0)
        );
    }

    /// Streaming a world-anchored set cell by cell in any order gives `place_set_with_origin`.
    #[test]
    fn world_cells_in_any_order_match_the_whole_set() {
        let map = exact_hills();
        let set = backdrop_set();
        let origin = PlacementOrigin::World(WorldAnchor::at([10.0, -20.0]));
        let whole = place_set_with_origin(&set, inputs(&map), &origin).expect("place");
        let mut cells = Vec::new();
        for (ci, class) in set.classes.iter().enumerate() {
            let grid = PlacementGrid::new(&map, class.cell_size, &origin);
            // Reverse order, one cell at a time.
            for cz in (grid.first[1]..grid.end[1]).rev() {
                for cx in (grid.first[0]..grid.end[0]).rev() {
                    let mut one = Vec::new();
                    place_class_cell_with_origin(
                        set.seed,
                        ci,
                        class,
                        None,
                        inputs(&map),
                        &origin,
                        cx,
                        cz,
                        &mut one,
                    );
                    cells.extend(one);
                }
            }
        }
        let key = |p: &PlacedInstance| rock_bits(p);
        let mut a: Vec<_> = whole.iter().map(key).collect();
        let mut b: Vec<_> = cells.iter().map(key).collect();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b);
    }

    /// Instances of the pinned placement; the count and `PINNED_DIGEST` were identical on x86_64
    /// Linux and aarch64 macOS (2026-11-10). See `placement_bits_are_pinned_for_every_platform`.
    const PINNED_COUNT: usize = 18635;
    /// The FNV-1a digest of that placement (`scale_bias` 1).
    const PINNED_DIGEST: u64 = 0x789c_29de_d512_1bb6;
    /// The digest with `scale_bias` 2.5 (through `det_powf`), pinned on x86_64 Linux.
    const PINNED_BIASED_DIGEST: u64 = 0xadc7_d950_977f_031e;
}
