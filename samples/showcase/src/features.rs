// SPDX-License-Identifier: MIT-0
//! Optional engine features a scene directory can switch on. Each one is a file (or a block of
//! `sky.ron`) and is off when the file is absent, so a scene written before the feature existed
//! loads and renders as it did.
//!
//! | Feature | Switch | Engine side |
//! |---|---|---|
//! | Terrain grass | `terrain/grass.ron`, or `--grass` | [`regia_terrain::GrassLayer`] on the terrain draw |
//! | Valley mist | a `mist:` block in `sky.ron` | [`regia_render::ValleyMist`] with a height grid of the map |
//! | Water | `terrain/stream.json` | a [`regia_terrain::WaterBody`] ribbon and `WaterPlugin` |
//! | Entry streak | `entry.ron` | an [`regia_vfx::EntryStreak`] and `EntryStreakPlugin` |
//! | Contact AO | `--contact-ao` | [`regia_terrain::ContactMap`] fed by the scatter field |
//! | Tunnels | `tunnels/tunnels.ron` | the mesh layers cooked at load, the mouth cut on the terrain draw, tube meshes and lights, indoor cells ([`crate::tunnels`], ADR-8825) |
//!
//! Everything here is CPU work done at load time and needs no GPU: the grass mask is baked from the
//! splat packs, the mist grid is sampled from the heightfield, the water ribbon and the entry track
//! are parsed and validated, and the tunnels are cooked. A malformed file is a [`ShowcaseError`] with its own code, never a panic.

use std::path::Path;
use std::sync::Arc;

use regia_core::math::Vec3;
use regia_render::ValleyMist;
use regia_terrain::{
    GrassClass, GrassLayerRule, GrassMask, GrassMaskInputs, GrassSet, LayerWeightMap, Rgba8Image,
    RibbonPoint, TerrainLayers, TerrainMap, WaterBody, WaterLook, mist_grid_from_map,
};
use regia_vfx::EntryStreak;
use regia_vfx::entry::{EntryProfile, EntryTier, EntryTrack, TrackPoint};
use regia_vfx::entry_runtime::ENTRY_FRAME_RATE;
use serde::{Deserialize, Serialize};

use crate::sky::SkyFile;
use crate::{ErrorCode, ShowcaseError};

/// The grass file, relative to the scene directory.
pub const GRASS_FILE: &str = "terrain/grass.ron";
/// The stream centerline, relative to the scene directory.
pub const STREAM_FILE: &str = "terrain/stream.json";
/// The entry streak's track, relative to the scene directory.
pub const ENTRY_FILE: &str = "entry.ron";

/// Format version of `terrain/grass.ron`.
pub const GRASS_FILE_VERSION: u32 = 1;
/// Format version of `entry.ron`.
pub const ENTRY_FILE_VERSION: u32 = 1;

/// Most centerline points a stream file may hold.
pub const MAX_STREAM_POINTS: usize = 100_000;
/// Most points an entry track may hold.
pub const MAX_ENTRY_POINTS: usize = 4096;

/// Which optional features to switch on besides the files that are present.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeatureOptions {
    /// Grow grass with the default [`GrassFile`] even without `terrain/grass.ron` (`--grass`).
    pub grass: bool,
}

/// What the scene turned on, built once at load time.
#[derive(Debug, Default)]
pub struct Features {
    /// The baked grass, when `terrain/grass.ron` exists or [`FeatureOptions::grass`] is set.
    pub grass: Option<GrassPlan>,
    /// The valley mist (grid included), when `sky.ron` has a `mist:` block.
    pub mist: Option<ValleyMist>,
    /// The stream's ribbon, when `terrain/stream.json` exists.
    pub water: Option<WaterBody>,
    /// The entry streak, when `entry.ron` exists.
    pub entry: Option<EntryPlan>,
    /// The armed tunnels, when `tunnels/tunnels.ron` exists.
    pub tunnels: Option<Arc<crate::tunnels::TunnelsPlan>>,
}

impl Features {
    /// The names of the features that are on, in a fixed order (for the load summary and tests).
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.grass.is_some() {
            out.push("grass");
        }
        if self.mist.is_some() {
            out.push("mist");
        }
        if self.water.is_some() {
            out.push("water");
        }
        if self.entry.is_some() {
            out.push("entry");
        }
        if self.tunnels.is_some() {
            out.push("tunnels");
        }
        out
    }
}

/// What [`load_features`] reads besides the files.
pub struct FeatureInputs<'a> {
    /// The scene directory.
    pub dir: &'a Path,
    /// The heightfield.
    pub map: &'a TerrainMap,
    /// The splat layer stack.
    pub layers: &'a TerrainLayers,
    /// The splat weight packs, when the scene has them.
    pub splat: Option<&'a [Rgba8Image; 2]>,
    /// The parsed sky (its `mist:` block).
    pub sky: &'a SkyFile,
}

/// Reads every optional feature of a scene.
///
/// # Errors
/// [`ErrorCode::GrassMalformed`], [`ErrorCode::WaterMalformed`], [`ErrorCode::EntryMalformed`] or a tunnels code
/// ([`crate::tunnels::load_tunnels`]) naming the file at fault.
pub fn load_features(
    inputs: &FeatureInputs<'_>,
    options: &FeatureOptions,
) -> Result<Features, ShowcaseError> {
    let _span = tracing::info_span!("showcase_load_features").entered();
    let grass_path = inputs.dir.join(GRASS_FILE);
    let grass = if grass_path.is_file() {
        let text = read_text(&grass_path, ErrorCode::GrassMalformed)?;
        let file = GrassFile::from_ron(&text, &grass_path)?;
        Some(build_grass(&file, inputs, &grass_path)?)
    } else if options.grass {
        Some(build_grass(&GrassFile::default(), inputs, &grass_path)?)
    } else {
        None
    };
    let mist = inputs.sky.mist.as_ref().map(|spec| {
        let grid = mist_grid_from_map(inputs.map, spec.grid_cell_m, spec.concavity_radius_m).map(Arc::new);
        spec.to_valley_mist(grid)
    });
    let stream_path = inputs.dir.join(STREAM_FILE);
    let water = if stream_path.is_file() {
        let text = read_text(&stream_path, ErrorCode::WaterMalformed)?;
        Some(water_from_stream(&text, &stream_path)?)
    } else {
        None
    };
    let entry_path = inputs.dir.join(ENTRY_FILE);
    let entry = if entry_path.is_file() {
        let text = read_text(&entry_path, ErrorCode::EntryMalformed)?;
        let file = EntryFile::from_ron(&text, &entry_path)?;
        let (lowest, _) = inputs.map.min_max_height();
        Some(file.plan(lowest, &entry_path)?)
    } else {
        None
    };
    let tunnels = crate::tunnels::load_tunnels(inputs.dir, inputs.map)?.map(Arc::new);
    Ok(Features {
        grass,
        mist,
        water,
        entry,
        tunnels,
    })
}

fn read_text(path: &Path, code: ErrorCode) -> Result<String, ShowcaseError> {
    std::fs::read_to_string(path).map_err(|e| ShowcaseError::new(code, path, e.to_string()))
}

// ---------------------------------------------------------------------------------------------
// Grass
// ---------------------------------------------------------------------------------------------

/// The parsed `terrain/grass.ron`.
///
/// ```text
/// Grass(
///     version: 1,
///     dead_grass: "dead_grass",   // the terrain layer the dead stems grow on (pack0 G in the showcase scene)
///     shoots: "shoots",           // the layer the green shoots grow on (pack0 B)
///     texel_m: 2.0,               // mask texel edge, metres
///     seed: 0,
///     set: None,                  // Some((version: 1, ...)) replaces the default winter set
/// )
/// ```
///
/// With `set: None` the host grows one class of dead winter grass ([`GrassClass::winter_dead`]) where the
/// named layers' splat weights are, and the `shoots` layer adds green shoots. With `set: Some(..)` the
/// set is used as written (its layer rules name layers by index) and the two names are not read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename = "Grass", deny_unknown_fields)]
pub struct GrassFile {
    /// Format version ([`GRASS_FILE_VERSION`]).
    pub version: u32,
    /// Name of the layer the dead stems grow on.
    #[serde(default = "default_dead_grass")]
    pub dead_grass: String,
    /// Name of the layer the green shoots grow on.
    #[serde(default = "default_shoots")]
    pub shoots: String,
    /// Mask texel edge, metres (`0.5..=16`).
    #[serde(default = "default_texel")]
    pub texel_m: f32,
    /// Seed of the grass set when the file builds the default one.
    #[serde(default)]
    pub seed: u64,
    /// A whole grass set, replacing the default one.
    #[serde(default)]
    pub set: Option<GrassSet>,
}

fn default_dead_grass() -> String {
    "dead_grass".into()
}

fn default_shoots() -> String {
    "shoots".into()
}

fn default_texel() -> f32 {
    2.0
}

impl Default for GrassFile {
    fn default() -> Self {
        Self {
            version: GRASS_FILE_VERSION,
            dead_grass: default_dead_grass(),
            shoots: default_shoots(),
            texel_m: default_texel(),
            seed: 0,
            set: None,
        }
    }
}

impl GrassFile {
    /// Parses and validates `text`.
    ///
    /// # Errors
    /// [`ErrorCode::GrassMalformed`] naming `path`.
    pub fn from_ron(text: &str, path: &Path) -> Result<Self, ShowcaseError> {
        let bad = |message: String| ShowcaseError::new(ErrorCode::GrassMalformed, path, message);
        let file: GrassFile = ron::from_str(text).map_err(|e| bad(e.to_string()))?;
        if file.version != GRASS_FILE_VERSION {
            return Err(bad(format!(
                "version {} (this build reads {GRASS_FILE_VERSION})",
                file.version
            )));
        }
        if !file.texel_m.is_finite() || !(0.5..=16.0).contains(&file.texel_m) {
            return Err(bad("texel_m must be within 0.5..=16".into()));
        }
        if file.set.is_none() && (file.dead_grass.is_empty() || file.shoots.is_empty()) {
            return Err(bad("dead_grass and shoots must name layers".into()));
        }
        Ok(file)
    }
}

/// A baked grass layer, ready for the GPU.
#[derive(Debug)]
pub struct GrassPlan {
    /// The grass set.
    pub set: GrassSet,
    /// Its mask, baked from the heightfield and the splat packs.
    pub mask: GrassMask,
}

/// The splat weights of the scene's packs as a layer weight map: layer `k` is channel `k % 4` of pack
/// `k / 4` (pack0 R, G, B, A then pack1 R, G, B, A), the terrain draw's own mapping.
///
/// # Errors
/// A message when the packs do not fit a weight map.
pub fn painted_weights(layer_count: usize, splat: &[Rgba8Image; 2]) -> Result<LayerWeightMap, String> {
    let (width, height) = (splat[0].width, splat[0].height);
    let used = layer_count.min(8);
    let mut map = LayerWeightMap::new(width, height, used as u32).map_err(|e| e.to_string())?;
    for layer in 0..used {
        let pack = &splat[layer / 4];
        let channel = layer % 4;
        for row in 0..height {
            for col in 0..width {
                let index = (row as usize * width as usize + col as usize) * 4 + channel;
                let weight = pack.data.get(index).copied().unwrap_or(0);
                map.set(layer as u32, col, row, weight);
            }
        }
    }
    Ok(map)
}

fn layer_index(layers: &TerrainLayers, name: &str) -> Option<u8> {
    layers
        .layers
        .iter()
        .position(|l| l.name == name)
        .and_then(|i| u8::try_from(i).ok())
}

/// The default grass set of a `grass.ron` that names no set: dead winter grass on the `dead_grass` layer,
/// green shoots (shorter, mostly green) on the `shoots` layer.
#[must_use]
pub fn default_grass_set(dead: u8, shoots: u8, seed: u64) -> GrassSet {
    let mut class = GrassClass::winter_dead();
    class.layers = vec![
        GrassLayerRule {
            layer: dead,
            density: 1.0,
            green: 0.0,
            height: 1.0,
        },
        GrassLayerRule {
            layer: shoots,
            density: 0.8,
            green: 0.85,
            height: 0.6,
        },
    ];
    GrassSet {
        seed,
        classes: vec![class],
        ..GrassSet::default()
    }
}

fn build_grass(
    file: &GrassFile,
    inputs: &FeatureInputs<'_>,
    path: &Path,
) -> Result<GrassPlan, ShowcaseError> {
    let _span = tracing::info_span!("showcase_bake_grass").entered();
    let bad = |message: String| ShowcaseError::new(ErrorCode::GrassMalformed, path, message);
    let set = match &file.set {
        Some(set) => set.clone(),
        None => {
            let find = |name: &str| {
                layer_index(inputs.layers, name).ok_or_else(|| {
                    let known: Vec<&str> = inputs.layers.layers.iter().map(|l| l.name.as_str()).collect();
                    bad(format!(
                        "no terrain layer named '{name}' (the layers are {})",
                        known.join(", ")
                    ))
                })
            };
            default_grass_set(find(&file.dead_grass)?, find(&file.shoots)?, file.seed)
        }
    };
    set.validate(Some(inputs.layers.count()))
        .map_err(|e| bad(e.to_string()))?;
    let painted = match inputs.splat {
        Some(splat) => Some(painted_weights(inputs.layers.count(), splat).map_err(bad)?),
        None => None,
    };
    let mask = GrassMask::build(
        &set,
        &GrassMaskInputs {
            layers: Some(inputs.layers),
            painted: painted.as_ref(),
            texel_m: file.texel_m,
            ..GrassMaskInputs::new(inputs.map)
        },
    )
    .map_err(|e| bad(e.to_string()))?;
    Ok(GrassPlan { set, mask })
}

// ---------------------------------------------------------------------------------------------
// Valley mist
// ---------------------------------------------------------------------------------------------

/// The `mist:` block of `sky.ron`: the valley mist's numbers (the defaults are [`ValleyMist::default`]'s)
/// and how its height grid is sampled from the heightfield.
///
/// ```text
/// mist: Some((density: 0.012, max_height_m: 45.0, grid_cell_m: 32.0, concavity_radius_m: 300.0)),
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MistSpec {
    /// Extinction at the ground of a full hollow, 1/metres.
    pub density: f32,
    /// Height over which the density falls by `1/e`, metres.
    pub scale_height_m: f32,
    /// Height where the mist has ended, metres.
    pub max_height_m: f32,
    /// Fraction of the hollow density that open ground keeps, `0..=1`.
    pub open_ground_fraction: f32,
    /// Concavity at which ground counts as a full hollow, metres.
    pub hollow_depth_m: f32,
    /// Size of the biggest noise feature, metres.
    pub noise_scale_m: f32,
    /// How far the noise moves the density, `0..=1`.
    pub noise_strength: f32,
    /// Drift of the noise, metres per second on world X and Z.
    pub wind_m_per_s: (f32, f32),
    /// Steps of the march toward the sun.
    pub sun_steps: u32,
    /// Draw a reduced mist on profiles without volumetric fog.
    pub reduced_on_low_tiers: bool,
    /// Edge of a height-grid cell, metres.
    pub grid_cell_m: f32,
    /// Radius of the window the concavity is measured in, metres.
    pub concavity_radius_m: f32,
}

impl Default for MistSpec {
    fn default() -> Self {
        let mist = ValleyMist::default();
        Self {
            density: mist.density,
            scale_height_m: mist.scale_height_m,
            max_height_m: mist.max_height_m,
            open_ground_fraction: mist.open_ground_fraction,
            hollow_depth_m: mist.hollow_depth_m,
            noise_scale_m: mist.noise_scale_m,
            noise_strength: mist.noise_strength,
            wind_m_per_s: (mist.wind_m_per_s[0], mist.wind_m_per_s[1]),
            sun_steps: mist.sun_steps,
            reduced_on_low_tiers: mist.reduced_on_low_tiers,
            grid_cell_m: 32.0,
            concavity_radius_m: 300.0,
        }
    }
}

impl MistSpec {
    /// Checks every number.
    ///
    /// # Errors
    /// A message naming the field.
    pub fn validate(&self) -> Result<(), String> {
        let numbers = [
            self.density,
            self.scale_height_m,
            self.max_height_m,
            self.open_ground_fraction,
            self.hollow_depth_m,
            self.noise_scale_m,
            self.noise_strength,
            self.wind_m_per_s.0,
            self.wind_m_per_s.1,
            self.grid_cell_m,
            self.concavity_radius_m,
        ];
        if numbers.iter().any(|v| !v.is_finite()) {
            return Err("mist: a number is not finite".into());
        }
        let positive = [
            ("scale_height_m", self.scale_height_m),
            ("max_height_m", self.max_height_m),
            ("hollow_depth_m", self.hollow_depth_m),
            ("noise_scale_m", self.noise_scale_m),
            ("concavity_radius_m", self.concavity_radius_m),
        ];
        for (name, value) in positive {
            if value <= 0.0 {
                return Err(format!("mist.{name} must be > 0"));
            }
        }
        if self.density < 0.0 {
            return Err("mist.density must be >= 0".into());
        }
        if !(0.0..=1.0).contains(&self.open_ground_fraction) || !(0.0..=1.0).contains(&self.noise_strength) {
            return Err("mist.open_ground_fraction and mist.noise_strength must be within 0..=1".into());
        }
        if !(0.5..=1024.0).contains(&self.grid_cell_m) {
            return Err("mist.grid_cell_m must be within 0.5..=1024".into());
        }
        if self.sun_steps > regia_render::MIST_MAX_SUN_STEPS {
            return Err(format!(
                "mist.sun_steps {} is above the limit {}",
                self.sun_steps,
                regia_render::MIST_MAX_SUN_STEPS
            ));
        }
        Ok(())
    }

    /// The engine's mist resource for these numbers over `grid`.
    #[must_use]
    pub fn to_valley_mist(&self, grid: Option<Arc<regia_render::MistHeightGrid>>) -> ValleyMist {
        ValleyMist {
            density: self.density,
            scale_height_m: self.scale_height_m,
            max_height_m: self.max_height_m,
            open_ground_fraction: self.open_ground_fraction,
            hollow_depth_m: self.hollow_depth_m,
            noise_scale_m: self.noise_scale_m,
            noise_strength: self.noise_strength,
            wind_m_per_s: [self.wind_m_per_s.0, self.wind_m_per_s.1],
            sun_steps: self.sun_steps,
            reduced_on_low_tiers: self.reduced_on_low_tiers,
            grid,
            ..ValleyMist::default()
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Water
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct StreamFile {
    points: Vec<StreamPoint>,
}

#[derive(Deserialize)]
struct StreamPoint {
    x: f32,
    z: f32,
    water_y: f32,
    width_m: f32,
    depth_m: f32,
}

/// Builds the stream's ribbon from the text of `terrain/stream.json`: `{ "points": [{ "x", "z",
/// "water_y", "width_m", "depth_m" }, ...] }` from the source downstream (other keys are ignored).
///
/// # Errors
/// [`ErrorCode::WaterMalformed`] naming `path`.
pub fn water_from_stream(text: &str, path: &Path) -> Result<WaterBody, ShowcaseError> {
    let bad = |message: String| ShowcaseError::new(ErrorCode::WaterMalformed, path, message);
    let file: StreamFile = serde_json::from_str(text).map_err(|e| bad(e.to_string()))?;
    if file.points.len() > MAX_STREAM_POINTS {
        return Err(bad(format!(
            "{} points (at most {MAX_STREAM_POINTS})",
            file.points.len()
        )));
    }
    let points = file
        .points
        .iter()
        .map(|p| RibbonPoint {
            position: [p.x, p.water_y, p.z],
            width: p.width_m,
            depth: p.depth_m,
            speed: None,
        })
        .collect();
    let body = WaterBody::ribbon(points, WaterLook::clear_stream());
    body.validate().map_err(|e| bad(e.to_string()))?;
    Ok(body)
}

// ---------------------------------------------------------------------------------------------
// Entry streak
// ---------------------------------------------------------------------------------------------

/// The cost level of the entry streak in `entry.ron`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryTierSpec {
    /// Head, ion trail, fine train, shed pieces.
    #[default]
    Full,
    /// The cheap level for Low, Deck and Mobile.
    Cheap,
}

/// One timed position of `entry.ron`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EntryKey {
    /// Seconds from the start of the streak.
    pub time: f32,
    /// World position, metres, +Y up.
    pub position: (f32, f32, f32),
}

/// The parsed `entry.ron`.
///
/// ```text
/// Entry(
///     version: 1,
///     points: [(time: 0.0, position: (90000.0, 80000.0, 107000.0)), (time: 120.0, position: (700.0, 105.7, 5.0))],
///     start_frame: 0,      // the showcase clock frame at which track time 0 plays
///     tier: Full,
///     shed_count: Some(4), // every profile knob is optional
/// )
/// ```
///
/// The streak runs on the showcase's frame clock: at clock frame `f` it shows track time
/// `(f - start_frame) / 60` seconds (frames before `start_frame` show time 0).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename = "Entry", deny_unknown_fields)]
pub struct EntryFile {
    /// Format version ([`ENTRY_FILE_VERSION`]).
    pub version: u32,
    /// The timed track, times strictly increasing.
    pub points: Vec<EntryKey>,
    /// Clock frame at which track time 0 plays.
    #[serde(default)]
    pub start_frame: i32,
    /// Cost level.
    #[serde(default)]
    pub tier: EntryTierSpec,
    /// World Y altitudes are measured from. Absent: the lowest ground of the map.
    #[serde(default)]
    pub ground_y: Option<f32>,
    /// Seed of the shed pieces and the train jitter.
    #[serde(default)]
    pub seed: Option<u64>,
    /// Head radiance at full heating.
    #[serde(default)]
    pub peak_radiance: Option<f32>,
    /// Radius of the luminous head, metres.
    #[serde(default)]
    pub head_radius_m: Option<f32>,
    /// Radius of the dark body, metres.
    #[serde(default)]
    pub body_radius_m: Option<f32>,
    /// Seconds the ionised trail stays visible.
    #[serde(default)]
    pub ion_life_s: Option<f32>,
    /// Seconds the dust and smoke train lasts.
    #[serde(default)]
    pub train_life_s: Option<f32>,
    /// Wind the train drifts with, metres per second.
    #[serde(default)]
    pub wind: Option<(f32, f32, f32)>,
    /// Altitude at which pieces shed, metres.
    #[serde(default)]
    pub shed_altitude_m: Option<f32>,
    /// Number of shed pieces.
    #[serde(default)]
    pub shed_count: Option<u32>,
    /// Opacity of the faint smoke from on-board damage.
    #[serde(default)]
    pub damage_smoke: Option<f32>,
    /// Smallest apparent width of the head and dark body, pixels.
    #[serde(default)]
    pub min_px: Option<f32>,
}

impl EntryFile {
    /// Parses `text`.
    ///
    /// # Errors
    /// [`ErrorCode::EntryMalformed`] naming `path`.
    pub fn from_ron(text: &str, path: &Path) -> Result<Self, ShowcaseError> {
        let file: EntryFile = ron::from_str(text)
            .map_err(|e| ShowcaseError::new(ErrorCode::EntryMalformed, path, e.to_string()))?;
        Ok(file)
    }

    /// Validates the file and builds the plan. `default_ground_y` is used when the file names none.
    ///
    /// # Errors
    /// [`ErrorCode::EntryMalformed`] naming `path`.
    pub fn plan(&self, default_ground_y: f32, path: &Path) -> Result<EntryPlan, ShowcaseError> {
        let bad = |message: String| ShowcaseError::new(ErrorCode::EntryMalformed, path, message);
        if self.version != ENTRY_FILE_VERSION {
            return Err(bad(format!(
                "version {} (this build reads {ENTRY_FILE_VERSION})",
                self.version
            )));
        }
        if self.points.len() > MAX_ENTRY_POINTS {
            return Err(bad(format!(
                "{} points (at most {MAX_ENTRY_POINTS})",
                self.points.len()
            )));
        }
        let track = EntryTrack::new(
            self.points
                .iter()
                .map(|k| TrackPoint {
                    time: k.time,
                    position: Vec3::new(k.position.0, k.position.1, k.position.2),
                })
                .collect(),
        )
        .map_err(|e| bad(e.to_string()))?;
        let defaults = EntryProfile::default();
        let wind = self.wind.map_or(defaults.wind, |w| Vec3::new(w.0, w.1, w.2));
        let profile = EntryProfile {
            ground_y: self.ground_y.unwrap_or(default_ground_y),
            seed: self.seed.unwrap_or(defaults.seed),
            peak_radiance: self.peak_radiance.unwrap_or(defaults.peak_radiance),
            head_radius_m: self.head_radius_m.unwrap_or(defaults.head_radius_m),
            body_radius_m: self.body_radius_m.unwrap_or(defaults.body_radius_m),
            ion_life_s: self.ion_life_s.unwrap_or(defaults.ion_life_s),
            train_life_s: self.train_life_s.unwrap_or(defaults.train_life_s),
            wind,
            shed_altitude_m: self.shed_altitude_m.unwrap_or(defaults.shed_altitude_m),
            shed_count: self.shed_count.unwrap_or(defaults.shed_count),
            damage_smoke: self.damage_smoke.unwrap_or(defaults.damage_smoke),
            ..defaults
        };
        let finite = [
            profile.ground_y,
            profile.peak_radiance,
            profile.head_radius_m,
            profile.body_radius_m,
            profile.ion_life_s,
            profile.train_life_s,
            profile.wind.x,
            profile.wind.y,
            profile.wind.z,
            profile.shed_altitude_m,
            profile.damage_smoke,
        ];
        if finite.iter().any(|v| !v.is_finite()) {
            return Err(bad("a profile number is not finite".into()));
        }
        let non_negative = [
            ("peak_radiance", profile.peak_radiance),
            ("head_radius_m", profile.head_radius_m),
            ("body_radius_m", profile.body_radius_m),
            ("ion_life_s", profile.ion_life_s),
            ("train_life_s", profile.train_life_s),
            ("damage_smoke", profile.damage_smoke),
        ];
        for (name, value) in non_negative {
            if value < 0.0 {
                return Err(bad(format!("{name} must be >= 0")));
            }
        }
        let min_px = self.min_px.unwrap_or(2.0);
        if !min_px.is_finite() || min_px < 0.0 {
            return Err(bad("min_px must be finite and >= 0".into()));
        }
        Ok(EntryPlan {
            track,
            profile,
            tier: match self.tier {
                EntryTierSpec::Full => EntryTier::Full,
                EntryTierSpec::Cheap => EntryTier::Cheap,
            },
            start_frame: i64::from(self.start_frame),
            min_px,
        })
    }
}

/// An entry streak ready to spawn.
#[derive(Clone, Debug)]
pub struct EntryPlan {
    /// The path of the entering body.
    pub track: EntryTrack,
    /// Look and physics knobs.
    pub profile: EntryProfile,
    /// Cost level.
    pub tier: EntryTier,
    /// Clock frame at which track time 0 plays.
    pub start_frame: i64,
    /// Smallest apparent width of the head, pixels.
    pub min_px: f32,
}

impl EntryPlan {
    /// The component to spawn: at frame 0 and not playing, so the host sets [`Self::streak_frame`] each
    /// fixed step and the streak is a function of the clock alone.
    #[must_use]
    pub fn streak(&self) -> EntryStreak {
        let mut streak = EntryStreak::new(self.track.clone());
        streak.profile = self.profile.clone();
        streak.tier = self.tier;
        streak.min_px = self.min_px;
        streak.playing = false;
        streak.frame = 0;
        streak
    }

    /// The streak's frame counter at showcase clock frame `clock`.
    #[must_use]
    pub fn streak_frame(&self, clock: i64) -> u32 {
        u32::try_from((clock - self.start_frame).max(0)).unwrap_or(u32::MAX)
    }

    /// Track seconds at showcase clock frame `clock`.
    #[must_use]
    pub fn time_at(&self, clock: i64) -> f32 {
        self.streak_frame(clock) as f32 / ENTRY_FRAME_RATE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> &'static Path {
        Path::new("f")
    }

    #[test]
    fn a_grass_file_needs_only_its_version() {
        let file = GrassFile::from_ron("Grass(version: 1)", path()).expect("parses");
        assert_eq!(file, GrassFile::default());
        assert_eq!(file.texel_m, 2.0);
    }

    #[test]
    fn bad_grass_files_have_the_grass_code() {
        for text in [
            "nope",
            "Grass(version: 2)",
            "Grass(version: 1, texel_m: 0.0)",
            "Grass(version: 1, texel_m: 99.0)",
            "Grass(version: 1, dead_grass: \"\")",
            "Grass(version: 1, extra: 1)",
        ] {
            let err = GrassFile::from_ron(text, path()).expect_err(text);
            assert_eq!(err.code, ErrorCode::GrassMalformed, "{text}");
        }
    }

    #[test]
    fn the_default_set_is_valid_and_grows_on_two_layers() {
        let set = default_grass_set(1, 2, 7);
        set.validate(Some(8)).expect("valid");
        assert_eq!(set.classes.len(), 1);
        assert_eq!(set.classes[0].layers.len(), 2);
        assert_eq!(set.seed, 7);
        assert!(
            set.validate(Some(2)).is_err(),
            "layer 2 is out of range for 2 layers"
        );
    }

    #[test]
    fn splat_channels_become_layer_planes() {
        let mut p0 = Rgba8Image {
            width: 2,
            height: 2,
            data: vec![0; 16],
        };
        let mut p1 = p0.clone();
        // Texel (1, 0): pack0 G = 200, pack1 B = 90.
        p0.data[4 + 1] = 200;
        p1.data[4 + 2] = 90;
        let map = painted_weights(8, &[p0, p1]).expect("map");
        assert_eq!((map.width(), map.height(), map.layers()), (2, 2, 8));
        assert_eq!(map.get(1, 1, 0), 200, "layer 1 is pack0 G");
        assert_eq!(map.get(6, 1, 0), 90, "layer 6 is pack1 B");
        assert_eq!(map.get(1, 0, 0), 0);
    }

    #[test]
    fn mist_defaults_match_the_engine_and_bad_numbers_are_refused() {
        let spec = MistSpec::default();
        spec.validate().expect("valid");
        assert_eq!(spec.to_valley_mist(None), ValleyMist::default());
        for bad in [
            MistSpec {
                density: -1.0,
                ..MistSpec::default()
            },
            MistSpec {
                noise_strength: 2.0,
                ..MistSpec::default()
            },
            MistSpec {
                grid_cell_m: 0.0,
                ..MistSpec::default()
            },
            MistSpec {
                max_height_m: f32::NAN,
                ..MistSpec::default()
            },
            MistSpec {
                sun_steps: 1000,
                ..MistSpec::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    const STREAM: &str = r#"{"flows":"west","points":[
        {"x":10.0,"z":0.0,"water_y":5.0,"width_m":1.4,"depth_m":0.12},
        {"x":8.0,"z":0.1,"water_y":4.9,"width_m":1.5,"depth_m":0.13}]}"#;

    #[test]
    fn a_stream_becomes_a_ribbon_in_source_order() {
        let body = water_from_stream(STREAM, path()).expect("ribbon");
        let regia_terrain::WaterShape::Ribbon { points } = &body.shape else {
            panic!("a ribbon");
        };
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].position, [10.0, 5.0, 0.0]);
        assert_eq!((points[1].width, points[1].depth), (1.5, 0.13));
        assert_eq!(points[0].speed, None);
    }

    #[test]
    fn bad_streams_have_the_water_code() {
        for text in [
            "nope",
            r#"{"points":[]}"#,
            r#"{"points":[{"x":0,"z":0,"water_y":0,"width_m":1,"depth_m":1}]}"#,
            r#"{"points":[{"x":0,"z":0,"water_y":0,"width_m":1,"depth_m":1},{"x":0,"z":0,"water_y":0,"width_m":1,"depth_m":1}]}"#,
            r#"{"points":[{"x":0,"z":0,"water_y":0,"width_m":-1,"depth_m":1},{"x":3,"z":0,"water_y":0,"width_m":1,"depth_m":1}]}"#,
            r#"{"points":[{"x":0,"z":0}]}"#,
        ] {
            let err = water_from_stream(text, path()).expect_err(text);
            assert_eq!(err.code, ErrorCode::WaterMalformed, "{text}");
        }
    }

    const ENTRY: &str = r#"Entry(
        version: 1,
        points: [(time: 0.0, position: (0.0, 80000.0, 0.0)), (time: 10.0, position: (500.0, 100.0, 5.0))],
        start_frame: 30,
        tier: Cheap,
        shed_count: Some(2),
    )"#;

    #[test]
    fn an_entry_file_runs_on_the_frame_clock() {
        let file = EntryFile::from_ron(ENTRY, path()).expect("parses");
        let plan = file.plan(100.0, path()).expect("plan");
        assert_eq!(plan.tier, EntryTier::Cheap);
        assert_eq!(plan.profile.shed_count, 2);
        assert_eq!(plan.profile.ground_y, 100.0, "the map's lowest ground by default");
        assert_eq!(plan.streak_frame(-60), 0);
        assert_eq!(plan.streak_frame(30), 0);
        assert_eq!(plan.streak_frame(90), 60);
        assert_eq!(plan.time_at(90), 1.0);
        let streak = plan.streak();
        assert!(!streak.playing, "the clock drives the frame");
        assert_eq!(streak.frame, 0);
    }

    #[test]
    fn bad_entry_files_have_the_entry_code() {
        let one_point = ENTRY.replace(", (time: 10.0, position: (500.0, 100.0, 5.0))", "");
        let backwards = ENTRY.replace("time: 10.0", "time: -1.0");
        let nan = ENTRY.replace("80000.0", "inf");
        let wrong_version = ENTRY.replace("version: 1", "version: 9");
        let negative = ENTRY.replace("shed_count: Some(2)", "head_radius_m: Some(-1.0)");
        for text in [
            "nope".to_string(),
            one_point,
            backwards,
            nan,
            wrong_version,
            negative,
        ] {
            let result = EntryFile::from_ron(&text, path()).and_then(|f| f.plan(0.0, path()));
            let err = result.expect_err(&text);
            assert_eq!(err.code, ErrorCode::EntryMalformed, "{text}");
        }
    }
}
