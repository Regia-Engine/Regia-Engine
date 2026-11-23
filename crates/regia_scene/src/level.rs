// SPDX-License-Identifier: MPL-2.0
//! Levels: settings, the load/unload lifecycle and streaming basics
//! (`docs/specs/14-scene-and-framework.md` A4).
//!
//! A [`Level`] is a scene file plus [`LevelSettings`] (kept in the file's `resources` map under
//! [`LEVEL_SETTINGS_PATH`], `regia_scene::level::LevelSettings`). [`LevelManager`] opens levels
//! `Replace` (the normal one-level world) or `Additive` (the primitive world streaming builds on:
//! open and unload per region).
//! Every spawned entity carries [`LevelMember`] unless created [`Persistent`], so a `Replace`
//! open or an `unload_level` can despawn exactly the old level.
//!
//! Data arrives through the [`LevelLibrary`] resource — the seam the asset server (`specs/12`
//! § 3) fills when a `.level.ron` finishes loading. A level opened before its data exists sits in
//! `Loading` state and completes on a later frame; the lifecycle messages
//! ([`LevelLoading`], [`LevelLoaded`], [`LevelUnloading`]) fire on the transitions, and the
//! `OnLevelLoaded` / `OnLevelUnloading` schedules run.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::components::EntityGuid;
use crate::format::{SceneFile, SceneFormatError};
use crate::spawn::{SceneSpawnError, SnapshotFilter, SpawnOptions};

/// Identity of a loaded level: the level asset's id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct LevelId(pub Guid);

impl std::fmt::Display for LevelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Tags entities that belong to a level, so unloading despawns exactly them. `None`-safe: the
/// raw id is a `u128` (reflection has no `Guid` yet); use [`LevelMember::level`].
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[reflect(Component, Default)]
pub struct LevelMember(pub u128);

impl LevelMember {
    /// Tags membership of `level`.
    pub fn from_level(level: LevelId) -> Self {
        Self(level.0.as_u128())
    }

    /// The owning level.
    pub fn level(&self) -> LevelId {
        LevelId(Guid::from_u128(self.0))
    }
}

/// The root of the loading-screen prefab instance the [`LevelManager`] shows while a `Replace` load
/// waits for its data (`specs/14` A4, ADR-7263). It is not a [`LevelMember`]: the unload of the old
/// level leaves it, and the manager despawns it when no `Replace` load is pending (the load
/// completed, failed or was unloaded). `regia_framework::loading_screen` keeps
/// [`LoadingScreen::progress`] up to date; the prefab's own systems or scripts read it.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct LoadingScreen {
    /// The level being loaded (the newest pending `Replace` load).
    pub level: LevelId,
    /// Its load progress shown so far, `0..1`; never goes down while `level` stays the same.
    pub progress: f32,
}

/// Entities with this marker survive level transitions (`specs/14` A4).
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[reflect(Component, Default)]
pub struct Persistent;

/// Per-level rules and environment (`docs/specs/14-scene-and-framework.md` A4). Saved in the
/// level file's `resources` map as `regia_scene::level::LevelSettings` ([`LEVEL_SETTINGS_PATH`]); inserting it as a world
/// resource is part of every level load.
///
/// # Example
///
/// ```
/// use regia_scene::level::LevelSettings;
///
/// let settings = LevelSettings::default();
/// assert_eq!(settings.gravity.y, -9.81); // m/s², standard earth gravity
/// let text = settings.to_ron().expect("serialises to canonical RON");
/// let back = LevelSettings::from_ron(&text).expect("parses");
/// assert_eq!(settings, back);
/// ```
///
/// `Serialize` is written by hand (ADR-7232): a text format (RON) omits `terrain` when it is
/// `None`, so level files save as before; a binary format (postcard, used by cooked levels, saves
/// and snapshots) writes every field, because it is positional and could not read a skipped one.
#[derive(Resource, Reflect, Clone, Debug, PartialEq, Deserialize)]
#[reflect(Resource, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelSettings {
    /// Gravity for physics and character movement, m/s². Default: standard earth gravity.
    pub gravity: Vec3,
    /// Sky/environment asset (`asset:<guid>`), if any.
    pub environment: Option<String>,
    /// Registered name of the level's `GameRules` implementation (`specs/14` B3). Resolved at level
    /// load through `regia_framework`'s `GameRulesRegistry`; an unknown name logs a warning and
    /// falls back to `DefaultRules`. `None` keeps `DefaultRules`.
    pub game_rules: Option<String>,
    /// Ambient audio bed asset (`asset:<guid>`).
    pub ambient_audio: Option<String>,
    /// Whether additive streaming may load further levels into this one's slots (Tier 2,
    /// `specs/22` T2.11).
    pub streaming_enabled: bool,
    /// The planet's sky: atmosphere preset, sun clock, clouds and a body in the sky
    /// (scene format v2; v1 levels load as today's Earth — the published baseline).
    #[serde(default = "LevelAtmosphere::today")]
    pub atmosphere: LevelAtmosphere,
    /// The map's exposure meter, colour grade and LUT (scene format v3, LUT v4; older
    /// levels load with the engine's default look).
    pub look: LevelLook,
    /// Overrides of the acoustic medium the atmosphere preset implies (scene format v6;
    /// every field `None` = the preset's air and ground, `regia_audio::AudioMedium`).
    pub audio_medium: LevelAudioMedium,
    /// The level's terrain (scene format v12, ADR-2411): the layer file its heightfield is
    /// cooked from, the world mapping and the map name its side files are named after. The
    /// editor and `regia_terrain::level_terrain` build the terrain from it. `None` (the default,
    /// and every migrated v11 level) names no terrain: the host supplies one, and the side files
    /// keep the level's own name, as before v12. Not written to text when `None`.
    pub terrain: Option<LevelTerrain>,
}

impl Serialize for LevelSettings {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        // Destructured so a new field fails to compile here until it is written.
        let Self {
            gravity,
            environment,
            game_rules,
            ambient_audio,
            streaming_enabled,
            atmosphere,
            look,
            audio_medium,
            terrain,
        } = self;
        let skip_none = serializer.is_human_readable();
        let skip_terrain = skip_none && terrain.is_none();
        let mut s = serializer.serialize_struct("LevelSettings", 9 - usize::from(skip_terrain))?;
        s.serialize_field("gravity", gravity)?;
        s.serialize_field("environment", environment)?;
        s.serialize_field("game_rules", game_rules)?;
        s.serialize_field("ambient_audio", ambient_audio)?;
        s.serialize_field("streaming_enabled", streaming_enabled)?;
        s.serialize_field("atmosphere", atmosphere)?;
        s.serialize_field("look", look)?;
        s.serialize_field("audio_medium", audio_medium)?;
        if skip_terrain {
            s.skip_field("terrain")?;
        } else {
            s.serialize_field("terrain", terrain)?;
        }
        s.end()
    }
}

/// The terrain a level names (scene format v12, ADR-2411, `docs/formats/scene-files.md`).
///
/// Paths are project-relative under `assets/` with `/` separators, no `..` and no root, like
/// every other path a level names.
///
/// # Example
///
/// ```
/// use regia_scene::level::{LevelSettings, LevelTerrain};
///
/// let mut settings = LevelSettings::default();
/// settings.terrain = Some(LevelTerrain {
///     layers: "maps/arena/terrain.layers.ron".into(),
///     map: "maps/arena".into(),
///     ..LevelTerrain::default()
/// });
/// let back = LevelSettings::from_ron(&settings.to_ron().expect("ron")).expect("parses");
/// assert_eq!(back.terrain, settings.terrain);
/// assert_eq!(back.terrain.as_ref().map(|t| t.height_scale_m), Some(256.0));
/// ```
#[derive(Reflect, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelTerrain {
    /// The terrain's `.terrain.layers.ron`: its layer stack, and (by the cook convention,
    /// `docs/formats/terrain.md` § 9) the map id its cooked heightfield derives from. Empty: no
    /// heightfield is named, only the side files (`map`).
    pub layers: String,
    /// The map the terrain's side files are named after: `<map>.holes.ron`, `<map>.weights.bin`,
    /// `<map>.scatterpaint.bin` and `<map>.grassdensity.bin`. Empty: the level file's own path
    /// (the pre-v12 convention, so a migrated level keeps its files).
    pub map: String,
    /// World width of the heightfield, metres. `None`: the cooked terrain index's width (the
    /// layer file's `heightfield.json` `size_m`), else one metre per sample.
    pub world_width_m: Option<f32>,
    /// Metres between the lowest and the highest 16-bit sample (`TerrainMap::new`'s `scale`).
    pub height_scale_m: f32,
    /// Height of sample 0, metres.
    pub height_offset_m: f32,
    /// Where the samples sit in the map's extent (scene format v14, ADR-10079):
    /// [`TerrainSampleLayout::VertexCentered`] (the default, every earlier level) puts the
    /// first and last samples on the edges; [`TerrainSampleLayout::CellCentered`] puts them at
    /// the centers of `n` equal cells across `world_width_m`.
    pub sample_layout: TerrainSampleLayout,
}

impl Default for LevelTerrain {
    fn default() -> Self {
        Self {
            layers: String::new(),
            map: String::new(),
            world_width_m: None,
            height_scale_m: 256.0,
            height_offset_m: 0.0,
            sample_layout: TerrainSampleLayout::VertexCentered,
        }
    }
}

/// Where a terrain's `n` samples per axis sit in its world extent `W` (scene format v14,
/// ADR-10079; `regia_terrain::sampler::TerrainMap::with_sample_layout`).
///
/// ```
/// use regia_scene::level::TerrainSampleLayout;
///
/// // 4 samples across 8 m.
/// assert_eq!(TerrainSampleLayout::VertexCentered.sample_position(0, 4, 8.0), -4.0);
/// assert_eq!(TerrainSampleLayout::CellCentered.sample_position(0, 4, 8.0), -3.0);
/// assert_eq!(TerrainSampleLayout::CellCentered.sample_span(4, 8.0), 6.0);
/// ```
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TerrainSampleLayout {
    /// Sample `i` at `i / (n − 1) · W − W / 2`: the first and last samples lie on the edges
    /// (spacing `W / (n − 1)`). Every level before v14.
    #[default]
    VertexCentered,
    /// Sample `i` at `(i + 0.5) · W / n − W / 2`: the extent is `n` equal cells and each sample
    /// sits at its cell's center (spacing `W / n`), as a cell-registered ("pixel is area")
    /// raster or a cell-spaced simulation grid stores heights.
    CellCentered,
}

impl TerrainSampleLayout {
    /// The distance from the first to the last of `n` samples across an extent `extent`
    /// metres wide: `extent` for [`Self::VertexCentered`], `extent · (n − 1) / n` for
    /// [`Self::CellCentered`] (computed in f64, one rounding).
    #[must_use]
    pub fn sample_span(self, n: u32, extent: f32) -> f32 {
        match self {
            Self::VertexCentered => extent,
            Self::CellCentered => {
                let n = f64::from(n.max(2));
                (f64::from(extent) * (n - 1.0) / n) as f32
            }
        }
    }

    /// World coordinate of sample `i` of `n` across an extent `extent` metres wide centered on
    /// 0 (computed in f64, one rounding). The reference the runtime map is tested against.
    #[must_use]
    pub fn sample_position(self, i: u32, n: u32, extent: f32) -> f32 {
        let (i, n, w) = (f64::from(i), f64::from(n.max(2)), f64::from(extent));
        match self {
            Self::VertexCentered => (i / (n - 1.0) * w - 0.5 * w) as f32,
            Self::CellCentered => ((i + 0.5) * w / n - 0.5 * w) as f32,
        }
    }

    /// The RON / JSON spelling (`VertexCentered`, `CellCentered`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VertexCentered => "VertexCentered",
            Self::CellCentered => "CellCentered",
        }
    }
}

impl LevelTerrain {
    /// The map path the side files are named after, relative to the project's `assets/`:
    /// [`Self::map`], or the level's own path when it is empty. `level` is the level file's path
    /// relative to `assets/` (`maps/arena.level.ron`).
    #[must_use]
    pub fn map_for(&self, level: &str) -> String {
        if self.map.is_empty() {
            level.to_owned()
        } else {
            self.map.clone()
        }
    }

    /// Checks the paths and numbers: paths relative, `/`-separated, without `..`, a drive or a
    /// root; a finite positive width; a finite, positive scale within 1e6 m (above it the f32
    /// sample quantum is meaningless and the derived mips' four-tap sums can overflow); a
    /// finite offset. Returns the first problem.
    ///
    /// # Errors
    ///
    /// A message naming the field.
    pub fn validate(&self) -> Result<(), String> {
        for (name, path) in [("layers", &self.layers), ("map", &self.map)] {
            if path.is_empty() {
                continue;
            }
            let bad = path.starts_with('/')
                || path.contains('\\')
                || path.contains(':')
                || path
                    .split('/')
                    .any(|part| part == ".." || part == "." || part.is_empty());
            if bad {
                return Err(format!(
                    "terrain.{name} `{path}` must be relative to assets/ without `..`"
                ));
            }
        }
        let file = self.layers.rsplit('/').next().unwrap_or_default();
        if !self.layers.is_empty() && file != "terrain.layers.ron" && !file.ends_with(".terrain.layers.ron") {
            return Err(format!(
                "terrain.layers `{}` must name a .terrain.layers.ron",
                self.layers
            ));
        }
        if let Some(w) = self.world_width_m
            && !(w.is_finite() && w > 0.0)
        {
            return Err(format!("terrain.world_width_m {w} must be finite and above 0"));
        }
        // The upper bound is 1e6 m: the height scale maps 16-bit samples to metres, and above
        // it the f32 sample quantum (scale / 65535, ≈ 15 m per step at the bound) swamps any
        // real relief, while a scale near f32::MAX overflows the f32 four-tap sums the
        // derived height mips accumulate (heights never reach the GPU there). The largest
        // authored terrains are a few kilometres tall; 1e6 m leaves three orders of
        // magnitude of headroom.
        if !(self.height_scale_m.is_finite() && self.height_scale_m > 0.0 && self.height_scale_m <= 1.0e6) {
            return Err(format!(
                "terrain.height_scale_m {} must be finite and within 0..=1e6",
                self.height_scale_m
            ));
        }
        if !self.height_offset_m.is_finite() {
            return Err(format!(
                "terrain.height_offset_m {} must be finite",
                self.height_offset_m
            ));
        }
        Ok(())
    }
}

/// Per-level overrides of how sound propagates (`docs/specs/16-audio.md` § 3c). The atmosphere
/// preset already picks the medium (Earth air, Mars CO₂, airless); these only refine it.
#[derive(Reflect, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelAudioMedium {
    /// Force the airborne path on or off. `None` follows the preset (off when airless).
    pub airborne: Option<bool>,
    /// Speed of sound in the air, m/s. `None` = the preset's (343 Earth, 240 Mars).
    pub speed_of_sound_mps: Option<f32>,
    /// Level of airborne arrivals relative to Earth air for the same source, dB. `None` = the
    /// preset's (0 Earth, −20 Mars).
    pub level_offset_db: Option<f32>,
    /// Air absorption `k` in `α(f) = k·f²`, dB per metre per kHz². `None` = the preset's.
    pub absorption_db_per_m_khz2: Option<f32>,
    /// Default ground material sound travels through (`soil`, `bedrock`, `ice`, `regolith`,
    /// `mars_regolith`, …; `regia_audio::GroundMaterial::presets`). `None` = the preset's.
    pub ground: Option<String>,
}

/// Which planet's air the renderer integrates for this map (`docs/decisions.md`, planet presets).
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AtmospherePreset {
    /// Earth: Rayleigh, Mie and ozone (blue sky, orange sunsets, aerial haze).
    #[default]
    Earth,
    /// Mars: thin CO₂ and dust (butterscotch day sky, blue sunset glow, dust haze).
    Mars,
    /// No atmosphere (Europa, the Moon): black sky, stars, hard sun, no haze or fog.
    Airless,
}

/// Local solar time and latitude that place the sun (`regia_render::TimeOfDay`).
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelTimeOfDay {
    /// Local solar hour, 0–24.
    pub hour: f32,
    /// Day of the year, 1–365.
    pub day_of_year: u16,
    /// Map latitude in degrees, north positive.
    pub latitude_deg: f32,
}

impl Default for LevelTimeOfDay {
    fn default() -> Self {
        Self {
            hour: 12.0,
            day_of_year: 172,
            latitude_deg: 40.0,
        }
    }
}

/// How a map meters exposure and grades colour (`docs/specs/13-renderer.md` § 5 step 12).
///
/// Every field that is `None` keeps the view's own post settings, so a level that says
/// nothing looks like the engine default. `exposure_bias_ev` adds to the view's
/// compensation. The runtime applies the block to every view's post stack.
///
/// # Example
///
/// ```
/// use regia_scene::level::{LevelGrade, LevelLook, LevelMeter};
///
/// // A high-key dusty desert: half a stop up, centre-weighted, warm, softer toe.
/// let look = LevelLook {
///     exposure_bias_ev: 0.5,
///     meter: Some(LevelMeter { center_weight: 0.6, ..LevelMeter::default() }),
///     grade: Some(LevelGrade { white_balance_kelvin: 5900.0, ..LevelGrade::default() }),
///     ..LevelLook::default()
/// };
/// assert!(look.grade.is_some());
/// ```
///
/// `Serialize` is written by hand (ADR-7232): text omits `lightmap` and `captures` when they are
/// `None`; a binary format writes every field (see [`LevelSettings`]).
#[derive(Reflect, Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct LevelLook {
    /// EV added to the metered (or fixed) exposure; positive brightens.
    pub exposure_bias_ev: f32,
    /// Lowest EV100 auto exposure may pick. `None` keeps the view's.
    pub min_ev100: Option<f32>,
    /// Highest EV100 auto exposure may pick (a sunlit map's ceiling). `None` keeps the view's.
    pub max_ev100: Option<f32>,
    /// What the auto-exposure meter reads. `None` keeps the view's.
    pub meter: Option<LevelMeter>,
    /// Display contrast of the look (S-curve around mid grey). `None` keeps the view's.
    pub contrast: Option<f32>,
    /// Display saturation of the look. `None` keeps the view's.
    pub saturation: Option<f32>,
    /// White balance and lift / gamma / gain. `None` keeps the view's (neutral by default).
    pub grade: Option<LevelGrade>,
    /// A 3D colour LUT applied to the display image after the grade (scene format v4):
    /// a `.cube` file or a PNG strip (N slices of N×N side by side), project-relative.
    /// `None` is the identity.
    pub lut: Option<String>,
    /// Local exposure (scene format v9): lifts shadows and holds highlights region by region,
    /// keeping their texture, instead of one exposure for the whole frame. `None` (the
    /// default) is off.
    pub local_exposure: Option<LevelLocalExposure>,
    /// The map's baked GI volume (scene format v10): the framework loads the `.fgiv` when the
    /// level loads and installs it in the renderer (`book/agents/baked-gi-maps.md`). `None`
    /// (the default) names no bake.
    pub baked_gi: Option<LevelBakedGi>,
    /// The map's lightmap (scene format v11, ADR-2313): the framework loads the `.flmap` when the
    /// level opens, attaches every keyed mesh part of the level as a receiver and asks the
    /// renderer to warm the lightmap variants (`book/agents/lightmaps.md`). `None` (the default)
    /// names no lightmap, and is not written to text, so a level without one saves as before.
    pub lightmap: Option<LevelLightmap>,
    /// The map's local reflection capture bake (scene format v13, ADR-2860): the framework reads
    /// the `.fcap` when the level opens and the renderer installs each capture's map with no face
    /// rendered (`book/agents/reflection-captures.md`). `None` (the default) names no bake, and is
    /// not written to text, so a level without one saves as before.
    pub captures: Option<LevelCaptures>,
}

impl Serialize for LevelLook {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        // Destructured so a new field fails to compile here until it is written.
        let Self {
            exposure_bias_ev,
            min_ev100,
            max_ev100,
            meter,
            contrast,
            saturation,
            grade,
            lut,
            local_exposure,
            baked_gi,
            lightmap,
            captures,
        } = self;
        let skip_none = serializer.is_human_readable();
        let skip_lightmap = skip_none && lightmap.is_none();
        let skip_captures = skip_none && captures.is_none();
        let len = 12 - usize::from(skip_lightmap) - usize::from(skip_captures);
        let mut s = serializer.serialize_struct("LevelLook", len)?;
        s.serialize_field("exposure_bias_ev", exposure_bias_ev)?;
        s.serialize_field("min_ev100", min_ev100)?;
        s.serialize_field("max_ev100", max_ev100)?;
        s.serialize_field("meter", meter)?;
        s.serialize_field("contrast", contrast)?;
        s.serialize_field("saturation", saturation)?;
        s.serialize_field("grade", grade)?;
        s.serialize_field("lut", lut)?;
        s.serialize_field("local_exposure", local_exposure)?;
        s.serialize_field("baked_gi", baked_gi)?;
        if skip_lightmap {
            s.skip_field("lightmap")?;
        } else {
            s.serialize_field("lightmap", lightmap)?;
        }
        if skip_captures {
            s.skip_field("captures")?;
        } else {
            s.serialize_field("captures", captures)?;
        }
        s.end()
    }
}

/// A map's local reflection capture bake (`docs/formats/fcap.md`, ADR-2860). Written by
/// `regia bake-captures --file maps/hall.fcap` (optional `--no-level`) and then `level.save`.
///
/// # Example
///
/// ```
/// use regia_scene::level::{LevelCaptures, LevelSettings};
///
/// let mut settings = LevelSettings::default();
/// settings.look.captures = Some(LevelCaptures { file: "maps/hall.fcap".into() });
/// let back = LevelSettings::from_ron(&settings.to_ron().expect("ron")).expect("parses");
/// assert_eq!(back.look.captures, settings.look.captures);
/// ```
#[derive(Reflect, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelCaptures {
    /// Asset path of the bake, project-relative under `assets/` (`maps/hall.fcap`). An empty path
    /// names none.
    pub file: String,
}

/// A map's lightmap: which `.flmap` to load and the per-map overrides of the renderer's lightmap
/// settings (`docs/formats/lightmap.md`, ADR-2313). Written by `regia bake-lightmaps --scene
/// <level> --write-level`.
///
/// # Example
///
/// ```
/// use regia_scene::level::{LevelLightmap, LevelSettings};
///
/// let mut settings = LevelSettings::default();
/// settings.look.lightmap = Some(LevelLightmap {
///     file: "maps/yard.flmap".into(),
///     ..LevelLightmap::default()
/// });
/// let back = LevelSettings::from_ron(&settings.to_ron().expect("ron")).expect("parses");
/// assert_eq!(back.look.lightmap, settings.look.lightmap);
/// ```
#[derive(Reflect, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelLightmap {
    /// Asset path of the lightmap, project-relative under `assets/` (`maps/yard.flmap`). An empty
    /// path names none.
    pub file: String,
    /// Whether receivers shade with it while the level is open. `None` turns lightmaps on (the
    /// level asked for its lightmap); `Some(false)` loads it but keeps the probe light. The host's
    /// flag comes back when the level closes.
    pub enabled: Option<bool>,
    /// Multiplier on the baked irradiance; `None` keeps the renderer's.
    pub intensity: Option<f32>,
}

/// A map's baked GI volume: which `.fgiv` to load and the per-map overrides of the renderer's
/// baked-GI settings (`docs/formats/baked-gi.md`, ADR-207).
#[derive(Reflect, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelBakedGi {
    /// Asset path of the volume, project-relative under `assets/` (`maps/bore_twelve.fgiv`).
    /// An empty path names no bake.
    pub volume: String,
    /// Per-map overrides of the renderer's baked-GI settings; `None` keeps the renderer's.
    pub settings: LevelBakedGiSettings,
}

/// The baked-GI settings a map may override. Every `None` keeps the renderer's default
/// (`regia_render::BakedGiSettings`); the others (biases, sun and fill following) stay with
/// the host.
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelBakedGiSettings {
    /// Multiplier on the whole baked irradiance.
    pub intensity: Option<f32>,
    /// The sky component's light relative to the bake (1 = the sky it was baked under).
    pub sky_scale: Option<f32>,
    /// The emissive component's light relative to the bake.
    pub emissive_scale: Option<f32>,
    /// Whether the baked fill replaces the live sky-body fill where the volume covers.
    pub replace_live_fill: Option<bool>,
}

/// Local exposure: an edge-aware tone adjustment before the tonemap. The image's local
/// brightness (an edge-preserving blur of its log luminance, a bilateral grid) is pulled
/// toward middle grey by the contrast scales; the detail on top of it is kept. A dark hull
/// beside sunlit snow lifts as a whole and keeps its panel lines.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelLocalExposure {
    /// Contrast of regions darker than middle grey, 0–1: 1 leaves them, 0.75 lifts a region
    /// 4 EV under middle grey by 1 EV.
    pub shadow_contrast: f32,
    /// Contrast of regions brighter than middle grey, 0–1: 1 leaves them, lower holds them
    /// down toward middle grey.
    pub highlight_contrast: f32,
    /// Local detail kept on top of the adjusted regions: 1 keeps it, above 1 sharpens texture.
    pub detail_strength: f32,
}

impl Default for LevelLocalExposure {
    fn default() -> Self {
        Self {
            shadow_contrast: 1.0,
            highlight_contrast: 1.0,
            detail_strength: 1.0,
        }
    }
}

/// The auto-exposure meter: which pixels it averages and where it looks.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelMeter {
    /// Fraction of the darkest pixels ignored (0–1).
    pub low_percentile: f32,
    /// Fraction of pixels, from the dark end, above which the brightest are ignored (0–1).
    pub high_percentile: f32,
    /// 0 meters the whole frame evenly; 1 weights the centre of the frame up to 9×.
    pub center_weight: f32,
    /// Bright-subject (snow) compensation threshold, cd/m² (scene format v7): a metered pixel
    /// from this luminance up to 16× it counts as bright subject, such as sunlit snow or ice.
    /// `0` (the default) turns the compensation off.
    pub bright_threshold_cd_m2: f32,
    /// EV added (brighter) when bright subject fills the metered frame, scaled by its
    /// coverage (none below 25 %, all above 60 %): the photographer's +1 EV for snow, applied
    /// only as far as the frame is snow. `0` by default.
    pub bright_bias_ev: f32,
}

impl Default for LevelMeter {
    fn default() -> Self {
        Self {
            low_percentile: 0.40,
            high_percentile: 0.95,
            center_weight: 0.0,
            bright_threshold_cd_m2: 0.0,
            bright_bias_ev: 0.0,
        }
    }
}

/// White balance before the tonemap; lift / gamma / gain on the display image after it.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelGrade {
    /// Colour temperature the camera is balanced for, kelvin. 6500 is neutral (D65);
    /// a lower value cools the image, a higher one warms it.
    pub white_balance_kelvin: f32,
    /// Green–magenta tint, −1..1 (positive is magenta).
    pub tint: f32,
    /// Added to the shadows, per channel (display-referred; 0 is neutral).
    pub lift: [f32; 3],
    /// Mid-tone power, per channel (1 is neutral; above 1 brightens).
    pub gamma: [f32; 3],
    /// Highlight multiplier, per channel (1 is neutral).
    pub gain: [f32; 3],
}

impl Default for LevelGrade {
    fn default() -> Self {
        Self {
            white_balance_kelvin: 6500.0,
            tint: 0.0,
            lift: [0.0; 3],
            gamma: [1.0; 3],
            gain: [1.0; 3],
        }
    }
}

/// A layer of dust or mist near the ground, drawn as froxel fog on Desktop High and Ultra.
///
/// On a planet with non-Earth air the layer scatters like the planet's aerosol (Mars dust
/// is butterscotch). A `VolumetricFogSettings` resource, when a game inserts one, wins.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelHaze {
    /// Extinction at `base_height_m`, 1/metres (visibility ≈ 3.9 / density).
    pub density: f32,
    /// Height over which the density falls by e, metres.
    pub scale_height_m: f32,
    /// World Y of the map's floor, where `density` applies, metres.
    pub base_height_m: f32,
}

impl Default for LevelHaze {
    fn default() -> Self {
        Self {
            density: 0.0,
            scale_height_m: 200.0,
            base_height_m: 0.0,
        }
    }
}

/// A lit sphere drawn in the sky: Jupiter from Europa, a moon from a planet.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SkyBody {
    /// Direction from the viewer toward the body's centre (world +Y up). Need not be unit length.
    pub direction: [f32; 3],
    /// Apparent diameter in degrees (Jupiter from Europa ≈ 12°).
    pub angular_diameter_deg: f32,
    /// Linear albedo tint.
    pub tint: [f32; 3],
    /// Gas-giant latitude band strength, 0 (plain disc) to 1.
    pub bands: f32,
    /// Planet-shine: the shadowless fill light the body throws on the ground (Jupiter-shine
    /// on Europa), lux on a surface facing it. `None` takes the physical value at the body's
    /// current phase ([`Self::planet_shine_lux`]); `Some` is an authored value used as is.
    pub fill_lux: Option<f32>,
}

impl Default for SkyBody {
    fn default() -> Self {
        Self::jupiter_from_europa()
    }
}

impl SkyBody {
    /// Jupiter from Europa as the DO27 reference frames show it: 14° across (300 of 1080
    /// rows through a 90° horizontal lens, a third of the way to the corner; the physical
    /// size is 12.2°, [`Self::physical_diameter_deg`]), banded, warm albedo.
    #[must_use]
    pub fn jupiter_from_europa() -> Self {
        Self {
            direction: [-0.55, 0.42, -0.72],
            angular_diameter_deg: 14.0,
            tint: [0.62, 0.55, 0.47],
            bands: 1.0,
            fill_lux: None,
        }
    }

    /// Io seen from Europa beside Jupiter, at the reference frames' scale: a plain
    /// pale-yellow disc 3° across (physically about 0.5° at closest approach).
    #[must_use]
    pub fn io_from_europa() -> Self {
        Self {
            direction: [-0.80, 0.36, -0.48],
            angular_diameter_deg: 3.0,
            tint: [0.74, 0.66, 0.42],
            bands: 0.0,
            fill_lux: None,
        }
    }

    /// Physical planet-shine illuminance (lux) on a surface facing the body, for a sun of
    /// `sun_lux` toward `to_sun`: `E_sun · p · sin²(θ/2) · Φ(α)`. `p` is the geometric
    /// albedo (the luminance of [`Self::tint`]), `θ` the apparent diameter, and `Φ` the
    /// Lambert-sphere phase law `(sin α + (π − α) cos α) / π` at phase angle `α`, which is
    /// 180° minus the sun–body angle in the sky (full phase when the body is opposite the
    /// sun). Jupiter from Europa at full phase under a 4.4 klx sun is about 30 lux.
    #[must_use]
    pub fn planet_shine_lux(&self, sun_lux: f32, to_sun: [f32; 3]) -> f32 {
        let norm = |v: [f32; 3]| {
            let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            if l > 1.0e-6 {
                [v[0] / l, v[1] / l, v[2] / l]
            } else {
                [0.0, 1.0, 0.0]
            }
        };
        let (b, s) = (norm(self.direction), norm(to_sun));
        let cos_sep = (b[0] * s[0] + b[1] * s[1] + b[2] * s[2]).clamp(-1.0, 1.0);
        let alpha = std::f32::consts::PI - cos_sep.acos();
        let phase = (alpha.sin() + (std::f32::consts::PI - alpha) * alpha.cos()) / std::f32::consts::PI;
        let albedo = 0.2126 * self.tint[0] + 0.7152 * self.tint[1] + 0.0722 * self.tint[2];
        let half = (self.angular_diameter_deg.clamp(0.0, 180.0) * 0.5).to_radians();
        (sun_lux.max(0.0) * albedo.clamp(0.0, 1.0) * half.sin().powi(2) * phase.max(0.0)).max(0.0)
    }

    /// The fill this body throws: the authored [`Self::fill_lux`], else
    /// [`Self::planet_shine_lux`].
    #[must_use]
    pub fn fill_illuminance(&self, sun_lux: f32, to_sun: [f32; 3]) -> f32 {
        self.fill_lux
            .map_or_else(|| self.planet_shine_lux(sun_lux, to_sun), |lux| lux.max(0.0))
    }

    /// Apparent diameter in degrees of a sphere of `radius_km` seen from `distance_km`
    /// (centre to centre): `2 · asin(r / d)`. Jupiter from Europa: 71 492 km at 671 100 km
    /// is 12.2°.
    #[must_use]
    pub fn physical_diameter_deg(radius_km: f32, distance_km: f32) -> f32 {
        // NaN and infinite radii fail the comparisons below and take the degenerate answer.
        if radius_km.is_nan() || distance_km.is_nan() || radius_km <= 0.0 || distance_km <= radius_km {
            return 180.0;
        }
        if !radius_km.is_finite() {
            return 180.0;
        }
        2.0 * (radius_km / distance_km).asin().to_degrees()
    }
}

/// Aerosol optical depth τ of a typical clear day: the default for *new* Earth levels, so a
/// fresh map starts with the warmer shadows and whiter horizon of real clear-day air
/// (≈ 10–15 kK shadow light) instead of the published near-aerosol-free baseline.
pub const CLEAR_DAY_AEROSOL_TURBIDITY: f32 = 0.1;

/// Aerosol optical depth τ of today's near-aerosol-free Earth air (the published baseline's
/// ground-level Mie extinction × scale height: `4.440e-3/km × 1.2 km`). This is the serde
/// default, so an existing Earth level that does not name the field keeps its exact look.
pub const TODAY_AEROSOL_TURBIDITY: f32 = 5.328e-3;

/// Per-map sky settings.
#[derive(Reflect, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelAtmosphere {
    /// Planet preset. Default Earth.
    pub preset: AtmospherePreset,
    /// Sun clock. `None` keeps the authored directional light.
    pub time_of_day: Option<LevelTimeOfDay>,
    /// Sun illuminance at the top of the atmosphere (lux). `None` uses the preset's
    /// (120 klx Earth, 52 klx Mars, 4.4 klx at Jupiter).
    pub sun_illuminance_lux: Option<f32>,
    /// Fraction of the sky the cloud layer covers, 0–1. Ignored on an airless preset.
    pub cloud_coverage: f32,
    /// Cloud type, 0 stratus … 0.5 cumulus … 1 towering cumulus.
    pub cloud_type: f32,
    /// A body in the sky, if any.
    pub sky_body: Option<SkyBody>,
    /// Smaller bodies (moons) drawn in front of the sky and behind [`Self::sky_body`] where
    /// they overlap, each lit by the sun so its phase follows the sun direction.
    pub moons: Vec<SkyBody>,
    /// Dust or mist near the ground (scene format v3). Density 0 is clear air.
    pub haze: LevelHaze,
    /// Aerosol turbidity of the Earth air: the vertical aerosol (Mie) optical depth τ at the
    /// ground, which scales the preset's Mie density so `τ = extinction × scale height`.
    /// `0` is Rayleigh-only air; a typical clear day is ≈ 0.1. On load the field defaults to
    /// [`TODAY_AEROSOL_TURBIDITY`] (existing levels keep their look), while [`Self::default`]
    /// uses [`CLEAR_DAY_AEROSOL_TURBIDITY`] for new levels. Ignored on Mars and airless presets.
    #[serde(default = "today_aerosol_turbidity")]
    pub aerosol_turbidity: f32,
}

/// The serde default for [`LevelAtmosphere::aerosol_turbidity`]: today's value, so an existing
/// level that does not name the field keeps its exact look (no migration needed).
fn today_aerosol_turbidity() -> f32 {
    TODAY_AEROSOL_TURBIDITY
}

impl Default for LevelAtmosphere {
    fn default() -> Self {
        Self {
            preset: AtmospherePreset::Earth,
            time_of_day: None,
            sun_illuminance_lux: None,
            cloud_coverage: 0.0,
            cloud_type: 0.0,
            sky_body: None,
            moons: Vec::new(),
            haze: LevelHaze::default(),
            aerosol_turbidity: CLEAR_DAY_AEROSOL_TURBIDITY,
        }
    }
}

impl LevelAtmosphere {
    /// Today's atmosphere: the Earth preset at the published near-aerosol-free baseline. This
    /// is the serde default for [`LevelSettings::atmosphere`], so a level that predates the
    /// `aerosol_turbidity` field keeps its exact look on load. New levels use
    /// [`Self::default`] (a clear day) instead.
    #[must_use]
    pub fn today() -> Self {
        Self {
            aerosol_turbidity: TODAY_AEROSOL_TURBIDITY,
            ..Self::default()
        }
    }
}

impl Default for LevelSettings {
    fn default() -> Self {
        Self {
            gravity: Vec3::new(0.0, -9.81, 0.0),
            environment: None,
            game_rules: None,
            ambient_audio: None,
            streaming_enabled: false,
            atmosphere: LevelAtmosphere::default(),
            look: LevelLook::default(),
            audio_medium: LevelAudioMedium::default(),
            terrain: None,
        }
    }
}

/// The `LevelSettings` type path as written into level files.
pub const LEVEL_SETTINGS_PATH: &str = "regia_scene::level::LevelSettings";

/// A loaded level: its scene and settings.
#[derive(Clone, Debug)]
pub struct Level {
    /// The level's asset id.
    pub id: LevelId,
    /// Scene settings (also mirrored into `scene.resources` on save).
    pub settings: LevelSettings,
    /// The scene contents.
    pub scene: SceneFile,
}

impl Level {
    /// Builds a level from a parsed scene file, reading [`LevelSettings`] from its `resources`
    /// map (defaults when absent).
    pub fn from_scene_file(id: LevelId, scene: SceneFile) -> Self {
        let settings = scene
            .resources
            .get(LEVEL_SETTINGS_PATH)
            .and_then(|raw| LevelSettings::from_ron(raw.get_ron()).ok())
            .unwrap_or_default();
        Self { id, settings, scene }
    }

    /// Serializes to canonical `.level.ron` text (settings included).
    pub fn to_ron_string(&self) -> Result<String, SceneFormatError> {
        let mut scene = self.scene.clone();
        if let Ok(text) = self.settings.to_ron()
            && let Ok(raw) = ron::value::RawValue::from_ron(text.trim_end())
        {
            scene
                .resources
                .insert(LEVEL_SETTINGS_PATH.to_owned(), raw.to_owned());
        }
        scene.to_ron_string()
    }

    /// Parses `.level.ron` text. A settings resource that is present but does not parse is an
    /// error (the infallible [`Level::from_scene_file`] would fall back to defaults silently).
    pub fn from_ron_str(id: LevelId, text: &str) -> Result<Self, SceneFormatError> {
        let scene = SceneFile::from_ron_str(text)?;
        if let Some(raw) = scene.resources.get(LEVEL_SETTINGS_PATH) {
            LevelSettings::from_ron(raw.get_ron())?;
        }
        Ok(Self::from_scene_file(id, scene))
    }
}

impl LevelSettings {
    /// Serializes to canonical RON.
    pub fn to_ron(&self) -> Result<String, SceneFormatError> {
        ron::ser::to_string_pretty(self, regia_ecs::serialize::canonical_pretty_config())
            .map(|mut text| {
                text.push('\n');
                text
            })
            .map_err(SceneFormatError::from)
    }

    /// Parses canonical RON.
    pub fn from_ron(text: &str) -> Result<Self, SceneFormatError> {
        ron::from_str(text).map_err(SceneFormatError::from)
    }
}

/// Errors of the level lifecycle.
#[derive(Debug, thiserror::Error)]
pub enum LevelError {
    /// Scene format problem.
    #[error("scene format: {0}")]
    Format(#[from] SceneFormatError),
    /// Spawning problem.
    #[error("scene spawn: {0}")]
    Spawn(#[from] SceneSpawnError),
    /// The level's data has not arrived yet (not an error for async loading, but for forced ops).
    #[error("level {0} is not available in the `LevelLibrary`")]
    NotAvailable(LevelId),
}

/// Error returned by [`LevelManager::try_unload_level`] when `level` is not currently loaded.
///
/// Kept separate from [`LevelError`] so that the (exhaustive, public) [`LevelError`] enum does not
/// gain a variant and break game code `match`es.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("level {0} is not loaded")]
pub struct LevelNotLoaded(pub LevelId);

/// A level started loading. (Buffered lifecycle messages, bevy "messages", `docs/specs/10-core.md` § 2.1.)
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelLoading {
    /// Which level.
    pub level: LevelId,
    /// The load mode requested.
    pub mode: LoadMode,
}

/// A level finished loading and spawning.
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelLoaded {
    /// Which level.
    pub level: LevelId,
    /// Entities the level spawned.
    pub entities: usize,
}

/// A level is about to be unloaded (fired before its entities despawn).
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelUnloading {
    /// Which level.
    pub level: LevelId,
}

/// How [`LevelManager::open_level`] treats the currently loaded level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LoadMode {
    /// Despawn the current level's non-persistent entities, then load (the normal case).
    #[default]
    Replace,
    /// Keep everything and load on top (streaming regions, sub-areas).
    Additive,
}

/// Lifecycle state of one level known to the manager.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelState {
    /// Opened, waiting for its data (asset server seam).
    Loading,
    /// Spawned and live.
    Active,
}

/// Owns the level lifecycle: which level is current, which are loading, and the load/unload
/// operations themselves.
#[derive(Resource, Clone, Debug, Default)]
pub struct LevelManager {
    current: Option<LevelId>,
    records: HashMap<LevelId, LevelState>,
    /// The `LoadMode` each `Loading` level was opened with; applied when its data arrives
    /// (`Replace` also switches the primary level at completion).
    pending_modes: HashMap<LevelId, LoadMode>,
    /// Submission order of each `Loading` level (from `next_seq`), so a batch that completes in
    /// one frame resolves in the order the levels were opened, not in id order.
    pending_seq: HashMap<LevelId, u64>,
    next_seq: u64,
    /// The `LevelSettings` each active level was loaded with: the world resource holds only the
    /// current level's, so saving or promoting another level reads its own copy from here.
    settings: HashMap<LevelId, LevelSettings>,
    /// The load each active level came from (see [`LevelManager::load_generation`]).
    load_generations: HashMap<LevelId, u64>,
    /// Finished loads of any level. Generations come from this count, so a level unloaded and
    /// opened again between two frames still gets a new one with no entry kept for it meanwhile.
    loads_finished: u64,
    /// Prefab shown while a `Replace` load waits for its data (the project's loading screen,
    /// `specs/14` A4, ADR-7263): `regia_framework::loading_screen` fills it from `Regia.toml`
    /// `[project] loading_screen`; a game may set it itself. `None` shows nothing.
    pub loading_screen: Option<Guid>,
    /// The live loading-screen instance root, while one is shown.
    loading_screen_entity: Option<Entity>,
}

impl LevelManager {
    /// The current (primary) level, if any.
    pub fn current(&self) -> Option<LevelId> {
        self.current
    }

    /// The state of a level known to the manager.
    pub fn state(&self, level: LevelId) -> Option<LevelState> {
        self.records.get(&level).copied()
    }

    /// All levels the manager tracks.
    pub fn levels(&self) -> impl Iterator<Item = (LevelId, LevelState)> + '_ {
        self.records.iter().map(|(id, state)| (*id, *state))
    }

    /// The root of the loading-screen prefab instance while one is shown (see [`LoadingScreen`]).
    #[must_use]
    pub fn loading_screen_entity(&self) -> Option<Entity> {
        self.loading_screen_entity
    }

    /// Whether a `Replace` open is still waiting for its data.
    fn replace_pending(&self) -> bool {
        self.records.iter().any(|(id, state)| {
            *state == LevelState::Loading && self.pending_modes.get(id) == Some(&LoadMode::Replace)
        })
    }

    /// Shows the [`LevelManager::loading_screen`] prefab for `level`, a `Replace` open that waits
    /// for its data. A screen already up switches to `level` (its progress starts over).
    fn show_loading_screen(&mut self, world: &mut World, level: LevelId) {
        let Some(prefab) = self.loading_screen else {
            return;
        };
        if let Some(entity) = self.loading_screen_entity
            && world.get_entity(entity).is_ok()
        {
            if let Some(mut screen) = world.get_mut::<LoadingScreen>(entity)
                && screen.level != level
            {
                *screen = LoadingScreen { level, progress: 0.0 };
            }
            return;
        }
        let root = loading_screen_guid(level, self.next_seq);
        match crate::prefab::PrefabWorldExt::spawn_prefab(world, prefab, None, &SpawnOptions::default(), root)
        {
            Ok(entity) => {
                if let Ok(mut entity_mut) = world.get_entity_mut(entity) {
                    entity_mut.insert(LoadingScreen { level, progress: 0.0 });
                }
                self.loading_screen_entity = Some(entity);
            }
            Err(err) => {
                tracing::warn!(target: "regia_scene", "level {level}: loading screen {prefab}: {err}");
            }
        }
    }

    /// Despawns the loading screen once no `Replace` open waits for its data any more (the load
    /// completed, failed or was unloaded).
    fn hide_loading_screen_when_done(&mut self, world: &mut World) {
        if self.loading_screen_entity.is_none() || self.replace_pending() {
            return;
        }
        if let Some(entity) = self.loading_screen_entity.take()
            && world.get_entity(entity).is_ok()
            && !crate::hierarchy::despawn_tree(world, entity)
        {
            tracing::warn!(target: "regia_scene", "loading screen {entity:?} was already despawned");
        }
    }

    /// Opens a level. When the level's data is already in the world's [`LevelLibrary`] the load
    /// completes synchronously; otherwise the level is recorded `Loading` and
    /// [`level_lifecycle`] completes it on a later frame (messages fire on the transitions).
    ///
    /// `Replace` unloads the current level first and becomes the new `current()`;
    /// `Additive` keeps everything and only becomes `current()` when no level is open yet
    /// (streamed regions do not move the primary level). The same rules apply when a level
    /// opened before its data completes asynchronously.
    ///
    /// A `Replace` open whose level fails validation keeps the current level; one that passes
    /// validation but fails to spawn returns the error and opens the replaced level again from
    /// the [`LevelLibrary`], so the world is not left with no level.
    pub fn open_level(
        &mut self,
        world: &mut World,
        level: LevelId,
        mode: LoadMode,
    ) -> Result<(), LevelError> {
        world.write_message(LevelLoading { level, mode });
        if let Some(loaded) = world
            .get_resource::<LevelLibrary>()
            .and_then(|library| library.get(level).cloned())
        {
            self.complete_load(world, &loaded, mode)
        } else {
            self.records.insert(level, LevelState::Loading);
            self.pending_modes.insert(level, mode);
            if mode == LoadMode::Replace {
                self.show_loading_screen(world, level);
            }
            self.pending_seq.insert(level, self.next_seq);
            self.next_seq += 1;
            Ok(())
        }
    }

    /// Which load `level` came from, 0 when it is not active: a reload under the same id moves
    /// it, so [`crate::layers::LayerTable`] starts the level's layers over.
    #[must_use]
    pub fn load_generation(&self, level: LevelId) -> u64 {
        self.load_generations.get(&level).copied().unwrap_or(0)
    }

    /// True while `level` has spawned entities in the world: it has a record and a load
    /// generation. Its record can say `Loading` when it was re-opened before its data was back
    /// in the [`LevelLibrary`].
    fn is_spawned(&self, level: LevelId) -> bool {
        self.records.contains_key(&level) && self.load_generations.contains_key(&level)
    }

    /// Spawns a level whose data has arrived and applies the `mode` bookkeeping. Used by the
    /// synchronous open path and by [`level_lifecycle`] for levels whose data arrived later.
    fn complete_load(&mut self, world: &mut World, level: &Level, mode: LoadMode) -> Result<(), LevelError> {
        let result = self.complete_load_inner(world, level, mode);
        if result.is_err() {
            // A failed load must not stay `Loading`: drop its pending bookkeeping so
            // `level_lifecycle` does not retry it on every frame (a retried `Replace` would
            // unload the level that was already put back and spawn it again with new entity
            // ids, forever). A re-open of a level that is still spawned (it failed validation
            // before anything was unloaded) keeps the level, so its record goes back to
            // `Active`: the record is what lets it be unloaded later. A re-open that failed after
            // unloading the old copy already removed the record and the generation.
            if self.is_spawned(level.id) {
                self.records.insert(level.id, LevelState::Active);
            } else {
                self.records.remove(&level.id);
            }
            self.pending_modes.remove(&level.id);
            self.pending_seq.remove(&level.id);
        }
        // Done or failed: the loading screen goes once no other `Replace` load waits.
        self.hide_loading_screen_when_done(world);
        result
    }

    fn complete_load_inner(
        &mut self,
        world: &mut World,
        level: &Level,
        mode: LoadMode,
    ) -> Result<(), LevelError> {
        // A level whose file fails validation never spawns; check it before `Replace` unloads
        // the current level, so the world keeps that level instead of ending up empty (audit
        // wave 5, stream H7).
        level.scene.validate()?;
        let replaced = if mode == LoadMode::Replace {
            self.current.take()
        } else {
            None
        };
        if let Some(previous) = replaced {
            self.unload_level(world, previous)?;
        }
        // Opening a level that is already spawned reloads it in place instead of stacking a
        // second copy that shares one record. A spawned level re-opened before its data was back
        // in the library is recorded `Loading`; its load generation says it is still spawned.
        if self.is_spawned(level.id) {
            self.unload_inner(world, level.id, false)?;
        }
        let adopt = persistent_members(world, level.id);
        // `Replace` switches the primary level; an additive load only becomes current when
        // nothing else is (the world was level-less before). An additive load while another
        // level is primary must not replace the primary level's `LevelSettings`.
        let becomes_current = mode == LoadMode::Replace || self.current.is_none();
        if let Err(err) = spawn_loaded(world, self, level, becomes_current, &adopt) {
            // A valid file can still fail to spawn (a prefab that nests itself); a failed spawn
            // leaves none of its rows. Put the replaced level back from the library rather than
            // leave the world with no level (audit wave 10c, stream H7).
            if let Some(previous) = replaced
                && previous != level.id
            {
                self.restore_replaced(world, previous);
            }
            return Err(err);
        }
        self.loads_finished += 1;
        self.load_generations.insert(level.id, self.loads_finished);
        if becomes_current {
            self.current = Some(level.id);
        }
        Ok(())
    }

    /// Spawns `previous` again from the [`LevelLibrary`] after a `Replace` open that unloaded it
    /// failed to spawn its own level; it becomes current again. Its runtime state is not kept:
    /// it opens as its file says, with [`LevelLoaded`] written again.
    fn restore_replaced(&mut self, world: &mut World, previous: LevelId) {
        let Some(level) = world
            .get_resource::<LevelLibrary>()
            .and_then(|library| library.get(previous).cloned())
        else {
            tracing::warn!(target: "regia_scene", "level {previous}: not in the library, cannot put it back");
            return;
        };
        let adopt = persistent_members(world, previous);
        match spawn_loaded(world, self, &level, true, &adopt) {
            Ok(()) => {
                self.loads_finished += 1;
                self.load_generations.insert(previous, self.loads_finished);
                self.current = Some(previous);
            }
            Err(err) => {
                tracing::warn!(target: "regia_scene", "level {previous}: putting it back failed: {err}");
            }
        }
    }

    /// Unloads a level: fires [`LevelUnloading`], runs `OnLevelUnloading`, then despawns its
    /// non-persistent entities. Unloading the current level hands the primary role to the remaining
    /// active level with the lowest id, if any (see below).
    ///
    /// Unloading a level that is not loaded is a no-op (`Ok(())`); use
    /// [`LevelManager::try_unload_level`] to treat that as an error instead.
    ///
    /// When the unloaded level was current and another level is still `Active`, the remaining
    /// level with the lowest id becomes current and its settings (gravity, atmosphere, game
    /// rules) replace the unloaded level's in the world.
    pub fn unload_level(&mut self, world: &mut World, level: LevelId) -> Result<(), LevelError> {
        self.unload_inner(world, level, true)
    }

    /// [`LevelManager::unload_level`]; `promote` = false skips the successor step (a reload that
    /// is about to respawn the same level).
    fn unload_inner(&mut self, world: &mut World, level: LevelId, promote: bool) -> Result<(), LevelError> {
        if !self.records.contains_key(&level) {
            return Ok(()); // nothing to do; not an error
        }
        let _mem = regia_core::memory::enter(regia_core::memory::Subsystem::Scene);
        world.write_message(LevelUnloading { level });
        if let Err(err) = world.try_run_schedule(regia_app::OnLevelUnloading) {
            tracing::debug!(target: "regia_scene", "OnLevelUnloading schedule: {err}");
        }
        despawn_level_entities(world, level);
        self.records.remove(&level);
        self.pending_modes.remove(&level);
        self.pending_seq.remove(&level);
        self.settings.remove(&level);
        self.load_generations.remove(&level);
        if self.current == Some(level) {
            self.current = None;
            if promote {
                let successor = self
                    .records
                    .iter()
                    .filter(|(_, state)| **state == LevelState::Active)
                    .map(|(id, _)| *id)
                    .min();
                if let Some(next) = successor {
                    self.current = Some(next);
                    if let Some(settings) = self.settings.get(&next) {
                        world.insert_resource(settings.clone());
                    }
                }
            }
        }
        // Unloading a level that was still loading cancels its load (and its loading screen).
        self.hide_loading_screen_when_done(world);
        Ok(())
    }

    /// Like [`LevelManager::unload_level`], but returns [`LevelNotLoaded`] instead of
    /// silently succeeding when `level` is not currently loaded.
    pub fn try_unload_level(&mut self, world: &mut World, level: LevelId) -> Result<(), LevelNotLoaded> {
        if !self.records.contains_key(&level) {
            return Err(LevelNotLoaded(level));
        }
        // `unload_level` cannot fail here: `level` is in `records`, so it despawns and returns
        // `Ok(())`; the error arm is unreachable and maps only to keep the `Result` types honest.
        self.unload_level(world, level).map_err(|_| LevelNotLoaded(level))
    }

    /// Records `level` as active without spawning it, for a caller that spawned the level's scene
    /// itself with [`SpawnOptions::level`] = `Some(level.id)`: play-in-editor spawns its snapshot
    /// with the editor's unknown-type policy. The level gets a load generation and keeps its
    /// settings, and becomes current when no level is, so [`crate::layers`] can despawn and
    /// respawn its members from the [`LevelLibrary`] copy as in a game. No message is written and
    /// no schedule runs.
    pub fn record_spawned(&mut self, level: &Level) {
        self.records.insert(level.id, LevelState::Active);
        self.pending_modes.remove(&level.id);
        self.pending_seq.remove(&level.id);
        self.settings.insert(level.id, level.settings.clone());
        self.loads_finished += 1;
        self.load_generations.insert(level.id, self.loads_finished);
        if self.current.is_none() {
            self.current = Some(level.id);
        }
    }
}

/// The root guid of the loading screen shown for `level`'s open number `seq`: derived, not
/// random, so a replay or a lockstep peer spawns the same guid.
fn loading_screen_guid(level: LevelId, seq: u64) -> Guid {
    // One xorshift-multiply round over (level, seq) with a fixed salt; never nil.
    let mut x = level.0.as_u128() ^ 0x4c6f_6164_696e_6753_6372_6565_6e00_0000 ^ u128::from(seq);
    x ^= x >> 67;
    x = x.wrapping_mul(0x9e37_79b9_7f4a_7c15_f39c_c060_5ced_c835);
    x ^= x >> 61;
    Guid::from_u128(x.max(1))
}

/// Opens a level through the world's [`LevelManager`] resource.
///
/// # Errors
/// See [`LevelManager::open_level`].
pub fn open_level(world: &mut World, level: LevelId, mode: LoadMode) -> Result<(), LevelError> {
    let Some(mut manager) = world.remove_resource::<LevelManager>() else {
        return Err(LevelError::NotAvailable(level));
    };
    let result = manager.open_level(world, level, mode);
    world.insert_resource(manager);
    result
}

/// Unloads a level through the world's [`LevelManager`] resource.
///
/// # Errors
/// See [`LevelManager::unload_level`].
pub fn unload_level(world: &mut World, level: LevelId) -> Result<(), LevelError> {
    let Some(mut manager) = world.remove_resource::<LevelManager>() else {
        return Err(LevelError::NotAvailable(level));
    };
    let result = manager.unload_level(world, level);
    world.insert_resource(manager);
    result
}

/// Unloads a level through the world's [`LevelManager`] resource, returning
/// [`LevelNotLoaded`] when it is not loaded.
///
/// # Errors
/// See [`LevelManager::try_unload_level`].
pub fn try_unload_level(world: &mut World, level: LevelId) -> Result<(), LevelNotLoaded> {
    let Some(mut manager) = world.remove_resource::<LevelManager>() else {
        return Err(LevelNotLoaded(level));
    };
    let result = manager.try_unload_level(world, level);
    world.insert_resource(manager);
    result
}

/// The children of the entities a spawn will adopt, taken before the spawn: what
/// [`shift_new_roots_to_origin`] must leave alone under an adopted group.
pub(crate) fn children_of_adopted(world: &World, adopt: &HashMap<Guid, Entity>) -> HashSet<Entity> {
    adopt
        .values()
        .filter_map(|entity| world.get::<Children>(*entity))
        .flat_map(|children| children.iter())
        .collect()
}

/// Whether `entity` passes the world origin's frame to its children: neither it nor any
/// ancestor reached through `Transform`-less links has a `Transform` (propagation hands an
/// identity frame down from a `Transform`-less root). A `ChildOf` loop answers `false`.
fn passes_origin_frame(world: &World, entity: Entity) -> bool {
    let mut seen: HashSet<Entity> = HashSet::new();
    let mut cursor = entity;
    loop {
        if world.get::<crate::Transform>(cursor).is_some() || !seen.insert(cursor) {
            return false;
        }
        match world.get::<ChildOf>(cursor) {
            Some(link) => cursor = link.parent(),
            None => return true,
        }
    }
}

/// A file is authored in world meters. With the floating origin moved, the freshly spawned roots
/// of a file go into local space (spec 78 § 12); entities the spawn adopted are already there. A
/// root without a `Transform` shifts its nearest transformed descendants instead. So does an
/// adopted entity that passes the origin's frame (a live `Transform`-less group): its fresh
/// children (any not in `old_children`, taken by [`children_of_adopted`] before the spawn) carry
/// world meters too.
/// Shared by the level open and the data-layer respawn, which spawn from the same file.
pub(crate) fn shift_new_roots_to_origin(
    world: &mut World,
    roots: &[Entity],
    adopt: &HashMap<Guid, Entity>,
    old_children: &HashSet<Entity>,
) {
    let offset = world
        .get_resource::<crate::streaming::OriginOffset>()
        .map_or([0.0; 3], |o| o.0);
    if offset == [0.0; 3] {
        return;
    }
    let adopted: HashSet<Entity> = adopt.values().copied().collect();
    // A root without a `Transform` (an empty group) passes no frame down: its children carry the
    // world position then, so the walk goes down through transform-less entities.
    let mut stack: Vec<Entity> = roots
        .iter()
        .filter(|root| !adopted.contains(root))
        .copied()
        .collect();
    // Adopted groups that pass the origin frame: start at their fresh children. Sorted so the
    // walk order does not depend on the map's iteration order.
    let mut groups: Vec<Entity> = adopted
        .iter()
        .copied()
        .filter(|entity| passes_origin_frame(world, *entity))
        .collect();
    groups.sort();
    for group in groups {
        if let Some(children) = world.get::<Children>(group) {
            stack.extend(
                children
                    .iter()
                    .filter(|child| !adopted.contains(child) && !old_children.contains(child)),
            );
        }
    }
    while let Some(entity) = stack.pop() {
        if let Some(mut transform) = world.get_mut::<crate::Transform>(entity) {
            // Subtract in f64 and round once: rounding the offset first loses a meter at 2^24.
            let t = transform.translation;
            transform.translation = Vec3::new(
                (f64::from(t.x) - offset[0]) as f32,
                (f64::from(t.y) - offset[1]) as f32,
                (f64::from(t.z) - offset[2]) as f32,
            );
        } else if let Some(children) = world.get::<Children>(entity) {
            stack.extend(children.iter().filter(|child| !adopted.contains(child)));
        }
    }
}

/// Spawns a loaded level and finalizes the manager's record. Only a level that becomes the
/// current one replaces the world's [`LevelSettings`]: a streamed region loaded on top keeps the
/// open map's look, sun and baked GI (its own settings stay in the [`LevelLibrary`]).
fn spawn_loaded(
    world: &mut World,
    manager: &mut LevelManager,
    level: &Level,
    becomes_current: bool,
    adopt: &HashMap<Guid, Entity>,
) -> Result<(), LevelError> {
    let _span = tracing::info_span!("level_spawn").entered();
    let opts = SpawnOptions {
        level: Some(level.id),
        persistent: false,
        unknown_types: Default::default(),
    };
    let previous_settings = world.get_resource::<LevelSettings>().cloned();
    let old_children = children_of_adopted(world, adopt);
    let spawned = crate::spawn::spawn_scene_adopting(world, &level.scene, &opts, adopt)?;
    for warning in &spawned.warnings {
        tracing::warn!(target: "regia_scene", "level {}: {warning}", level.id);
    }
    // A level a placed instance streams is authored relative to the instance: its roots go under
    // the instance (`placed::reattach_streamed`), which is in local space already.
    if crate::placed::streamed_by(world, level.id).is_none() {
        shift_new_roots_to_origin(world, &spawned.roots, adopt, &old_children);
    }
    match (becomes_current, previous_settings) {
        // `spawn_scene` applies the file's settings resource; a non-primary level restores the
        // primary level's settings afterwards.
        (false, Some(previous)) => world.insert_resource(previous),
        _ => world.insert_resource(level.settings.clone()),
    }
    manager.records.insert(level.id, LevelState::Active);
    // The open-time mode only matters while the level is `Loading`.
    manager.pending_modes.remove(&level.id);
    manager.settings.insert(level.id, level.settings.clone());
    manager.pending_seq.remove(&level.id);
    world.write_message(LevelLoaded {
        level: level.id,
        entities: spawned.entities,
    });
    if let Err(err) = world.try_run_schedule(regia_app::OnLevelLoaded) {
        tracing::debug!(target: "regia_scene", "OnLevelLoaded schedule: {err}");
    }
    Ok(())
}

/// The `Persistent` members of `level` by guid: they survive an unload, and a respawn adopts
/// them instead of adding a second entity with the same guid.
fn persistent_members(world: &mut World, level: LevelId) -> HashMap<Guid, Entity> {
    let mut adopt: HashMap<Guid, Entity> = HashMap::new();
    let mut survivors = QueryState::<(Entity, &LevelMember, &EntityGuid), With<Persistent>>::new(world);
    for (entity, member, guid) in survivors.iter(world) {
        if member.level() == level {
            adopt.insert(guid.0, entity);
        }
    }
    adopt
}

/// Despawns every non-persistent [`LevelMember`] of `level`.
///
/// Persistent members that sit *inside* a despawned subtree are detached from their parent
/// first (they keep their world pose from the last propagation run; the next run treats them as
/// roots), so `despawn`'s linked-subtree removal cannot take them with it.
fn despawn_level_entities(world: &mut World, level: LevelId) {
    let mut members: Vec<(Entity, bool)> = Vec::new();
    // Level members that stay alive: persistent members of this level and every member of
    // another level. Any of them parented under a dying entity is detached first.
    let mut survivors: Vec<Entity> = Vec::new();
    {
        let mut query = QueryState::<(Entity, &LevelMember, Option<&Persistent>)>::new(world);
        for (entity, member, persistent) in query.iter(world) {
            if member.level() == level {
                members.push((entity, persistent.is_some()));
                if persistent.is_some() {
                    survivors.push(entity);
                }
            } else {
                survivors.push(entity);
            }
        }
    }
    // Query order depends on archetype creation order; despawn in entity order.
    members.sort_by_key(|(entity, _)| *entity);
    let dying: HashSet<Entity> = members
        .iter()
        .filter(|(_, persistent)| !persistent)
        .map(|(entity, _)| *entity)
        .collect();
    if dying.is_empty() {
        return;
    }

    // Detach survivors that would die with a despawned ancestor.
    for entity in survivors {
        let mut cursor = world.get::<ChildOf>(entity).map(|c| c.parent());
        let mut dies = false;
        // `ChildOf` does not reject cycles: stop when a parent repeats.
        let mut visited: HashSet<Entity> = HashSet::new();
        while let Some(parent) = cursor {
            if dying.contains(&parent) {
                dies = true;
                break;
            }
            if !visited.insert(parent) {
                break;
            }
            cursor = world.get::<ChildOf>(parent).map(|c| c.parent());
        }
        if dies && world.get_entity(entity).is_ok() {
            // Bake the world pose into the local transform so the next propagation run (which
            // treats the entity as a root) does not teleport it.
            let keep = world
                .get::<crate::transform::Transform>(entity)
                .is_some()
                .then(|| crate::hierarchy::local_under_parent(world, entity, None));
            if let Ok(mut entity_mut) = world.get_entity_mut(entity) {
                if let Some(local) = keep {
                    entity_mut.insert(local);
                }
                entity_mut.remove::<ChildOf>();
            }
        }
    }

    for (entity, persistent) in members {
        if persistent || !world.entities().contains(entity) {
            continue;
        }
        // The subtree goes too (its members are level members as well), iteratively, so a deep
        // authored hierarchy cannot overflow the stack (`despawn_tree`, ADR-2306).
        if !crate::hierarchy::despawn_tree(world, entity) {
            tracing::warn!(target: "regia_scene", "unload: {entity:?} was already despawned");
        }
    }
}

/// The levels available to load, keyed by asset id. The asset server inserts here when a
/// `.level.ron` finishes loading (`specs/12` § 3) — the seam that makes `open_level` async.
#[derive(Resource, Clone, Debug, Default)]
pub struct LevelLibrary {
    levels: HashMap<LevelId, Arc<Level>>,
}

impl LevelLibrary {
    /// An empty library.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a parsed level.
    pub fn insert(&mut self, level: Level) {
        self.levels.insert(level.id, Arc::new(level));
    }

    /// Looks a level up.
    pub fn get(&self, id: LevelId) -> Option<&Arc<Level>> {
        self.levels.get(&id)
    }

    /// Whether the level's data is present.
    pub fn contains(&self, id: LevelId) -> bool {
        self.levels.contains_key(&id)
    }

    /// Forgets a level's data (after unloading it, so a streamed region's parsed file does not
    /// stay resident). Returns it when it was present.
    pub fn remove(&mut self, id: LevelId) -> Option<Arc<Level>> {
        self.levels.remove(&id)
    }
}

/// Completes `Loading` levels whose data has arrived. Registered in `PreUpdate` by
/// [`crate::ScenePlugin`].
#[tracing::instrument(target = "regia_scene", skip_all)]
pub fn level_lifecycle(world: &mut World) {
    let _span = tracing::debug_span!("level_lifecycle").entered();
    let Some(mut manager) = world.remove_resource::<LevelManager>() else {
        return;
    };
    let mut loading: Vec<(LevelId, LoadMode)> = manager
        .records
        .iter()
        .filter(|(_, state)| **state == LevelState::Loading)
        .map(|(id, _)| *id)
        .map(|id| (id, manager.pending_modes.get(&id).copied().unwrap_or_default()))
        .collect();
    // `records` is a hash map: complete in submission order so the result is the same every run.
    // `Replace` levels go first (an additive overlay opened in the same batch must not become
    // current only to be unloaded by the base level's `Replace`).
    loading.sort_by_key(|(id, mode)| {
        (
            *mode == LoadMode::Additive,
            manager.pending_seq.get(id).copied().unwrap_or(u64::MAX),
            *id,
        )
    });
    for (id, mode) in loading {
        let Some(level) = world
            .get_resource::<LevelLibrary>()
            .and_then(|library| library.get(id).cloned())
        else {
            continue;
        };
        if let Err(err) = manager.complete_load(world, &level, mode) {
            tracing::warn!(target: "regia_scene", "loading level {id}: {err}");
        }
    }
    world.insert_resource(manager);
}

/// Snapshots the entities of `level` (plus its [`LevelSettings`]) back into a [`Level`] file.
pub fn snapshot_level(world: &mut World, level: LevelId) -> Result<Level, SceneSpawnError> {
    snapshot_level_cached(world, level, None)
}

/// [`snapshot_level`] through a [`crate::spawn::SnapshotCache`] (ADR-7748): the same level, with
/// the components of entities the cache vouches for taken from it.
///
/// # Errors
///
/// As [`snapshot_level`].
pub fn snapshot_level_cached(
    world: &mut World,
    level: LevelId,
    cache: Option<&mut (dyn crate::spawn::SnapshotCache + '_)>,
) -> Result<Level, SceneSpawnError> {
    let filter = SnapshotFilter {
        level: Some(level),
        resources: vec![LEVEL_SETTINGS_PATH.to_owned()],
    };
    // The world resource holds the current level's settings; another level saves its own.
    let own_settings = world
        .get_resource::<LevelManager>()
        .filter(|manager| manager.current != Some(level))
        .and_then(|manager| manager.settings.get(&level).cloned());
    let swapped = own_settings.map(|own| {
        let previous = world.get_resource::<LevelSettings>().cloned();
        world.insert_resource(own);
        previous
    });
    let scene = crate::spawn::snapshot_scene_cached(world, &filter, cache);
    match swapped {
        Some(Some(previous)) => world.insert_resource(previous),
        Some(None) => {
            world.remove_resource::<LevelSettings>();
        }
        None => {}
    }
    Ok(Level::from_scene_file(level, scene?))
}

/// Reads the world's current [`LevelSettings`] resource (default when absent).
pub fn current_settings(world: &World) -> LevelSettings {
    world.get_resource::<LevelSettings>().cloned().unwrap_or_default()
}

/// Registers the level types on `registry` (part of [`crate::ScenePlugin`]; exposed for tests and
/// tools that build bare worlds).
pub fn register_types(registry: &mut TypeRegistry) {
    registry.register::<LevelMember>();
    registry.register::<Persistent>();
    registry.register::<LevelSettings>();
}

#[cfg(test)]
mod tests {
    #[test]
    fn terrain_height_scale_is_bounded() {
        // A scale near f32::MAX passes "finite and above 0" but overflows the f32 four-tap
        // sums the derived height mips accumulate; validate must reject it. 1e6 m still
        // passes: three orders of magnitude above the largest authored terrain.
        let mut t = LevelTerrain::default();
        assert!(t.validate().is_ok());
        t.height_scale_m = 3.0e38;
        assert!(t.validate().is_err());
        t.height_scale_m = f32::INFINITY;
        assert!(t.validate().is_err());
        t.height_scale_m = 1.0e6;
        assert!(t.validate().is_ok());
        t.height_scale_m = 1.0e6 * (1.0 + f32::EPSILON);
        assert!(t.validate().is_err());
    }

    #[test]
    fn planet_shine_follows_the_phase_and_an_authored_fill_wins() {
        let body = SkyBody {
            direction: [0.0, 0.0, 1.0],
            ..SkyBody::jupiter_from_europa()
        };
        // Body opposite the sun: full phase. Beside it: half lit. Behind it: new.
        let full = body.planet_shine_lux(4_400.0, [0.0, 0.0, -1.0]);
        let quarter = body.planet_shine_lux(4_400.0, [1.0, 0.0, 0.0]);
        let new = body.planet_shine_lux(4_400.0, [0.0, 0.0, 1.0]);
        // 4.4 klx · 0.55 albedo · sin²(7°) ≈ 36 lux.
        assert!((30.0..42.0).contains(&full), "{full}");
        assert!(
            (quarter / full - 1.0 / std::f32::consts::PI).abs() < 0.01,
            "{quarter} vs {full}"
        );
        assert!(new < 1.0e-3, "{new}");
        let authored = SkyBody {
            fill_lux: Some(1_600.0),
            ..body
        };
        assert_eq!(authored.fill_illuminance(4_400.0, [0.0, 0.0, 1.0]), 1_600.0);
        assert!((body.fill_illuminance(4_400.0, [0.0, 0.0, -1.0]) - full).abs() < 1.0e-3);
    }

    use super::*;
    use regia_app::prelude::*;
    use regia_core::math::Vec3;

    #[test]
    fn documented_level_settings_key_is_the_one_the_loader_reads() {
        let id = LevelId(Guid::from_u128(1));
        let good = format!(
            r#"(format_version: 1, entities: [], resources: {{ "{LEVEL_SETTINGS_PATH}": (gravity: (0.0, -20.0, 0.0)) }})"#
        );
        let level = Level::from_ron_str(id, &good).expect("the documented key parses");
        assert_eq!(level.settings.gravity.y, -20.0);
        let wrong = r#"(format_version: 1, entities: [], resources: { "regia_scene::LevelSettings": (gravity: (0.0, -20.0, 0.0)) })"#;
        let ignored = Level::from_ron_str(id, wrong).expect("the short key is not a parse error");
        assert!(
            (ignored.settings.gravity.y - -9.81).abs() < 1.0e-4,
            "the short key is ignored, gravity stayed {}",
            ignored.settings.gravity.y
        );
    }

    fn level_text(root: Guid, child: Guid, gravity_y: f32) -> String {
        format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{root}", name: "Origin", components: {{
            "regia_scene::transform::Transform": (translation: (0.0, 0.0, 0.0)),
        }}),
        (guid: "{child}", name: "Prop", parent: Some("{root}"), components: {{
            "regia_scene::transform::Transform": (translation: (3.0, 0.0, 0.0)),
        }}),
    ],
    resources: {{
        "{LEVEL_SETTINGS_PATH}": (gravity: (0.0, {gravity_y}, 0.0)),
    }},
)
"#
        )
    }

    fn app_with_level() -> (App, LevelId, Guid, Guid) {
        let mut app = App::new();
        app.add_plugins(crate::ScenePlugin);
        let id = LevelId(Guid::new_random());
        let root = Guid::new_random();
        let child = Guid::new_random();
        let level = Level::from_ron_str(id, &level_text(root, child, -20.0)).expect("parse");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        (app, id, root, child)
    }

    /// A file is authored in world meters: with the floating origin moved, its roots open in local
    /// space, and a child stays relative to its parent (spec 78 § 12).
    #[test]
    fn a_level_opened_with_the_origin_moved_lands_in_local_space() {
        let (mut app, id, root, child) = app_with_level();
        let offset = [1024.0, 0.0, -2048.0];
        app.insert_resource(crate::streaming::OriginOffset(offset));
        crate::level::open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        let world = app.world_mut();
        let mut query = world.query::<(&crate::components::EntityGuid, &crate::transform::Transform)>();
        let mut at = |guid: Guid| {
            query
                .iter(world)
                .find(|(g, _)| g.0 == guid)
                .map(|(_, t)| t.translation)
                .expect("spawned")
        };
        // `level_text` authors the root at the world origin and the child 3 m along X from it.
        assert_eq!(at(root), Vec3::new(-1024.0, 0.0, 2048.0));
        assert_eq!(at(child), Vec3::new(3.0, 0.0, 0.0));
    }

    /// Finding scene-ecs 6 (v0.5.0 rc1): a root without a `Transform` (an authored empty group)
    /// passes no frame to its children, so they are the ones the floating origin shifts; the
    /// group itself gains no `Transform`.
    #[test]
    fn a_transformless_group_root_shifts_its_children() {
        let mut app = App::new();
        app.add_plugins(crate::ScenePlugin);
        let id = LevelId(Guid::new_random());
        let root = Guid::new_random();
        let child = Guid::new_random();
        let text = level_text(root, child, -20.0).replacen(
            r#""regia_scene::transform::Transform": (translation: (0.0, 0.0, 0.0)),"#,
            "",
            1,
        );
        let level = Level::from_ron_str(id, &text).expect("parse");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        app.insert_resource(crate::streaming::OriginOffset([1024.0, 0.0, 0.0]));
        crate::level::open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        let world = app.world_mut();
        let mut query = world.query::<(Entity, &crate::components::EntityGuid)>();
        let mut find = |world: &World, guid: Guid| {
            let mut found = None;
            for (entity, g) in query.iter(world) {
                if g.0 == guid {
                    found = Some(entity);
                }
            }
            found.expect("spawned")
        };
        let group = find(world, root);
        let prop = find(world, child);
        assert!(
            world.get::<crate::transform::Transform>(group).is_none(),
            "no Transform added"
        );
        let x = world
            .get::<crate::transform::GlobalTransform>(prop)
            .map(|g| g.translation().x);
        // Authored 3 m along X in world meters: 3 - 1024 in local space.
        assert_eq!(x, Some(-1021.0));
    }

    /// The offset is subtracted in f64: a root at 2^24 + 2 with an offset of 2^24 + 1 lands at 1 m,
    /// where an f32-rounded offset would land it at 2 m (or 0).
    #[test]
    fn a_level_opened_far_from_the_origin_subtracts_in_double_precision() {
        let mut app = App::new();
        app.add_plugins(crate::ScenePlugin);
        let id = LevelId(Guid::new_random());
        let root = Guid::new_random();
        let text = level_text(root, Guid::new_random(), -20.0).replacen(
            "(translation: (0.0, 0.0, 0.0))",
            "(translation: (16777218.0, 0.0, 0.0))",
            1,
        );
        let level = Level::from_ron_str(id, &text).expect("parse");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        app.insert_resource(crate::streaming::OriginOffset([16_777_217.0, 0.0, 0.0]));
        crate::level::open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        let world = app.world_mut();
        let mut query = world.query::<(&crate::components::EntityGuid, &crate::transform::Transform)>();
        let x = query
            .iter(world)
            .find(|(g, _)| g.0 == root)
            .map(|(_, t)| t.translation.x)
            .expect("spawned");
        assert_eq!(x, 1.0);
    }

    #[test]
    fn open_level_loads_entities_settings_and_fires_messages() {
        let (mut app, id, root, _child) = app_with_level();
        crate::level::open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();

        let manager = app.world().resource::<LevelManager>();
        assert_eq!(manager.current(), Some(id));
        assert_eq!(manager.state(id), Some(LevelState::Active));

        let world = app.world_mut();
        let roots = world
            .query::<(Entity, &crate::components::EntityGuid)>()
            .iter(world)
            .filter(|(_, g)| g.0 == root)
            .count();
        assert_eq!(roots, 1);
        assert_eq!(
            world.resource::<LevelSettings>().gravity,
            Vec3::new(0.0, -20.0, 0.0)
        );
    }

    #[test]
    fn async_load_clears_its_pending_mode_when_active() {
        let (mut app, id, _root, _child) = app_with_level();
        // Open a level whose data has not arrived yet.
        let late = LevelId(Guid::new_random());
        crate::level::open_level(app.world_mut(), late, LoadMode::Additive).expect("open pending");
        assert_eq!(
            app.world().resource::<LevelManager>().state(late),
            Some(LevelState::Loading)
        );
        assert_eq!(app.world().resource::<LevelManager>().pending_modes.len(), 1);

        let level = Level::from_ron_str(late, &level_text(Guid::new_random(), Guid::new_random(), -9.81))
            .expect("parse");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        app.update();

        let manager = app.world().resource::<LevelManager>();
        assert_eq!(manager.state(late), Some(LevelState::Active));
        assert!(manager.pending_modes.is_empty(), "stale mode left behind");
        let _ = id;
    }

    /// Captures lifecycle messages so tests can assert them.
    #[derive(Resource, Default)]
    struct CapturedLoads(Vec<LevelLoaded>);

    fn capture_loaded(mut reader: MessageReader<LevelLoaded>, mut captured: ResMut<CapturedLoads>) {
        captured.0.extend(reader.read().cloned());
    }

    #[test]
    fn level_lifecycle_messages_fire() {
        let (mut app, id, _root, _child) = app_with_level();
        app.insert_resource(CapturedLoads::default());
        app.add_systems(regia_app::PreUpdate, capture_loaded);
        // The plugin's lifecycle system runs in PreUpdate; capture reads the same frame's buffer.
        crate::level::open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        app.update();

        let captured = app.world().resource::<CapturedLoads>();
        assert!(
            captured.0.iter().any(|m| m.level == id),
            "LevelLoaded fired: {:?}",
            captured.0
        );
    }

    /// What the lifecycle conformance test saw: the start and unload messages, and how many of
    /// the level's entities were still alive when `OnLevelUnloading` ran.
    #[derive(Resource, Default)]
    struct CapturedLifecycle {
        loading: Vec<LevelLoading>,
        unloading: Vec<LevelUnloading>,
        members_at_unloading: Vec<usize>,
    }

    fn capture_lifecycle(
        mut loading: MessageReader<LevelLoading>,
        mut unloading: MessageReader<LevelUnloading>,
        mut captured: ResMut<CapturedLifecycle>,
    ) {
        captured.loading.extend(loading.read().copied());
        captured.unloading.extend(unloading.read().copied());
    }

    fn count_members_at_unloading(members: Query<&LevelMember>, mut captured: ResMut<CapturedLifecycle>) {
        captured.members_at_unloading.push(members.iter().count());
    }

    /// Spec 14 A4: `LevelLoading` (with the requested mode) fires when a level opens and
    /// `LevelUnloading` fires once when it unloads, before its entities despawn.
    #[test]
    fn level_loading_and_unloading_messages_fire_with_their_fields() {
        let (mut app, id, _root, _child) = app_with_level();
        app.init_resource::<CapturedLifecycle>();
        app.add_systems(regia_app::PreUpdate, capture_lifecycle);
        app.add_systems(regia_app::OnLevelUnloading, count_members_at_unloading);

        crate::level::open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        {
            let captured = app.world().resource::<CapturedLifecycle>();
            assert_eq!(
                captured.loading,
                vec![LevelLoading {
                    level: id,
                    mode: LoadMode::Replace
                }]
            );
            assert!(captured.unloading.is_empty(), "nothing unloaded yet");
        }

        crate::level::unload_level(app.world_mut(), id).expect("unload");
        app.update();
        let captured = app.world().resource::<CapturedLifecycle>();
        assert_eq!(captured.unloading, vec![LevelUnloading { level: id }]);
        assert_eq!(
            captured.members_at_unloading,
            vec![2],
            "root and child are still alive when OnLevelUnloading runs"
        );
        assert_eq!(captured.loading.len(), 1, "an unload is not a load");
        let world = app.world_mut();
        assert_eq!(
            world.query::<&LevelMember>().iter(world).count(),
            0,
            "the level's entities are gone after the unload"
        );
    }

    /// Spec 14 A4: an `Additive` open reports its mode too.
    #[test]
    fn level_loading_reports_the_additive_mode() {
        let (mut app, id, _root, _child) = app_with_level();
        app.init_resource::<CapturedLifecycle>();
        app.add_systems(regia_app::PreUpdate, capture_lifecycle);
        crate::level::open_level(app.world_mut(), id, LoadMode::Additive).expect("open");
        app.update();
        let captured = app.world().resource::<CapturedLifecycle>();
        assert_eq!(
            captured.loading,
            vec![LevelLoading {
                level: id,
                mode: LoadMode::Additive
            }]
        );
    }

    #[test]
    fn additive_reopen_of_active_level_does_not_duplicate() {
        let (mut app, id, _root, _child) = app_with_level();
        for _ in 0..2 {
            let world = app.world_mut();
            crate::level::open_level(world, id, LoadMode::Additive).expect("open");
            app.update();
        }
        let world = app.world_mut();
        let members = world.query::<&LevelMember>().iter(world).count();
        assert_eq!(members, 2, "root + child exactly once");
    }

    #[test]
    fn unload_keeps_persistent_world_pose() {
        use crate::transform::{GlobalTransform, Transform};
        let mut world = World::new();
        let level = LevelId(Guid::new_random());
        let member = LevelMember::from_level(level);
        let root = world
            .spawn((Transform::from_translation(Vec3::new(100.0, 0.0, 0.0)), member))
            .id();
        let player = world
            .spawn((
                Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
                member,
                Persistent,
                ChildOf(root),
            ))
            .id();
        crate::propagate::propagate_transforms(&mut world);
        despawn_level_entities(&mut world, level);
        crate::propagate::propagate_transforms(&mut world);
        assert!(world.get_entity(root).is_err(), "level root despawned");
        let global = world.get::<GlobalTransform>(player).expect("global");
        assert_eq!(global.translation(), Vec3::new(101.0, 0.0, 0.0));
    }

    /// Audit wave 5 (stream H7): a `Replace` open of a level whose file fails validation (two
    /// entities share a guid) is refused before the current level is unloaded, so that level and
    /// its entities stay.
    #[test]
    fn a_replace_open_of_an_invalid_level_keeps_the_current_level() {
        let (mut app, id, root, _child) = app_with_level();
        crate::level::open_level(app.world_mut(), id, LoadMode::Replace).expect("open 1");
        app.update();
        let id2 = LevelId(Guid::new_random());
        let mut level2 = Level::from_ron_str(id2, &level_text(Guid::new_random(), Guid::new_random(), -9.81))
            .expect("parse 2");
        let twin = level2.scene.entities[0].clone();
        level2.scene.entities.push(twin);
        assert!(level2.scene.validate().is_err(), "the fixture must be invalid");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level2);
        assert!(crate::level::open_level(app.world_mut(), id2, LoadMode::Replace).is_err());
        app.update();
        let world = app.world_mut();
        assert_eq!(world.resource::<LevelManager>().current(), Some(id));
        assert!(
            world
                .query::<&crate::components::EntityGuid>()
                .iter(world)
                .any(|g| g.0 == root),
            "the current level's root is still there"
        );
    }

    /// Audit wave 10c (stream H7, the part wave 5 left): a `Replace` open of a valid level whose
    /// spawn still fails (a prefab that nests itself fails only at the nesting limit) puts the
    /// replaced level back instead of leaving the world with no level.
    #[test]
    fn a_replace_open_whose_spawn_fails_puts_the_replaced_level_back() {
        let (mut app, id, root, _child) = app_with_level();
        crate::level::open_level(app.world_mut(), id, LoadMode::Replace).expect("open 1");
        app.update();
        let asset = Guid::new_random();
        let looping = SceneFile::from_ron_str(&format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{}", name: "Loop", components: {{}},
         prefab_instance: (prefab: "asset:{asset}", overrides: [])),
    ],
)
"#,
            Guid::new_random()
        ))
        .expect("parse prefab");
        let registry = app
            .world()
            .resource::<regia_ecs::reflect::AppTypeRegistry>()
            .0
            .clone();
        let template =
            crate::prefab::PrefabTemplate::from_scene_file(asset, &looping, &registry.read()).expect("bake");
        app.world_mut()
            .resource_mut::<crate::prefab::PrefabLibrary>()
            .insert(template);
        let id2 = LevelId(Guid::new_random());
        let level2 = Level::from_ron_str(
            id2,
            &format!(
                r#"
(
    format_version: 1,
    entities: [
        (guid: "{}", name: "Holder", components: {{}},
         prefab_instance: (prefab: "asset:{asset}", overrides: [])),
    ],
)
"#,
                Guid::new_random()
            ),
        )
        .expect("parse 2");
        assert!(level2.scene.validate().is_ok(), "the fixture is a valid file");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level2);
        assert!(crate::level::open_level(app.world_mut(), id2, LoadMode::Replace).is_err());
        app.update();
        let world = app.world_mut();
        assert_eq!(world.resource::<LevelManager>().current(), Some(id));
        assert_eq!(
            world.resource::<LevelManager>().state(id),
            Some(LevelState::Active)
        );
        assert_eq!(world.resource::<LevelManager>().state(id2), None);
        assert!(
            world
                .query::<&crate::components::EntityGuid>()
                .iter(world)
                .any(|g| g.0 == root),
            "the replaced level's root is back"
        );
    }

    /// The entities of `level` by guid, for "the respawn did not run again" assertions.
    fn member_entities(app: &mut App, level: LevelId) -> HashMap<Guid, Entity> {
        let world = app.world_mut();
        let mut query = world.query::<(Entity, &LevelMember, &crate::components::EntityGuid)>();
        query
            .iter(world)
            .filter(|(_, member, _)| member.level() == level)
            .map(|(entity, _, guid)| (guid.0, entity))
            .collect()
    }

    /// Audit fix lane (scene correctness, finding 1): a `Replace` open that starts before the
    /// level's data is in the library and then fails to spawn (the self-nesting prefab) is
    /// dropped after its one failed completion. The replaced level is put back once, and
    /// `level_lifecycle` must not retry the load on every frame (a retry would unload and
    /// respawn the restored level with new entity ids forever).
    #[test]
    fn a_pending_replace_open_whose_spawn_fails_is_dropped_after_one_attempt() {
        let (mut app, id, root, _child) = app_with_level();
        crate::level::open_level(app.world_mut(), id, LoadMode::Replace).expect("open 1");
        app.update();

        // The same self-nesting fixture as `a_replace_open_whose_spawn_fails_puts_the_replaced_level_back`.
        let asset = Guid::new_random();
        let looping = SceneFile::from_ron_str(&format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{}", name: "Loop", components: {{}},
         prefab_instance: (prefab: "asset:{asset}", overrides: [])),
    ],
)
"#,
            Guid::new_random()
        ))
        .expect("parse prefab");
        let registry = app
            .world()
            .resource::<regia_ecs::reflect::AppTypeRegistry>()
            .0
            .clone();
        let template =
            crate::prefab::PrefabTemplate::from_scene_file(asset, &looping, &registry.read()).expect("bake");
        app.world_mut()
            .resource_mut::<crate::prefab::PrefabLibrary>()
            .insert(template);
        let id2 = LevelId(Guid::new_random());
        let level2 = Level::from_ron_str(
            id2,
            &format!(
                r#"
(
    format_version: 1,
    entities: [
        (guid: "{}", name: "Holder", components: {{}},
         prefab_instance: (prefab: "asset:{asset}", overrides: [])),
    ],
)
"#,
                Guid::new_random()
            ),
        )
        .expect("parse 2");
        assert!(level2.scene.validate().is_ok(), "the fixture is a valid file");

        // Open before the data arrives: the level stays `Loading` and the call succeeds.
        crate::level::open_level(app.world_mut(), id2, LoadMode::Replace).expect("open pending");
        assert_eq!(
            app.world().resource::<LevelManager>().state(id2),
            Some(LevelState::Loading)
        );
        app.world_mut().resource_mut::<LevelLibrary>().insert(level2);

        app.update();
        assert_eq!(app.world().resource::<LevelManager>().current(), Some(id));
        assert_eq!(
            app.world().resource::<LevelManager>().state(id),
            Some(LevelState::Active)
        );
        assert_eq!(
            app.world().resource::<LevelManager>().state(id2),
            None,
            "a pending load whose spawn fails is dropped, not retried"
        );
        assert!(
            app.world().resource::<LevelManager>().pending_modes.is_empty(),
            "no pending bookkeeping left for the failed level"
        );
        let world = app.world_mut();
        assert!(
            world
                .query::<&crate::components::EntityGuid>()
                .iter(world)
                .any(|g| g.0 == root),
            "the replaced level's root is back"
        );

        // A retry would unload the restored level and respawn it with new entity ids: the
        // members must be identical across the next frame.
        let members = member_entities(&mut app, id);
        assert!(!members.is_empty());
        app.update();
        assert_eq!(
            member_entities(&mut app, id),
            members,
            "no second unload/respawn of the restored level"
        );
        assert_eq!(
            app.world().resource::<LevelManager>().state(id2),
            None,
            "still dropped on the following frame"
        );
    }

    /// Correctness lane ecs-scene: re-opening a level that is already `Active` with library data
    /// that now fails validation (a hot-reloaded file with a duplicate guid) returns the error
    /// and keeps the level: its entities stay, so the manager must keep its `Active` record.
    /// Dropping the record (the failed-load cleanup meant for `Loading` levels) left the entities
    /// in the world with no level that owns them: `state` said `None`, `unload_level` was a
    /// no-op, and the entities leaked until the world was cleared.
    #[test]
    fn a_failed_reopen_of_an_active_level_keeps_its_record() {
        for mode in [LoadMode::Replace, LoadMode::Additive] {
            let (mut app, id, root, _child) = app_with_level();
            crate::level::open_level(app.world_mut(), id, LoadMode::Replace).expect("open 1");
            app.update();
            let before = member_entities(&mut app, id);
            assert_eq!(before.len(), 2, "{mode:?}");

            let mut invalid =
                Level::from_ron_str(id, &level_text(root, Guid::new_random(), -9.81)).expect("parse invalid");
            let twin = invalid.scene.entities[0].clone();
            invalid.scene.entities.push(twin);
            assert!(invalid.scene.validate().is_err(), "the fixture must be invalid");
            app.world_mut().resource_mut::<LevelLibrary>().insert(invalid);

            assert!(
                crate::level::open_level(app.world_mut(), id, mode).is_err(),
                "{mode:?}: an invalid file does not open"
            );
            let manager = app.world().resource::<LevelManager>();
            assert_eq!(
                manager.state(id),
                Some(LevelState::Active),
                "{mode:?}: the level that is still spawned keeps its record"
            );
            assert_eq!(manager.current(), Some(id), "{mode:?}");
            assert_eq!(
                member_entities(&mut app, id),
                before,
                "{mode:?}: entities untouched"
            );

            // The record is what lets the level be unloaded: its entities must go with it.
            crate::level::unload_level(app.world_mut(), id).expect("unload");
            assert!(
                member_entities(&mut app, id).is_empty(),
                "{mode:?}: unloading the kept level despawns its entities"
            );
        }
    }

    /// How many entities carry `LevelMember` of `level` (duplicates by guid count twice).
    fn member_count(app: &mut App, level: LevelId) -> usize {
        let world = app.world_mut();
        let mut query = world.query::<&LevelMember>();
        query.iter(world).filter(|member| member.level() == level).count()
    }

    /// Correctness lane ecs-scene: re-opening an `Active` level whose data left the
    /// [`LevelLibrary`] records it `Loading` until the data is back. Its old entities are still
    /// spawned, so completing that load must reload in place, not stack a second copy that
    /// shares the guids. And when the returning data fails validation, the old copy stays and
    /// keeps an `Active` record.
    #[test]
    fn reopening_an_active_level_before_its_data_returns_reloads_in_place() {
        for mode in [LoadMode::Additive, LoadMode::Replace] {
            let (mut app, id, root, child) = app_with_level();
            crate::level::open_level(app.world_mut(), id, LoadMode::Replace).expect("open 1");
            app.update();
            assert_eq!(member_count(&mut app, id), 2, "{mode:?}");

            let data = app
                .world_mut()
                .resource_mut::<LevelLibrary>()
                .remove(id)
                .expect("in the library");
            crate::level::open_level(app.world_mut(), id, mode).expect("pending re-open");
            app.world_mut()
                .resource_mut::<LevelLibrary>()
                .insert(Level::clone(&data));
            app.update();
            let manager = app.world().resource::<LevelManager>();
            assert_eq!(manager.state(id), Some(LevelState::Active), "{mode:?}");
            assert_eq!(manager.current(), Some(id), "{mode:?}");
            assert_eq!(
                member_count(&mut app, id),
                2,
                "{mode:?}: the re-open reloads in place instead of stacking a second copy"
            );

            // Same again, but the data that returns fails validation: the old copy stays and
            // keeps its record, so it can still be unloaded.
            app.world_mut().resource_mut::<LevelLibrary>().remove(id);
            crate::level::open_level(app.world_mut(), id, mode).expect("pending re-open 2");
            let mut invalid =
                Level::from_ron_str(id, &level_text(root, child, -9.81)).expect("parse invalid");
            let twin = invalid.scene.entities[0].clone();
            invalid.scene.entities.push(twin);
            app.world_mut().resource_mut::<LevelLibrary>().insert(invalid);
            app.update();
            assert_eq!(
                app.world().resource::<LevelManager>().state(id),
                Some(LevelState::Active),
                "{mode:?}: the spawned copy keeps an Active record"
            );
            assert_eq!(member_count(&mut app, id), 2, "{mode:?}");
            crate::level::unload_level(app.world_mut(), id).expect("unload");
            assert_eq!(member_count(&mut app, id), 0, "{mode:?}: nothing leaks");
        }
    }

    /// Audit fix lane (scene correctness, finding 1, the validation early return): a `Replace`
    /// open that starts before the level's data is in the library and whose file fails
    /// validation is dropped too; the current level is not unloaded and keeps its entities.
    #[test]
    fn a_pending_replace_open_of_an_invalid_level_is_dropped() {
        let (mut app, id, root, _child) = app_with_level();
        crate::level::open_level(app.world_mut(), id, LoadMode::Replace).expect("open 1");
        app.update();
        let before = member_entities(&mut app, id);
        assert!(before.values().any(|entity| {
            app.world()
                .get::<crate::components::EntityGuid>(*entity)
                .is_some_and(|g| g.0 == root)
        }));

        let id2 = LevelId(Guid::new_random());
        let mut level2 = Level::from_ron_str(id2, &level_text(Guid::new_random(), Guid::new_random(), -9.81))
            .expect("parse 2");
        let twin = level2.scene.entities[0].clone();
        level2.scene.entities.push(twin);
        assert!(level2.scene.validate().is_err(), "the fixture must be invalid");

        // Open before the data arrives, then insert the invalid file.
        crate::level::open_level(app.world_mut(), id2, LoadMode::Replace).expect("open pending");
        assert_eq!(
            app.world().resource::<LevelManager>().state(id2),
            Some(LevelState::Loading)
        );
        app.world_mut().resource_mut::<LevelLibrary>().insert(level2);
        app.update();

        assert_eq!(app.world().resource::<LevelManager>().current(), Some(id));
        assert_eq!(
            app.world().resource::<LevelManager>().state(id2),
            None,
            "a pending load whose file fails validation is dropped"
        );
        assert!(
            app.world().resource::<LevelManager>().pending_modes.is_empty(),
            "no pending bookkeeping left for the failed level"
        );
        assert_eq!(
            member_entities(&mut app, id),
            before,
            "the current level kept its entities"
        );
    }

    #[test]
    fn replace_unloads_previous_level_but_keeps_persistent() {
        let (mut app, id, root, _child) = app_with_level();
        {
            let world = app.world_mut();
            crate::level::open_level(world, id, LoadMode::Replace).expect("open 1");
        }
        app.update();

        // Mark the prop entity persistent.
        let mut prop = None;
        {
            let world = app.world_mut();
            for (entity, guid, _) in world
                .query::<(Entity, &crate::components::EntityGuid, &LevelMember)>()
                .iter(world)
            {
                if guid.0 != root {
                    prop = Some(entity);
                }
            }
        }
        let prop = prop.expect("prop entity");
        app.world_mut().entity_mut(prop).insert(Persistent);

        // Load another level in Replace mode.
        let id2 = LevelId(Guid::new_random());
        let root2 = Guid::new_random();
        let child2 = Guid::new_random();
        {
            let world = app.world_mut();
            let level2 = Level::from_ron_str(id2, &level_text(root2, child2, -9.81)).expect("parse 2");
            world.resource_mut::<LevelLibrary>().insert(level2);
            crate::level::open_level(world, id2, LoadMode::Replace).expect("open 2");
        }
        app.update();

        let world = app.world_mut();
        let manager = world.resource::<LevelManager>();
        assert_eq!(manager.current(), Some(id2));
        assert_eq!(manager.state(id), None, "old level unloaded");
        // Old root gone, persistent prop survived, new level present.
        assert!(
            !world
                .query::<(Entity, &crate::components::EntityGuid)>()
                .iter(world)
                .any(|(_, g)| g.0 == root),
            "old level root despawned"
        );
        assert!(
            world.get::<crate::components::EntityGuid>(prop).is_some(),
            "persistent kept"
        );
        assert!(
            world
                .query::<(Entity, &crate::components::EntityGuid)>()
                .iter(world)
                .any(|(_, g)| g.0 == root2),
            "new level root present"
        );
    }

    #[test]
    fn additive_keeps_previous_and_unload_removes_only_that_level() {
        let (mut app, id, _root, _child) = app_with_level();
        {
            let world = app.world_mut();
            crate::level::open_level(world, id, LoadMode::Replace).expect("open 1");
        }
        let id2 = LevelId(Guid::new_random());
        {
            let world = app.world_mut();
            let level2 = Level::from_ron_str(id2, &level_text(Guid::new_random(), Guid::new_random(), -9.81))
                .expect("parse 2");
            world.resource_mut::<LevelLibrary>().insert(level2);
            crate::level::open_level(world, id2, LoadMode::Additive).expect("open 2");
        }
        app.update();
        {
            let world = app.world();
            // Additive loading does not move the primary level.
            assert_eq!(world.resource::<LevelManager>().current(), Some(id));
            assert_eq!(
                world.resource::<LevelManager>().state(id2),
                Some(LevelState::Active)
            );
        }

        // Unload the additive level: the primary stays.
        {
            let world = app.world_mut();
            crate::level::unload_level(world, id2).expect("unload");
        }
        app.update();
        {
            let world = app.world();
            let manager = world.resource::<LevelManager>();
            assert_eq!(manager.current(), Some(id));
            assert_eq!(manager.state(id2), None);
        }
    }

    /// A streamed region loaded on top keeps the open map's settings (its look, sun, baked GI;
    /// ADR-1097); a level opened additively into an empty world still brings its own.
    #[test]
    fn an_additive_load_keeps_the_current_levels_settings() {
        let (mut app, id, _root, _child) = app_with_level();
        let id2 = LevelId(Guid::new_random());
        {
            let world = app.world_mut();
            let level2 = Level::from_ron_str(id2, &level_text(Guid::new_random(), Guid::new_random(), -3.7))
                .expect("parse 2");
            world.resource_mut::<LevelLibrary>().insert(level2);
            crate::level::open_level(world, id2, LoadMode::Additive).expect("open into an empty world");
        }
        assert_eq!(app.world().resource::<LevelManager>().current(), Some(id2));
        assert_eq!(
            app.world().resource::<LevelSettings>().gravity,
            Vec3::new(0.0, -3.7, 0.0)
        );
        {
            let world = app.world_mut();
            crate::level::open_level(world, id, LoadMode::Replace).expect("open 1");
            crate::level::open_level(world, id2, LoadMode::Additive).expect("stream 2 on top");
        }
        app.update();
        let world = app.world();
        assert_eq!(world.resource::<LevelManager>().current(), Some(id));
        assert_eq!(
            world.resource::<LevelManager>().state(id2),
            Some(LevelState::Active)
        );
        assert_eq!(
            world.resource::<LevelSettings>().gravity,
            Vec3::new(0.0, -20.0, 0.0),
            "the streamed level did not replace the map's settings"
        );
    }

    #[test]
    fn unload_level_keeps_entities_that_belong_to_another_level() {
        let (mut app, id, root, _child) = app_with_level();
        let id2 = LevelId(Guid::new_random());
        let world = app.world_mut();
        crate::level::open_level(world, id, LoadMode::Replace).expect("open 1");
        let level2 = Level::from_ron_str(id2, &level_text(Guid::new_random(), Guid::new_random(), -1.6))
            .expect("parse 2");
        world.resource_mut::<LevelLibrary>().insert(level2);
        crate::level::open_level(world, id2, LoadMode::Additive).expect("open 2");
        let root_a = world
            .query::<(Entity, &crate::components::EntityGuid)>()
            .iter(world)
            .find(|(_, g)| g.0 == root)
            .map(|(e, _)| e)
            .expect("level A root");
        let prop_b = world
            .query::<(Entity, &LevelMember)>()
            .iter(world)
            .find(|(_, m)| m.level() == id2)
            .map(|(e, _)| e)
            .expect("level B member");
        world.entity_mut(prop_b).insert(ChildOf(root_a));
        crate::level::unload_level(world, id).expect("unload A");
        assert!(
            world.entities().contains(prop_b),
            "B's entity survived A's unload"
        );
        assert_eq!(world.get::<LevelMember>(prop_b).map(|m| m.level()), Some(id2));
        assert_eq!(
            world.resource::<LevelManager>().state(id2),
            Some(LevelState::Active)
        );
    }

    #[test]
    fn additive_load_preserves_primary_level_settings() {
        let (mut app, id, _root, _child) = app_with_level();
        let id2 = LevelId(Guid::new_random());
        let world = app.world_mut();
        crate::level::open_level(world, id, LoadMode::Replace).expect("open 1");
        let level2 = Level::from_ron_str(id2, &level_text(Guid::new_random(), Guid::new_random(), -1.6))
            .expect("parse 2");
        world.resource_mut::<LevelLibrary>().insert(level2);
        crate::level::open_level(world, id2, LoadMode::Additive).expect("open 2");
        assert_eq!(current_settings(world).gravity.y, -20.0);
        crate::level::unload_level(world, id2).expect("unload 2");
        assert_eq!(current_settings(world).gravity.y, -20.0);
        let snap = snapshot_level(world, id).expect("snapshot");
        assert_eq!(snap.settings.gravity.y, -20.0);
    }

    /// Opens `id` (primary, gravity -20) and a second level (additive, gravity -1.6).
    fn two_levels() -> (App, LevelId, LevelId) {
        let (mut app, id, _root, _child) = app_with_level();
        let id2 = LevelId(Guid::new_random());
        let world = app.world_mut();
        crate::level::open_level(world, id, LoadMode::Replace).expect("open 1");
        let level2 = Level::from_ron_str(id2, &level_text(Guid::new_random(), Guid::new_random(), -1.6))
            .expect("parse 2");
        world.resource_mut::<LevelLibrary>().insert(level2);
        crate::level::open_level(world, id2, LoadMode::Additive).expect("open 2");
        (app, id, id2)
    }

    #[test]
    fn an_additive_level_saves_its_own_settings() {
        let (mut app, id, id2) = two_levels();
        let world = app.world_mut();
        let snap = snapshot_level(world, id2).expect("snapshot additive level");
        assert_eq!(snap.settings.gravity.y, -1.6, "the additive level's own gravity");
        assert_eq!(
            current_settings(world).gravity.y,
            -20.0,
            "the primary settings are put back"
        );
        let snap = snapshot_level(world, id).expect("snapshot primary level");
        assert_eq!(snap.settings.gravity.y, -20.0);
    }

    #[test]
    fn unloading_the_primary_hands_over_to_the_remaining_level() {
        let (mut app, id, id2) = two_levels();
        let world = app.world_mut();
        crate::level::unload_level(world, id).expect("unload primary");
        let manager = world.resource::<LevelManager>();
        assert_eq!(manager.current(), Some(id2));
        assert_eq!(current_settings(world).gravity.y, -1.6);
        crate::level::unload_level(world, id2).expect("unload last");
        assert_eq!(world.resource::<LevelManager>().current(), None);
    }

    #[test]
    fn cross_level_detach_keeps_the_survivors_world_pose() {
        use crate::transform::{GlobalTransform, Transform};
        let mut world = World::new();
        let level_a = LevelId(Guid::new_random());
        let level_b = LevelId(Guid::new_random());
        let root = world
            .spawn((
                Transform::from_translation(Vec3::new(100.0, 0.0, 0.0)),
                LevelMember::from_level(level_a),
            ))
            .id();
        let prop = world
            .spawn((
                Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
                LevelMember::from_level(level_b),
                ChildOf(root),
            ))
            .id();
        crate::propagate::propagate_transforms(&mut world);
        despawn_level_entities(&mut world, level_a);
        crate::propagate::propagate_transforms(&mut world);
        assert!(world.get_entity(root).is_err());
        let global = world.get::<GlobalTransform>(prop).expect("global");
        assert_eq!(global.translation(), Vec3::new(101.0, 0.0, 0.0));
        assert!(world.get::<ChildOf>(prop).is_none());
    }

    #[test]
    fn a_parent_cycle_in_another_level_does_not_hang_unload() {
        let mut world = World::new();
        let level_a = LevelId(Guid::new_random());
        let level_b = LevelId(Guid::new_random());
        world.spawn(LevelMember::from_level(level_a));
        let x = world.spawn(LevelMember::from_level(level_b)).id();
        let y = world.spawn((LevelMember::from_level(level_b), ChildOf(x))).id();
        world.entity_mut(x).insert(ChildOf(y));
        despawn_level_entities(&mut world, level_a);
        assert!(world.entities().contains(x) && world.entities().contains(y));
    }

    #[test]
    fn an_overlay_opened_in_the_same_batch_survives_the_base_replace() {
        for overlay_first in [false, true] {
            let mut app = App::new();
            app.add_plugins(crate::ScenePlugin);
            // The overlay's id sorts before the base's.
            let overlay = LevelId(Guid::from_u128(1));
            let base = LevelId(Guid::from_u128(9));
            let world = app.world_mut();
            if overlay_first {
                crate::level::open_level(world, overlay, LoadMode::Additive).expect("open overlay");
                crate::level::open_level(world, base, LoadMode::Replace).expect("open base");
            } else {
                crate::level::open_level(world, base, LoadMode::Replace).expect("open base");
                crate::level::open_level(world, overlay, LoadMode::Additive).expect("open overlay");
            }
            for id in [overlay, base] {
                let level =
                    Level::from_ron_str(id, &level_text(Guid::new_random(), Guid::new_random(), -9.81))
                        .expect("parse");
                world.resource_mut::<LevelLibrary>().insert(level);
            }
            app.update();
            let manager = app.world().resource::<LevelManager>();
            assert_eq!(manager.current(), Some(base), "overlay_first={overlay_first}");
            assert_eq!(
                manager.state(overlay),
                Some(LevelState::Active),
                "overlay_first={overlay_first}"
            );
            assert_eq!(manager.state(base), Some(LevelState::Active));
        }
    }

    #[test]
    fn reopening_a_level_adopts_its_persistent_entities() {
        let (mut app, id, root, child) = app_with_level();
        let world = app.world_mut();
        crate::level::open_level(world, id, LoadMode::Replace).expect("open");
        let player = world
            .query::<(Entity, &crate::components::EntityGuid)>()
            .iter(world)
            .find(|(_, g)| g.0 == root)
            .map(|(e, _)| e)
            .expect("root entity");
        world.entity_mut(player).insert(Persistent);
        crate::level::open_level(world, id, LoadMode::Additive).expect("reopen");
        let roots: Vec<Entity> = world
            .query::<(Entity, &crate::components::EntityGuid)>()
            .iter(world)
            .filter(|(_, g)| g.0 == root)
            .map(|(e, _)| e)
            .collect();
        assert_eq!(roots, vec![player], "one root, the surviving persistent entity");
        let children: Vec<Entity> = world
            .query::<(Entity, &crate::components::EntityGuid)>()
            .iter(world)
            .filter(|(_, g)| g.0 == child)
            .map(|(e, _)| e)
            .collect();
        assert_eq!(children.len(), 1);
        assert_eq!(
            world.get::<ChildOf>(children[0]).map(|c| c.parent()),
            Some(player)
        );
    }

    #[test]
    fn unparseable_level_settings_on_a_current_file_fail_the_load() {
        let text = format!(
            r#"( format_version: 10, entities: [], resources: {{ "{LEVEL_SETTINGS_PATH}": (gravity: "nope") }} )"#
        );
        let err = Level::from_ron_str(LevelId(Guid::new_random()), &text);
        assert!(err.is_err(), "bad settings must not load as defaults");
    }

    #[test]
    fn try_unload_unknown_level_errors_but_unload_noops() {
        let (mut app, _id, _root, _child) = app_with_level();
        let unknown = LevelId(Guid::new_random());
        {
            let world = app.world_mut();
            assert!(matches!(
                crate::level::try_unload_level(world, unknown),
                Err(LevelNotLoaded(found)) if found == unknown
            ));
            assert!(
                crate::level::unload_level(world, unknown).is_ok(),
                "no-op, not an error"
            );
        }
        assert!(app.world().resource::<LevelManager>().current().is_none());
    }

    #[test]
    fn try_unload_loaded_level_succeeds() {
        let (mut app, id, _root, _child) = app_with_level();
        {
            let world = app.world_mut();
            crate::level::open_level(world, id, LoadMode::Replace).expect("open");
        }
        app.update();
        {
            let world = app.world_mut();
            crate::level::try_unload_level(world, id).expect("unload");
        }
        assert_eq!(app.world().resource::<LevelManager>().state(id), None);
    }

    #[test]
    fn open_before_data_completes_when_data_arrives() {
        let mut app = App::new();
        app.add_plugins(crate::ScenePlugin);
        let id = LevelId(Guid::new_random());
        {
            let world = app.world_mut();
            crate::level::open_level(world, id, LoadMode::Replace).expect("open");
        }
        {
            let world = app.world();
            assert_eq!(
                world.resource::<LevelManager>().state(id),
                Some(LevelState::Loading)
            );
        }

        // Data arrives; the PreUpdate lifecycle system completes the load.
        let level = Level::from_ron_str(id, &level_text(Guid::new_random(), Guid::new_random(), -9.81))
            .expect("parse");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        app.update();

        let world = app.world();
        assert_eq!(
            world.resource::<LevelManager>().state(id),
            Some(LevelState::Active)
        );
        assert_eq!(world.resource::<LevelManager>().current(), Some(id));
    }

    #[test]
    fn levels_arriving_on_one_frame_complete_in_submission_order() {
        // Six additive levels open before their data exists, and all of it arrives on one frame.
        // Completion order decides the current level; it must follow the order the levels were
        // opened in (then the ids), not the manager's per-process hash order (ADR-867). The
        // first to complete becomes current and keeps its `LevelSettings`: the later additive
        // loads do not replace them (ADR-1097).
        let mut app = App::new();
        app.add_plugins(crate::ScenePlugin);
        let ids: Vec<LevelId> = (0..6).map(|_| LevelId(Guid::new_random())).collect();
        for id in &ids {
            crate::level::open_level(app.world_mut(), *id, LoadMode::Additive).expect("open");
        }
        let mut sorted = ids.clone();
        sorted.sort();
        for id in &ids {
            let rank = sorted.iter().position(|s| s == id).unwrap_or(0) as f32;
            let level = Level::from_ron_str(
                *id,
                &level_text(Guid::new_random(), Guid::new_random(), -1.0 - rank),
            )
            .expect("parse");
            app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        }
        app.update();
        let world = app.world();
        let manager = world.resource::<LevelManager>();
        assert!(
            ids.iter()
                .all(|id| manager.state(*id) == Some(LevelState::Active))
        );
        assert_eq!(
            manager.current(),
            ids.first().copied(),
            "the first opened level completes first"
        );
        let first_rank = sorted.iter().position(|s| Some(s) == ids.first()).unwrap_or(0) as f32;
        assert_eq!(
            world.resource::<LevelSettings>().gravity,
            Vec3::new(0.0, -1.0 - first_rank, 0.0),
            "the current level's settings stay"
        );
    }

    #[test]
    fn level_round_trips_through_text_with_settings() {
        let id = LevelId(Guid::new_random());
        let level = Level::from_ron_str(id, &level_text(Guid::new_random(), Guid::new_random(), -3.7))
            .expect("parse");
        let text = level.to_ron_string().expect("write");
        let back = Level::from_ron_str(id, &text).expect("reparse");
        assert_eq!(back.settings, level.settings);
        assert_eq!(back.scene.entities.len(), 2);
        assert!(back.scene.resources.contains_key(LEVEL_SETTINGS_PATH));
    }

    #[test]
    fn simultaneous_replace_loads_complete_in_level_id_order() {
        let mut app = App::new();
        app.add_plugins(crate::ScenePlugin);
        let ids = [
            LevelId(Guid::from_u128(5)),
            LevelId(Guid::from_u128(2)),
            LevelId(Guid::from_u128(9)),
        ];
        for id in ids {
            crate::level::open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        }
        for id in ids {
            let level = Level::from_ron_str(id, &level_text(Guid::new_random(), Guid::new_random(), -9.81))
                .expect("parse");
            app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        }
        app.update();
        let manager = app.world().resource::<LevelManager>();
        // Completed 2, 5, 9 in order; each Replace unloaded the previous, leaving only 9.
        assert_eq!(manager.current(), Some(LevelId(Guid::from_u128(9))));
    }

    #[test]
    fn snapshot_level_captures_entities_and_settings() {
        let (mut app, id, _root, _child) = app_with_level();
        {
            let world = app.world_mut();
            crate::level::open_level(world, id, LoadMode::Replace).expect("open");
        }
        app.update();

        let snapshot = snapshot_level(app.world_mut(), id).expect("snapshot");
        assert_eq!(snapshot.id, id);
        assert_eq!(snapshot.scene.entities.len(), 2);
        assert_eq!(snapshot.settings.gravity, Vec3::new(0.0, -20.0, 0.0));

        // The snapshot reloads into a fresh world with the same content.
        let text = snapshot.to_ron_string().expect("write");
        let reparsed = Level::from_ron_str(id, &text).expect("reparse");
        assert_eq!(reparsed.scene.entities.len(), 2);
    }

    #[test]
    fn settings_default_to_earth_gravity() {
        assert_eq!(LevelSettings::default().gravity, Vec3::new(0.0, -9.81, 0.0));
        let settings: LevelSettings = ron::from_str("()").expect("unit");
        assert_eq!(settings.gravity, Vec3::new(0.0, -9.81, 0.0));
    }

    #[test]
    fn aerosol_turbidity_new_levels_are_clear_day_and_old_levels_keep_today() {
        // A new Earth level (Rust default) starts on a typical clear day.
        assert_eq!(
            LevelAtmosphere::default().aerosol_turbidity,
            CLEAR_DAY_AEROSOL_TURBIDITY
        );
        assert_eq!(LevelSettings::default().atmosphere.aerosol_turbidity, 0.1);
        // Today's atmosphere is the serde default, so old levels keep their look.
        assert_eq!(
            LevelAtmosphere::today().aerosol_turbidity,
            TODAY_AEROSOL_TURBIDITY
        );
        assert_eq!(LevelAtmosphere::today().preset, AtmospherePreset::Earth);
        // An existing level that omits the field keeps today's clean-air look (serde default).
        let omitted: LevelAtmosphere = ron::from_str("(preset: Earth)").expect("parse");
        assert_eq!(omitted.aerosol_turbidity, TODAY_AEROSOL_TURBIDITY);
        // A level with no `atmosphere` block at all (pre-v2) also loads as today's Earth.
        let full: LevelSettings = ron::from_str("()").expect("unit settings");
        assert_eq!(full.atmosphere.aerosol_turbidity, TODAY_AEROSOL_TURBIDITY);
        // An authored value round-trips.
        let explicit: LevelAtmosphere = ron::from_str("(aerosol_turbidity: 0.05)").expect("parse");
        assert_eq!(explicit.aerosol_turbidity, 0.05);
    }
}
