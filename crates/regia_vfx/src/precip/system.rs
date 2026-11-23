// SPDX-License-Identifier: MPL-2.0
//! The ECS side of the precipitation: [`step_precipitation`] integrates the offsets on game time
//! and hands the draw what falls; the render hook ([`crate::runtime`]) adds this frame's eye and
//! camera and uploads one uniform.

use std::sync::{Arc, Mutex};

use regia_app::prelude::*;
use regia_render::{PrecipOcclusionRequest, QualityTier, RenderSettings};
use regia_scene::level::LevelSettings;
use regia_scene::weather::{SceneWind, SceneWindMode};

use super::{
    MAX_EYE_SPEED_M_PER_S, PrecipBudget, PrecipCounts, PrecipFrame, PrecipIntegrator, PrecipitationGpu,
    PrecipitationReport, PrecipitationSettings,
};
use crate::weather::{
    PrecipitationRates, RAIN_CLASSES, SNOW_CLASSES, SNOW_FALL_SPEEDS_M_PER_S, Weather, WeatherConfig,
    medium_is_airless, rain_class_diameters_mm, rain_fall_speed_m_per_s, scene_wind_surface_mean,
};

/// Everything the render hook needs to draw a frame's precipitation.
#[derive(Clone, Debug, PartialEq)]
pub struct PrecipState {
    /// Advances the offsets.
    pub integrator: PrecipIntegrator,
    /// The weather clock.
    pub clock_s: f64,
    /// What falls.
    pub rates: PrecipitationRates,
    /// The project's config.
    pub config: WeatherConfig,
    /// The project's switches.
    pub settings: PrecipitationSettings,
    /// The wind carrying the drops.
    pub wind_m_per_s: [f32; 3],
    /// The tier.
    pub quality: QualityTier,
    /// Draw at all (the effect is on and the level has air).
    pub active: bool,
    /// The eye of the last frame, for the streak's relative velocity.
    pub last_eye: Option<[f64; 3]>,
    /// Game seconds since the last drawn frame.
    pub since_draw_s: f32,
    /// Cloud cover, `0..1`.
    pub cloud_coverage: f32,
}

impl Default for PrecipState {
    fn default() -> Self {
        Self {
            integrator: PrecipIntegrator::default(),
            clock_s: 0.0,
            rates: PrecipitationRates::NONE,
            config: WeatherConfig::default(),
            settings: PrecipitationSettings::default(),
            wind_m_per_s: [0.0; 3],
            quality: QualityTier::High,
            active: false,
            last_eye: None,
            since_draw_s: 0.0,
            cloud_coverage: 0.0,
        }
    }
}

impl PrecipState {
    /// The occlusion view this state wants: along the mean fall of the heaviest rain class, or
    /// straight down for snow. `None` while nothing falls.
    #[must_use]
    pub fn occlusion_request(&self) -> Option<PrecipOcclusionRequest> {
        if !self.active || !self.settings.occlusion || self.rates.is_none() {
            return None;
        }
        let mut diameters = [0.0; RAIN_CLASSES];
        rain_class_diameters_mm(&self.rates, &self.config, &mut diameters);
        let fall = if self.rates.rain_mm_per_h > 0.0 {
            rain_fall_speed_m_per_s(diameters[RAIN_CLASSES - 1])
        } else {
            SNOW_FALL_SPEEDS_M_PER_S[SNOW_CLASSES - 1]
        };
        let wind = self.wind_m_per_s;
        let length = (wind[0] * wind[0] + fall * fall + wind[2] * wind[2])
            .sqrt()
            .max(1.0e-4);
        Some(PrecipOcclusionRequest {
            fall_dir: [wind[0] / length, -fall / length, wind[2] / length],
        })
    }

    /// The frame drawn with the eye at `eye`, looking along `forward`, and the eye's motion
    /// measured since the last call.
    pub fn frame_at(&mut self, eye: [f64; 3], forward: [f32; 3]) -> PrecipFrame {
        let length = forward[0].hypot(forward[2]);
        let forward_xz = if length > 1.0e-4 {
            [forward[0] / length, forward[2] / length]
        } else {
            [0.0, -1.0]
        };
        let mut velocity = [0.0; 3];
        if let Some(last) = self.last_eye
            && self.since_draw_s > 0.0
        {
            for axis in 0..3 {
                velocity[axis] = ((eye[axis] - last[axis]) / f64::from(self.since_draw_s)) as f32;
            }
            let speed =
                (velocity[0] * velocity[0] + velocity[1] * velocity[1] + velocity[2] * velocity[2]).sqrt();
            if speed > 4.0 * MAX_EYE_SPEED_M_PER_S || !speed.is_finite() {
                velocity = [0.0; 3];
            } else if speed > MAX_EYE_SPEED_M_PER_S {
                velocity = velocity.map(|v| v * MAX_EYE_SPEED_M_PER_S / speed);
            }
        }
        if self.since_draw_s > 0.0 || self.last_eye.is_none() {
            self.last_eye = Some(eye);
            self.since_draw_s = 0.0;
        }
        self.integrator.frame(
            eye,
            velocity,
            forward_xz,
            self.clock_s,
            &self.rates,
            &self.config,
            &self.settings,
            self.wind_m_per_s,
            self.quality,
        )
    }
}

/// The state shared with the render hook, and the GPU side once it exists.
#[derive(Resource, Clone, Default)]
pub struct PrecipitationHandle {
    /// The state [`step_precipitation`] writes and the hook reads.
    pub state: Arc<Mutex<PrecipState>>,
    /// The draw, created with the VFX context.
    pub gpu: Arc<Mutex<Option<PrecipitationGpu>>>,
}

/// The tier of the app's render settings: the preset whose profile they hold, else `High`.
#[must_use]
pub fn precipitation_tier(settings: Option<&RenderSettings>) -> QualityTier {
    settings
        .and_then(|settings| {
            QualityTier::EVERY
                .into_iter()
                .find(|tier| tier.profile() == settings.profile)
                // Very Low runs Low's budgets (ADR-7882).
                .map(QualityTier::content_tier)
        })
        .unwrap_or(QualityTier::High)
}

/// `Update`, after the weather drivers: advances the fall offsets and the wind drift on game time
/// and writes [`PrecipitationReport`]. Does nothing without a [`Weather`].
#[allow(clippy::too_many_arguments)] // ECS systems pull their inputs as parameters
pub fn step_precipitation(
    time: Option<Res<Time>>,
    weather: Option<Res<Weather>>,
    config: Option<Res<WeatherConfig>>,
    settings: Option<Res<PrecipitationSettings>>,
    wind: Option<Res<SceneWind>>,
    clouds: Option<Res<regia_render::CloudSettings>>,
    level: Option<Res<LevelSettings>>,
    render: Option<Res<RenderSettings>>,
    handle: Option<Res<PrecipitationHandle>>,
    mut report: Option<ResMut<PrecipitationReport>>,
    mut commands: Commands,
) {
    let (Some(config), Some(settings)) = (config, settings) else {
        return;
    };
    let _span = tracing::trace_span!("step_precipitation").entered();
    let quality = precipitation_tier(render.as_deref());
    let budget = PrecipBudget::for_quality(quality);
    let airless = medium_is_airless(level.as_deref());
    let weather = weather.as_deref();
    let active = settings.enabled && !airless && weather.is_some();
    let rates = weather
        .filter(|_| active)
        .map_or(PrecipitationRates::NONE, |w| w.rates(&config));
    let counts = PrecipCounts::of(&rates, &config, &settings, &budget);
    let wind3 = match (wind.as_deref(), weather) {
        (_, _) if airless => [0.0; 3],
        (Some(wind), _) => {
            if wind.mode == SceneWindMode::Off {
                [0.0; 3]
            } else {
                wind.velocity3()
            }
        }
        (None, Some(w)) => {
            let mean = scene_wind_surface_mean(w, clouds.as_deref(), &config).mean_m_per_s;
            [mean[0], 0.0, mean[1]]
        }
        (None, None) => [0.0; 3],
    };
    let dt = if weather.is_some_and(|w| w.paused) {
        0.0
    } else {
        time.map_or(0.0, |t| t.delta_secs())
    };
    if let Some(handle) = handle.as_deref()
        && let Ok(mut state) = handle.state.lock()
    {
        state
            .integrator
            .step(&rates, &config, [wind3[0], wind3[2]], &budget, dt);
        state.clock_s = weather.map_or(0.0, Weather::clock_s);
        state.rates = rates;
        if state.config != *config {
            state.config = (*config).clone();
        }
        state.settings = *settings;
        state.wind_m_per_s = wind3;
        state.quality = quality;
        state.active = active;
        state.since_draw_s += dt;
        state.cloud_coverage = weather.map_or(0.0, |w| w.current().coverage);
    }
    let occlusion = report.as_deref().and_then(|r| r.occlusion);
    let next = PrecipitationReport {
        quality,
        rain_drawn: PrecipCounts::instances(counts.rain),
        snow_drawn: PrecipCounts::instances(counts.snow),
        // The splash vertex stage culls every splash without the occlusion view (spec 99 § 5.5),
        // so none counts as drawn until the renderer reports one (ADR-5823).
        splashes: if occlusion.is_some() {
            PrecipCounts::instances(counts.splashes)
        } else {
            0
        },
        budget,
        occlusion,
    };
    match report.as_deref_mut() {
        Some(report) => {
            if *report != next {
                *report = next;
            }
        }
        None => commands.insert_resource(next),
    }
}
