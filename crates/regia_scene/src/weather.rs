// SPDX-License-Identifier: MPL-2.0
//! Weather data the renderer, physics and audio read without depending on `regia_vfx`
//! (spec 99, ADR-1847): the wind near the ground, the state of wet or snowy surfaces and the
//! extinction of the falling rain and snow. Data only: no systems live here.
//! `regia_vfx::weather` fills these resources while the project's effects are on
//! (`config/weather.ron`); a host or an agent may also insert and set them by hand.

use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

/// Who writes the [`SceneWind`].
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SceneWindMode {
    /// The weather writes it every frame (when a `Weather` exists).
    #[default]
    Weather,
    /// The host or the agent holds the stored values.
    Manual,
    /// Consumers act as if there were no scene wind.
    Off,
}

/// The wind near the ground, 10 m up, on world X and Z (m/s).
#[derive(Resource, Reflect, Clone, Copy, Debug, PartialEq)]
#[reflect(Resource, Default)]
pub struct SceneWind {
    /// Who writes it.
    pub mode: SceneWindMode,
    /// The wind now, gusts included: what consumers read.
    pub velocity_m_per_s: [f32; 2],
    /// The wind without gusts.
    pub mean_m_per_s: [f32; 2],
}

impl Default for SceneWind {
    fn default() -> Self {
        Self::CALM
    }
}

impl SceneWind {
    /// [`SceneWindMode::Manual`], no wind.
    pub const CALM: Self = Self {
        mode: SceneWindMode::Manual,
        velocity_m_per_s: [0.0, 0.0],
        mean_m_per_s: [0.0, 0.0],
    };

    /// A held wind: [`SceneWindMode::Manual`], no gusts.
    #[must_use]
    pub fn manual(wind_m_per_s: [f32; 2]) -> Self {
        Self {
            mode: SceneWindMode::Manual,
            velocity_m_per_s: wind_m_per_s,
            mean_m_per_s: wind_m_per_s,
        }
    }

    /// The speed of the wind now (m/s).
    #[must_use]
    pub fn speed(&self) -> f32 {
        self.velocity_m_per_s[0].hypot(self.velocity_m_per_s[1])
    }

    /// The speed of the wind without gusts (m/s).
    #[must_use]
    pub fn mean_speed(&self) -> f32 {
        self.mean_m_per_s[0].hypot(self.mean_m_per_s[1])
    }

    /// The wind now as a world vector, `[x, 0, z]`.
    #[must_use]
    pub fn velocity3(&self) -> [f32; 3] {
        [self.velocity_m_per_s[0], 0.0, self.velocity_m_per_s[1]]
    }

    /// What `render.set_foliage_wind` writes when a [`SceneWind`] exists, so the weather does not
    /// overwrite an explicit choice the next frame: [`SceneWindMode::Manual`] with the value, or
    /// [`SceneWindMode::Off`] for `None`.
    pub fn set_explicitly(&mut self, wind: Option<[f32; 2]>) {
        *self = match wind {
            Some(wind) => Self::manual(wind),
            None => Self {
                mode: SceneWindMode::Off,
                ..*self
            },
        };
    }

    /// What a consumer reads: the wind now, or `None` in [`SceneWindMode::Off`].
    #[must_use]
    pub fn active(&self) -> Option<[f32; 2]> {
        (self.mode != SceneWindMode::Off).then_some(self.velocity_m_per_s)
    }
}

/// Who writes the [`SurfaceWeather`].
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SurfaceWeatherMode {
    /// The weather integrates it over game time.
    #[default]
    Simulated,
    /// The host or the agent holds the stored values (a wet street with no weather clock).
    Manual,
}

/// The state of exposed surfaces: how wet they are and how much snow lies on them.
#[derive(Resource, Reflect, Clone, Copy, Debug, PartialEq)]
#[reflect(Resource, Default)]
pub struct SurfaceWeather {
    /// Who writes it.
    pub mode: SurfaceWeatherMode,
    /// The water film on exposed surfaces, `0..1`.
    pub wetness: f32,
    /// How full the hollows are, `0..1`.
    pub puddles: f32,
    /// Settled snow as water equivalent, mm (the conserved quantity).
    pub snow_we_mm: f32,
    /// Settled snow depth, m (`snow_we_mm · snow_ratio / 1000` while simulated).
    pub snow_depth_m: f32,
    /// The visual snow cover, `0..1` (from the depth while simulated).
    pub snow_cover: f32,
    /// Rain falling now, mm/h (splashes).
    pub rain_mm_per_h: f32,
    /// Snow falling now as water equivalent, mm/h.
    pub snow_we_mm_per_h: f32,
    /// The rain rate over the project's maximum, `0..1`: the renderer shades ripples from it,
    /// and it cannot read the project's weather config.
    pub ripple: f32,
}

impl Default for SurfaceWeather {
    fn default() -> Self {
        Self::DRY
    }
}

impl SurfaceWeather {
    /// Nothing wet, nothing settled, nothing falling, [`SurfaceWeatherMode::Simulated`].
    pub const DRY: Self = Self {
        mode: SurfaceWeatherMode::Simulated,
        wetness: 0.0,
        puddles: 0.0,
        snow_we_mm: 0.0,
        snow_depth_m: 0.0,
        snow_cover: 0.0,
        rain_mm_per_h: 0.0,
        snow_we_mm_per_h: 0.0,
        ripple: 0.0,
    };

    /// `true` when a surface shades as if there were no weather.
    #[must_use]
    pub fn is_dry(&self) -> bool {
        self.wetness <= 0.0 && self.puddles <= 0.0 && self.snow_cover <= 0.0 && self.ripple <= 0.0
    }
}

/// The air between the camera and a surface while rain or snow falls: a constant,
/// height-independent extinction the frame applies wherever it applies its fog.
#[derive(Resource, Reflect, Clone, Copy, Debug, Default, PartialEq)]
#[reflect(Resource, Default)]
pub struct WeatherAir {
    /// Extinction of the falling rain and snow (1/m).
    pub extinction_per_m: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_wind_constructors_and_readers() {
        assert_eq!(SceneWind::CALM.speed(), 0.0);
        assert_eq!(SceneWind::default(), SceneWind::CALM);
        let wind = SceneWind::manual([3.0, 4.0]);
        assert_eq!(wind.mode, SceneWindMode::Manual);
        assert_eq!(wind.speed(), 5.0);
        assert_eq!(wind.mean_speed(), 5.0);
        assert_eq!(wind.velocity3(), [3.0, 0.0, 4.0]);
        assert_eq!(wind.active(), Some([3.0, 4.0]));
        let off = SceneWind {
            mode: SceneWindMode::Off,
            ..wind
        };
        assert_eq!(off.active(), None, "Off reads as no wind, the values are kept");
        assert_eq!(off.speed(), 5.0);
    }

    /// Spec 99 § 3.3: the foliage call turns the weather's wind into an explicit choice.
    #[test]
    fn an_explicit_wind_takes_over_from_the_weather() {
        let mut wind = SceneWind {
            mode: SceneWindMode::Weather,
            velocity_m_per_s: [5.0, 0.0],
            mean_m_per_s: [5.0, 0.0],
        };
        wind.set_explicitly(Some([1.0, 2.0]));
        assert_eq!(wind, SceneWind::manual([1.0, 2.0]));
        wind.set_explicitly(None);
        assert_eq!(wind.mode, SceneWindMode::Off);
        assert_eq!(wind.velocity_m_per_s, [1.0, 2.0]);
    }

    #[test]
    fn dry_is_dry_and_any_state_is_not() {
        assert!(SurfaceWeather::DRY.is_dry());
        assert_eq!(SurfaceWeather::default(), SurfaceWeather::DRY);
        for wet in [
            SurfaceWeather {
                wetness: 0.1,
                ..SurfaceWeather::DRY
            },
            SurfaceWeather {
                puddles: 0.1,
                ..SurfaceWeather::DRY
            },
            SurfaceWeather {
                snow_cover: 0.1,
                ..SurfaceWeather::DRY
            },
            SurfaceWeather {
                ripple: 0.1,
                ..SurfaceWeather::DRY
            },
        ] {
            assert!(!wet.is_dry(), "{wet:?}");
        }
    }
}
