// SPDX-License-Identifier: MPL-2.0
//! Splat-mapped material layers for the terrain (spec 26 § 1): up to 8 layers selected by
//! slope and altitude bands with soft height-blended transitions, per-layer roughness, tiling
//! and an optional triplanar projection for cliffs, plus a wet/dry parameter.
//!
//! Frozen RON format v5, documented in `docs/formats/terrain.md`. v2 adds optional per-layer
//! texture slots (`albedo_texture`/`normal_texture`/`height_texture`) and the
//! `detail_normal_strength` / `height_blend` knobs; v3 adds `orm_texture`; v4 adds the weather
//! response (a per-layer `weather` block and the stack's `puddles`, spec 99 § 7.3); v5 adds the
//! weather block's `porosity_from_orm` (the ORM map's blue channel as porosity, ADR-3367). Older files
//! migrate step by step; the defaults render the previous look (flat albedo, procedural mottle,
//! soft blending, the default weather response). The [`crate::material`] runtime resolves the
//! texture slots into the GPU material.

use regia_core::version::{self, VersionStep};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum number of material layers (spec 26 § 1).
pub const MAX_LAYERS: usize = 8;

/// Version of the frozen layers format.
///
/// v2 adds optional per-layer texture slots (`albedo_texture`, `normal_texture`,
/// `height_texture`) and the height-blend / detail-normal knobs. v3 adds `orm_texture`
/// (glTF occlusion/roughness/metallic). v4 adds the weather response: a per-layer
/// [`LayerDesc::weather`] block and the stack's [`TerrainLayers::puddles`] (spec 99 § 7.3).
/// v5 adds [`LayerWeather::porosity_from_orm`] (ADR-3367): an ORM map's blue channel is read
/// as porosity only on a layer that opts in. An older file migrates through every step; the
/// defaults render the previous look.
pub const TERRAIN_LAYERS_FORMAT_VERSION: u32 = 5;

/// Default per-layer micro-normal detail strength (v1 look: none).
const DEFAULT_DETAIL_NORMAL_STRENGTH: f32 = 0.0;

/// Default per-layer height-blend amount (v1 look: none).
const DEFAULT_HEIGHT_BLEND: f32 = 0.0;

/// Errors returned for invalid layer stacks.
#[derive(Clone, Debug, Error)]
pub enum LayersError {
    /// Unknown format version.
    #[error("unsupported terrain layers version {found}, expected {expected}")]
    BadVersion {
        /// Version found in the file.
        found: u32,
        /// Version this build reads/writes.
        expected: u32,
    },
    /// Too many layers.
    #[error("{count} layers exceeds the maximum of {max}")]
    TooManyLayers {
        /// Layer count found.
        count: usize,
        /// Maximum allowed.
        max: usize,
    },
    /// At least one layer is required.
    #[error("a terrain needs at least one material layer")]
    NoLayers,
    /// A layer parameter is out of range (finite, slopes ordered, roughness/albedo in 0..=1).
    #[error("layer {index} ({name}) has invalid parameters: {field} = {value}")]
    BadParameter {
        /// Index of the offending layer.
        index: usize,
        /// Name of the offending layer.
        name: String,
        /// Field name.
        field: &'static str,
        /// Offending value.
        value: f32,
    },
    /// A migration step failed or is missing.
    #[error("terrain layers migration failed: {0}")]
    Migration(String),
}

/// One material layer of the splat stack.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayerDesc {
    /// Human-readable layer name.
    pub name: String,
    /// Base colour (linear RGB, 0..=1).
    pub albedo: [f32; 3],
    /// Roughness in 0..=1.
    pub roughness: f32,
    /// Texture tiling factor (metres per texture repeat is `1 / tiling`).
    pub tiling: f32,
    /// Slope band start, radians from up (0 = flat).
    pub slope_min: f32,
    /// Slope band end, radians from up.
    pub slope_max: f32,
    /// Altitude band start, metres.
    pub altitude_min: f32,
    /// Altitude band end, metres.
    pub altitude_max: f32,
    /// Blend sharpness: fraction of the band edges over which the weight ramps (0.01..=0.5).
    pub blend_sharpness: f32,
    /// Use triplanar projection for this layer (cliff faces).
    pub triplanar: bool,
    /// Asset path of a tiling albedo texture (RGBA8, sRGB). `None` uses the flat `albedo`
    /// colour (v1 look); when set, `albedo` acts as a tint multiplier.
    #[serde(default)]
    pub albedo_texture: Option<String>,
    /// Asset path of a tiling tangent-space normal map (linear RGBA8). `None` = smooth.
    #[serde(default)]
    pub normal_texture: Option<String>,
    /// Asset path of a tiling height map (linear, R channel) driving height-blended
    /// transitions. `None` uses the procedural shader mottle.
    #[serde(default)]
    pub height_texture: Option<String>,
    /// Asset path of a glTF ORM map (R occlusion, G roughness, B metallic; linear).
    /// `None` keeps the scalar `roughness` and full occlusion (the v2 look). Terrain is never
    /// metallic: B is not read unless the layer's weather block sets
    /// [`LayerWeather::porosity_from_orm`], which reads it as porosity.
    #[serde(default)]
    pub orm_texture: Option<String>,
    /// Strength of the tiling micro-normal detail in `0..=2` (0 disables; distance-faded).
    #[serde(default = "default_detail_normal_strength")]
    pub detail_normal_strength: f32,
    /// Height-blend amount in `0..=1`: how strongly the per-layer height sharpens splat
    /// transitions (0 = the v1 soft blend).
    #[serde(default = "default_height_blend")]
    pub height_blend: f32,
    /// How rain and snow change this layer (v4; spec 99 § 7.3). `None` is the default response:
    /// porosity from the blended roughness, full snow cover.
    #[serde(default)]
    pub weather: Option<LayerWeather>,
}

/// How the weather changes one terrain layer (spec 99 § 7.3), v4. Every field has the default
/// response's value, so `weather: Some(())` is the same as no block.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayerWeather {
    /// Porosity `0..=1`: how much the wet layer darkens (sand and soil soak, rock and ice shed
    /// a film). `None` derives it from the blended roughness
    /// (`clamp((roughness - 0.25) / 0.5, 0, 1)`), or reads the ORM map with
    /// [`Self::porosity_from_orm`].
    #[serde(default)]
    pub porosity: Option<f32>,
    /// Scales the snow cover, `0..=1` (0: snow never settles on this layer, e.g. a warm road).
    #[serde(default = "default_layer_snow")]
    pub snow: f32,
    /// v5 (ADR-3367): the porosity of each texel is the blue channel of the layer's ORM map
    /// (`orm_texture`, linear: 0 sheds a film, 1 soaks), read with the colour pass's own tap,
    /// where `porosity` names none. Off by default: an ORM map's blue is glTF metallic, and
    /// existing maps are not reinterpreted. A layer without an ORM map keeps the derived
    /// porosity. Only wet frames read it: the dry frame is unchanged.
    #[serde(default)]
    pub porosity_from_orm: bool,
}

impl Default for LayerWeather {
    fn default() -> Self {
        Self {
            porosity: None,
            snow: default_layer_snow(),
            porosity_from_orm: false,
        }
    }
}

fn default_layer_snow() -> f32 {
    1.0
}

impl LayerWeather {
    /// Every value finite and in `0..=1`.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let unit = |v: f32| (0.0..=1.0).contains(&v);
        self.porosity.is_none_or(unit) && unit(self.snow)
    }

    /// Whether this block changes anything against the default response.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.porosity.is_none() && self.snow == default_layer_snow() && !self.porosity_from_orm
    }

    /// Where a layer with this block gets its porosity, given whether it has an ORM map:
    /// its own `porosity`, its ORM map's blue channel, or its roughness.
    #[must_use]
    pub fn porosity_source(&self, has_orm: bool) -> PorositySource {
        if self.porosity.is_some() {
            PorositySource::Layer
        } else if self.porosity_from_orm && has_orm {
            PorositySource::Orm
        } else {
            PorositySource::Roughness
        }
    }
}

/// Where a terrain layer's wet-weather porosity comes from ([`LayerWeather::porosity_source`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PorositySource {
    /// The weather block's own `porosity`.
    Layer,
    /// The blue channel of the layer's ORM map, per texel (`porosity_from_orm`, v5).
    Orm,
    /// Derived from the layer's roughness (the default response).
    Roughness,
}

impl PorositySource {
    /// The agent protocol's name: `layer`, `orm` or `roughness`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Layer => "layer",
            Self::Orm => "orm",
            Self::Roughness => "roughness",
        }
    }
}

fn default_detail_normal_strength() -> f32 {
    DEFAULT_DETAIL_NORMAL_STRENGTH
}

fn default_height_blend() -> f32 {
    DEFAULT_HEIGHT_BLEND
}

impl LayerDesc {
    /// A grass-like default layer.
    pub fn ground() -> Self {
        Self {
            name: "ground".into(),
            albedo: [0.18, 0.28, 0.10],
            roughness: 0.95,
            tiling: 8.0,
            slope_min: 0.0,
            slope_max: 0.6,
            altitude_min: -1000.0,
            altitude_max: 1000.0,
            blend_sharpness: 0.15,
            triplanar: false,
            albedo_texture: None,
            normal_texture: None,
            height_texture: None,
            orm_texture: None,
            detail_normal_strength: DEFAULT_DETAIL_NORMAL_STRENGTH,
            height_blend: DEFAULT_HEIGHT_BLEND,
            weather: None,
        }
    }

    /// Validates one layer (see [`LayersError::BadParameter`]).
    fn validate(&self, index: usize) -> Result<(), LayersError> {
        let bad = |field: &'static str, value: f32| LayersError::BadParameter {
            index,
            name: self.name.clone(),
            field,
            value,
        };
        for (i, &c) in self.albedo.iter().enumerate() {
            if !c.is_finite() || !(0.0..=1.0).contains(&c) {
                return Err(bad("albedo", c));
            }
            let _ = i;
        }
        if !self.roughness.is_finite() || !(0.0..=1.0).contains(&self.roughness) {
            return Err(bad("roughness", self.roughness));
        }
        if !self.tiling.is_finite() || self.tiling <= 0.0 {
            return Err(bad("tiling", self.tiling));
        }
        for value in [
            self.slope_min,
            self.slope_max,
            self.altitude_min,
            self.altitude_max,
        ] {
            if !value.is_finite() {
                return Err(bad("band", value));
            }
        }
        if self.slope_min < 0.0 || self.slope_max <= self.slope_min {
            return Err(bad("slope_max", self.slope_max));
        }
        if self.altitude_max <= self.altitude_min {
            return Err(bad("altitude_max", self.altitude_max));
        }
        if !self.blend_sharpness.is_finite() || !(0.01..=0.5).contains(&self.blend_sharpness) {
            return Err(bad("blend_sharpness", self.blend_sharpness));
        }
        for value in [self.detail_normal_strength, self.height_blend] {
            if !value.is_finite() || value < 0.0 {
                return Err(bad("detail/height blend", value));
            }
        }
        if self.detail_normal_strength > 2.0 {
            return Err(bad("detail_normal_strength", self.detail_normal_strength));
        }
        if self.height_blend > 1.0 {
            return Err(bad("height_blend", self.height_blend));
        }
        if let Some(weather) = self.weather
            && !weather.is_valid()
        {
            let (field, value) = match weather.porosity {
                Some(p) if !(0.0..=1.0).contains(&p) => ("weather.porosity", p),
                _ => ("weather.snow", weather.snow),
            };
            return Err(bad(field, value));
        }
        Ok(())
    }

    /// Splat weight of this layer at a slope/altitude point, `0..=1`. This is the CPU mirror
    /// of the photoreal WGSL evaluation (`slope_band`): a slope range starting at 0 keeps full
    /// weight on flat ground. Without the map's altitude range the altitude edges are plain
    /// ramps; see [`LayerDesc::weight_in`].
    pub fn weight(&self, slope_radians: f32, altitude: f32) -> f32 {
        self.weight_in(slope_radians, altitude, None)
    }

    /// [`LayerDesc::weight`] with the map's `[lowest, highest]` ground altitude: an altitude
    /// band that reaches the lowest or highest ground keeps full weight there (the WGSL
    /// `altitude_band`), so crests do not fall through to the fallback layer.
    pub fn weight_in(&self, slope_radians: f32, altitude: f32, altitude_range: Option<[f32; 2]>) -> f32 {
        let slope = if self.slope_min > 0.0 {
            band(
                slope_radians,
                self.slope_min,
                self.slope_max,
                self.blend_sharpness,
            )
        } else {
            let (_, upper) = band_halves(
                slope_radians,
                self.slope_min,
                self.slope_max,
                self.blend_sharpness,
            );
            upper
        };
        let (mut lower, mut upper) = band_halves(
            altitude,
            self.altitude_min,
            self.altitude_max,
            self.blend_sharpness,
        );
        if let Some([lo, hi]) = altitude_range {
            if self.altitude_min <= lo + 1e-3 {
                lower = 1.0;
            }
            if self.altitude_max >= hi - 1e-3 {
                upper = 1.0;
            }
        }
        slope * lower * upper
    }
}

/// The rising and falling halves of [`band`].
fn band_halves(value: f32, lo: f32, hi: f32, sharpness: f32) -> (f32, f32) {
    let span = (hi - lo).max(1e-5);
    let t = ((value - lo) / span).clamp(0.0, 1.0);
    let s = (sharpness / span).clamp(0.01, 0.5);
    (smoothstep01(t / s), 1.0 - smoothstep01((t - (1.0 - s)) / s))
}

/// Soft band membership: a plateau between ramped edges (shared CPU/GPU definition).
pub fn band(value: f32, lo: f32, hi: f32, sharpness: f32) -> f32 {
    let span = (hi - lo).max(1e-5);
    let t = ((value - lo) / span).clamp(0.0, 1.0);
    let s = (sharpness / span).clamp(0.01, 0.5);
    smoothstep01(t / s) * (1.0 - smoothstep01((t - (1.0 - s)) / s))
}

/// `smoothstep` on the unit interval, clamped.
fn smoothstep01(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A terrain's material layer stack (`.terrain.layers.ron`, format v4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TerrainLayers {
    /// Format version; loaders reject unknown versions.
    pub version: u32,
    /// Default wetness parameter in `0..=1` (darkens albedo, adds specular).
    pub wetness_default: f32,
    /// The layers, bottom (base) first.
    pub layers: Vec<LayerDesc>,
    /// Flat, upward ground of this terrain may hold puddles in rain (v4; spec 99 § 7.3).
    /// `false` keeps the ground a wet film only (a desert, a scree field).
    #[serde(default = "default_puddles")]
    pub puddles: bool,
}

fn default_puddles() -> bool {
    true
}

impl Default for TerrainLayers {
    fn default() -> Self {
        Self {
            version: TERRAIN_LAYERS_FORMAT_VERSION,
            wetness_default: 0.0,
            layers: vec![LayerDesc::ground()],
            puddles: default_puddles(),
        }
    }
}

impl TerrainLayers {
    /// Validates the stack.
    ///
    /// # Errors
    ///
    /// See [`LayersError`].
    pub fn validate(&self) -> Result<(), LayersError> {
        if self.version != TERRAIN_LAYERS_FORMAT_VERSION {
            return Err(LayersError::BadVersion {
                found: self.version,
                expected: TERRAIN_LAYERS_FORMAT_VERSION,
            });
        }
        if self.layers.is_empty() {
            return Err(LayersError::NoLayers);
        }
        if self.layers.len() > MAX_LAYERS {
            return Err(LayersError::TooManyLayers {
                count: self.layers.len(),
                max: MAX_LAYERS,
            });
        }
        if !self.wetness_default.is_finite() || !(0.0..=1.0).contains(&self.wetness_default) {
            return Err(LayersError::BadParameter {
                index: 0,
                name: "<stack>".into(),
                field: "wetness_default",
                value: self.wetness_default,
            });
        }
        for (index, layer) in self.layers.iter().enumerate() {
            layer.validate(index)?;
        }
        Ok(())
    }

    /// Serialises to a RON string.
    ///
    /// # Errors
    ///
    /// Returns a `ron` error if serialisation fails, and [`LayersError`] if the stack is
    /// invalid.
    pub fn to_ron(&self) -> Result<String, crate::TerrainError> {
        self.validate()?;
        // `\n` on every OS (ron's default is `\r\n` on Windows), so files match across hosts.
        Ok(ron::ser::to_string_pretty(self, regia_core::ron_pretty())?)
    }

    /// Parses and validates a RON string.
    ///
    /// # Errors
    ///
    /// Returns a `ron` parse error or [`LayersError`] for invalid content. The `version` field is
    /// peeked before the full deserialise, so a *newer* version is rejected even when its body no
    /// longer matches the current struct.
    pub fn from_ron(text: &str) -> Result<Self, crate::TerrainError> {
        let version = regia_core::version::peek_ron_version(text).map_err(|e| {
            crate::TerrainError::InvalidLayers(LayersError::Migration(format!("bad version: {e}")))
        })?;
        if version > TERRAIN_LAYERS_FORMAT_VERSION {
            return Err(crate::TerrainError::InvalidLayers(LayersError::BadVersion {
                found: version,
                expected: TERRAIN_LAYERS_FORMAT_VERSION,
            }));
        }
        let mut parsed: Self = ron::from_str(text)?;
        parsed.migrate_to_latest()?;
        parsed.validate()?;
        Ok(parsed)
    }

    /// Reject a newer version, or run the registered migrator chain to bring an older file up to
    /// [`TERRAIN_LAYERS_FORMAT_VERSION`] (`docs/specs/12-assets.md` § 4a).
    pub fn migrate_to_latest(&mut self) -> Result<u32, LayersError> {
        if self.version > TERRAIN_LAYERS_FORMAT_VERSION {
            return Err(LayersError::BadVersion {
                found: self.version,
                expected: TERRAIN_LAYERS_FORMAT_VERSION,
            });
        }
        if self.version < TERRAIN_LAYERS_FORMAT_VERSION {
            let to = version::migrate(
                self,
                self.version,
                builtin_migrations(),
                TERRAIN_LAYERS_FORMAT_VERSION,
            )
            .map_err(|e| LayersError::Migration(e.to_string()))?;
            self.version = to;
        }
        Ok(self.version)
    }

    /// Whether any layer's weather block differs from the default response (the shader then
    /// blends the per-layer response on a wet or snowy frame).
    #[must_use]
    pub fn has_weather_overrides(&self) -> bool {
        self.layers
            .iter()
            .any(|l| l.weather.is_some_and(|w| !w.is_default()))
    }

    /// Number of layers.
    pub fn count(&self) -> usize {
        self.layers.len()
    }

    /// The layer at `index`, if any.
    pub fn layer(&self, index: usize) -> Option<&LayerDesc> {
        self.layers.get(index)
    }

    /// Normalised weight of every layer at a slope/altitude point (falls back to layer 0
    /// everywhere when all weights vanish).
    pub fn weights(&self, slope_radians: f32, altitude: f32) -> Vec<f32> {
        self.weights_in(slope_radians, altitude, None)
    }

    /// [`TerrainLayers::weights`] with the map's `[lowest, highest]` altitude (see
    /// [`LayerDesc::weight_in`]); callers that know the map should use this so the CPU weights
    /// match the drawn ground on crests.
    pub fn weights_in(
        &self,
        slope_radians: f32,
        altitude: f32,
        altitude_range: Option<[f32; 2]>,
    ) -> Vec<f32> {
        let mut weights: Vec<f32> = self
            .layers
            .iter()
            .map(|l| l.weight_in(slope_radians, altitude, altitude_range))
            .collect();
        let total: f32 = weights.iter().sum();
        if total <= 0.0 {
            weights.fill(0.0);
            if let Some(first) = weights.first_mut() {
                *first = 1.0;
            }
        } else {
            for w in &mut weights {
                *w /= total;
            }
        }
        weights
    }

    /// [`TerrainLayers::weights`] with painted weights layered on top (the brush "paint layer"
    /// mode, [`crate::LayerWeightMap`]).
    ///
    /// `painted[i]` is layer `i`'s painted weight in `0..=1`. The painted coverage
    /// `c = min(1, Σ painted)` replaces that fraction of the procedural weights: the result is
    /// `(1 - c) · procedural + c · painted / Σ painted`, which sums to 1 and equals the
    /// procedural weights where nothing is painted.
    pub fn blend_painted(&self, slope_radians: f32, altitude: f32, painted: &[f32]) -> Vec<f32> {
        self.blend_painted_in(slope_radians, altitude, None, painted)
    }

    /// [`TerrainLayers::blend_painted`] with the map's altitude range
    /// (see [`TerrainLayers::weights_in`]).
    pub fn blend_painted_in(
        &self,
        slope_radians: f32,
        altitude: f32,
        altitude_range: Option<[f32; 2]>,
        painted: &[f32],
    ) -> Vec<f32> {
        let mut weights = self.weights_in(slope_radians, altitude, altitude_range);
        let painted: Vec<f32> = (0..weights.len())
            // A NaN weight paints nothing (`clamp` would keep the NaN and poison every layer).
            .map(|i| {
                let w = painted.get(i).copied().unwrap_or(0.0);
                if w.is_nan() { 0.0 } else { w.clamp(0.0, 1.0) }
            })
            .collect();
        let total: f32 = painted.iter().sum();
        if total <= 0.0 {
            return weights;
        }
        let coverage = total.min(1.0);
        for (w, p) in weights.iter_mut().zip(&painted) {
            *w = *w * (1.0 - coverage) + coverage * p / total;
        }
        weights
    }
}

/// The built-in migration chain for `.terrain.layers.ron`.
///
/// v1 files gain the v2 texture slots and blend knobs with their defaults (the v1 look:
/// flat albedo, no micro normals, soft blending — `None`/`0.0`). v2 files gain
/// `orm_texture: None`, which keeps the scalar roughness. v3 files gain the default weather
/// response (`weather: None` per layer, `puddles: true`): the version changes, nothing else.
/// v4 files keep their ORM maps' blue channel unread (`porosity_from_orm: false`).
pub fn builtin_migrations() -> &'static [VersionStep<TerrainLayers>] {
    &[
        VersionStep {
            from: 1,
            to: 2,
            migrate: |layers| {
                for layer in &mut layers.layers {
                    layer.albedo_texture = None;
                    layer.normal_texture = None;
                    layer.height_texture = None;
                    layer.detail_normal_strength = DEFAULT_DETAIL_NORMAL_STRENGTH;
                    layer.height_blend = DEFAULT_HEIGHT_BLEND;
                }
                Ok(())
            },
        },
        VersionStep {
            from: 2,
            to: 3,
            migrate: |layers| {
                for layer in &mut layers.layers {
                    layer.orm_texture = None;
                }
                Ok(())
            },
        },
        VersionStep {
            from: 3,
            to: 4,
            migrate: |layers| {
                for layer in &mut layers.layers {
                    layer.weather = None;
                }
                layers.puddles = default_puddles();
                Ok(())
            },
        },
        VersionStep {
            from: 4,
            to: 5,
            // v5 adds `weather.porosity_from_orm` (default off): a v4 ORM map's blue channel
            // stays unread, so the version changes and nothing else (ADR-3367).
            migrate: |layers| {
                for weather in layers.layers.iter_mut().filter_map(|l| l.weather.as_mut()) {
                    weather.porosity_from_orm = false;
                }
                Ok(())
            },
        },
    ]
}

/// The versions a built-in migration step leaves (`from` side), for `cargo xtask check-formats`
/// to verify a version bump ships with a migrator from the previous version.
pub fn terrain_layers_migration_sources() -> Vec<u32> {
    builtin_migrations().iter().map(|s| s.from).collect()
}

/// The serde field-list fingerprint of the `.terrain.layers.ron` format (hashed by
/// `cargo xtask check-formats`).
///
/// The root struct *and* its nested layer and weather structs are listed.
pub fn terrain_layers_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<TerrainLayers>(),
        regia_core::schema::serde_field_names::<LayerDesc>(),
        regia_core::schema::serde_field_names::<LayerWeather>(),
    ])
}

/// What one layer's albedo comes to on screen (ADR-1425): its `albedo` tint times the mean
/// linear albedo of its texture, with a warning when that is not a plausible ground albedo.
#[derive(Clone, Debug, PartialEq)]
pub struct LayerAlbedo {
    /// Layer name.
    pub name: String,
    /// The layer's `albedo` (a tint when it has a texture).
    pub tint: [f32; 3],
    /// Mean linear albedo of the layer's texture, when the draw knows it.
    pub texture_mean: Option<[f32; 3]>,
    /// `tint × texture_mean` (the tint alone without a texture).
    pub effective: [f32; 3],
    /// Why the effective albedo looks wrong, or `None`.
    pub warning: Option<&'static str>,
}

/// Lowest broadband albedo of natural ground the report accepts: wet dark soil and fresh
/// asphalt sit at 0.04–0.08 (Oke, "Boundary Layer Climates", 2nd ed. 1987, table 1.1).
pub const MIN_GROUND_ALBEDO: f32 = 0.03;

/// The [`LayerAlbedo`] of each layer of `layers`, given the mean linear albedo of each layer's
/// texture (`texture_means[i]`, `None` where unknown or untextured). Warns when the effective
/// albedo is darker than any natural ground ([`MIN_GROUND_ALBEDO`]) or when the tint is itself a
/// ground albedo on a texture that already has one: the shader multiplies the two, so a scene
/// that graded its textures to their target albedo and also wrote that albedo as the tint
/// renders the ground at the square of it (the look round 1 showcase ground: dead grass
/// 0.36 × 0.21 ≈ 0.07 instead of 0.21, mud 0.006).
#[must_use]
pub fn layer_albedo_report(layers: &TerrainLayers, texture_means: &[Option<[f32; 3]>]) -> Vec<LayerAlbedo> {
    let lum = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    layers
        .layers
        .iter()
        .enumerate()
        .map(|(i, layer)| {
            let tint = layer.albedo;
            let mean = texture_means.get(i).copied().flatten();
            let effective = mean.map_or(tint, |m| std::array::from_fn(|c| tint[c] * m[c]));
            let warning = if lum(effective) < MIN_GROUND_ALBEDO {
                Some("effective albedo is darker than any natural ground (below 0.03)")
            } else if let Some(m) = mean
                && (MIN_GROUND_ALBEDO..=0.95).contains(&lum(m))
                && lum(tint) < 0.6
            {
                Some(
                    "the tint looks like an albedo and the texture already has one: the shader \
                     multiplies them (use a tint near 1 on a graded texture)",
                )
            } else {
                None
            };
            LayerAlbedo {
                name: layer.name.clone(),
                tint,
                texture_mean: mean,
                effective,
                warning,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three_layer_stack() -> TerrainLayers {
        TerrainLayers {
            layers: vec![
                LayerDesc {
                    name: "sand".into(),
                    albedo: [0.76, 0.70, 0.50],
                    roughness: 0.95,
                    tiling: 6.0,
                    slope_min: 0.0,
                    slope_max: 0.4,
                    altitude_min: -100.0,
                    altitude_max: 2.0,
                    blend_sharpness: 0.1,
                    triplanar: false,
                    ..LayerDesc::ground()
                },
                LayerDesc::ground(),
                LayerDesc {
                    name: "cliff".into(),
                    albedo: [0.42, 0.38, 0.34],
                    roughness: 0.85,
                    tiling: 4.0,
                    slope_min: 0.8,
                    slope_max: 1.6,
                    altitude_min: -100.0,
                    altitude_max: 500.0,
                    blend_sharpness: 0.08,
                    triplanar: true,
                    ..LayerDesc::ground()
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn ron_round_trip() {
        let stack = three_layer_stack();
        let text = stack.to_ron().expect("serialize");
        let back = TerrainLayers::from_ron(&text).expect("parse");
        assert_eq!(stack, back);
    }

    #[test]
    fn rejects_unknown_version() {
        let mut stack = three_layer_stack();
        stack.version = TERRAIN_LAYERS_FORMAT_VERSION + 1;
        let text = ron::ser::to_string(&stack).expect("raw serialize");
        assert!(matches!(
            TerrainLayers::from_ron(&text),
            Err(crate::TerrainError::InvalidLayers(LayersError::BadVersion {
                found: 6,
                ..
            }))
        ));
    }

    #[test]
    fn v1_migrates_to_v2_with_the_v1_look() {
        let v1_text = r#"
TerrainLayers(
    version: 1,
    wetness_default: 0.0,
    layers: [
        LayerDesc(
            name: "ground",
            albedo: (0.18, 0.28, 0.10),
            roughness: 0.95,
            tiling: 8.0,
            slope_min: 0.0,
            slope_max: 0.6,
            altitude_min: -1000.0,
            altitude_max: 1000.0,
            blend_sharpness: 0.15,
            triplanar: false,
        ),
    ],
)
"#;
        let layers = TerrainLayers::from_ron(v1_text).expect("v1 migrates");
        assert_eq!(layers.version, TERRAIN_LAYERS_FORMAT_VERSION);
        let layer = &layers.layers[0];
        assert!(layer.albedo_texture.is_none());
        assert!(layer.normal_texture.is_none());
        assert!(layer.height_texture.is_none());
        assert!(layer.orm_texture.is_none());
        assert_eq!(layer.detail_normal_strength, 0.0);
        assert_eq!(layer.height_blend, 0.0);
        // The old fields survive untouched.
        assert_eq!(layer.albedo, [0.18, 0.28, 0.10]);
        assert_eq!(layer.tiling, 8.0);
        assert!(layer.weather.is_none() && layers.puddles);
        // Re-serialising writes the current version and still loads.
        let text = layers.to_ron().expect("serialize the current version");
        assert!(
            text.contains(&format!("version: {TERRAIN_LAYERS_FORMAT_VERSION}")),
            "{text}"
        );
        let back = TerrainLayers::from_ron(&text).expect("reload the current version");
        assert_eq!(back, layers);
    }

    /// A v3 file (no weather fields) migrates to v4 with the default response: no layer block,
    /// puddles on, every other field untouched, and the shader sees no override.
    #[test]
    fn v3_migrates_to_v4_with_the_default_weather() {
        let v3_text = r#"
TerrainLayers(
    version: 3,
    wetness_default: 0.25,
    layers: [
        LayerDesc(
            name: "sand",
            albedo: (0.7, 0.6, 0.45),
            roughness: 0.9,
            tiling: 6.0,
            slope_min: 0.0,
            slope_max: 0.5,
            altitude_min: -10.0,
            altitude_max: 20.0,
            blend_sharpness: 0.1,
            triplanar: false,
            orm_texture: Some("sand_orm.png"),
            detail_normal_strength: 0.5,
            height_blend: 0.3,
        ),
    ],
)
"#;
        let layers = TerrainLayers::from_ron(v3_text).expect("v3 migrates");
        // Through v4 (the weather block) to v5 (ADR-3367), the current version.
        assert_eq!(layers.version, TERRAIN_LAYERS_FORMAT_VERSION);
        assert!(layers.puddles);
        assert_eq!(layers.wetness_default, 0.25);
        let layer = &layers.layers[0];
        assert!(layer.weather.is_none());
        assert_eq!(layer.orm_texture.as_deref(), Some("sand_orm.png"));
        assert_eq!(layer.height_blend, 0.3);
        assert!(!layers.has_weather_overrides());
        let text = layers.to_ron().expect("serialize v4");
        assert_eq!(TerrainLayers::from_ron(&text).expect("reload"), layers);
    }

    /// A v4 file (an ORM map and a weather block) migrates to v5 with its ORM blue still unread:
    /// the version moves, `porosity_from_orm` is off, every other field survives (ADR-3367).
    #[test]
    fn v4_migrates_to_v5_without_reading_orm_blue() {
        let v4_text = r#"
TerrainLayers(
    version: 4,
    wetness_default: 0.0,
    layers: [
        LayerDesc(
            name: "mud",
            albedo: (0.3, 0.25, 0.2),
            roughness: 0.8,
            tiling: 4.0,
            slope_min: 0.0,
            slope_max: 0.7,
            altitude_min: -50.0,
            altitude_max: 50.0,
            blend_sharpness: 0.1,
            triplanar: false,
            orm_texture: Some("mud_orm.png"),
            weather: Some((porosity: Some(0.7), snow: 0.5)),
        ),
        LayerDesc(
            name: "rock",
            albedo: (0.4, 0.4, 0.4),
            roughness: 0.6,
            tiling: 2.0,
            slope_min: 0.5,
            slope_max: 1.6,
            altitude_min: -50.0,
            altitude_max: 50.0,
            blend_sharpness: 0.1,
            triplanar: true,
            orm_texture: Some("rock_orm.png"),
        ),
    ],
    puddles: false,
)
"#;
        let layers = TerrainLayers::from_ron(v4_text).expect("v4 migrates");
        assert_eq!(layers.version, 5);
        assert!(!layers.puddles);
        let mud = layers.layers[0].weather.expect("mud keeps its block");
        assert_eq!(mud.porosity, Some(0.7));
        assert_eq!(mud.snow, 0.5);
        assert!(!mud.porosity_from_orm);
        assert!(layers.layers[1].weather.is_none());
        assert_eq!(layers.layers[1].orm_texture.as_deref(), Some("rock_orm.png"));
        let text = layers.to_ron().expect("serialize v5");
        assert!(text.contains("version: 5"));
        assert_eq!(TerrainLayers::from_ron(&text).expect("reload"), layers);
    }

    /// `porosity_from_orm` round trips, counts as an override, and names its source: the ORM map
    /// where the layer has one and names no porosity, the roughness without a map, the layer's
    /// own value when it names one.
    #[test]
    fn porosity_from_orm_round_trips_and_names_its_source() {
        let orm = LayerWeather {
            porosity_from_orm: true,
            ..LayerWeather::default()
        };
        assert!(!orm.is_default());
        assert_eq!(orm.porosity_source(true), PorositySource::Orm);
        assert_eq!(orm.porosity_source(false), PorositySource::Roughness);
        let named = LayerWeather {
            porosity: Some(0.2),
            ..orm
        };
        assert_eq!(named.porosity_source(true), PorositySource::Layer);
        assert_eq!(
            LayerWeather::default().porosity_source(true),
            PorositySource::Roughness
        );
        assert_eq!(
            [
                PorositySource::Layer,
                PorositySource::Orm,
                PorositySource::Roughness
            ]
            .map(PorositySource::as_str),
            ["layer", "orm", "roughness"]
        );

        let mut stack = three_layer_stack();
        stack.layers[1].orm_texture = Some("rock_orm.png".into());
        stack.layers[1].weather = Some(orm);
        assert!(stack.has_weather_overrides());
        let text = stack.to_ron().expect("serialize");
        assert!(text.contains("porosity_from_orm: true"), "{text}");
        assert_eq!(TerrainLayers::from_ron(&text).expect("parse"), stack);
        // A v5 block that omits the field reads it as off.
        let text = text.replace("porosity_from_orm: true,", "");
        let back = TerrainLayers::from_ron(&text).expect("parse without the field");
        assert_eq!(back.layers[1].weather, Some(LayerWeather::default()));
    }

    /// The v4 fields round trip, a bare `weather: Some(())` is the default response, and only a
    /// block that differs from it counts as an override.
    #[test]
    fn layer_weather_round_trips_and_defaults() {
        let mut stack = three_layer_stack();
        stack.puddles = false;
        stack.layers[0].weather = Some(LayerWeather {
            porosity: Some(0.95),
            snow: 1.0,
            porosity_from_orm: false,
        });
        stack.layers[2].weather = Some(LayerWeather {
            porosity: None,
            snow: 0.2,
            porosity_from_orm: false,
        });
        assert!(stack.has_weather_overrides());
        let text = stack.to_ron().expect("serialize");
        let back = TerrainLayers::from_ron(&text).expect("parse");
        assert_eq!(back, stack);

        let mut plain = three_layer_stack();
        let text = plain.to_ron().expect("serialize").replace(
            "height_blend: 0.0,\n            weather: None,",
            "height_blend: 0.0,\n            weather: Some(()),",
        );
        let bare = TerrainLayers::from_ron(&text).expect("bare block");
        assert_eq!(bare.layers[0].weather, Some(LayerWeather::default()));
        assert!(!bare.has_weather_overrides(), "a bare block is no override");
        plain.layers[1].weather = Some(LayerWeather::default());
        assert!(!plain.has_weather_overrides());
    }

    /// Porosity and snow outside `0..=1`, or not finite, are refused and named.
    #[test]
    fn layer_weather_out_of_range_is_refused() {
        for (weather, field) in [
            (
                LayerWeather {
                    porosity: Some(1.5),
                    snow: 1.0,
                    porosity_from_orm: false,
                },
                "weather.porosity",
            ),
            (
                LayerWeather {
                    porosity: Some(f32::NAN),
                    snow: 1.0,
                    porosity_from_orm: false,
                },
                "weather.porosity",
            ),
            (
                LayerWeather {
                    porosity: None,
                    snow: -0.1,
                    porosity_from_orm: false,
                },
                "weather.snow",
            ),
            (
                LayerWeather {
                    porosity: Some(0.5),
                    snow: f32::INFINITY,
                    porosity_from_orm: false,
                },
                "weather.snow",
            ),
        ] {
            let mut stack = three_layer_stack();
            stack.layers[1].weather = Some(weather);
            match stack.validate() {
                Err(LayersError::BadParameter {
                    index: 1, field: f, ..
                }) => assert_eq!(f, field, "{weather:?}"),
                other => panic!("{weather:?}: {other:?}"),
            }
        }
    }

    /// A v3 file without the stack-level field and a v4 file that omits it both hold puddles.
    #[test]
    fn puddles_default_on() {
        let stack = three_layer_stack();
        assert!(stack.puddles);
        let text = stack
            .to_ron()
            .expect("serialize")
            .replace("    puddles: true,\n", "");
        assert!(!text.contains("puddles"), "{text}");
        assert!(TerrainLayers::from_ron(&text).expect("parse").puddles);
    }

    #[test]
    fn migration_chain_registers_v1_and_v2_steps() {
        let sources = terrain_layers_migration_sources();
        assert!(
            sources.contains(&1) && sources.contains(&2) && sources.contains(&3),
            "v1 → v2, v2 → v3 and v3 → v4 steps must be registered: {sources:?}"
        );
    }

    #[test]
    fn rejects_too_many_layers() {
        let mut stack = three_layer_stack();
        stack.layers = vec![LayerDesc::ground(); 9];
        assert!(matches!(
            stack.validate(),
            Err(LayersError::TooManyLayers { count: 9, .. })
        ));
    }

    #[test]
    fn rejects_out_of_range_parameters() {
        let mut stack = three_layer_stack();
        stack.layers[0].roughness = 1.5;
        assert!(matches!(
            stack.validate(),
            Err(LayersError::BadParameter {
                index: 0,
                field: "roughness",
                ..
            })
        ));
        stack.layers[0].roughness = 0.9;
        stack.layers[2].slope_max = 0.5; // <= slope_min
        assert!(matches!(
            stack.validate(),
            Err(LayersError::BadParameter {
                index: 2,
                field: "slope_max",
                ..
            })
        ));
        stack.layers[2].slope_max = 1.6;
        stack.layers[0].detail_normal_strength = 3.0;
        assert!(matches!(
            stack.validate(),
            Err(LayersError::BadParameter {
                index: 0,
                field: "detail_normal_strength",
                ..
            })
        ));
        stack.layers[0].detail_normal_strength = 0.7;
        stack.layers[0].height_blend = 1.5;
        assert!(matches!(
            stack.validate(),
            Err(LayersError::BadParameter {
                index: 0,
                field: "height_blend",
                ..
            })
        ));
    }

    #[test]
    fn weights_normalise_and_fall_back() {
        let stack = three_layer_stack();
        // Flat low ground: mostly sand+ground, no cliff.
        let w = stack.weights(0.0, 0.0);
        let total: f32 = w.iter().sum();
        assert!((total - 1.0).abs() < 1e-4);
        assert!(w[2] < 1e-4, "cliff weight should vanish on flat ground");
        // A point no layer covers falls back to layer 0. (Flat ground at 300 m is covered by
        // "ground" now that a slope range starting at 0 keeps full weight on flat ground, like
        // the photoreal shader; slope 0.7 sits in the gap between "ground" and "cliff".)
        let w = stack.weights(0.7, 300.0);
        assert!((w[0] - 1.0).abs() < 1e-4);
        // Steep slope favours the cliff layer.
        let w = stack.weights(1.2, 10.0);
        assert!(w[2] > 0.5, "cliff should dominate steep slopes: {w:?}");
    }

    #[test]
    fn flat_ground_and_crests_keep_full_weight_like_the_photoreal_shader() {
        let layer = |smin: f32, smax: f32, amin: f32, amax: f32| LayerDesc {
            slope_min: smin,
            slope_max: smax,
            altitude_min: amin,
            altitude_max: amax,
            ..three_layer_stack().layers[0].clone()
        };
        // Slope range starting at 0: flat ground (slope 0) is full weight, not 0.
        let flat = layer(0.0, 0.5, -100.0, 100.0);
        assert!(flat.weight(0.0, 0.0) > 0.999);
        // A slope range that starts above 0 still ramps from 0 at its lower edge.
        let steep = layer(0.4, 1.0, -100.0, 100.0);
        assert!(steep.weight(0.4, 0.0) < 1e-4);
        // The map's summit: a band ending at the highest ground keeps full weight there.
        let top = layer(0.0, 1.5, 0.0, 50.0);
        assert!(top.weight_in(0.3, 50.0, Some([-5.0, 50.0])) > 0.999);
        assert!(top.weight_in(0.3, 50.0, None) < 1e-4);
        // The lowest ground likewise.
        let bottom = layer(0.0, 1.5, -5.0, 20.0);
        assert!(bottom.weight_in(0.3, -5.0, Some([-5.0, 50.0])) > 0.999);
        // A band strictly inside the map's range still fades at its ends.
        let mid = layer(0.0, 1.5, 10.0, 20.0);
        assert!(mid.weight_in(0.3, 20.0, Some([-5.0, 50.0])) < 1e-4);
        // The stack no longer falls back to layer 0 on a flat plateau that layer 1 owns.
        let mut stack = three_layer_stack();
        stack.layers[0] = layer(0.0, 0.0, -100.0, -90.0);
        stack.layers[1] = layer(0.0, 0.5, -100.0, 100.0);
        let w = stack.weights(0.0, 0.0);
        assert!(w[1] > 0.99, "flat plateau belongs to layer 1: {w:?}");
    }

    #[test]
    fn band_is_a_plateau_with_soft_edges() {
        assert!(band(0.5, 0.0, 1.0, 0.2) > 0.999);
        assert!(band(-1.0, 0.0, 1.0, 0.2) < 1e-4);
        assert!(band(2.0, 0.0, 1.0, 0.2) < 1e-4);
        // The ramp spans [0, s] = [0, 0.2]; its midpoint (0.1) sits at the S-curve's centre.
        assert!((band(0.1, 0.0, 1.0, 0.2) - 0.5).abs() < 0.05);
    }

    #[test]
    fn painted_weights_override_in_proportion_to_coverage() {
        let stack = three_layer_stack();
        let procedural = stack.weights(0.1, 5.0);
        assert_eq!(
            stack.blend_painted(0.1, 5.0, &[]),
            procedural,
            "unpainted = procedural"
        );
        let full = stack.blend_painted(0.1, 5.0, &[0.0, 0.0, 1.0]);
        assert!((full[2] - 1.0).abs() < 1e-6 && full[0].abs() < 1e-6, "{full:?}");
        let half = stack.blend_painted(0.1, 5.0, &[0.0, 0.0, 0.5]);
        assert!((half.iter().sum::<f32>() - 1.0).abs() < 1e-5);
        assert!((half[2] - (0.5 * procedural[2] + 0.5)).abs() < 1e-5, "{half:?}");
        // Over-painted weights renormalise.
        let over = stack.blend_painted(0.1, 5.0, &[1.0, 1.0, 0.0]);
        assert!((over[0] - 0.5).abs() < 1e-6 && (over[1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn empty_layer_stack_is_no_layers() {
        let stack = TerrainLayers {
            layers: Vec::new(),
            ..three_layer_stack()
        };
        assert!(matches!(stack.validate(), Err(LayersError::NoLayers)));
    }

    #[test]
    fn a_nan_painted_weight_paints_nothing() {
        let stack = three_layer_stack();
        let procedural = stack.weights(0.1, 5.0);
        assert_eq!(stack.blend_painted(0.1, 5.0, &[f32::NAN, 0.0, 0.0]), procedural);
        let mixed = stack.blend_painted(0.1, 5.0, &[f32::NAN, 0.0, 1.0]);
        assert!(mixed.iter().all(|w| w.is_finite()), "{mixed:?}");
        assert!((mixed[2] - 1.0).abs() < 1e-6, "{mixed:?}");
        // Infinite paint is full paint.
        let inf = stack.blend_painted(0.1, 5.0, &[0.0, f32::INFINITY, 0.0]);
        assert!((inf[1] - 1.0).abs() < 1e-6, "{inf:?}");
    }

    #[test]
    fn albedo_report_catches_a_tint_applied_twice() {
        let mut layers = three_layer_stack();
        layers.layers[0].albedo = [0.36, 0.3, 0.2];
        layers.layers[1].albedo = [1.0, 1.0, 1.0];
        let graded = Some([0.235, 0.205, 0.15]);
        let report = layer_albedo_report(&layers, &[graded, graded]);
        assert_eq!(report.len(), 3);
        assert!(
            report[2].texture_mean.is_none(),
            "no mean given for the third layer"
        );
        // Tint × texture: about 0.07, and flagged.
        assert!((report[0].effective[1] - 0.3 * 0.205).abs() < 1e-6);
        assert!(report[0].warning.is_some());
        // Tint 1 on the graded texture: the texture's albedo, no warning.
        assert_eq!(report[1].effective, [0.235, 0.205, 0.15]);
        assert!(report[1].warning.is_none());
        // Mud: 0.1 × 0.06 is darker than any natural ground.
        layers.layers[1].albedo = [0.1, 0.08, 0.06];
        let mud = layer_albedo_report(&layers, &[None, Some([0.068, 0.058, 0.047])]);
        assert!(mud[1].warning.is_some_and(|w| w.contains("darker")));
        // No texture: the colour is the albedo; a plausible one passes.
        layers.layers[0].albedo = [0.2, 0.18, 0.12];
        let flat = layer_albedo_report(&layers, &[]);
        assert_eq!(flat[0].effective, [0.2, 0.18, 0.12]);
        assert!(flat[0].warning.is_none() && flat[0].texture_mean.is_none());
    }
}
