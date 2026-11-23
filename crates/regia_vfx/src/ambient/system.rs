// SPDX-License-Identifier: MPL-2.0
//! The ECS side of ambient life: [`step_ambient_life`] turns the scene's time, season and weather
//! into counts and integrates the drifts; [`step_ambient_birds`] flies the birds; the render hook
//! ([`crate::runtime`]) adds the frame's eye and camera and uploads one uniform.

use std::sync::{Arc, Mutex};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_render::{QualityTier, RenderProfile, RenderProfilePreset, RenderSettings, TimeOfDay};
use regia_scene::level::LevelSettings;
use regia_scene::weather::{SceneWind, SceneWindMode};
use regia_scene::{GlobalTransform, Transform};

use super::birds::{AmbientBirds, BirdFlockSim, BirdInstance, BirdMode, FollowAmbientBird};
use super::{
    AmbientActivity, AmbientBudget, AmbientConditions, AmbientCounts, AmbientFrame, AmbientIntegrator,
    AmbientLifeSettings, MAX_BIRDS, Season,
};
use crate::plugin::VfxView;
use crate::weather::{Weather, WeatherConfig, medium_is_airless, scene_wind_surface_mean, sun_elevation_sin};

/// Everything the render hook needs to draw a frame's ambient life.
#[derive(Clone, Debug, PartialEq)]
pub struct AmbientState {
    /// Advances the drifts.
    pub integrator: AmbientIntegrator,
    /// The drawn counts.
    pub counts: AmbientCounts,
    /// How active each kind is.
    pub activity: AmbientActivity,
    /// The season drawn.
    pub season: Season,
    /// The seed.
    pub seed: u32,
    /// Draw at all (on, a tier that has it, and air).
    pub active: bool,
    /// The tier.
    pub quality: QualityTier,
    /// The birds in world space (`position_size.xyz` is the world position); the first
    /// `bird_count` are live.
    pub birds: [BirdInstance; MAX_BIRDS],
    /// Live entries of [`Self::birds`].
    pub bird_count: u32,
    /// Cloud cover, `0..1`.
    pub cloud_coverage: f32,
    /// Motes or swarms are drawn, so the layer asks for the occlusion view along the frame's sun
    /// (`TranslucentLayer::sun_occlusion_request`): only what the sun reaches glints.
    pub wants_sun_view: bool,
    /// Times the game clock ran backward, so the clock, the drifts and the flights restarted.
    pub restarts: u64,
    /// The flights restart on their next step (the clock has already).
    pub restart_flights: bool,
}

impl Default for AmbientState {
    fn default() -> Self {
        Self {
            integrator: AmbientIntegrator::default(),
            counts: AmbientCounts::default(),
            activity: AmbientActivity::default(),
            season: Season::Summer,
            seed: 0,
            active: false,
            quality: QualityTier::High,
            birds: [BirdInstance::default(); MAX_BIRDS],
            bird_count: 0,
            cloud_coverage: 0.0,
            wants_sun_view: false,
            restarts: 0,
            restart_flights: false,
        }
    }
}

impl AmbientState {
    /// The frame drawn with the eye at `eye`: the birds moved to eye-relative positions.
    #[must_use]
    pub fn frame_at(&self, eye: [f64; 3]) -> AmbientFrame {
        if !self.active {
            return AmbientFrame {
                eye,
                ..AmbientFrame::default()
            };
        }
        let mut frame = AmbientFrame::new(eye, &self.integrator, self.counts, self.season, self.seed);
        let count = (self.bird_count as usize).min(MAX_BIRDS);
        for (out, bird) in frame.birds.iter_mut().zip(&self.birds[..count]) {
            let p = bird.position_size;
            *out = BirdInstance {
                position_size: [
                    (f64::from(p[0]) - eye[0]) as f32,
                    (f64::from(p[1]) - eye[1]) as f32,
                    (f64::from(p[2]) - eye[2]) as f32,
                    p[3],
                ],
                ..*bird
            };
        }
        frame.bird_count = count as u32;
        frame
    }
}

/// The state shared with the render hook, and the GPU side once it exists.
#[derive(Resource, Clone, Default)]
pub struct AmbientLifeHandle {
    /// The state the systems write and the hook reads.
    pub state: Arc<Mutex<AmbientState>>,
    /// The draw, created with the VFX context.
    pub gpu: Arc<Mutex<Option<super::AmbientLifeGpu>>>,
}

/// What ambient life drew this frame, for the agent (`render.ambient_life`).
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct AmbientLifeReport {
    /// The tier.
    pub quality: QualityTier,
    /// The tier's budget.
    pub budget: AmbientBudget,
    /// The season drawn.
    pub season: Season,
    /// The inputs.
    pub conditions: AmbientConditions,
    /// How active each kind is.
    pub activity: AmbientActivity,
    /// The drawn counts.
    pub counts: AmbientCounts,
    /// Birds drawn, all flocks.
    pub birds_drawn: u32,
    /// Flocks flying.
    pub flocks: u32,
    /// Running and drawing.
    pub active: bool,
}

/// Marks the flock and the soaring bird [`AmbientLifeSettings::auto_birds`] put near the eye.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutoAmbientBirds;

/// `profile` without the levers that move on a tier without leaving it: the detector's terrain
/// virtual texture, the features [`RenderSettings::preset`] ignores (the host memory budget among
/// them, ADR-7891), and the player's render scale and shadow quality overrides.
fn without_levers(profile: &RenderProfile) -> RenderProfile {
    let mut profile = profile.clone();
    profile.terrain_budget.virtual_texture = false;
    profile.terrain_budget.vt_late_depth = false;
    profile.frame_pipelining = regia_render::FramePipelining::OFF;
    profile.subsurface_scattering = false;
    profile.blue_noise = false;
    profile.order_independent_transparency = false;
    profile.radiance_cache = false;
    profile.visibility_path = regia_render::visbuffer::VisibilityPath::DEFAULT;
    profile.visibility_buffer = false;
    profile.sun_shadow_trace = regia_render::hwrt::RtShadowRequest::Off;
    profile.render_scale = 1.0;
    profile.csm_resolution = 0;
    profile.local_shadow_atlas_size = 0;
    profile.local_shadow_max_lights = 0;
    // A host's CPU memory budget changes no pixel (ADR-7891).
    profile.host_memory = regia_render::host_memory::HostMemoryBudget::UNLIMITED;
    profile
}

/// The tier of the app's render settings, so that a tier that has no ambient life never gets it: a
/// Low tier on a card that takes the terrain virtual texture is still Low, and a Deck with the
/// player's own render scale is still a Deck. A profile no tier produces is `High` on the desktop
/// (a host that writes its own profile and turns ambient life on) and off on Android
/// ([`unmatched_profile_tier`] of the platform's default preset).
#[must_use]
pub fn ambient_tier(settings: Option<&RenderSettings>) -> QualityTier {
    let Some(settings) = settings else {
        return QualityTier::High;
    };
    let profile = without_levers(&settings.profile);
    QualityTier::EVERY
        .into_iter()
        .find(|tier| without_levers(&tier.profile()) == profile)
        // Very Low runs Low's budgets (ADR-7882).
        .map(QualityTier::content_tier)
        .unwrap_or_else(|| unmatched_profile_tier(RenderProfilePreset::platform_default()))
}

/// The ambient tier of a render profile no tier produces, on a platform whose default preset is
/// `platform`: `MobileLow` (no ambient life) where the default is a mobile preset, `High`
/// elsewhere. The platform choice is the render crate's ([`RenderProfilePreset::platform_default`]),
/// not a second `cfg` here (ADR-7431).
#[must_use]
pub fn unmatched_profile_tier(platform: RenderProfilePreset) -> QualityTier {
    match QualityTier::of_preset(platform) {
        QualityTier::MobileLow | QualityTier::MobileHigh => QualityTier::MobileLow,
        _ => QualityTier::High,
    }
}

fn wind_now(
    wind: Option<&SceneWind>,
    weather: Option<&Weather>,
    clouds: Option<&regia_render::CloudSettings>,
    config: Option<&WeatherConfig>,
    airless: bool,
) -> [f32; 2] {
    if airless {
        return [0.0; 2];
    }
    match (wind, weather) {
        (Some(wind), _) => {
            if wind.mode == SceneWindMode::Off {
                [0.0; 2]
            } else {
                wind.velocity_m_per_s
            }
        }
        (None, Some(weather)) => match config {
            Some(config) => scene_wind_surface_mean(weather, clouds, config).mean_m_per_s,
            None => scene_wind_surface_mean(weather, clouds, &WeatherConfig::default()).mean_m_per_s,
        },
        (None, None) => [0.0; 2],
    }
}

/// The scene's clock: a host's [`TimeOfDay`], else the level's sun clock (the one the sky draws),
/// else the default.
fn scene_clock(time: Option<&TimeOfDay>, level: Option<&LevelSettings>) -> TimeOfDay {
    time.copied()
        .or_else(|| {
            let clock = level?.atmosphere.time_of_day?;
            Some(TimeOfDay {
                hour: clock.hour,
                day_of_year: clock.day_of_year,
                latitude_deg: clock.latitude_deg,
            })
        })
        .unwrap_or_default()
}

/// `Update`, after the weather drivers: the conditions, activity and counts, and the drifts on
/// game time. Writes [`AmbientLifeReport`]. Does nothing until [`AmbientLifeSettings::enabled`].
///
/// A game clock that runs backward is a render-out job restarting it (ADR-1881): the animation
/// clock, the drifts and (in [`step_ambient_birds`]) the flights restart with it, so a job's frames
/// do not depend on how long the app ran before it.
#[allow(clippy::too_many_arguments)] // ECS systems pull their inputs as parameters
pub fn step_ambient_life(
    time: Option<Res<Time>>,
    settings: Option<Res<AmbientLifeSettings>>,
    time_of_day: Option<Res<TimeOfDay>>,
    weather: Option<Res<Weather>>,
    config: Option<Res<WeatherConfig>>,
    wind: Option<Res<SceneWind>>,
    clouds: Option<Res<regia_render::CloudSettings>>,
    level: Option<Res<LevelSettings>>,
    render: Option<Res<RenderSettings>>,
    handle: Option<Res<AmbientLifeHandle>>,
    mut report: Option<ResMut<AmbientLifeReport>>,
    mut clock_seen: Local<(std::time::Duration, bool)>,
    mut tier_seen: Local<Option<QualityTier>>,
    mut commands: Commands,
) {
    let Some(settings) = settings else {
        return;
    };
    // Both watched while off too: a clock restarted or a tier changed then still counts on the way
    // back on, and only a changed profile costs the comparison.
    let elapsed = time.as_deref().map_or(std::time::Duration::ZERO, Time::elapsed);
    clock_seen.1 |= elapsed < clock_seen.0;
    clock_seen.0 = elapsed;
    let quality = match (*tier_seen, render.as_ref()) {
        (Some(tier), Some(render)) if !render.is_changed() => tier,
        _ => {
            let tier = ambient_tier(render.as_deref());
            *tier_seen = Some(tier);
            tier
        }
    };
    if !settings.enabled && report.as_deref().is_none_or(|r| !r.active) {
        return;
    }
    let _span = tracing::trace_span!("step_ambient_life").entered();
    let budget = AmbientBudget::for_quality(quality);
    let airless = medium_is_airless(level.as_deref());
    let weather = weather.as_deref();
    // No clone of the config per frame: the default is built only for a weather without one.
    let rates = weather.map(|w| match config.as_deref() {
        Some(config) => w.rates(config),
        None => w.rates(&WeatherConfig::default()),
    });
    let clock = scene_clock(time_of_day.as_deref(), level.as_deref());
    let season = settings.season.unwrap_or_else(|| Season::of_time(&clock));
    let cloud_coverage = weather.map_or_else(
        || clouds.as_deref().map_or(0.0, |c| c.coverage),
        |w| w.current().coverage,
    );
    let conditions = AmbientConditions {
        sun_elevation_sin: sun_elevation_sin(Some(&clock)),
        rain_mm_per_h: rates.map_or(0.0, |r| r.rain_mm_per_h),
        snow_mm_per_h: rates.map_or(0.0, |r| r.snow_we_mm_per_h),
        wind_m_per_s: wind_now(
            wind.as_deref(),
            weather,
            clouds.as_deref(),
            config.as_deref(),
            airless,
        ),
        cloud_coverage,
        season,
    };
    let active = settings.enabled && !airless && !budget.is_off();
    let activity = AmbientActivity::of(&conditions);
    let counts = if active {
        AmbientCounts::of(&activity, &settings, &budget)
    } else {
        AmbientCounts::default()
    };
    // On game time like the birds: a paused weather holds the sky, not the creatures in it.
    let dt = time.as_deref().map_or(0.0, Time::delta_secs);
    let rewound = std::mem::take(&mut clock_seen.1);
    let mut birds_drawn = 0;
    if let Some(handle) = handle.as_deref()
        && let Ok(mut state) = handle.state.lock()
    {
        if rewound {
            state.integrator = AmbientIntegrator::default();
            state.restarts += 1;
            state.restart_flights = true;
        }
        state.integrator.step(conditions.wind_m_per_s, dt);
        state.counts = counts;
        state.activity = activity;
        state.season = season;
        state.seed = settings.seed;
        state.active = active;
        state.quality = quality;
        state.cloud_coverage = cloud_coverage;
        state.wants_sun_view = counts.motes > 0.0 || counts.swarm_members > 0.0;
        if !active {
            state.bird_count = 0;
        }
        birds_drawn = state.bird_count;
    }
    let next = AmbientLifeReport {
        quality,
        budget,
        season,
        conditions,
        activity,
        counts,
        birds_drawn,
        flocks: report.as_deref().map_or(0, |r| r.flocks),
        active,
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

/// The anchor's world position: its `GlobalTransform`, or its `Transform` while the global one is
/// still the identity (no propagation yet, or an app without the scene plugin).
fn anchor_of(global: Option<&GlobalTransform>, local: Option<&Transform>) -> Vec3 {
    match global {
        Some(g) if *g != GlobalTransform::IDENTITY => g.translation(),
        _ => local.map_or(Vec3::ZERO, |t| t.translation),
    }
}

/// An automatic flock this far (horizontally) from the eye moves back near it, metres.
pub const AUTO_BIRDS_RECENTER_M: f32 = 300.0;

/// How high the automatic flock flies over the ground at the eye, metres: over a forest's crowns,
/// where a viewer on the forest floor sees it through the gaps (the preset's 18 m is inside them).
const AUTO_FLOCK_HEIGHT_M: f32 = 32.0;
/// The automatic flock's wingspan, metres: a crow's, because at 80 m a jay is 5 pixels on a 1080p
/// frame, a dash nobody takes for a bird (the preset's is a finch's).
const AUTO_FLOCK_WINGSPAN_M: f32 = 0.7;

/// The seed of an automatic group of `mode` under the settings' `seed`.
fn auto_seed(seed: u32, mode: BirdMode) -> u32 {
    match mode {
        BirdMode::Flock => seed,
        BirdMode::Soaring => seed.wrapping_add(1),
    }
}

/// Where [`AmbientLifeSettings::auto_birds`] puts a group of `mode`: ahead of the view, off to
/// each side, so they are in the shot when they appear. `forward` is the way the camera looks.
fn auto_anchor(eye: Vec3, forward: Vec3, mode: BirdMode) -> Vec3 {
    let ground = Vec3::new(eye.x, eye.y - 1.7, eye.z);
    let ahead = Vec3::new(forward.x, 0.0, forward.z)
        .try_normalize()
        .unwrap_or(Vec3::NEG_Z);
    let side = Vec3::new(-ahead.z, 0.0, ahead.x);
    match mode {
        BirdMode::Flock => ground + ahead * 80.0 + side * 20.0,
        BirdMode::Soaring => ground + ahead * 100.0 - side * 45.0,
    }
}

/// The flocks [`step_ambient_birds`] flies: the anchor, its transform, its resolved visibility,
/// its flight, and whether [`AmbientLifeSettings::auto_birds`] placed it.
type FlockQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut AmbientBirds,
        Option<&'static GlobalTransform>,
        regia_render::DrawVisibility,
        Option<&'static mut Transform>,
        Option<&'static mut BirdFlockSim>,
        Has<AutoAmbientBirds>,
    ),
>;

/// `Update`, after [`step_ambient_life`]: inserts or rebuilds each [`AmbientBirds`]' flight,
/// flies it on game time, and hands the visible birds to the draw. With
/// [`AmbientLifeSettings::auto_birds`] and no anchor of the host's, puts a flock and a soaring
/// bird near the eye (and takes them away when the host adds its own or turns that off).
///
/// [`VfxView`] is the last drawn frame's camera, so on the frame the clock restarts, or the frame
/// they appear, it is not yet the camera of the job or the scene. The automatic birds wait one frame
/// for it: what a job delivers does not depend on where the camera was before the job.
#[allow(clippy::too_many_arguments)] // ECS systems pull their inputs as parameters
pub fn step_ambient_birds(
    time: Option<Res<Time>>,
    settings: Option<Res<AmbientLifeSettings>>,
    view: Option<Res<VfxView>>,
    handle: Option<Res<AmbientLifeHandle>>,
    mut report: Option<ResMut<AmbientLifeReport>>,
    mut flocks: FlockQuery,
    mut place_auto_next: Local<bool>,
    mut commands: Commands,
) {
    let Some(settings) = settings else {
        return;
    };
    let Some(handle) = handle else {
        return;
    };
    let Ok(mut state) = handle.state.lock() else {
        return;
    };
    if !state.active || !settings.birds {
        state.bird_count = 0;
        // Nothing is drawn, so nothing is visible: no call binds to a frozen bird.
        for (.., sim, _) in &mut flocks {
            if let Some(mut sim) = sim
                && sim.visible() > 0.0
            {
                sim.set_visible(0.0);
            }
        }
        if let Some(report) = report.as_deref_mut()
            && (report.flocks != 0 || report.birds_drawn != 0)
        {
            report.flocks = 0;
            report.birds_drawn = 0;
        }
        return;
    }
    let _span = tracing::trace_span!("step_ambient_birds").entered();
    let restarted = std::mem::take(&mut state.restart_flights);
    let place_auto = std::mem::replace(&mut *place_auto_next, restarted);
    let eye = view.as_deref().map_or(Vec3::ZERO, |v| v.camera.eye);
    let forward = view
        .as_deref()
        .map_or(Vec3::NEG_Z, |v| v.camera.target - v.camera.eye);
    let mut own = 0u32;
    let mut auto = 0u32;
    for (.., is_auto) in &flocks {
        if is_auto {
            auto += 1;
        } else {
            own += 1;
        }
    }
    let drop_auto = own > 0 || !settings.auto_birds;
    if drop_auto && auto > 0 {
        for (entity, .., is_auto) in &flocks {
            if is_auto {
                commands.entity(entity).despawn();
            }
        }
    } else if own == 0 && auto == 0 && settings.auto_birds {
        let mut flock = AmbientBirds::flock();
        flock.seed = auto_seed(settings.seed, BirdMode::Flock);
        flock.height_m = AUTO_FLOCK_HEIGHT_M;
        flock.wingspan_m = AUTO_FLOCK_WINGSPAN_M;
        let mut soaring = AmbientBirds::soaring();
        soaring.seed = auto_seed(settings.seed, BirdMode::Soaring);
        commands.spawn((
            flock,
            AutoAmbientBirds,
            Transform::from_translation(auto_anchor(eye, forward, BirdMode::Flock)),
        ));
        commands.spawn((
            soaring,
            AutoAmbientBirds,
            Transform::from_translation(auto_anchor(eye, forward, BirdMode::Soaring)),
        ));
        *place_auto_next = true;
    }
    let dt = time.map_or(0.0, |t| t.delta_secs());
    let share = (state.activity.birds * settings.density_scale.clamp(0.0, 4.0)).clamp(0.0, 1.0);
    // A host's NaN draws no birds, as it draws no motes.
    let share = if share.is_nan() { 0.0 } else { share };
    let mut remaining = state.counts.birds;
    let mut written = 0usize;
    let mut flying = 0u32;
    for (entity, mut config, global, inherited, mut local, sim, is_auto) in &mut flocks {
        if is_auto && drop_auto {
            continue;
        }
        // A hidden anchor (its own or an ancestor's `Visibility::Hidden`, or a parked layer
        // member) flies no birds: the renderer's shared filter (ADR-7200, ADR-7588). Not drawn, so
        // not visible to a call, and it still counts as the host's own flock.
        if !regia_render::is_drawn(inherited) {
            if let Some(mut sim) = sim
                && sim.visible() > 0.0
            {
                sim.set_visible(0.0);
            }
            continue;
        }
        if is_auto {
            let seed = auto_seed(settings.seed, config.mode);
            if config.seed != seed {
                config.seed = seed;
            }
            if restarted {
                // Not drawn this frame: see the system's docs.
                if let Some(mut sim) = sim {
                    sim.set_visible(0.0);
                }
                continue;
            }
        }
        let mut anchor = anchor_of(global, local.as_deref());
        // The camera travelled on, or the clock restarted: the automatic birds come to it, placed
        // far ahead again.
        let away = anchor - eye;
        let recentered = is_auto && (place_auto || away.x.hypot(away.z) > AUTO_BIRDS_RECENTER_M);
        if recentered {
            anchor = auto_anchor(eye, forward, config.mode);
            if let Some(local) = local.as_deref_mut() {
                local.translation = anchor;
            }
        }
        let Some(mut sim) = sim else {
            // Its first step is this frame's, as for a flight restarted here.
            let mut sim = BirdFlockSim::new(anchor, &config);
            sim.step(dt);
            commands.entity(entity).insert(sim);
            continue;
        };
        // Against what it flies, not a change tick: this also sees a flock changed while off.
        if restarted || recentered || *sim.config() != *config {
            *sim = BirdFlockSim::new(anchor, &config);
        } else if sim.anchor().distance_squared(anchor) > 1.0e-4 {
            // A host's moving anchor carries the flight along: a restart would hold the birds.
            sim.move_anchor(anchor);
        }
        sim.step(dt);
        flying += 1;
        let visible = (sim.len() as f32 * share).min(remaining.max(0.0));
        remaining -= visible;
        sim.set_visible(visible);
        if let Some(out) = state.birds.get_mut(written..) {
            written += sim.write_instances([0.0; 3], visible, out);
        }
    }
    state.bird_count = written as u32;
    if let Some(report) = report.as_deref_mut()
        && (report.flocks != flying || report.birds_drawn != written as u32)
    {
        report.flocks = flying;
        report.birds_drawn = written as u32;
    }
}

/// `Update`, after [`step_ambient_birds`]: moves every [`FollowAmbientBird`] entity to its bird,
/// and drops the link when the flock is gone.
pub fn follow_ambient_birds(
    mut followers: Query<(Entity, &FollowAmbientBird, &mut Transform)>,
    flocks: Query<Option<&BirdFlockSim>, With<AmbientBirds>>,
    mut commands: Commands,
) {
    for (entity, follow, mut transform) in &mut followers {
        let position = match flocks.get(follow.flock) {
            // Its flight starts next frame.
            Ok(None) => continue,
            Ok(Some(sim)) => sim.position(follow.bird as usize),
            Err(_) => None,
        };
        match position {
            Some(position) => {
                if transform.translation != position {
                    transform.translation = position;
                }
            }
            None => {
                commands.entity(entity).remove::<FollowAmbientBird>();
            }
        }
    }
}

/// Installs the ambient-life systems (added by [`crate::VfxPlugin`]). Nothing runs until
/// [`AmbientLifeSettings::enabled`].
pub struct AmbientLifePlugin;

impl Plugin for AmbientLifePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AmbientLifeSettings>()
            .init_resource::<AmbientLifeHandle>()
            .add_systems(
                Update,
                (step_ambient_life, step_ambient_birds, follow_ambient_birds)
                    // No sync points in every host's `Update`: what they spawn and insert arrives
                    // by the next frame, and these read it then.
                    .chain_ignore_deferred()
                    .after(crate::precip::step_precipitation),
            );
    }
}

/// The preset mode names `render.set_ambient_birds` takes.
pub const BIRD_PRESETS: [&str; 3] = ["flock", "soaring", "single"];

impl AmbientBirds {
    /// The preset called `name` (`flock`, `soaring`, `single`).
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        BirdMode::parse(name).map(Self::preset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-7431 (3): the fallback for an unknown profile follows the platform's default preset:
    /// no ambient life where the default is a mobile preset, `High` elsewhere; and this build's
    /// fallback is that mapping of `RenderProfilePreset::platform_default`.
    #[test]
    fn an_unmatched_profile_takes_the_platform_default_presets_tier() {
        assert_eq!(
            unmatched_profile_tier(RenderProfilePreset::MobileHigh),
            QualityTier::MobileLow
        );
        assert_eq!(
            unmatched_profile_tier(RenderProfilePreset::MobileLow),
            QualityTier::MobileLow
        );
        for desktop in [
            RenderProfilePreset::DesktopLow,
            RenderProfilePreset::DesktopHigh,
            RenderProfilePreset::DesktopUltra,
            RenderProfilePreset::Deck,
            RenderProfilePreset::Cinematic,
        ] {
            assert_eq!(unmatched_profile_tier(desktop), QualityTier::High, "{desktop:?}");
        }
        let mut settings = RenderSettings::default();
        settings.profile.render_scale = 0.123;
        settings.profile.csm_resolution = 777;
        settings.profile.bloom = !settings.profile.bloom;
        settings.profile.bloom_mip_count = 13;
        let expected = unmatched_profile_tier(RenderProfilePreset::platform_default());
        assert_eq!(ambient_tier(Some(&settings)), expected);
        let android = matches!(
            RenderProfilePreset::platform_default(),
            RenderProfilePreset::MobileLow | RenderProfilePreset::MobileHigh
        );
        assert_eq!(expected == QualityTier::MobileLow, android);
    }
}
