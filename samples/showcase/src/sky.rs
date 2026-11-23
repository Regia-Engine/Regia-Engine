// SPDX-License-Identifier: MIT-0
//! The sky and weather state file (`sky.ron`).
//!
//! The atmosphere block is the engine's own [`LevelAtmosphere`] (sun clock, haze, aerosol
//! turbidity, sky bodies) and the look block its [`LevelLook`] (exposure bias, grade); both are read
//! by the renderer the way a level's are. The weather is a [`regia_vfx::Weather`] state with
//! optional timed changes, stepped by the fixed step so a run is repeatable.
//!
//! ```text
//! Sky(
//!     version: 1,
//!     atmosphere: (time_of_day: Some((hour: 7.5, day_of_year: 40, latitude_deg: 58.0)), aerosol_turbidity: 0.1),
//!     look: (exposure_bias_ev: 0.0),
//!     weather: Preset("overcast"),
//!     changes: [(frame: 300, to: Preset("storm"), seconds: 12.0)],
//!     mist: Some((density: 0.012, max_height_m: 45.0)),   // optional: a valley mist on the heightfield
//!     cloud_look: Some((cirrus_coverage: Some(0.65), cirrus_optical_depth: Some(0.11))),   // optional: pins cloud look values (ADR-1492)
//! )
//! ```

use std::path::Path;

use regia_scene::level::{LevelAtmosphere, LevelLook};
use regia_vfx::{CloudLookOverride, WeatherState};
use serde::{Deserialize, Serialize};

use crate::features::MistSpec;
use crate::{ErrorCode, ShowcaseError};

/// Format version of `sky.ron`.
pub const SKY_VERSION: u32 = 1;

/// Most timed weather changes a file may hold.
pub const MAX_CHANGES: usize = 10_000;

/// A weather: a named preset or explicit numbers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum WeatherSpec {
    /// One of [`regia_vfx::WEATHER_PRESETS`] (`clear`, `fair`, `cloudy`, `overcast`, `storm`).
    Preset(String),
    /// Explicit values.
    Custom {
        /// Cloud coverage, `0..=1`.
        coverage: f32,
        /// Cloud type, `0` stratus to `1` towering cumulus.
        cloud_type: f32,
        /// Extinction inside a full-density cloud, 1/metres.
        density: f32,
        /// Wind at cloud height on world X and Z, metres per second.
        wind_m_per_s: (f32, f32),
        /// Rain or snow intensity, `0..=1`.
        precipitation: f32,
    },
}

impl WeatherSpec {
    /// The engine weather state, or a message when a preset name is unknown or a number is out of
    /// range.
    ///
    /// # Errors
    /// A message naming the problem.
    pub fn state(&self) -> Result<WeatherState, String> {
        match self {
            WeatherSpec::Preset(name) => WeatherState::preset(name).ok_or_else(|| {
                format!(
                    "unknown weather preset '{name}' (one of {})",
                    regia_vfx::WEATHER_PRESETS.join(", ")
                )
            }),
            WeatherSpec::Custom {
                coverage,
                cloud_type,
                density,
                wind_m_per_s,
                precipitation,
            } => {
                let all = [
                    *coverage,
                    *cloud_type,
                    *density,
                    wind_m_per_s.0,
                    wind_m_per_s.1,
                    *precipitation,
                ];
                if all.iter().any(|v| !v.is_finite()) {
                    return Err("a weather number is not finite".into());
                }
                let unit = |v: f32| (0.0..=1.0).contains(&v);
                if !unit(*coverage) || !unit(*cloud_type) || !unit(*precipitation) || *density < 0.0 {
                    return Err(
                        "coverage, cloud_type and precipitation must be within 0..=1 and density >= 0".into(),
                    );
                }
                Ok(WeatherState {
                    coverage: *coverage,
                    cloud_type: *cloud_type,
                    density: *density,
                    wind_m_per_s: [wind_m_per_s.0, wind_m_per_s.1],
                    precipitation: *precipitation,
                    // An explicit weather keeps the renderer's cloud heights and look (ADR-1408).
                    genus: None,
                    ..WeatherState::FAIR
                })
            }
        }
    }
}

/// A timed move to another weather.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeatherChange {
    /// Frame the change starts on.
    pub frame: u32,
    /// The weather to ease to.
    pub to: WeatherSpec,
    /// Seconds the ease takes (`0` jumps).
    #[serde(default)]
    pub seconds: f32,
}

/// The parsed `sky.ron`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename = "Sky", deny_unknown_fields)]
pub struct SkyFile {
    /// Format version ([`SKY_VERSION`]).
    pub version: u32,
    /// Planet preset, sun clock, haze and sky bodies. Absent: the engine's clear-day Earth.
    #[serde(default)]
    pub atmosphere: LevelAtmosphere,
    /// Exposure meter and grade. Absent: the engine default look.
    #[serde(default)]
    pub look: LevelLook,
    /// The weather at frame 0.
    pub weather: WeatherSpec,
    /// Timed weather changes, any order.
    #[serde(default)]
    pub changes: Vec<WeatherChange>,
    /// A valley mist that follows the heightfield ([`MistSpec`]). Absent: no mist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mist: Option<MistSpec>,
    /// Cloud look values pinned over the weather's genus look every step (ADR-1492): any of
    /// `diffusion`, `erosion`, `breakup`, `breakup_scale_m` and the `cirrus_*` fields, each
    /// `Some(value)` or absent. Absent block: the weather's look, exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_look: Option<CloudLookOverride>,
}

impl SkyFile {
    /// Parses and validates `text`.
    ///
    /// # Errors
    /// [`ErrorCode::SkyMalformed`] naming `path`.
    pub fn from_ron(text: &str, path: &Path) -> Result<Self, ShowcaseError> {
        let bad = |message: String| ShowcaseError::new(ErrorCode::SkyMalformed, path, message);
        let sky: SkyFile = ron::from_str(text).map_err(|e| bad(e.to_string()))?;
        sky.validate().map_err(bad)?;
        Ok(sky)
    }

    /// The exposure the sky file pins (EV100): `look.min_ev100` when it equals `look.max_ev100` (a meter range
    /// of one value). The app then sets `Camera.exposure_ev` to it, which turns auto exposure off: the filming
    /// setting, since a metered exposure adapts frame by frame.
    #[must_use]
    pub fn pinned_ev(&self) -> Option<f32> {
        match (self.look.min_ev100, self.look.max_ev100) {
            (Some(lo), Some(hi)) if lo.is_finite() && (lo - hi).abs() <= 1e-6 => Some(lo),
            _ => None,
        }
    }

    /// Checks every field.
    ///
    /// # Errors
    /// A message naming the field.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != SKY_VERSION {
            return Err(format!(
                "version {} (this build reads {SKY_VERSION})",
                self.version
            ));
        }
        self.weather.state()?;
        if let Some(mist) = &self.mist {
            mist.validate()?;
        }
        if let Some(look) = &self.cloud_look
            && !look.is_finite()
        {
            return Err("a cloud_look number is not finite".into());
        }
        if self.changes.len() > MAX_CHANGES {
            return Err(format!("{} changes (at most {MAX_CHANGES})", self.changes.len()));
        }
        for (i, change) in self.changes.iter().enumerate() {
            change.to.state().map_err(|e| format!("changes[{i}]: {e}"))?;
            if !change.seconds.is_finite() || change.seconds < 0.0 {
                return Err(format!("changes[{i}]: seconds must be >= 0"));
            }
        }
        if let Some(tod) = &self.atmosphere.time_of_day {
            if !tod.hour.is_finite() || !(0.0..=24.0).contains(&tod.hour) {
                return Err("atmosphere.time_of_day.hour must be within 0..=24".into());
            }
            if !(1..=365).contains(&tod.day_of_year) {
                return Err("atmosphere.time_of_day.day_of_year must be within 1..=365".into());
            }
            if !tod.latitude_deg.is_finite() || !(-90.0..=90.0).contains(&tod.latitude_deg) {
                return Err("atmosphere.time_of_day.latitude_deg must be within -90..=90".into());
            }
        }
        Ok(())
    }

    /// The weather resource this file starts with: the frame-0 weather and the `cloud_look`
    /// override. The default weather when the preset does not resolve (`validate` refuses that).
    #[must_use]
    pub fn initial_weather(&self) -> regia_vfx::Weather {
        let mut weather = self
            .weather
            .state()
            .map(regia_vfx::Weather::new)
            .unwrap_or_default();
        if let Some(look) = self.cloud_look {
            weather.look_override = look;
        }
        weather
    }

    /// The changes sorted by frame (stable).
    #[must_use]
    pub fn sorted_changes(&self) -> Vec<WeatherChange> {
        let mut changes = self.changes.clone();
        changes.sort_by_key(|c| c.frame);
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<SkyFile, ShowcaseError> {
        SkyFile::from_ron(text, Path::new("sky.ron"))
    }

    #[test]
    fn parses_the_documented_file() {
        let sky = parse(
            r#"Sky(
                version: 1,
                atmosphere: (time_of_day: Some((hour: 7.5, day_of_year: 40, latitude_deg: 58.0)), aerosol_turbidity: 0.1),
                look: (exposure_bias_ev: 0.5),
                weather: Preset("overcast"),
                changes: [(frame: 300, to: Preset("storm"), seconds: 12.0)],
            )"#,
        )
        .expect("parses");
        assert_eq!(sky.look.exposure_bias_ev, 0.5);
        assert_eq!(sky.atmosphere.time_of_day.map(|t| t.hour), Some(7.5));
        assert_eq!(sky.weather.state(), Ok(WeatherState::OVERCAST));
        assert_eq!(sky.changes.len(), 1);
    }

    #[test]
    fn a_meter_range_of_one_value_pins_the_exposure() {
        let pinned = parse(r#"Sky(version: 1, look: (min_ev100: Some(13.5), max_ev100: Some(13.5)), weather: Preset("overcast"))"#)
            .expect("parses");
        assert_eq!(pinned.pinned_ev(), Some(13.5));
        let range = parse(r#"Sky(version: 1, look: (min_ev100: Some(12.0), max_ev100: Some(15.0)), weather: Preset("overcast"))"#)
            .expect("parses");
        assert_eq!(range.pinned_ev(), None, "a real range meters");
        let open = parse(r#"Sky(version: 1, weather: Preset("overcast"))"#).expect("parses");
        assert_eq!(open.pinned_ev(), None);
    }

    #[test]
    fn custom_weather_is_range_checked() {
        let ok = WeatherSpec::Custom {
            coverage: 0.4,
            cloud_type: 0.5,
            density: 0.05,
            wind_m_per_s: (3.0, 1.0),
            precipitation: 0.0,
        };
        assert_eq!(ok.state().map(|s| s.coverage), Ok(0.4));
        let bad = WeatherSpec::Custom {
            coverage: 1.5,
            cloud_type: 0.5,
            density: 0.05,
            wind_m_per_s: (3.0, 1.0),
            precipitation: 0.0,
        };
        assert!(bad.state().is_err());
    }

    #[test]
    fn bad_files_are_refused_with_the_sky_code() {
        let bad = [
            "nope",
            r#"Sky(version: 2, weather: Preset("fair"))"#,
            r#"Sky(version: 1, weather: Preset("hail"))"#,
            r#"Sky(version: 1, weather: Preset("fair"), changes: [(frame: 1, to: Preset("fair"), seconds: -1.0)])"#,
            r#"Sky(version: 1, weather: Preset("fair"), changes: [(frame: 1, to: Preset("tornado"))])"#,
            r#"Sky(version: 1, weather: Preset("fair"), atmosphere: (time_of_day: Some((hour: 30.0, day_of_year: 40, latitude_deg: 0.0))))"#,
            r#"Sky(version: 1, weather: Preset("fair"), extra: 1)"#,
        ];
        for text in bad {
            let err = parse(text).expect_err(text);
            assert_eq!(err.code, ErrorCode::SkyMalformed, "{text}: {err}");
        }
    }

    #[test]
    fn changes_sort_by_frame_and_keep_ties_in_order() {
        let sky = parse(
            r#"Sky(version: 1, weather: Preset("clear"), changes: [
                (frame: 90, to: Preset("storm")),
                (frame: 30, to: Preset("fair")),
                (frame: 30, to: Preset("cloudy")),
            ])"#,
        )
        .expect("parses");
        let frames: Vec<u32> = sky.sorted_changes().iter().map(|c| c.frame).collect();
        assert_eq!(frames, [30, 30, 90]);
        assert_eq!(sky.sorted_changes()[1].to, WeatherSpec::Preset("cloudy".into()));
    }

    /// ADR-1492: the optional `cloud_look` block parses, round trips, rejects unknown fields and
    /// non-finite numbers, and an absent block leaves the weather exactly as before.
    #[test]
    fn cloud_look_block_is_optional_and_pins_the_look() {
        let plain = parse(r#"Sky(version: 1, weather: Preset("cirrus"))"#).expect("parses");
        assert!(plain.cloud_look.is_none());
        let before = regia_vfx::Weather::new(WeatherState::preset("cirrus").expect("cirrus"));
        assert_eq!(
            plain.initial_weather(),
            before,
            "no block: the weather is untouched"
        );
        let text = r#"Sky(version: 1, weather: Preset("cirrus"),
            cloud_look: Some((cirrus_coverage: Some(0.65), cirrus_optical_depth: Some(0.11), cirrus_scale_m: Some(1500.0))))"#;
        let pinned = parse(text).expect("parses");
        let look = pinned.cloud_look.expect("block");
        assert_eq!(look.cirrus_coverage, Some(0.65));
        assert_eq!(look.cirrus_stretch, None);
        let weather = pinned.initial_weather();
        assert_eq!(weather.look_override, look);
        let mut clouds = regia_render::CloudSettings::default();
        let mut stepped = weather;
        for _ in 0..30 {
            stepped.step(1.0 / 60.0);
            stepped.apply(&mut clouds);
        }
        assert_eq!(clouds.look.cirrus.coverage, 0.65);
        assert_eq!(clouds.look.cirrus.scale_m, 1500.0);
        // A written file round trips.
        let ron = ron::to_string(&pinned).expect("serialize");
        assert_eq!(parse(&ron).expect("reparse"), pinned);
        let bad =
            r#"Sky(version: 1, weather: Preset("cirrus"), cloud_look: Some((cirrus_bogus: Some(1.0))))"#;
        assert!(parse(bad).is_err(), "unknown field");
        let mut nan = pinned;
        nan.cloud_look = Some(CloudLookOverride {
            erosion: Some(f32::NAN),
            ..CloudLookOverride::default()
        });
        assert!(nan.validate().is_err(), "non-finite number");
    }
}
