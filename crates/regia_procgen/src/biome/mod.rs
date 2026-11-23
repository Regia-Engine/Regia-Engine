// SPDX-License-Identifier: MPL-2.0
//! Biomes and vegetation scatter (`docs/specs/65-biomes-and-vegetation-scatter.md`).
//!
//! A biome ([`def::BiomeDef`], `.biome.ron`) says what grows in a kind of place; biome areas
//! ([`area::BiomeAreaSnap`], captured from the `BiomeArea` component) say where. The `biome.*` graph
//! nodes turn the areas inside a generator's bounds into placed instances: claims → per-layer
//! candidates, channels, entry picks, placement, filters, self spacing and children → the
//! cross-layer resolve → exclusions → spawn. The run reads everything through [`BiomeWorld`], part
//! of the world snapshot, so it stays a pure function of its inputs.
//!
//! Layout: [`def`] the format, [`area`] claims, [`water`] the water distance field, [`pipeline`] the
//! stages of `biome.layer` and the resolve, [`grass`] the grass multipliers, [`ecs`] the `BiomeArea`
//! component, the host capture and the grass hand-off.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::data::ShapeData;

pub mod area;
pub mod def;
pub mod ecology;
pub mod ecs;
pub mod grass;
pub mod import;
pub mod pipeline;
pub mod water;

pub use area::{AreaOutline, BiomeAreaSnap, Claim, ClaimAt, KeyImage, KeyTables, claims_at};
pub use def::{
    BIOME_FORMAT_VERSION, BiomeDef, BiomeDefError, BiomeEntry, BiomeIssue, BiomeLayer, CandidateSource,
    ChildRule, EntryFilter, EntrySpawn,
};
pub use water::{WaterField, WaterLevel, WaterSnap};

/// The starter graph `starter/biomes.pgraph.ron` (spec 65 § 5): `in.self → biome.areas →
/// biome.layer (+ in.ground) → biome.resolve → biome.exclude → biome.spawn`, and `biome.areas →
/// biome.grass_mask`, which exports one mask per grass class as `grass_multiplier/<class>`.
pub const STARTER_BIOMES_GRAPH: &str = include_str!("../../starter/biomes.pgraph.ron");

/// The opt-in starter graph `starter/biomes_ecology.pgraph.ron` (ADR-2456): the starter graph with
/// `biome.ecology` between `biome.resolve` and `biome.exclude`, which applies the biomes' competition
/// rules (shade-needing understory only under crowns, light-demanding understory out of deep shade,
/// canopy self-thinning). `STARTER_BIOMES_GRAPH` is unchanged.
pub const STARTER_BIOMES_ECOLOGY_GRAPH: &str = include_str!("../../starter/biomes_ecology.pgraph.ron");

/// The tag that marks a level entity as an exclusion region (spec 65 § 5 `biome.exclude`).
pub const EXCLUDE_TAG: &str = "procgen_exclude";

/// A region `biome.exclude` removes points from.
#[derive(Clone, Debug, PartialEq)]
pub enum ExclusionRegion {
    /// A box or sphere (a tagged entity's shape).
    Shape(ShapeData),
    /// A closed XZ polygon (a tagged closed spline, a road outline).
    Polygon(Vec<[f32; 2]>),
    /// An open XZ polyline with a half width (a tagged open spline).
    Corridor {
        /// The polyline.
        points: Vec<[f32; 2]>,
        /// Half the corridor width, meters.
        half_width: f32,
    },
}

impl ExclusionRegion {
    /// Whether `p` lies inside.
    pub fn contains(&self, p: [f32; 3]) -> bool {
        crate::probe::hit_contains();
        match self {
            Self::Shape(s) => AreaOutline::Shape(*s).depth(p[0], p[2]) > 0.0,
            Self::Polygon(pts) => area::polygon_depth(pts, p[0], p[2]) > 0.0,
            Self::Corridor { points, half_width } => points.windows(2).any(|w| {
                let (a, b) = (w[0], w[1]);
                let (ex, ez) = (b[0] - a[0], b[1] - a[1]);
                let len2 = ex * ex + ez * ez;
                let t = if len2 > 0.0 {
                    (((p[0] - a[0]) * ex + (p[2] - a[1]) * ez) / len2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let (dx, dz) = (p[0] - (a[0] + t * ex), p[2] - (a[1] + t * ez));
                dx * dx + dz * dz <= half_width * half_width
            }),
        }
    }
}

/// What biome nodes read from the world (spec 65 § 4-5), captured before the run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BiomeWorld {
    /// Areas touching the generator bounds, in guid order.
    pub areas: Vec<BiomeAreaSnap>,
    /// Exclusion regions (tagged entities, roads).
    pub exclusions: Vec<ExclusionRegion>,
    /// Water for the `WaterDistance` filter.
    pub water: WaterSnap,
    /// Definitions by asset path (every biome the areas name).
    pub defs: BTreeMap<String, Arc<BiomeDef>>,
    /// Map texels whose key matched no candidate biome (statistics).
    pub unknown_map_texels: u64,
    /// `ScatterClass` sources: the placements of `(scatter set path, class)` over the run bounds.
    pub scatter: BTreeMap<(String, String), Arc<Vec<ScatterCandidate>>>,
    /// The terrain's layer stack for `TerrainLayer` rules and filters (procedural slope/altitude
    /// weights; painted weights are not captured yet).
    pub terrain_layers: Option<Arc<regia_terrain::TerrainLayers>>,
    /// The terrain's painted weights (`TerrainPaint`), blended over the procedural ones.
    pub painted_layers: Option<PaintedLayers>,
    /// Ecology profiles (`<name>.ecology.ron` beside `<name>.biome.ron`) by biome asset path, for the
    /// `biome.ecology` node (ADR-2456); biomes without one are absent.
    pub ecology: BTreeMap<String, Arc<ecology::EcologyProfile>>,
}

impl BiomeWorld {
    /// The procedural weight of the terrain layer `name` at a slope (radians) and altitude, when the
    /// layer stack was captured and has that layer.
    pub fn layer_weight(&self, name: &str, slope_radians: f32, altitude: f32) -> Option<f32> {
        self.layer_weight_at(name, slope_radians, altitude, None)
    }

    /// The weight of terrain layer `name`: procedural by slope and altitude, blended with the
    /// painted weights at world `xz` when the terrain has them, as the terrain's material does
    /// (`TerrainLayers::blend_painted`).
    pub fn layer_weight_at(
        &self,
        name: &str,
        slope_radians: f32,
        altitude: f32,
        xz: Option<[f32; 2]>,
    ) -> Option<f32> {
        let layers = self.terrain_layers.as_ref()?;
        let index = (0..layers.count()).find(|&i| layers.layer(i).is_some_and(|l| l.name == name))?;
        self.layer_weight_indexed(index, slope_radians, altitude, xz)
    }

    /// The weight of terrain layer `index` on a stack array.
    ///
    /// Same numbers as [`Self::layer_weight_at`] for a name that resolves to `index`.
    pub(crate) fn layer_weight_indexed(
        &self,
        index: usize,
        slope_radians: f32,
        altitude: f32,
        xz: Option<[f32; 2]>,
    ) -> Option<f32> {
        let layers = self.terrain_layers.as_ref()?;
        let count = layers.count().min(regia_terrain::MAX_LAYERS);
        (index < count).then(|| {
            stacked_layer_weight(
                layers,
                self.painted_layers.as_ref(),
                index,
                count,
                slope_radians,
                altitude,
                xz,
            )
        })
    }
}

/// Procedural weights blended with paint, without a heap vector.
///
/// This follows `TerrainLayers::weights` and `TerrainLayers::blend_painted` with no altitude
/// range: non-finite paint is 0, paint is clamped to 0..=1, coverage is `min(1, sum)`, the mix is
/// `(1 - coverage) * procedural + coverage * painted / sum`, and an all-zero procedural stack
/// falls back to layer 0 = 1. Keep this in step with those two methods.
fn stacked_layer_weight(
    layers: &regia_terrain::TerrainLayers,
    painted_layers: Option<&PaintedLayers>,
    index: usize,
    count: usize,
    slope_radians: f32,
    altitude: f32,
    xz: Option<[f32; 2]>,
) -> f32 {
    let mut weight = [0.0f32; regia_terrain::MAX_LAYERS];
    let mut total = 0.0f32;
    for (i, slot) in weight.iter_mut().enumerate().take(count) {
        let w = layers
            .layer(i)
            .map_or(0.0, |layer| layer.weight(slope_radians, altitude));
        *slot = w;
        total += w;
    }
    if total <= 0.0 {
        weight = [0.0; regia_terrain::MAX_LAYERS];
        if count > 0 {
            weight[0] = 1.0;
        }
    } else {
        for w in weight.iter_mut().take(count) {
            *w /= total;
        }
    }
    if let (Some(painted_layers), Some([x, z])) = (painted_layers, xz) {
        let painted = painted_layers.sample(x, z);
        let mut paint = [0.0f32; regia_terrain::MAX_LAYERS];
        let mut sum = 0.0f32;
        for (slot, v) in paint.iter_mut().zip(painted).take(count) {
            let v = if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
            *slot = v;
            sum += v;
        }
        if sum > 0.0 {
            let coverage = sum.min(1.0);
            for (w, p) in weight.iter_mut().zip(paint).take(count) {
                *w = *w * (1.0 - coverage) + coverage * p / sum;
            }
        }
    }
    weight[index]
}

/// The terrain's painted layer weights (`TerrainPaint`) with their world mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaintedLayers {
    /// The weights.
    pub weights: Arc<regia_terrain::LayerWeightMap>,
    /// The terrain's world width bits: the map spans it, centered on the origin.
    world_width_bits: u32,
}

impl PaintedLayers {
    /// Painted weights over a terrain `world_width` meters wide.
    pub fn new(weights: Arc<regia_terrain::LayerWeightMap>, world_width: f32) -> Self {
        Self {
            weights,
            world_width_bits: world_width.to_bits(),
        }
    }

    /// The painted weights of the nearest sample to world `(x, z)`.
    pub fn sample(&self, x: f32, z: f32) -> [f32; regia_terrain::MAX_LAYERS] {
        let world = f32::from_bits(self.world_width_bits).max(1e-3);
        let (w, h) = (self.weights.width(), self.weights.height());
        let at = |v: f32, n: u32| {
            let u = ((v / world + 0.5) * (n.saturating_sub(1)) as f32).round();
            (u.max(0.0) as u32).min(n.saturating_sub(1))
        };
        self.weights.sample(at(x, w), at(z, h))
    }
}

/// One position a `.scatter.ron` class places (a `ScatterClass` candidate, spec 65 § 3.1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScatterCandidate {
    /// Ground contact point.
    pub position: [f32; 3],
    /// Unit up axis.
    pub up: [f32; 3],
    /// Yaw about `up`, radians.
    pub yaw: f32,
    /// Uniform scale.
    pub scale: f32,
    /// The mesh the scatter set itself picked (index into the class's meshes; the biome picks again).
    pub mesh: u16,
}

impl BiomeWorld {
    /// The areas sorted by guid (call after filling `areas`).
    pub fn sort(&mut self) {
        self.areas.sort_by_key(|a| a.guid);
    }

    /// The claims at `(x, z)`.
    pub fn claims_at(&self, x: f32, z: f32) -> ClaimAt {
        claims_at(&self.areas, x, z)
    }
}

/// The `biome/layer/entry` name of an entry.
pub fn entry_path(biome: &str, layer: &str, entry: &str) -> String {
    format!("{biome}/{layer}/{entry}")
}
