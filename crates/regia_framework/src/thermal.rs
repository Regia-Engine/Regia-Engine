// SPDX-License-Identifier: MPL-2.0
//! Thermal quality governor (book `user-guide/android.md`, ADR-198): steps the render scale down
//! before an Android device throttles to *severe*, and back up once it has cooled.
//!
//! [`ThermalGovernorPlugin`] reads [`regia_platform::ThermalState`] (Android's `PowerManager`
//! thermal status and 10-second headroom forecast; always idle on desktop) and scales
//! `RenderSettings.profile.render_scale` by a factor per level. The player's or preset's own value
//! stays the base: a change made by anyone else while the governor is active becomes the new base,
//! and turning the governor off restores it. Hosts turn the policy off with
//! `ThermalGovernor { enabled: false, .. }` (a resource, changeable at run time).
//!
//! Hooks (ADR-527): every level change writes a [`ThermalLevelChanged`] message, so a game can
//! shed its own load (simulation detail, particle budgets, AI tick rate) with the renderer; an
//! optional frame cap per level ([`ThermalGovernor::max_fps`], off by default) lowers
//! `FramePacing::target_fps` the same way the scale is lowered and restores it after.

use std::time::Instant;

use regia_app::prelude::*;
use regia_platform::{ThermalState, ThermalStatus};
use regia_render::RenderSettings;

/// Thermal quality policy (a resource; insert your own before `DefaultPlugins` to change it).
#[derive(Resource, Clone, Debug)]
pub struct ThermalGovernor {
    /// Run the policy (default `true`; it only acts where the OS reports thermal state).
    pub enabled: bool,
    /// Render-scale factor per level 0..=3 (level 0 must be `1.0`).
    pub factors: [f32; 4],
    /// The scale never goes below this, unless the base is lower already.
    pub min_scale: f32,
    /// Seconds the device must stay cooler than the current level before it steps back up one
    /// level (stepping down is immediate).
    pub relax_seconds: f64,
    /// Frame cap per level 0..=3 (`None` = leave the cap alone). Default: no cap at any level, so
    /// only the render scale moves; a game that prefers a steady lower rate sets e.g.
    /// `[None, None, Some(45.0), Some(30.0)]`. A cap never raises a lower cap already set.
    pub max_fps: [Option<f32>; 4],
    level: u8,
    calm_since: Option<f64>,
    base: Option<f32>,
    applied: Option<f32>,
    /// The frame cap the game asks for while the governor holds it lower.
    fps_base: Option<Option<f32>>,
    /// The cap the governor wrote, while it holds one.
    fps_applied: Option<f32>,
}

/// Written when the [`ThermalGovernor`] level changes (ADR-527): the hook for game-side load
/// shedding. Level 0 is full quality, 3 the lowest.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct ThermalLevelChanged {
    /// The level before.
    pub from: u8,
    /// The level now in effect.
    pub to: u8,
    /// The reading that moved it (the last reading when the policy was turned off).
    pub reading: ThermalState,
}

impl Default for ThermalGovernor {
    fn default() -> Self {
        Self {
            enabled: true,
            factors: [1.0, 0.85, 0.7, 0.55],
            min_scale: 0.5,
            relax_seconds: 30.0,
            max_fps: [None; 4],
            level: 0,
            calm_since: None,
            base: None,
            applied: None,
            fps_base: None,
            fps_applied: None,
        }
    }
}

impl ThermalGovernor {
    /// The level (0 = full quality … 3 = lowest) now in effect.
    pub fn level(&self) -> u8 {
        self.level
    }

    /// The render scale the player's profile asks for while the governor holds it lower; `None`
    /// when it is not holding anything down.
    pub fn base_scale(&self) -> Option<f32> {
        self.applied.and(self.base)
    }

    /// The level a reading asks for. Headroom is a forecast where 1.0 is the severe threshold, so
    /// the steps start well before it; status covers devices that report no headroom.
    pub fn wanted_level(state: &ThermalState) -> u8 {
        let by_status = match state.status {
            ThermalStatus::None | ThermalStatus::Light => 0,
            ThermalStatus::Moderate => 1,
            ThermalStatus::Severe => 2,
            _ => 3,
        };
        let by_headroom = match state.headroom {
            Some(h) if h >= 1.0 => 3,
            Some(h) if h >= 0.92 => 2,
            Some(h) if h >= 0.8 => 1,
            _ => 0,
        };
        by_status.max(by_headroom)
    }

    /// Moves the level toward `wanted` at time `now` (seconds): up at once, down one step after
    /// [`relax_seconds`](Self::relax_seconds) of being cooler. Returns the level in effect.
    pub fn advance(&mut self, wanted: u8, now: f64) -> u8 {
        let wanted = wanted.min(3);
        if wanted >= self.level {
            self.level = wanted;
            self.calm_since = None;
        } else {
            let since = *self.calm_since.get_or_insert(now);
            if now - since >= self.relax_seconds {
                self.level -= 1;
                self.calm_since = Some(now);
            }
        }
        self.level
    }

    /// The frame cap the game asks for while the governor holds a lower one; `None` when it is
    /// not holding the cap (the inner `None` is "uncapped").
    pub fn base_max_fps(&self) -> Option<Option<f32>> {
        self.fps_applied.and(self.fps_base)
    }

    /// The frame cap to run at for the game's own cap `base` at the current level.
    pub fn max_fps_for(&self, base: Option<f32>) -> Option<f32> {
        let Some(cap) = self.max_fps[usize::from(self.level.min(3))].filter(|c| c.is_finite() && *c > 0.0)
        else {
            return base;
        };
        Some(base.map_or(cap, |b| b.min(cap)))
    }

    /// The scale to render at for `base` at the current level.
    pub fn scale_for(&self, base: f32) -> f32 {
        if self.level == 0 {
            return base;
        }
        (base * self.factors[usize::from(self.level)])
            .max(self.min_scale)
            .min(base)
    }
}

/// Adds [`ThermalGovernor`] and its system (in `regia::DefaultPlugins`).
#[derive(Default)]
pub struct ThermalGovernorPlugin;

impl Plugin for ThermalGovernorPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ThermalGovernor>()
            .add_message::<ThermalLevelChanged>()
            .add_systems(Update, apply_thermal_governor);
    }
}

fn apply_thermal_governor(
    mut governor: ResMut<ThermalGovernor>,
    state: Option<Res<ThermalState>>,
    render: Option<ResMut<RenderSettings>>,
    pacing: Option<ResMut<FramePacing>>,
    mut changed: MessageWriter<ThermalLevelChanged>,
    mut epoch: Local<Option<Instant>>,
) {
    let Some(mut render) = render else {
        return;
    };
    let now = epoch.get_or_insert_with(Instant::now).elapsed().as_secs_f64();
    let reading = state.as_deref().copied().unwrap_or_default();
    let wanted = match (governor.enabled, state) {
        (true, Some(state)) => ThermalGovernor::wanted_level(&state),
        _ => 0,
    };
    let previous = governor.level;
    let level = if governor.enabled {
        governor.advance(wanted, now)
    } else {
        governor.level = 0;
        governor.calm_since = None;
        0
    };
    let current = render.profile.render_scale;
    // Someone else changed the scale while we held it down: that value is the new base.
    if governor.applied.is_none_or(|a| (current - a).abs() > 1e-4) {
        governor.base = Some(current);
    }
    let base = governor.base.unwrap_or(current);
    let target = governor.scale_for(base);
    if (target - current).abs() > 1e-4 {
        render.profile.render_scale = target;
    }
    governor.applied = (level > 0).then_some(target);
    if let Some(mut pacing) = pacing {
        let current_fps = pacing.target_fps;
        // As with the scale: a cap changed by anyone else while we hold ours is the new base.
        if governor.fps_applied.is_none_or(|a| current_fps != Some(a)) {
            governor.fps_base = Some(current_fps);
        }
        let base_fps = governor.fps_base.unwrap_or(current_fps);
        let target_fps = governor.max_fps_for(base_fps);
        if target_fps != current_fps {
            pacing.target_fps = target_fps;
        }
        governor.fps_applied = if target_fps == base_fps { None } else { target_fps };
    }
    if level != previous {
        tracing::info!(
            target: "regia_framework",
            "thermal governor: level {previous} -> {level}, render scale {current:.2} -> {target:.2}"
        );
        changed.write(ThermalLevelChanged {
            from: previous,
            to: level,
            reading,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(status: ThermalStatus, headroom: Option<f32>) -> ThermalState {
        ThermalState { status, headroom }
    }

    #[test]
    fn levels_follow_status_and_headroom() {
        let w = ThermalGovernor::wanted_level;
        assert_eq!(w(&reading(ThermalStatus::None, None)), 0);
        assert_eq!(w(&reading(ThermalStatus::None, Some(0.5))), 0);
        assert_eq!(w(&reading(ThermalStatus::None, Some(0.85))), 1);
        assert_eq!(w(&reading(ThermalStatus::Light, Some(0.95))), 2);
        assert_eq!(w(&reading(ThermalStatus::Moderate, None)), 1);
        assert_eq!(w(&reading(ThermalStatus::Severe, Some(0.1))), 2);
        assert_eq!(w(&reading(ThermalStatus::Critical, None)), 3);
    }

    #[test]
    fn steps_down_at_once_and_up_slowly() {
        let mut g = ThermalGovernor::default();
        assert_eq!(g.advance(2, 0.0), 2);
        assert_eq!(g.advance(0, 1.0), 2, "no relaxing yet");
        assert_eq!(g.advance(0, 20.0), 2);
        assert_eq!(g.advance(0, 31.0), 1, "one step after 30 s");
        assert_eq!(g.advance(0, 40.0), 1);
        assert_eq!(g.advance(2, 41.0), 2, "heat returns: immediate");
        assert_eq!(g.advance(0, 42.0), 2, "the calm timer restarted");
    }

    #[test]
    fn scale_respects_floor_and_base() {
        let mut g = ThermalGovernor::default();
        assert_eq!(g.scale_for(0.7), 0.7);
        g.level = 3;
        assert!((g.scale_for(0.7) - 0.5).abs() < 1e-6, "floor");
        assert!((g.scale_for(1.0) - 0.55).abs() < 1e-6);
        assert!((g.scale_for(0.4) - 0.4).abs() < 1e-6, "never above the base");
    }

    fn app_with(scale: f32) -> App {
        let mut app = App::new();
        let mut settings = RenderSettings::default();
        settings.profile.render_scale = scale;
        app.insert_resource(settings)
            .init_resource::<ThermalState>()
            .add_plugins(ThermalGovernorPlugin);
        app
    }

    fn scale(app: &App) -> f32 {
        app.world().resource::<RenderSettings>().profile.render_scale
    }

    #[test]
    fn heat_lowers_the_scale_and_disabling_restores_it() {
        let mut app = app_with(1.0);
        app.update();
        assert_eq!(scale(&app), 1.0);
        *app.world_mut().resource_mut::<ThermalState>() = reading(ThermalStatus::Moderate, None);
        app.update();
        assert!((scale(&app) - 0.85).abs() < 1e-6);
        app.world_mut().resource_mut::<ThermalGovernor>().enabled = false;
        app.update();
        assert_eq!(scale(&app), 1.0, "policy off: the base returns");
    }

    #[derive(Resource, Default)]
    struct Seen(Vec<(u8, u8)>);

    #[test]
    fn level_changes_are_written_as_messages() {
        let mut app = app_with(1.0);
        app.init_resource::<Seen>().add_systems(
            PostUpdate,
            |mut r: MessageReader<ThermalLevelChanged>, mut seen: ResMut<Seen>| {
                seen.0.extend(r.read().map(|m| (m.from, m.to)));
            },
        );
        app.update();
        *app.world_mut().resource_mut::<ThermalState>() = reading(ThermalStatus::Critical, None);
        app.update();
        app.update();
        app.world_mut().resource_mut::<ThermalGovernor>().enabled = false;
        app.update();
        assert_eq!(
            app.world().resource::<Seen>().0,
            [(0, 3), (3, 0)],
            "one message per change"
        );
    }

    #[test]
    fn frame_caps_are_off_by_default_and_applied_per_level_when_set() {
        let mut app = app_with(1.0);
        app.insert_resource(FramePacing::new(Some(60.0)));
        *app.world_mut().resource_mut::<ThermalState>() = reading(ThermalStatus::Critical, None);
        app.update();
        let fps = |app: &App| app.world().resource::<FramePacing>().target_fps;
        assert_eq!(
            fps(&app),
            Some(60.0),
            "no cap configured: the frame rate is left alone"
        );

        app.world_mut().resource_mut::<ThermalGovernor>().max_fps = [None, None, Some(45.0), Some(30.0)];
        app.update();
        assert_eq!(fps(&app), Some(30.0), "level 3 cap");
        assert_eq!(
            app.world().resource::<ThermalGovernor>().base_max_fps(),
            Some(Some(60.0))
        );
        app.world_mut().resource_mut::<ThermalGovernor>().enabled = false;
        app.update();
        assert_eq!(fps(&app), Some(60.0), "policy off: the game's cap returns");
    }

    #[test]
    fn a_cap_never_raises_a_lower_one() {
        let mut g = ThermalGovernor {
            max_fps: [None, Some(45.0), Some(45.0), Some(30.0)],
            ..Default::default()
        };
        assert_eq!(g.max_fps_for(Some(60.0)), Some(60.0), "level 0: no cap");
        g.level = 1;
        assert_eq!(g.max_fps_for(Some(60.0)), Some(45.0));
        assert_eq!(g.max_fps_for(Some(30.0)), Some(30.0));
        assert_eq!(g.max_fps_for(None), Some(45.0), "uncapped gets the cap");
    }

    #[test]
    fn an_outside_change_becomes_the_new_base() {
        let mut app = app_with(1.0);
        *app.world_mut().resource_mut::<ThermalState>() = reading(ThermalStatus::Severe, None);
        app.update();
        assert!((scale(&app) - 0.7).abs() < 1e-6);
        app.world_mut()
            .resource_mut::<RenderSettings>()
            .profile
            .render_scale = 0.8;
        app.update();
        assert!((scale(&app) - 0.56).abs() < 1e-6, "0.8 x 0.7 = {}", scale(&app));
    }
}
