// SPDX-License-Identifier: MPL-2.0
//! Live effects at one moment, to put them back after a stretch that must leave them where it found
//! them (ADR-2633).
//!
//! Live effects run on the render clock (ADR-1524), so a render-out job with the world held by a
//! `time_map` of scale 0 still moves them: its smoke drifts, its fires flicker. That is the look such
//! a shot asks for, and it stays. Without more, the next held job starts from where the last one left
//! the effects and draws other frames. [`LiveEffectsCapture`] holds what the effects need to play the
//! same frames again.
//!
//! What it holds: the instances of [`VfxScene`] (ages, spawn accumulators, random streams, death
//! records, pending jobs), the particle pool on the GPU with the simulation's clock and ring cursor,
//! every [`CrashSequence`], the fires and their clock, the debris clock, the decal pool, the camera
//! shake, the seed counter of spawned effects, the [`Weather`] with its transition, the surface wetness
//! and the fall offsets of rain and snow. A crash entity that went away during the stretch stays gone,
//! and static decals keep following their entities (ADR-2728).

use regia_ecs::prelude::*;

use crate::crash::CrashSequence;
use crate::debris_mesh::DebrisClock;
use crate::decal::{DecalPool, SpawnDecal};
use crate::fire_light::FireLights;
use crate::gpu::VfxPoolCapture;
use crate::plugin::{SpawnEffect, VfxSeedCounter};
use crate::precip::{PrecipIntegrator, PrecipitationHandle};
use crate::runtime::VfxGpuHandle;
use crate::scene::{VfxScene, VfxSceneCapture};
use crate::shake::{CameraShake, CameraShakeRequest};
use crate::weather::Weather;

/// What the live effects were at one moment ([`Self::capture`], [`Self::restore`]).
pub struct LiveEffectsCapture {
    scene: Option<VfxSceneCapture>,
    pool: Option<VfxPoolCapture>,
    crashes: Vec<(Entity, CrashSequence)>,
    fires: Option<FireLights>,
    debris: Option<DebrisClock>,
    decals: Option<DecalPool>,
    static_decals: Option<std::collections::HashMap<Entity, crate::decal::DecalId>>,
    shake: Option<CameraShake>,
    seeds: Option<u64>,
    weather: Option<Weather>,
    surface: Option<regia_scene::weather::SurfaceWeather>,
    precipitation: Option<PrecipIntegrator>,
}

impl LiveEffectsCapture {
    /// Takes the live effects as they stand. A host without a part of the effects plugin (no GPU
    /// context, no scene) captures the rest.
    #[must_use]
    pub fn capture(world: &mut World) -> Self {
        let _span = tracing::info_span!("vfx_capture_live_effects").entered();
        // The pool to put back must exist before the job's effects enter it (ADR-2814).
        crate::runtime::install_context_for_job(world);
        let crashes = world
            .query::<(Entity, &CrashSequence)>()
            .iter(world)
            .map(|(entity, sequence)| (entity, sequence.clone()))
            .collect();
        let pool = world
            .get_resource::<VfxGpuHandle>()
            .and_then(VfxGpuHandle::lock)
            .map(|gpu| gpu.capture_pool());
        Self {
            scene: world.get_resource::<VfxScene>().map(VfxScene::capture_instances),
            pool,
            crashes,
            fires: world.get_resource::<FireLights>().cloned(),
            debris: world.get_resource::<DebrisClock>().copied(),
            decals: world.get_resource::<DecalPool>().cloned(),
            static_decals: crate::plugin::ingested_static_decals(world),
            shake: world.get_resource::<CameraShake>().copied(),
            seeds: world.get_resource::<VfxSeedCounter>().map(|seeds| seeds.0),
            weather: world.get_resource::<Weather>().cloned(),
            surface: world
                .get_resource::<regia_scene::weather::SurfaceWeather>()
                .copied(),
            precipitation: world
                .get_resource::<PrecipitationHandle>()
                .and_then(|handle| handle.state.lock().ok().map(|state| state.integrator.clone())),
        }
    }

    /// Puts the live effects back. Requests still queued from the stretch are dropped: they belong to
    /// the effects that were just taken back.
    pub fn restore(self, world: &mut World) {
        let _span = tracing::info_span!("vfx_restore_live_effects").entered();
        let scene_back = match (self.scene, world.get_resource_mut::<VfxScene>()) {
            (Some(capture), Some(mut scene)) => scene.restore_instances(capture),
            _ => false,
        };
        // The pool and the instances name each other's emitter slots: one goes back only with the other.
        if scene_back && let Some(mut gpu) = world.get_resource::<VfxGpuHandle>().and_then(VfxGpuHandle::lock)
        {
            match self.pool {
                Some(pool) => gpu.restore_pool(pool),
                // The context was built during the stretch (a cold host): the captured scene knows
                // nothing of what it simulated.
                None => gpu.reset_pool(),
            }
        }
        for (entity, sequence) in self.crashes {
            if let Some(mut live) = world.get_mut::<CrashSequence>(entity) {
                *live = sequence;
            }
        }
        if let Some(fires) = self.fires {
            world.insert_resource(fires);
        }
        if let Some(debris) = self.debris {
            world.insert_resource(debris);
        }
        if let Some(mut decals) = self.decals {
            if let Some(captured) = self.static_decals {
                crate::plugin::resync_static_decals(world, captured, &mut decals);
            }
            world.insert_resource(decals);
        }
        if let Some(shake) = self.shake {
            world.insert_resource(shake);
        }
        if let (Some(seeds), Some(mut counter)) = (self.seeds, world.get_resource_mut::<VfxSeedCounter>()) {
            counter.0 = seeds;
        }
        if let Some(weather) = self.weather {
            world.insert_resource(weather);
        }
        if let Some(surface) = self.surface {
            world.insert_resource(surface);
        }
        if let (Some(integrator), Some(handle)) =
            (self.precipitation, world.get_resource::<PrecipitationHandle>())
            && let Ok(mut state) = handle.state.lock()
        {
            state.integrator = integrator;
        }
        clear_requests::<SpawnEffect>(world);
        clear_requests::<SpawnDecal>(world);
        clear_requests::<CameraShakeRequest>(world);
    }
}

/// The seed stream unseeded effects draw from during a render-out job ([`EffectsJobStart`]): the
/// same in every job, apart from the stream the app draws from outside jobs.
pub const JOB_SEED_STREAM: u64 = 0x2687_5eed_0f0b_0001;

/// What a render-out job restarts in the effects layer at its start, so effects it spawns look the
/// same in every job (ADR-2687), and hands back at its end.
///
/// The live effects themselves are the job's input (a plain job continues them; a held job puts
/// them back, [`LiveEffectsCapture`]). What this restarts is history that only picks how *new*
/// effects look:
///
/// - first the context itself, if it is not there yet: a job started while it builds on a warm
///   worker runs the way later jobs do (ADR-2814);
/// - the seed counter of effects spawned without a seed: during the job they draw from
///   [`JOB_SEED_STREAM`]; the app's own stream continues after the job where it stood;
/// - the instance slots, when no instance is live ([`VfxScene::restart_idle_slots`]);
/// - the particle clock and spawn ring, when no particle can be alive
///   ([`crate::gpu::VfxGpu::restart_idle_clock`]);
/// - the camera shake's noise clock, when it has no trauma ([`CameraShake::restart_idle_clock`]);
/// - the context's warm frames ([`crate::gpu::VfxGpu::hold_layer_warm_frames`]): none during the
///   job, the app's handed back at its end, with any a window resize armed meanwhile. A warm frame
///   simulates an empty layer, and each one it simulates moves the particle clock, so a new context's
///   first job (8 warm frames left) reached its first spawn at another clock, another heat-haze phase,
///   than every later job (ADR-2741, ADR-2808).
pub struct EffectsJobStart {
    seeds: Option<u64>,
    pool_restarted: bool,
}

impl EffectsJobStart {
    /// Restarts the seed stream and an idle particle clock; see the type.
    #[must_use]
    pub fn restart(world: &mut World) -> Self {
        let _span = tracing::info_span!("vfx_job_restart").entered();
        crate::runtime::install_context_for_job(world);
        let seeds = world
            .get_resource_mut::<VfxSeedCounter>()
            .map(|mut counter| std::mem::replace(&mut counter.0, JOB_SEED_STREAM));
        if let Some(mut scene) = world.get_resource_mut::<VfxScene>() {
            scene.restart_idle_slots();
        }
        // The scene's own count, not the flag its last upload left: an instance may have gone
        // since.
        let scene_live = world
            .get_resource::<VfxScene>()
            .is_some_and(|scene| scene.live_instances() > 0);
        let pool_restarted = world
            .get_resource::<VfxGpuHandle>()
            .and_then(VfxGpuHandle::lock)
            .is_some_and(|mut gpu| {
                gpu.set_scene_live(scene_live);
                // The job's layer is live only for what it draws (ADR-2741).
                gpu.hold_layer_warm_frames();
                gpu.restart_idle_clock()
            });
        if let Some(mut shake) = world.get_resource_mut::<CameraShake>() {
            shake.restart_idle_clock();
        }
        Self {
            seeds,
            pool_restarted,
        }
    }

    /// The particle clock and ring started over (nothing was alive).
    #[must_use]
    pub fn pool_restarted(&self) -> bool {
        self.pool_restarted
    }

    /// Hands the app's seed stream and warm frames back. Run after a [`LiveEffectsCapture::restore`]
    /// of the same job.
    pub fn finish(self, world: &mut World) {
        if let (Some(seeds), Some(mut counter)) = (self.seeds, world.get_resource_mut::<VfxSeedCounter>()) {
            counter.0 = seeds;
        }
        if let Some(mut gpu) = world.get_resource::<VfxGpuHandle>().and_then(VfxGpuHandle::lock) {
            gpu.release_layer_warm_frames();
        }
    }
}

fn clear_requests<M: Message>(world: &mut World) {
    if let Some(mut messages) = world.get_resource_mut::<Messages<M>>() {
        messages.clear();
    }
}

#[cfg(test)]
mod tests {
    use regia_core::math::Vec3;

    use super::*;
    use crate::crash::CrashBurst;
    use crate::fire_light::{FireLight, FireLightInstance};
    use crate::gpu::determinism_tests::scene_library;
    use crate::precip::PrecipBudget;
    use crate::scene::VfxLibrary;
    use crate::sim::LodInput;
    use crate::weather::{PrecipitationRates, WeatherConfig};
    use regia_scene::weather::SurfaceWeather;

    fn scene() -> VfxScene {
        VfxScene::new(scene_library())
    }

    #[test]
    fn restore_puts_every_part_back_and_a_host_without_parts_captures_the_rest() {
        let mut world = World::new();
        LiveEffectsCapture::capture(&mut world).restore(&mut world);

        let mut effects = scene();
        let id = effects
            .spawn("determinism", Vec3::ZERO, Vec3::Z, 7)
            .expect("spawn");
        let lod = LodInput {
            camera_pos: Vec3::ZERO,
        };
        effects.update_budgets(0.25, &lod, 1000, 1000);
        let age = effects.instance(id).expect("alive").age();
        world.insert_resource(effects);
        let mut fires = FireLights::default();
        fires.set_frame(100);
        world.insert_resource(fires);
        world.insert_resource(DebrisClock { frame: 5, auto: true });
        world.insert_resource(CameraShake::default());
        world.insert_resource(VfxSeedCounter(9));
        let crash = world.spawn(CrashSequence::new(CrashBurst::default(), 1.0)).id();

        let capture = LiveEffectsCapture::capture(&mut world);

        world
            .resource_mut::<VfxScene>()
            .update_budgets(1.0, &lod, 1000, 1000);
        let moved = world.resource::<VfxScene>().instance(id).expect("alive").age();
        assert!(moved > age);
        world.resource_mut::<FireLights>().set_frame(200);
        world.resource_mut::<FireLights>().spawn(FireLightInstance {
            position: Vec3::ZERO,
            radius: 1.0,
            light: FireLight::default(),
            start_frame: 200,
        });
        world.resource_mut::<DebrisClock>().frame = 50;
        world.resource_mut::<CameraShake>().add_trauma(0.5);
        world.resource_mut::<VfxSeedCounter>().0 = 90;
        world.get_mut::<CrashSequence>(crash).expect("crash").impact_in = 3.0;

        capture.restore(&mut world);

        let back = world.resource::<VfxScene>().instance(id).expect("alive").age();
        assert_eq!(back, age);
        let fires = world.resource::<FireLights>();
        assert_eq!(fires.frame(), 100);
        assert!(fires.fires().is_empty());
        assert_eq!(world.resource::<DebrisClock>().frame, 5);
        assert_eq!(world.resource::<CameraShake>().trauma(), 0.0);
        assert_eq!(world.resource::<VfxSeedCounter>().0, 9);
        assert_eq!(world.get::<CrashSequence>(crash).expect("crash").impact_in, 1.0);
    }

    /// ADR-2728: an owner may still hold an id from the stretch; it finds the instance gone, and no
    /// later spawn takes that id over.
    #[test]
    fn ids_of_the_stretch_are_not_used_again_after_a_restore() {
        let mut world = World::new();
        world.insert_resource(scene());
        let capture = LiveEffectsCapture::capture(&mut world);
        let during = world
            .resource_mut::<VfxScene>()
            .spawn("determinism", Vec3::ZERO, Vec3::Z, 7)
            .expect("spawn");
        capture.restore(&mut world);
        let mut effects = world.resource_mut::<VfxScene>();
        assert!(
            effects.instance(during).is_none(),
            "the stretch's instance went back"
        );
        let after = effects
            .spawn("determinism", Vec3::ZERO, Vec3::Z, 7)
            .expect("spawn");
        assert_ne!(after, during);
        assert!(effects.instance(during).is_none());
    }

    /// The parts the render clock moves besides particles: a transition, the wetness, the fall offsets.
    #[test]
    fn the_weather_and_what_falls_go_back() {
        let mut world = World::new();
        let weather = Weather::default();
        world.insert_resource(weather.clone());
        world.insert_resource(SurfaceWeather::DRY);
        let handle = PrecipitationHandle::default();
        let offsets = handle.state.lock().expect("state").integrator.clone();
        world.insert_resource(handle.clone());

        let capture = LiveEffectsCapture::capture(&mut world);

        world.resource_mut::<Weather>().step(10.0);
        world.resource_mut::<SurfaceWeather>().wetness = 0.5;
        {
            let mut state = handle.state.lock().expect("state");
            let rates = PrecipitationRates {
                rain_mm_per_h: 8.0,
                ..PrecipitationRates::NONE
            };
            let budget = PrecipBudget::for_quality(regia_render::QualityTier::High);
            state
                .integrator
                .step(&rates, &WeatherConfig::default(), [1.0, 0.0], &budget, 0.5);
            assert!(state.integrator != offsets, "the fall offsets moved");
        }
        assert!(*world.resource::<Weather>() != weather, "the clock moved");

        capture.restore(&mut world);

        assert!(*world.resource::<Weather>() == weather);
        assert_eq!(
            world.resource::<SurfaceWeather>().wetness,
            SurfaceWeather::DRY.wetness
        );
        assert!(handle.state.lock().expect("state").integrator == offsets);
    }

    #[test]
    fn a_crash_that_went_away_stays_gone() {
        let mut world = World::new();
        let crash = world.spawn(CrashSequence::new(CrashBurst::default(), 1.0)).id();
        let capture = LiveEffectsCapture::capture(&mut world);
        world.despawn(crash);
        capture.restore(&mut world);
        assert_eq!(world.query::<&CrashSequence>().iter(&world).count(), 0);
    }

    /// ADR-2687: an unseeded effect spawned in a job takes the same seed in every job, whatever the
    /// app spawned before; the app's own stream continues after the job where it stood.
    #[test]
    fn a_job_start_restarts_the_seed_stream_and_hands_the_apps_back() {
        let seed_in_job = |before: u64| {
            let mut world = World::new();
            world.insert_resource(VfxSeedCounter(before));
            let start = EffectsJobStart::restart(&mut world);
            let in_job = world.resource::<VfxSeedCounter>().0;
            start.finish(&mut world);
            assert_eq!(
                world.resource::<VfxSeedCounter>().0,
                before,
                "the app's stream is back"
            );
            in_job
        };
        // Two jobs after different amounts of play draw from the same stream.
        assert_eq!(
            seed_in_job(0),
            seed_in_job(0x9e37_79b9_7f4a_7c15_u64.wrapping_mul(5))
        );
        assert_eq!(seed_in_job(3), JOB_SEED_STREAM);
        // No counter (no effects plugin): nothing to restart.
        let mut world = World::new();
        EffectsJobStart::restart(&mut world).finish(&mut world);
        assert!(world.get_resource::<VfxSeedCounter>().is_none());
    }

    /// ADR-2687: an idle camera shake's noise clock restarts at a job; a shake in progress keeps it.
    #[test]
    fn a_job_start_restarts_an_idle_shake_clock_only() {
        let shaken_in_job = |play_before: f32| {
            let mut world = World::new();
            let mut shake = CameraShake::default();
            shake.update(play_before, 1.0, 1.0, 0.1);
            world.insert_resource(shake);
            let _start = EffectsJobStart::restart(&mut world);
            let mut shake = *world.resource::<CameraShake>();
            shake.add_trauma(1.0);
            shake.update(0.1, 1.0, 1.0, 0.1);
            shake.offset()
        };
        assert_eq!(shaken_in_job(0.0), shaken_in_job(7.3));

        let mut busy = CameraShake::default();
        busy.add_trauma(1.0);
        busy.update(0.2, 1.0, 1.0, 0.1);
        let mut kept = busy;
        assert!(!kept.restart_idle_clock(), "a shake in progress keeps its clock");
        kept.update(0.1, 1.0, 1.0, 0.1);
        busy.update(0.1, 1.0, 1.0, 0.1);
        assert_eq!(kept.offset(), busy.offset());
    }

    #[test]
    fn instances_do_not_go_back_onto_a_smaller_library() {
        let capture = scene().capture_instances();
        assert!(!VfxScene::new(VfxLibrary::new()).restore_instances(capture));
    }
}
