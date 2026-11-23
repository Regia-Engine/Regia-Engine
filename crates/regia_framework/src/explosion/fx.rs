// SPDX-License-Identifier: MPL-2.0
//! The visual and camera half of an explosion (feature `explosion-fx`): the `regia_vfx::crash`
//! layers at the scale of the charge, and the ground-shock mount shake at the first camera rig.
//! A replica plays both; they change no simulation state.

use regia_app::prelude::*;
use regia_scene::Transform;
use regia_vfx::crash::{CrashBurst, CrashScale, CrashTier, ensure_crash_effects, spawn_crash};
use regia_vfx::{VfxScene, VfxSettings};

use super::{ExplosionEvent, ExplosionStats, apply_explosions};
use crate::camera::CameraRig;
use crate::ground_shock::GroundShock;
use crate::shock_shake::ShockShakeSchedule;

/// TNT-equivalent kilograms to kilotons (the unit of [`GroundShock::yield_kt`]).
const KG_TO_KT: f32 = 1.0e-6;

/// Plays the effects of every [`ExplosionEvent`]: add it with [`super::ExplosionPlugin`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ExplosionFxPlugin;

impl Plugin for ExplosionFxPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            FixedUpdate,
            play_explosion_fx
                .after(apply_explosions)
                .before(regia_physics::PhysicsStep),
        );
    }
}

/// Fires the crash layers and schedules the camera shake of this step's explosions (span
/// `play_explosion_fx`). Without a `VfxScene`, a camera rig or a `ShockShakeSchedule` the
/// matching half is skipped.
pub fn play_explosion_fx(
    mut events: MessageReader<ExplosionEvent>,
    scene: Option<ResMut<VfxScene>>,
    settings: Option<Res<VfxSettings>>,
    schedule: Option<ResMut<ShockShakeSchedule>>,
    cameras: Query<&Transform, With<CameraRig>>,
    mut stats: ResMut<ExplosionStats>,
) {
    if events.is_empty() {
        return;
    }
    let _span = tracing::debug_span!("play_explosion_fx").entered();
    let tier = settings.map_or(CrashTier::Full, |s| CrashTier::for_profile(&s.profile));
    let mut scene = scene;
    let mut schedule = schedule;
    for ExplosionEvent(explosion) in events.read() {
        if !explosion.fx || explosion.validate().is_err() || explosion.tnt_kg <= 0.0 {
            continue;
        }
        if let Some(scene) = scene.as_deref_mut() {
            let scale = CrashScale::from_tonnes(explosion.tnt_kg / 1000.0);
            ensure_crash_effects(scene, scale, tier, None);
            let burst = CrashBurst {
                position: explosion.center,
                scale,
                seed: explosion.seed,
                ..CrashBurst::default()
            };
            stats.fx_played += spawn_crash(scene, &burst).len() as u64;
        }
        if let (Some(schedule), Some(camera)) = (schedule.as_deref_mut(), cameras.iter().next()) {
            let law = GroundShock {
                yield_kt: explosion.tnt_kg * KG_TO_KT,
                ..GroundShock::default()
            };
            let distance = camera.translation.distance(explosion.center);
            let now = schedule.frame();
            let (seismic, air) = schedule.schedule_ground(&law, distance, now);
            stats.shakes += u64::from(seismic.is_some()) + u64::from(air.is_some());
        }
    }
}
