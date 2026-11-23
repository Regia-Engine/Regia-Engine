// SPDX-License-Identifier: MPL-2.0
//! The biome definition asset `<name>.biome.ron` (spec 65 § 3, format `biome`, `format_version` 1).
//!
//! Every field has a default (`docs/formats/biome-ron.md`), so a file only names what it changes.
//! [`BiomeDef::from_ron`] parses, migrates and validates; [`BiomeDef::to_ron`] writes the canonical
//! form (every field, defaults filled in).

use std::collections::BTreeSet;

use regia_core::version::{VersionStep, migrate};
use serde::{Deserialize, Serialize};

/// Current `.biome.ron` format version.
pub const BIOME_FORMAT_VERSION: u32 = 1;

/// Migration steps of `.biome.ron` (none yet: version 1 is the first).
pub const BIOME_MIGRATIONS: &[VersionStep<ron::Value>] = &[];

/// The `format_version` values the migrator reads (the current one only).
pub fn biome_migration_sources() -> Vec<u32> {
    BIOME_MIGRATIONS.iter().map(|s| s.from).collect()
}

/// A field-name fingerprint of the format for `check-formats`: every struct and enum of the file
/// with its fields or variants, in declaration order.
pub fn biome_schema_fingerprint() -> String {
    [
        "BiomeDef{format_version,name,color,map_key,priority,layers,grass}",
        "BiomeLayer{name,rank,sharing,source,channels,entries,max_child_depth}",
        "Sharing{Exclusive,Shared}",
        "CandidateSource{Ground{spacing_m,jitter,noise},Poisson{min_dist_m},ScatterClass{set,class},Graph{graph,params}}",
        "SourceNoise{scale_m,octaves,min,max}",
        "BiomeChannel{name,rule}",
        "ChannelRule{Slope{min_deg,max_deg},Altitude{min,max},TerrainLayer{name,min_weight},Mask{asset,channel,min},Attr{sel,min,max},Random{fraction},Rest}",
        "BiomeEntry{name,spawn,weight,channels,enabled,filters,filters_off,footprint,placement,render,children}",
        "EntrySpawn{None,Mesh,Prefab}",
        "EntryFilter{Altitude{min,max,feather},Slope{min_deg,max_deg,feather},Density{min,max,feather},WaterDistance{min_m,max_m,feather},TerrainLayer{name,min,max,feather},Mask{asset,channel,min,max,feather},Attr{sel,min,max,feather},Graph{graph,attr,min,max,feather}}",
        "Footprint{core_m,spread_m}",
        "Placement{upright,yaw,scale,scale_from_source,sink_m,offset}",
        "Yaw{Random,Fixed,AlongSlope}",
        "ScaleRule{Keep,Range,Fixed}",
        "EntryRender{cast_shadows,draw_distance_m,fade_m,collision}",
        "EntryCollision{None,Simple,Mesh}",
        "ChildRule{entry,count,radius_m,rate,keep_clear_of_parent,scale_with_parent,graph}",
        "KeepClear{Core,Spread,None}",
        "BiomeGrass{class,density,outside}",
        "GrassOutside{Keep,Clear}",
    ]
    .join(";")
}

/// One problem found in a definition (`biome_invalid` lists them; a child naming a missing entry is
/// `biome_entry_unknown`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BiomeIssue {
    /// Stable code (`biome_invalid`, `biome_entry_unknown`).
    pub code: String,
    /// Where: `layers/<layer>/entries/<entry>/...`.
    pub path: String,
    /// Detail.
    pub message: String,
}

/// Why a definition could not be read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BiomeDefError {
    /// The text is not valid RON for the format.
    #[error("biome_invalid: {0}")]
    Parse(String),
    /// The file is newer than this build reads.
    #[error("biome_invalid: format_version {0} is newer than {BIOME_FORMAT_VERSION}")]
    Newer(u32),
    /// The definition parsed but breaks a rule (every issue, in file order).
    #[error("{}: {} issue(s)", .0.first().map_or("biome_invalid", |i| i.code.as_str()), .0.len())]
    Invalid(Vec<BiomeIssue>),
}

impl BiomeDefError {
    /// The stable code: `biome_entry_unknown` when the first issue is one, else `biome_invalid`.
    pub fn code(&self) -> &str {
        match self {
            Self::Invalid(list) => list.first().map_or("biome_invalid", |i| i.code.as_str()),
            _ => "biome_invalid",
        }
    }
}

fn d_version() -> u32 {
    BIOME_FORMAT_VERSION
}
fn d_color() -> (f32, f32, f32) {
    (0.5, 0.5, 0.5)
}
fn d_one() -> f64 {
    1.0
}
fn d_true() -> bool {
    true
}
fn d_spacing() -> f64 {
    4.0
}
fn d_jitter() -> f64 {
    0.8
}
fn d_min_dist() -> f64 {
    2.0
}
fn d_noise_scale() -> f64 {
    50.0
}
fn d_octaves() -> u32 {
    3
}
fn d_lo() -> f64 {
    -1.0e9
}
fn d_hi() -> f64 {
    1.0e9
}
fn d_slope_hi() -> f64 {
    90.0
}
fn d_depth() -> u32 {
    1
}
fn d_draw() -> f32 {
    300.0
}
fn d_fade() -> f32 {
    20.0
}
fn d_count() -> (u32, u32) {
    (1, 3)
}
fn d_radius() -> (f32, f32) {
    (0.5, 2.0)
}

/// A biome (spec 65 § 2): what grows in one kind of place.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BiomeDef {
    /// Format version (1).
    #[serde(default = "d_version")]
    pub format_version: u32,
    /// Display name; the first part of `biome/layer/entry`.
    #[serde(default)]
    pub name: String,
    /// Debug color (linear RGB, 0..1); also the default map key.
    #[serde(default = "d_color")]
    pub color: (f32, f32, f32),
    /// 8-bit key in biome map textures (`None`: from `color`).
    #[serde(default)]
    pub map_key: Option<(u8, u8, u8)>,
    /// Higher wins where areas overlap.
    #[serde(default)]
    pub priority: i32,
    /// Placement layers.
    #[serde(default)]
    pub layers: Vec<BiomeLayer>,
    /// Optional grass density multipliers per grass class (§ 8).
    #[serde(default)]
    pub grass: Vec<BiomeGrass>,
}

impl Default for BiomeDef {
    fn default() -> Self {
        Self {
            format_version: BIOME_FORMAT_VERSION,
            name: String::new(),
            color: d_color(),
            map_key: None,
            priority: 0,
            layers: Vec::new(),
            grass: Vec::new(),
        }
    }
}

/// Whether a layer takes part in the cross-layer resolve (spec 65 § 6.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sharing {
    /// Removes and is removed by overlapping cores of other ranks.
    #[default]
    Exclusive,
    /// Neither removes nor is removed.
    Shared,
}

/// Value-noise density modulation of a `Ground` source.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceNoise {
    /// Feature size, meters.
    #[serde(default = "d_noise_scale")]
    pub scale_m: f64,
    /// fBm octaves.
    #[serde(default = "d_octaves")]
    pub octaves: u32,
    /// Density at noise 0.
    #[serde(default)]
    pub min: f64,
    /// Density at noise 1.
    #[serde(default = "d_one")]
    pub max: f64,
}

/// Where a layer's root candidates come from (spec 65 § 3.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CandidateSource {
    /// A jittered world grid on the ground (`sample.ground`).
    Ground {
        /// Cell size, meters.
        #[serde(default = "d_spacing")]
        spacing_m: f64,
        /// Fraction of a cell a candidate may move.
        #[serde(default = "d_jitter")]
        jitter: f64,
        /// Optional density noise.
        #[serde(default)]
        noise: Option<SourceNoise>,
    },
    /// World-anchored blue noise (`sample.poisson`).
    Poisson {
        /// Minimum distance between candidates, meters.
        #[serde(default = "d_min_dist")]
        min_dist_m: f64,
    },
    /// The positions a `.scatter.ron` class places over the run bounds (the host runs the scatter
    /// placement; classes with a density map are not captured yet, ADR-1665).
    ScatterClass {
        /// The scatter set asset.
        set: String,
        /// The class name.
        class: String,
    },
    /// Any graph called with the claim as its `region` input; its `out` output must be points.
    Graph {
        /// The graph asset.
        graph: String,
        /// Parameter overrides as RON text per name.
        #[serde(default)]
        params: std::collections::BTreeMap<String, String>,
    },
}

impl Default for CandidateSource {
    fn default() -> Self {
        Self::Ground {
            spacing_m: d_spacing(),
            jitter: d_jitter(),
            noise: None,
        }
    }
}

/// A channel rule (spec 65 § 3.2): candidates go to the first channel whose rule matches.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ChannelRule {
    /// Ground slope in degrees.
    Slope {
        /// Lower bound.
        #[serde(default)]
        min_deg: f64,
        /// Upper bound.
        #[serde(default = "d_slope_hi")]
        max_deg: f64,
    },
    /// Height in meters.
    Altitude {
        /// Lower bound.
        #[serde(default = "d_lo")]
        min: f64,
        /// Upper bound.
        #[serde(default = "d_hi")]
        max: f64,
    },
    /// A terrain layer's weight at least `min_weight` (procedural slope/altitude weights of the
    /// terrain's layer stack; painted weights are not captured yet, ADR-1665).
    TerrainLayer {
        /// Layer name.
        name: String,
        /// Minimum weight.
        #[serde(default)]
        min_weight: f64,
    },
    /// A mask texture channel at least `min` (mapped over the generator bounds).
    Mask {
        /// Mask texture.
        asset: String,
        /// `r`, `g`, `b`, `a` or `luma`.
        #[serde(default = "d_channel")]
        channel: String,
        /// Minimum value.
        #[serde(default)]
        min: f64,
    },
    /// A numeric attribute in `[min, max]`.
    Attr {
        /// Attribute selector.
        sel: String,
        /// Lower bound.
        #[serde(default = "d_lo")]
        min: f64,
        /// Upper bound.
        #[serde(default = "d_hi")]
        max: f64,
    },
    /// A deterministic fraction of candidates (draw slot 0 of stage 2).
    Random {
        /// Fraction 0..1.
        #[serde(default)]
        fraction: f64,
    },
    /// Everything not taken by an earlier channel.
    Rest,
}

fn d_channel() -> String {
    "r".to_string()
}

/// A named channel of a layer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BiomeChannel {
    /// Channel name (unique in the layer).
    pub name: String,
    /// Its rule.
    pub rule: ChannelRule,
}

/// What an entry places.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntrySpawn {
    /// A spacer: claims space, places nothing.
    #[default]
    None,
    /// A mesh instance.
    Mesh(String),
    /// A prefab instance.
    Prefab(String),
}

impl EntrySpawn {
    /// The asset path (`None` for a spacer).
    pub fn asset(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Mesh(p) | Self::Prefab(p) => Some(p),
        }
    }
}

/// An entry filter (spec 65 § 3.3): a window and a linear `feather` outside it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EntryFilter {
    /// Height, meters.
    Altitude {
        /// Window low.
        #[serde(default = "d_lo")]
        min: f64,
        /// Window high.
        #[serde(default = "d_hi")]
        max: f64,
        /// Falloff width, meters.
        #[serde(default)]
        feather: f64,
    },
    /// Ground slope, degrees.
    Slope {
        /// Window low.
        #[serde(default)]
        min_deg: f64,
        /// Window high.
        #[serde(default = "d_slope_hi")]
        max_deg: f64,
        /// Falloff width, degrees.
        #[serde(default)]
        feather: f64,
    },
    /// The candidate's density.
    Density {
        /// Window low.
        #[serde(default)]
        min: f64,
        /// Window high.
        #[serde(default = "d_one")]
        max: f64,
        /// Falloff width.
        #[serde(default)]
        feather: f64,
    },
    /// Horizontal distance to water, meters (negative inside water).
    WaterDistance {
        /// Window low.
        #[serde(default = "d_lo")]
        min_m: f64,
        /// Window high.
        #[serde(default = "d_hi")]
        max_m: f64,
        /// Falloff width, meters.
        #[serde(default)]
        feather: f64,
    },
    /// A terrain layer's procedural weight (painted weights are not captured yet, ADR-1665).
    TerrainLayer {
        /// Layer name.
        name: String,
        /// Window low.
        #[serde(default)]
        min: f64,
        /// Window high.
        #[serde(default = "d_one")]
        max: f64,
        /// Falloff width.
        #[serde(default)]
        feather: f64,
    },
    /// A mask texture channel (mapped over the generator bounds).
    Mask {
        /// Mask texture.
        asset: String,
        /// `r`, `g`, `b`, `a` or `luma`.
        #[serde(default = "d_channel")]
        channel: String,
        /// Window low.
        #[serde(default)]
        min: f64,
        /// Window high.
        #[serde(default = "d_one")]
        max: f64,
        /// Falloff width.
        #[serde(default)]
        feather: f64,
    },
    /// A numeric attribute.
    Attr {
        /// Attribute selector.
        sel: String,
        /// Window low.
        #[serde(default = "d_lo")]
        min: f64,
        /// Window high.
        #[serde(default = "d_hi")]
        max: f64,
        /// Falloff width.
        #[serde(default)]
        feather: f64,
    },
    /// A graph run on the points writes `attr`, which is then tested.
    Graph {
        /// The graph asset (input pin `in`, output pin `out`).
        graph: String,
        /// The attribute it writes.
        attr: String,
        /// Window low.
        #[serde(default = "d_lo")]
        min: f64,
        /// Window high.
        #[serde(default = "d_hi")]
        max: f64,
        /// Falloff width.
        #[serde(default)]
        feather: f64,
    },
}

impl EntryFilter {
    /// The variant name (`Altitude`, ...), used in counter names.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Altitude { .. } => "Altitude",
            Self::Slope { .. } => "Slope",
            Self::Density { .. } => "Density",
            Self::WaterDistance { .. } => "WaterDistance",
            Self::TerrainLayer { .. } => "TerrainLayer",
            Self::Mask { .. } => "Mask",
            Self::Attr { .. } => "Attr",
            Self::Graph { .. } => "Graph",
        }
    }

    /// `(min, max, feather)` of the window.
    pub fn window(&self) -> (f64, f64, f64) {
        match self {
            Self::Altitude { min, max, feather }
            | Self::Density { min, max, feather }
            | Self::TerrainLayer {
                min, max, feather, ..
            }
            | Self::Mask {
                min, max, feather, ..
            }
            | Self::Attr {
                min, max, feather, ..
            }
            | Self::Graph {
                min, max, feather, ..
            } => (*min, *max, *feather),
            Self::Slope {
                min_deg,
                max_deg,
                feather,
            } => (*min_deg, *max_deg, *feather),
            Self::WaterDistance {
                min_m,
                max_m,
                feather,
            } => (*min_m, *max_m, *feather),
        }
    }
}

/// The density factor of a window with a feather (spec 65 § 3.3): 1 inside `[min, max]`, falling
/// linearly to 0 over `feather` outside, 0 beyond.
pub fn window_factor(v: f64, min: f64, max: f64, feather: f64) -> f64 {
    if v.is_nan() {
        return 0.0;
    }
    let out = if v < min {
        min - v
    } else if v > max {
        v - max
    } else {
        return 1.0;
    };
    if feather > 0.0 && out < feather {
        1.0 - out / feather
    } else {
        0.0
    }
}

/// An entry's two radii (spec 65 § 3.5). `None` = from the mesh bounds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Footprint {
    /// Trunk or base radius, meters.
    #[serde(default)]
    pub core_m: Option<f32>,
    /// Canopy radius, meters.
    #[serde(default)]
    pub spread_m: Option<f32>,
}

/// The yaw rule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Yaw {
    /// Uniform in [0, 360) (draw slot 0 of stage 4).
    #[default]
    Random,
    /// A fixed angle, degrees.
    Fixed(f64),
    /// Faces downhill.
    AlongSlope,
}

/// The scale rule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum ScaleRule {
    /// The source's scale.
    #[default]
    Keep,
    /// Uniform in `[lo, hi]` (draw slot 3 of stage 4).
    Range(f64, f64),
    /// A fixed uniform scale.
    Fixed(f64),
}

/// A local pivot fix: translation, rotation (Euler degrees) and scale.
pub type PivotOffset = ((f64, f64, f64), (f64, f64, f64), (f64, f64, f64));

/// Placement of an entry's instances (spec 65 § 3.4).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    /// 0 = aligned to the ground normal, 1 = world up.
    #[serde(default = "d_one")]
    pub upright: f64,
    /// Yaw rule.
    #[serde(default)]
    pub yaw: Yaw,
    /// Scale rule.
    #[serde(default)]
    pub scale: ScaleRule,
    /// `Range`/`Fixed` multiply the source scale instead of replacing it.
    #[serde(default)]
    pub scale_from_source: bool,
    /// Lowers the instance along its up axis, meters.
    #[serde(default)]
    pub sink_m: f64,
    /// Local pivot fix applied before the point transform.
    #[serde(default)]
    pub offset: Option<PivotOffset>,
}

impl Default for Placement {
    fn default() -> Self {
        Self {
            upright: 1.0,
            yaw: Yaw::Random,
            scale: ScaleRule::Keep,
            scale_from_source: false,
            sink_m: 0.0,
            offset: None,
        }
    }
}

/// Collision of spawned instances.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryCollision {
    /// None.
    #[default]
    None,
    /// A box per instance.
    Simple,
    /// The mesh's collision mesh.
    Mesh,
}

/// Render settings of an entry's instances.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EntryRender {
    /// Casts shadows.
    #[serde(default = "d_true")]
    pub cast_shadows: bool,
    /// Draw distance, meters.
    #[serde(default = "d_draw")]
    pub draw_distance_m: f32,
    /// Fade band, meters.
    #[serde(default = "d_fade")]
    pub fade_m: f32,
    /// Collision.
    #[serde(default)]
    pub collision: EntryCollision,
}

impl Default for EntryRender {
    fn default() -> Self {
        Self {
            cast_shadows: true,
            draw_distance_m: d_draw(),
            fade_m: d_fade(),
            collision: EntryCollision::None,
        }
    }
}

/// Which children are removed near their parent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeepClear {
    /// Inside the parent's core.
    #[default]
    Core,
    /// Inside the parent's spread.
    Spread,
    /// Keep all.
    None,
}

/// The most children one child rule may give one parent (`count.1`): the desktop per-item point
/// budget (`RunLimits::DESKTOP.points_per_item`), the largest any platform allows. A run still checks
/// its own (possibly lower) budget before it allocates a depth's children.
pub const MAX_CHILDREN_PER_PARENT: usize = crate::exec::RunLimits::DESKTOP.points_per_item;

/// A child rule (spec 65 § 3.6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChildRule {
    /// The child entry's name (in the same layer).
    pub entry: String,
    /// Children per parent, uniform in `[lo, hi]`.
    #[serde(default = "d_count")]
    pub count: (u32, u32),
    /// Ring radii, meters.
    #[serde(default = "d_radius")]
    pub radius_m: (f32, f32),
    /// Fraction of parents that get children.
    #[serde(default = "d_one")]
    pub rate: f64,
    /// Which children are removed near the parent.
    #[serde(default)]
    pub keep_clear_of_parent: KeepClear,
    /// Radii and child scale follow the parent's scale.
    #[serde(default)]
    pub scale_with_parent: bool,
    /// A graph placing the children instead (input `parent`, output `out` in the parent's frame).
    #[serde(default)]
    pub graph: Option<String>,
}

/// One thing a layer can place (spec 65 § 2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BiomeEntry {
    /// Entry name (unique in the layer).
    pub name: String,
    /// What it places.
    #[serde(default)]
    pub spawn: EntrySpawn,
    /// Pick weight (0 with no channels = child-only).
    #[serde(default = "d_one")]
    pub weight: f64,
    /// Channels it takes (`[]` with weight > 0 = all).
    #[serde(default)]
    pub channels: Vec<String>,
    /// Disabled entries never place and are skipped as children.
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Filters in order.
    #[serde(default)]
    pub filters: Vec<EntryFilter>,
    /// Skip every filter.
    #[serde(default)]
    pub filters_off: bool,
    /// Footprint.
    #[serde(default)]
    pub footprint: Footprint,
    /// Placement.
    #[serde(default)]
    pub placement: Placement,
    /// Render settings.
    #[serde(default)]
    pub render: EntryRender,
    /// Children.
    #[serde(default)]
    pub children: Vec<ChildRule>,
}

impl BiomeEntry {
    /// An enabled entry with defaults.
    pub fn new(name: &str, spawn: EntrySpawn) -> Self {
        Self {
            name: name.to_string(),
            spawn,
            weight: 1.0,
            channels: Vec::new(),
            enabled: true,
            filters: Vec::new(),
            filters_off: false,
            footprint: Footprint::default(),
            placement: Placement::default(),
            render: EntryRender::default(),
            children: Vec::new(),
        }
    }

    /// Child-only: weight 0 and no channels.
    pub fn child_only(&self) -> bool {
        self.weight <= 0.0 && self.channels.is_empty()
    }
}

/// One placement layer of a biome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BiomeLayer {
    /// Layer name (unique in the biome).
    pub name: String,
    /// Higher-rank layers keep their spots in the resolve.
    #[serde(default)]
    pub rank: i32,
    /// Exclusive or Shared.
    #[serde(default)]
    pub sharing: Sharing,
    /// Candidate source.
    #[serde(default)]
    pub source: CandidateSource,
    /// Channels in order (none = one implicit channel `all`).
    #[serde(default)]
    pub channels: Vec<BiomeChannel>,
    /// Entries.
    #[serde(default)]
    pub entries: Vec<BiomeEntry>,
    /// Child generations (0 = no children).
    #[serde(default = "d_depth")]
    pub max_child_depth: u32,
}

impl BiomeLayer {
    /// A layer with defaults.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            rank: 0,
            sharing: Sharing::Exclusive,
            source: CandidateSource::default(),
            channels: Vec::new(),
            entries: Vec::new(),
            max_child_depth: d_depth(),
        }
    }
}

/// What a biome does to grass outside every area.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrassOutside {
    /// Keep the class's own mask (multiplier 1).
    #[default]
    Keep,
    /// Clear it (multiplier 0).
    Clear,
}

/// A grass multiplier of a biome (spec 65 § 8).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BiomeGrass {
    /// Grass class name.
    pub class: String,
    /// Density multiplier inside the biome.
    #[serde(default = "d_one")]
    pub density: f64,
    /// Outside every area.
    #[serde(default)]
    pub outside: GrassOutside,
}

impl BiomeDef {
    /// The 8-bit map key: `map_key`, else `color` quantized.
    pub fn key(&self) -> [u8; 3] {
        match self.map_key {
            Some((r, g, b)) => [r, g, b],
            None => {
                let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                [q(self.color.0), q(self.color.1), q(self.color.2)]
            }
        }
    }

    /// Parses, migrates and validates a `.biome.ron` text.
    ///
    /// # Errors
    ///
    /// [`BiomeDefError::Parse`], [`BiomeDefError::Newer`] or [`BiomeDefError::Invalid`] with every
    /// issue.
    pub fn from_ron(text: &str) -> Result<Self, BiomeDefError> {
        let def = Self::parse_unchecked(text)?;
        let issues = def.validate();
        if issues.is_empty() {
            Ok(def)
        } else {
            Err(BiomeDefError::Invalid(issues))
        }
    }

    /// Parses and migrates without validating (for editors that show the issues).
    ///
    /// # Errors
    ///
    /// [`BiomeDefError::Parse`] or [`BiomeDefError::Newer`].
    pub fn parse_unchecked(text: &str) -> Result<Self, BiomeDefError> {
        let version = regia_core::version::peek_ron_version(text).unwrap_or(BIOME_FORMAT_VERSION);
        if version > BIOME_FORMAT_VERSION {
            return Err(BiomeDefError::Newer(version));
        }
        if version < BIOME_FORMAT_VERSION {
            let mut value: ron::Value =
                ron::from_str(text).map_err(|e| BiomeDefError::Parse(e.to_string()))?;
            migrate(&mut value, version, BIOME_MIGRATIONS, BIOME_FORMAT_VERSION)
                .map_err(|e| BiomeDefError::Parse(format!("{e:?}")))?;
            let mut def: Self = value
                .into_rust()
                .map_err(|e| BiomeDefError::Parse(e.to_string()))?;
            def.format_version = BIOME_FORMAT_VERSION;
            return Ok(def);
        }
        ron::from_str(text).map_err(|e| BiomeDefError::Parse(e.to_string()))
    }

    /// The canonical text: every field written, defaults filled in.
    pub fn to_ron(&self) -> String {
        let cfg = regia_core::ron_pretty()
            .struct_names(true)
            .indentor("    ".to_string());
        ron::ser::to_string_pretty(self, cfg).unwrap_or_default()
    }

    /// Every rule violation (spec 65 § 3): unique names, known channels and children, finite
    /// non-negative weights, ordered ranges.
    pub fn validate(&self) -> Vec<BiomeIssue> {
        let mut out = Vec::new();
        let mut bad = |code: &str, path: String, message: String| {
            out.push(BiomeIssue {
                code: code.to_string(),
                path,
                message,
            });
        };
        if self.format_version != BIOME_FORMAT_VERSION {
            bad(
                "biome_invalid",
                "format_version".into(),
                format!("expected {BIOME_FORMAT_VERSION}"),
            );
        }
        let mut layer_names = BTreeSet::new();
        for layer in &self.layers {
            let lp = format!("layers/{}", layer.name);
            if layer.name.is_empty() || !layer_names.insert(layer.name.as_str()) {
                bad(
                    "biome_invalid",
                    lp.clone(),
                    "layer names must be non-empty and unique".into(),
                );
            }
            match &layer.source {
                CandidateSource::Ground {
                    spacing_m, jitter, ..
                } => {
                    if !(spacing_m.is_finite() && *spacing_m >= 0.05) || !(0.0..=1.0).contains(jitter) {
                        bad(
                            "biome_invalid",
                            format!("{lp}/source"),
                            "spacing_m must be >= 0.05 and jitter in 0..1".into(),
                        );
                    }
                }
                CandidateSource::Poisson { min_dist_m } => {
                    if !(min_dist_m.is_finite() && *min_dist_m >= 0.05) {
                        bad(
                            "biome_invalid",
                            format!("{lp}/source"),
                            "min_dist_m must be >= 0.05".into(),
                        );
                    }
                }
                CandidateSource::ScatterClass { .. } | CandidateSource::Graph { .. } => {}
            }
            let mut channel_names = BTreeSet::new();
            for c in &layer.channels {
                if c.name.is_empty() || !channel_names.insert(c.name.as_str()) {
                    bad(
                        "biome_invalid",
                        format!("{lp}/channels/{}", c.name),
                        "channel names must be non-empty and unique".into(),
                    );
                }
            }
            if layer.channels.is_empty() {
                channel_names.insert("all");
            }
            let mut entry_names = BTreeSet::new();
            for e in &layer.entries {
                if e.name.is_empty() || !entry_names.insert(e.name.as_str()) {
                    bad(
                        "biome_invalid",
                        format!("{lp}/entries/{}", e.name),
                        "entry names must be non-empty and unique".into(),
                    );
                }
            }
            for e in &layer.entries {
                let ep = format!("{lp}/entries/{}", e.name);
                if !(e.weight.is_finite() && e.weight >= 0.0) {
                    bad(
                        "biome_invalid",
                        format!("{ep}/weight"),
                        "weight must be finite and >= 0".into(),
                    );
                }
                for c in &e.channels {
                    if !channel_names.contains(c.as_str()) {
                        bad(
                            "biome_invalid",
                            format!("{ep}/channels"),
                            format!("unknown channel `{c}`"),
                        );
                    }
                }
                if let ScaleRule::Range(lo, hi) = e.placement.scale
                    && !(lo.is_finite() && hi.is_finite() && lo > 0.0 && lo <= hi)
                {
                    bad(
                        "biome_invalid",
                        format!("{ep}/placement/scale"),
                        "Range needs 0 < lo <= hi".into(),
                    );
                }
                if !(0.0..=1.0).contains(&e.placement.upright) {
                    bad(
                        "biome_invalid",
                        format!("{ep}/placement/upright"),
                        "upright must be in 0..1".into(),
                    );
                }
                for (k, f) in e.filters.iter().enumerate() {
                    let (lo, hi, feather) = f.window();
                    if lo.is_nan() || hi.is_nan() || lo > hi || feather.is_nan() || feather < 0.0 {
                        bad(
                            "biome_invalid",
                            format!("{ep}/filters/{k}"),
                            "filter windows need min <= max and feather >= 0".into(),
                        );
                    }
                }
                for (k, c) in e.children.iter().enumerate() {
                    let cp = format!("{ep}/children/{k}");
                    if !entry_names.contains(c.entry.as_str()) {
                        bad(
                            "biome_entry_unknown",
                            cp.clone(),
                            format!("no entry `{}` in layer `{}`", c.entry, layer.name),
                        );
                    }
                    if c.count.0 > c.count.1
                        || !(c.radius_m.0 >= 0.0 && c.radius_m.0 <= c.radius_m.1)
                        || !(0.0..=1.0).contains(&c.rate)
                    {
                        bad(
                            "biome_invalid",
                            cp.clone(),
                            "children need count lo <= hi, 0 <= radius lo <= hi, rate in 0..1".into(),
                        );
                    }
                    // One parent's children alone may not pass the largest per-item point budget
                    // any platform allows: such a rule could only fail, after allocating them.
                    if c.count.1 as usize > MAX_CHILDREN_PER_PARENT {
                        bad(
                            "biome_invalid",
                            format!("{cp}/count"),
                            format!(
                                "count hi {} is above the per-item point budget ({MAX_CHILDREN_PER_PARENT})",
                                c.count.1
                            ),
                        );
                    }
                }
            }
        }
        for g in &self.grass {
            if g.class.is_empty() || !(g.density.is_finite() && g.density >= 0.0) {
                bad(
                    "biome_invalid",
                    format!("grass/{}", g.class),
                    "grass entries need a class and a density >= 0".into(),
                );
            }
        }
        // Issues of the more specific kind first so the error code names them.
        out.sort_by_key(|i| i.code != "biome_entry_unknown");
        out
    }

    /// A layer by name.
    pub fn layer(&self, name: &str) -> Option<&BiomeLayer> {
        self.layers.iter().find(|l| l.name == name)
    }
}
