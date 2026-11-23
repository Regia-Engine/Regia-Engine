// SPDX-License-Identifier: MPL-2.0
//! `config/weather.ron` (format v1, spec 99 § 7.1): which weather effects a project runs, its
//! named weathers and the numbers of the model. A missing file means every default, and every
//! effect off. Registered with `cargo xtask check-formats`; `docs/formats/weather-ron.md`.

use regia_app::prelude::*;
use regia_platform::{MountPoint, Vfs, VfsError, VfsPath};
use regia_scene::weather::SceneWindMode;
use serde::{Deserialize, Serialize};

use super::{PrecipitationKind, WEATHER_PRESETS, WeatherState};

/// Version of `config/weather.ron` this build reads and writes.
pub const WEATHER_CONFIG_FORMAT_VERSION: u32 = 1;
/// Where the config lives, relative to the `config://` mount.
pub const WEATHER_CONFIG_PATH: &str = "weather.ron";

/// Why `config/weather.ron` was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum WeatherConfigError {
    /// The file is newer than this build.
    #[error("weather.ron format_version {found} is not supported (this build reads {supported})")]
    Version {
        /// The file's version.
        found: u32,
        /// The version this build reads.
        supported: u32,
    },
    /// The file does not parse, has an unknown field, or a value is out of range.
    #[error("weather.ron: {0}")]
    Invalid(String),
}

impl WeatherConfigError {
    /// The agent protocol's code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Version { .. } => "weather_config_version",
            Self::Invalid(_) => "weather_config_invalid",
        }
    }
}

/// Which weather effects run. Each is off unless the project turns it on, so a host that already
/// has a `Weather` for its clouds renders the same frames.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WeatherEffects {
    /// Rain and snow drawn, and the air they thicken.
    pub precipitation: bool,
    /// Surfaces get wet and collect snow.
    pub surface: bool,
    /// The weather's wind reaches the ground as a `SceneWind`.
    pub scene_wind: bool,
}

/// A named weather beside the built-in ones: `base` with the given fields replaced.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WeatherPresetDef {
    /// The name `render.set_weather` and [`WeatherConfig::preset`] use.
    pub name: String,
    /// The built-in preset, or an earlier config preset, it starts from.
    pub base: String,
    /// Replaces [`WeatherState::coverage`].
    pub coverage: Option<f32>,
    /// Replaces [`WeatherState::cloud_type`].
    pub cloud_type: Option<f32>,
    /// Replaces [`WeatherState::density`].
    pub density: Option<f32>,
    /// Replaces [`WeatherState::wind_m_per_s`].
    pub wind_m_per_s: Option<[f32; 2]>,
    /// Replaces [`WeatherState::precipitation`].
    pub precipitation: Option<f32>,
    /// Replaces [`WeatherState::temperature_c`].
    pub temperature_c: Option<f32>,
    /// Replaces [`WeatherState::precipitation_kind`].
    pub precipitation_kind: Option<PrecipitationKind>,
    /// Replaces [`WeatherState::gustiness`].
    pub gustiness: Option<f32>,
}

/// The numbers of the precipitation model (§ 3.2).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrecipitationConfig {
    /// The rain rate at `precipitation` 1, mm/h.
    pub max_rain_mm_per_h: f32,
    /// The snow rate at `precipitation` 1, mm/h of water.
    pub max_snow_we_mm_per_h: f32,
    /// All snow at or below this temperature, °C.
    pub snow_all_below_c: f32,
    /// All rain at or above this temperature, °C.
    pub rain_all_above_c: f32,
    /// Fresh snow depth per depth of water.
    pub snow_ratio: f32,
    /// Only drops above this size are drawn, mm.
    pub visible_drop_mm: f32,
    /// Multiplies the drawn counts, `0..=4`.
    pub density_scale: f32,
    /// The shutter the streaks are stretched by, seconds.
    pub streak_exposure_s: f32,
    /// Draw splashes on the tiers that have them.
    pub splashes: bool,
}

impl Default for PrecipitationConfig {
    fn default() -> Self {
        Self {
            max_rain_mm_per_h: 50.0,
            max_snow_we_mm_per_h: 5.0,
            snow_all_below_c: 0.0,
            rain_all_above_c: 2.5,
            snow_ratio: 10.0,
            visible_drop_mm: 1.0,
            density_scale: 1.0,
            streak_exposure_s: 0.0167,
            splashes: true,
        }
    }
}

/// How the weather's wind reaches the ground (§ 3.3).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindCoupling {
    /// The mode of the `SceneWind` the plugin inserts.
    pub mode: SceneWindMode,
    /// Multiplies the cloud wind; `None` is the height profile `(10 m / cloud base)^(1/7)`.
    pub surface_factor: Option<f32>,
    /// Turns the surface wind relative to the wind aloft, degrees.
    pub veer_deg: f32,
    /// Multiplies the weather's gustiness, `0..=4`.
    pub gust_scale: f32,
}

impl Default for WindCoupling {
    fn default() -> Self {
        Self {
            mode: SceneWindMode::Weather,
            surface_factor: None,
            veer_deg: 0.0,
            gust_scale: 1.0,
        }
    }
}

/// The rates of the surface state (§ 3.4).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SurfaceConfig {
    /// Water that wets a surface fully, mm.
    pub wet_mm: f32,
    /// Seconds a wet surface takes to dry at a drying factor of 1.
    pub dry_time_s: f32,
    /// Water the ground takes without pooling, mm/h.
    pub drain_mm_per_h: f32,
    /// Water that fills a hollow, mm.
    pub puddle_mm: f32,
    /// Seconds a full hollow takes to empty at a drying factor of 1.
    pub puddle_dry_time_s: f32,
    /// Snow depth that hides the ground, m.
    pub full_cover_m: f32,
    /// Degree-day melt, mm of water per °C per day.
    pub melt_mm_per_c_day: f32,
    /// Multiplies the melt (a realistic melt takes days of game time).
    pub melt_scale: f32,
}

impl Default for SurfaceConfig {
    fn default() -> Self {
        Self {
            wet_mm: 0.2,
            dry_time_s: 1800.0,
            drain_mm_per_h: 2.5,
            puddle_mm: 5.0,
            puddle_dry_time_s: 3600.0,
            full_cover_m: 0.03,
            melt_mm_per_c_day: 4.0,
            melt_scale: 1.0,
        }
    }
}

/// What the falling rain and snow do to the air (§ 3.2).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VisibilityConfig {
    /// Thicken the air while precipitation draws.
    pub couple_fog: bool,
    /// Visibility in the heaviest rain, m.
    pub rain_visibility_m: f32,
    /// Visibility in the heaviest snow, m.
    pub snow_visibility_m: f32,
}

impl Default for VisibilityConfig {
    fn default() -> Self {
        Self {
            couple_fog: true,
            rain_visibility_m: 3000.0,
            snow_visibility_m: 400.0,
        }
    }
}

/// The project's weather config: `config/weather.ron`, or the defaults without one.
#[derive(Resource, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WeatherConfig {
    /// File format version.
    pub format_version: u32,
    /// Named weathers beside the built-in ones.
    pub presets: Vec<WeatherPresetDef>,
    /// Which effects run.
    pub effects: WeatherEffects,
    /// The preset to start in when a level opens without a `Weather`; `None` leaves the sky alone.
    pub start: Option<String>,
    /// The precipitation model.
    pub precipitation: PrecipitationConfig,
    /// The coupling of the wind to the ground.
    pub wind: WindCoupling,
    /// The surface state's rates.
    pub surface: SurfaceConfig,
    /// The air.
    pub visibility: VisibilityConfig,
}

impl Default for WeatherConfig {
    fn default() -> Self {
        Self {
            format_version: WEATHER_CONFIG_FORMAT_VERSION,
            presets: Vec::new(),
            effects: WeatherEffects::default(),
            start: None,
            precipitation: PrecipitationConfig::default(),
            wind: WindCoupling::default(),
            surface: SurfaceConfig::default(),
            visibility: VisibilityConfig::default(),
        }
    }
}

/// Reads the version alone: a newer file may have fields this build does not know.
#[derive(Deserialize)]
struct VersionProbe {
    #[serde(default = "first_version")]
    format_version: u32,
}

fn first_version() -> u32 {
    WEATHER_CONFIG_FORMAT_VERSION
}

fn invalid(message: impl Into<String>) -> WeatherConfigError {
    WeatherConfigError::Invalid(message.into())
}

fn positive(name: &str, value: f32) -> Result<(), WeatherConfigError> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(invalid(format!("`{name}` must be positive, got {value}")))
    }
}

fn scale(name: &str, value: f32) -> Result<(), WeatherConfigError> {
    if value.is_finite() && (0.0..=4.0).contains(&value) {
        Ok(())
    } else {
        Err(invalid(format!("`{name}` must be in 0..=4, got {value}")))
    }
}

impl WeatherConfig {
    /// Parses `config/weather.ron` and validates it.
    ///
    /// # Errors
    /// [`WeatherConfigError::Version`] for a newer file, [`WeatherConfigError::Invalid`] for a
    /// parse failure, an unknown field, a preset name clash or a value out of range.
    pub fn from_ron(text: &str) -> Result<Self, WeatherConfigError> {
        let probe: VersionProbe = ron::from_str(text).map_err(|e| invalid(e.to_string()))?;
        if probe.format_version != WEATHER_CONFIG_FORMAT_VERSION {
            return Err(WeatherConfigError::Version {
                found: probe.format_version,
                supported: WEATHER_CONFIG_FORMAT_VERSION,
            });
        }
        let config: Self = ron::from_str(text).map_err(|e| invalid(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Serializes to RON text.
    ///
    /// # Errors
    /// [`WeatherConfigError::Invalid`] if serialization fails.
    pub fn to_ron(&self) -> Result<String, WeatherConfigError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty()).map_err(|e| invalid(e.to_string()))
    }

    /// Reads `config://weather.ron`. A missing file (or no `config://` mount) is `Ok(None)`.
    ///
    /// # Errors
    /// What [`Self::from_ron`] refuses, and an unreadable file as
    /// [`WeatherConfigError::Invalid`].
    pub fn load(vfs: &Vfs) -> Result<Option<Self>, WeatherConfigError> {
        let path = VfsPath::root(MountPoint::Config).join(WEATHER_CONFIG_PATH);
        let path = path.map_err(|e| invalid(e.to_string()))?;
        match vfs.read_sync(&path) {
            Ok(bytes) => {
                let text = std::str::from_utf8(&bytes).map_err(|e| invalid(e.to_string()))?;
                Self::from_ron(text).map(Some)
            }
            Err(VfsError::NotFound { .. } | VfsError::NotMounted { .. }) => Ok(None),
            Err(err) => Err(invalid(err.to_string())),
        }
    }

    /// Checks the ranges of § 7.1, the preset names and the start preset.
    ///
    /// # Errors
    /// [`WeatherConfigError::Invalid`] naming the field.
    pub fn validate(&self) -> Result<(), WeatherConfigError> {
        let p = &self.precipitation;
        positive("precipitation.max_rain_mm_per_h", p.max_rain_mm_per_h)?;
        positive("precipitation.max_snow_we_mm_per_h", p.max_snow_we_mm_per_h)?;
        positive("precipitation.snow_ratio", p.snow_ratio)?;
        positive("precipitation.visible_drop_mm", p.visible_drop_mm)?;
        positive("precipitation.streak_exposure_s", p.streak_exposure_s)?;
        scale("precipitation.density_scale", p.density_scale)?;
        if !p.snow_all_below_c.is_finite()
            || !p.rain_all_above_c.is_finite()
            || p.snow_all_below_c >= p.rain_all_above_c
        {
            return Err(invalid(
                "`precipitation.snow_all_below_c` must be below `precipitation.rain_all_above_c`",
            ));
        }
        let w = &self.wind;
        if let Some(factor) = w.surface_factor {
            positive("wind.surface_factor", factor)?;
        }
        if !w.veer_deg.is_finite() {
            return Err(invalid("`wind.veer_deg` must be finite"));
        }
        scale("wind.gust_scale", w.gust_scale)?;
        let s = &self.surface;
        positive("surface.wet_mm", s.wet_mm)?;
        positive("surface.dry_time_s", s.dry_time_s)?;
        positive("surface.drain_mm_per_h", s.drain_mm_per_h)?;
        positive("surface.puddle_mm", s.puddle_mm)?;
        positive("surface.puddle_dry_time_s", s.puddle_dry_time_s)?;
        positive("surface.full_cover_m", s.full_cover_m)?;
        positive("surface.melt_mm_per_c_day", s.melt_mm_per_c_day)?;
        positive("surface.melt_scale", s.melt_scale)?;
        positive("visibility.rain_visibility_m", self.visibility.rain_visibility_m)?;
        positive("visibility.snow_visibility_m", self.visibility.snow_visibility_m)?;
        for (index, def) in self.presets.iter().enumerate() {
            if WEATHER_PRESETS.contains(&def.name.as_str()) {
                return Err(invalid(format!(
                    "preset `{}` clashes with a built-in preset",
                    def.name
                )));
            }
            if def.name.is_empty() {
                return Err(invalid("a preset has no name"));
            }
            if self.presets[..index].iter().any(|other| other.name == def.name) {
                return Err(invalid(format!("preset `{}` is defined twice", def.name)));
            }
            let earlier = self.presets[..index].iter().any(|other| other.name == def.base);
            if !earlier && WeatherState::preset(&def.base).is_none() {
                return Err(invalid(format!(
                    "preset `{}`: unknown base `{}`",
                    def.name, def.base
                )));
            }
        }
        if let Some(start) = &self.start
            && self.preset(start).is_none()
        {
            return Err(invalid(format!("`start`: unknown preset `{start}`")));
        }
        Ok(())
    }

    /// The named weather: a config preset, else a built-in one.
    #[must_use]
    pub fn preset(&self, name: &str) -> Option<WeatherState> {
        let Some(index) = self.presets.iter().position(|def| def.name == name) else {
            return WeatherState::preset(name);
        };
        // A base is built in or defined earlier, so this walks up the list and ends.
        let def = &self.presets[index];
        let earlier = Self {
            presets: self.presets[..index].to_vec(),
            ..Self::default()
        };
        let base = earlier.preset(&def.base)?;
        Some(
            WeatherState {
                coverage: def.coverage.unwrap_or(base.coverage),
                cloud_type: def.cloud_type.unwrap_or(base.cloud_type),
                density: def.density.unwrap_or(base.density),
                wind_m_per_s: def.wind_m_per_s.unwrap_or(base.wind_m_per_s),
                precipitation: def.precipitation.unwrap_or(base.precipitation),
                genus: base.genus,
                temperature_c: def.temperature_c.unwrap_or(base.temperature_c),
                precipitation_kind: def.precipitation_kind.unwrap_or(base.precipitation_kind),
                gustiness: def.gustiness.unwrap_or(base.gustiness),
            }
            .sanitized(),
        )
    }

    /// The built-in preset names, then the config's.
    #[must_use]
    pub fn preset_names(&self) -> Vec<String> {
        WEATHER_PRESETS
            .iter()
            .map(|name| (*name).to_owned())
            .chain(self.presets.iter().map(|def| def.name.clone()))
            .collect()
    }
}

/// What became of `config/weather.ron` at startup (`render.get_weather`'s `config`).
#[derive(Resource, Clone, Debug, Default, PartialEq)]
pub struct WeatherConfigStatus {
    /// The file that was read; `None` when the defaults are in use because there is none.
    pub path: Option<String>,
    /// The file's format version, when it loaded.
    pub format_version: Option<u32>,
    /// `(code, message)` when the file was refused; the defaults stay in use.
    pub error: Option<(String, String)>,
}

/// The `from` versions of the `config/weather.ron` migration steps (none at v1), for
/// `cargo xtask check-formats`.
#[must_use]
pub fn weather_config_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape fingerprint of `config/weather.ron` (hashed by `cargo xtask check-formats`):
/// the field list of every struct reachable from [`WeatherConfig`], then its enum spellings.
#[must_use]
pub fn weather_config_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<WeatherConfig>(),
        serde_field_names::<WeatherEffects>(),
        serde_field_names::<WeatherPresetDef>(),
        serde_field_names::<PrecipitationConfig>(),
        serde_field_names::<WindCoupling>(),
        serde_field_names::<SurfaceConfig>(),
        serde_field_names::<VisibilityConfig>(),
        serde_variant_names::<SceneWindMode>(),
        serde_variant_names::<PrecipitationKind>(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `docs/formats/weather.ron.schema.json` names every serde field of every struct of the
    /// format, and nothing else (spec 99 § 7.1): each object key the schema defines is a field,
    /// a definition named after one, or a JSON Schema keyword.
    #[test]
    fn the_json_schema_names_every_field() {
        use regia_core::schema::serde_field_names;
        const SCHEMA: &str = include_str!("../../../../docs/formats/weather.ron.schema.json");
        let lists = [
            serde_field_names::<WeatherConfig>(),
            serde_field_names::<WeatherEffects>(),
            serde_field_names::<WeatherPresetDef>(),
            serde_field_names::<PrecipitationConfig>(),
            serde_field_names::<WindCoupling>(),
            serde_field_names::<SurfaceConfig>(),
            serde_field_names::<VisibilityConfig>(),
        ];
        let fields: Vec<&str> = lists.iter().flatten().map(String::as_str).collect();
        assert!(fields.len() > 30, "{fields:?}");
        for field in &fields {
            assert!(
                SCHEMA.contains(&format!("\"{field}\": {{")),
                "the schema does not define `{field}`"
            );
        }
        // Every `"key": {` in the schema: a field, the preset item's definition, or a keyword.
        let keys = SCHEMA
            .split('"')
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|pair| pair[1].trim_start().starts_with(": {"))
            .map(|pair| pair[0])
            .collect::<Vec<_>>();
        assert!(!keys.is_empty());
        for key in keys {
            assert!(
                fields.contains(&key) || ["properties", "$defs", "items", "preset"].contains(&key),
                "the schema defines `{key}`, which is no field of the format"
            );
        }
        let version = format!("\"const\": {WEATHER_CONFIG_FORMAT_VERSION}");
        assert!(SCHEMA.contains(&version), "the schema's version is the format's");
    }

    /// § 11 test 7.
    #[test]
    fn the_default_round_trips_and_every_effect_starts_off() {
        let config = WeatherConfig::default();
        assert_eq!(config.effects, WeatherEffects::default());
        assert!(!config.effects.precipitation && !config.effects.surface && !config.effects.scene_wind);
        let back = WeatherConfig::from_ron(&config.to_ron().expect("ron")).expect("parses");
        assert_eq!(back, config);
        assert_eq!(WeatherConfig::from_ron("()").expect("empty"), config);
    }

    #[test]
    fn a_file_can_name_only_what_it_changes() {
        let config = WeatherConfig::from_ron(
            "(effects: (surface: true), surface: (wet_mm: 0.4), wind: (surface_factor: Some(0.3)))",
        )
        .expect("parses");
        assert!(config.effects.surface && !config.effects.precipitation);
        assert_eq!(config.surface.wet_mm, 0.4);
        assert_eq!(config.surface.dry_time_s, 1800.0);
        assert_eq!(config.wind.surface_factor, Some(0.3));
    }

    /// The example of `docs/formats/weather-ron.md`.
    #[test]
    fn the_documented_example_parses() {
        let text = include_str!("../../../../docs/formats/weather.example.ron");
        let config = WeatherConfig::from_ron(text).expect("parses");
        assert!(config.effects.precipitation);
        let monsoon = config.preset("monsoon").expect("monsoon");
        assert_eq!(monsoon.temperature_c, 27.0);
        assert_eq!(monsoon.precipitation, 1.0);
        assert_eq!(monsoon.gustiness, 0.5);
        assert_eq!(
            monsoon.coverage,
            WeatherState::STORM.coverage,
            "the rest is the base"
        );
        assert_eq!(
            config.preset("storm"),
            Some(WeatherState::STORM),
            "built-ins resolve"
        );
        assert_eq!(config.preset("nope"), None);
    }

    #[test]
    fn a_newer_version_is_a_version_error_even_with_unknown_fields() {
        let err = WeatherConfig::from_ron("(format_version: 2, new_thing: 1)").expect_err("newer");
        assert_eq!(err.code(), "weather_config_version");
    }

    #[test]
    fn unknown_fields_and_bad_values_are_invalid_and_name_the_field() {
        for (text, needle) in [
            ("(nonsense: 1)", "nonsense"),
            ("(surface: (wet: 1.0))", "wet"),
            ("(presets: [(name: \"storm\", base: \"fair\")])", "storm"),
            ("(presets: [(name: \"a\", base: \"nope\")])", "nope"),
            (
                "(presets: [(name: \"a\", base: \"fair\"), (name: \"a\", base: \"fair\")])",
                "twice",
            ),
            ("(start: Some(\"nope\"))", "nope"),
            ("(precipitation: (max_rain_mm_per_h: -1.0))", "max_rain_mm_per_h"),
            ("(precipitation: (density_scale: 5.0))", "density_scale"),
            ("(wind: (gust_scale: 5.0))", "gust_scale"),
            (
                "(precipitation: (snow_all_below_c: 3.0, rain_all_above_c: 2.0))",
                "snow_all_below_c",
            ),
            ("(visibility: (snow_visibility_m: 0.0))", "snow_visibility_m"),
        ] {
            let err = WeatherConfig::from_ron(text).expect_err(text);
            assert_eq!(err.code(), "weather_config_invalid", "{text}");
            assert!(err.to_string().contains(needle), "{text}: {err}");
        }
    }

    #[test]
    fn a_config_preset_resolves_through_an_earlier_one() {
        let config = WeatherConfig::from_ron(
            "(presets: [(name: \"a\", base: \"storm\", gustiness: Some(0.9)), \
             (name: \"b\", base: \"a\", temperature_c: Some(-4.0))], start: Some(\"b\"))",
        )
        .expect("parses");
        let b = config.preset("b").expect("b");
        assert_eq!((b.gustiness, b.temperature_c), (0.9, -4.0));
        assert_eq!(b.coverage, WeatherState::STORM.coverage);
        assert_eq!(config.preset_names().len(), WEATHER_PRESETS.len() + 2);
    }

    /// A missing file is the defaults, not an error; a bad file is the error.
    #[test]
    fn load_reads_the_config_mount() {
        let dir = regia_test_utils::unique_temp_dir("weather-config");
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::Config, regia_platform::DirMount::read_only(&dir));
        assert_eq!(WeatherConfig::load(&vfs), Ok(None));
        std::fs::write(dir.join("weather.ron"), "(effects: (surface: true))").expect("write");
        let config = WeatherConfig::load(&vfs).expect("loads").expect("present");
        assert!(config.effects.surface);
        std::fs::write(dir.join("weather.ron"), "(nonsense: 1)").expect("write");
        let err = WeatherConfig::load(&vfs).expect_err("refused");
        assert_eq!(err.code(), "weather_config_invalid");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_schema_fingerprint_names_the_fields() {
        let fingerprint = weather_config_schema_fingerprint();
        assert!(fingerprint.contains("max_rain_mm_per_h"));
        assert!(fingerprint.contains("Sleet"));
        assert!(weather_config_migration_sources().is_empty());
    }
}
